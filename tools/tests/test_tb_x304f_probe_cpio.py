"""Host-only tests. Never execute the wrapper's PID1/device-only body."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import os
from pathlib import Path
import shutil
import stat
import struct
import subprocess
import tempfile
import unittest


TOOLS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    'tb_x304f_probe_cpio', TOOLS / 'tb_x304f_probe_cpio.py')
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
WRAPPER = (TOOLS / 'tb-x304f-probe-init.sh').read_bytes()
ELF = struct.pack(
    '<16sHHIQQQIHHHHHH', b'\x7fELF\x02\x01\x01' + bytes(9),
    2, 183, 1, 0x400000, 64, 0, 0, 64, 56, 0, 0, 0, 0
) + b'original init payload\0\xff'
BUSYBOX = ELF + b'busybox fixture'


def record(name, contents=b'', mode=stat.S_IFREG | 0o755, nlink=1):
    """Independent newc fixture writer, not the implementation's encoder."""
    name = name.encode() + b'\0'
    values = [17, mode, 12, 34, nlink, 123456, len(contents),
              0, 0, 0, 0, len(name), 0]
    result = b'070701' + b''.join(b'%08x' % value for value in values) + name
    result += bytes(-len(result) % 4)
    result += contents
    return result + bytes(-len(result) % 4)


TRAILER = record('TRAILER!!!', mode=0)


def archive(*, init=ELF, init_mode=stat.S_IFREG | 0o755,
            busybox=BUSYBOX, busybox_mode=stat.S_IFREG | 0o755,
            extra=b''):
    result = record('bin', mode=stat.S_IFDIR | 0o755)
    if init is not None:
        result += record('init', init, init_mode)
    if busybox is not None:
        result += record('bin/busybox', busybox, busybox_mode)
    return result + extra + TRAILER


def decode_overlay(data):
    """Independent strict decoder for the three records the builder appends."""
    result = []
    offset = 0
    while offset < len(data):
        assert offset % 4 == 0
        header = data[offset:offset + 110]
        assert len(header) == 110 and header[:6] == b'070701'
        fields = [int(header[6 + 8*i:14 + 8*i], 16) for i in range(13)]
        namesize, size = fields[11], fields[6]
        name_start = offset + 110
        name_end = name_start + namesize
        name = data[name_start:name_end]
        assert name.endswith(b'\0')
        data_start = (name_end + 3) // 4 * 4
        data_end = data_start + size
        offset = (data_end + 3) // 4 * 4
        assert offset <= len(data)
        assert not any(data[name_end:data_start] + data[data_end:offset])
        result.append((name[:-1].decode(), fields, data[data_start:data_end]))
    assert result[-1][0] == 'TRAILER!!!' and not result[-1][2]
    return result


class ArchiveTests(unittest.TestCase):
    def test_overlay_preserves_full_base_original_init_and_metadata(self):
        for padding in (0, 1, 2, 3, 512):
            with self.subTest(padding=padding):
                base = archive(extra=record('bin/sh', b'busybox',
                                            stat.S_IFLNK | 0o777)) + bytes(padding)
                result = probe.diagnostic_archive(base, WRAPPER)
                self.assertEqual(result[:len(base)], base)
                overlay_offset = (len(base) + 3) // 4 * 4
                overlay = decode_overlay(result[overlay_offset:])
                self.assertEqual([entry[0] for entry in overlay],
                                 ['pocketboot.real-init', 'init', 'TRAILER!!!'])
                self.assertEqual(overlay[0][2], ELF)
                self.assertEqual(overlay[0][1][1:6],
                                 [stat.S_IFREG | 0o755, 12, 34, 1, 123456])
                self.assertEqual(overlay[1][2], WRAPPER)
                self.assertEqual(overlay[1][1][1], stat.S_IFREG | 0o755)
                effective = {entry.name: entry for entry in probe.parse_newc(result)}
                self.assertEqual(effective['init'].data, WRAPPER)
                self.assertEqual(effective['pocketboot.real-init'].data, ELF)
                self.assertEqual(effective['bin/busybox'].data, BUSYBOX)

    def test_only_exact_trailer_name_terminates_archive(self):
        base = archive(extra=record('./TRAILER!!!', b'ordinary file'))
        result = probe.diagnostic_archive(base, WRAPPER)
        self.assertEqual(result[:len(base)], base)
        with self.assertRaises(ValueError):
            probe.diagnostic_archive(base[:-len(TRAILER)], WRAPPER)

    def test_all_truncation_points_before_complete_trailer_are_rejected(self):
        base = archive()
        for end in range(len(base)):
            with self.subTest(end=end), self.assertRaises(ValueError):
                probe.diagnostic_archive(base[:end], WRAPPER)

    def test_malformed_headers_names_trailer_and_trailing_data(self):
        base = archive()
        bad_hex = base[:6] + b'+' + base[7:]
        bad_name = base[:113] + b'x' + base[114:]  # "bin" loses its NUL
        bad_checksum = base[:102] + b'00000001' + base[110:]
        bad_namesize = base[:94] + b'00000000' + base[102:]
        oversized_data = base[:54] + b'ffffffff' + base[62:]
        cases = [
            b'070702' + base[6:], bad_hex, bad_name, bad_checksum,
            bad_namesize, oversized_data, base + b'garbage',
            base + b'\0' + archive(), base[:-len(TRAILER)],
            base[:-len(TRAILER)] + record('TRAILER!!!', b'x', mode=0),
            archive(extra=record('../escape', b'never extracted')),
            archive(extra=record('bad\0name', b'never extracted')),
        ]
        for case in cases:
            with self.subTest(case=case[:120]), self.assertRaises(ValueError):
                probe.diagnostic_archive(case, WRAPPER)

    def test_required_members_must_be_present_regular_and_executable(self):
        for name in ('init', 'busybox'):
            for contents in (None, b''):
                with self.subTest(name=name, contents=contents):
                    with self.assertRaises(ValueError):
                        probe.diagnostic_archive(archive(**{name: contents}), WRAPPER)
            for mode in (stat.S_IFREG | 0o644, stat.S_IFLNK | 0o777,
                         stat.S_IFDIR | 0o755):
                with self.subTest(name=name, mode=mode):
                    with self.assertRaisesRegex(ValueError, 'regular executable'):
                        probe.diagnostic_archive(
                            archive(**{name + '_mode': mode}), WRAPPER)

    def test_required_executables_must_be_aarch64_elf(self):
        for name in ('init', 'busybox'):
            for contents in (b'#!/bin/sh\n', ELF[:40],
                             ELF[:18] + b'\x3e\0' + ELF[20:],
                             ELF[:4] + b'\x01' + ELF[5:],
                             ELF[:16] + b'\x01\0' + ELF[18:]):
                with self.subTest(name=name, contents=contents):
                    with self.assertRaisesRegex(ValueError, 'AArch64'):
                        probe.diagnostic_archive(archive(**{name: contents}), WRAPPER)

    def test_refuses_ambiguous_aliases_and_hardlinks(self):
        for extra in (record('./init', ELF), record('/init', ELF),
                      record('bin', b'elsewhere', stat.S_IFLNK | 0o777)):
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                probe.diagnostic_archive(archive(extra=extra), WRAPPER)
        base = record('init', ELF, nlink=2)
        base += record('bin/busybox', b'busybox') + TRAILER
        with self.assertRaisesRegex(ValueError, 'hardlinked'):
            probe.diagnostic_archive(base, WRAPPER)

    def test_refuses_already_wrapped_in_same_or_appended_archive(self):
        for base in (
            archive(extra=record('pocketboot.real-init', ELF)),
            archive(extra=record('./pocketboot.real-init', ELF)),
            probe.diagnostic_archive(archive(), WRAPPER),
        ):
            with self.subTest(base=base[:120]):
                with self.assertRaisesRegex(ValueError, 'already-wrapped'):
                    probe.diagnostic_archive(base, WRAPPER)


class FileAndCLITests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.input = Path(self.temp.name) / 'base.cpio'
        self.output = Path(self.temp.name) / 'diagnostic.cpio'
        self.base = archive()
        self.input.write_bytes(self.base)

    def test_cli_round_trip_leaves_input_unchanged(self):
        with redirect_stdout(io.StringIO()):
            self.assertEqual(probe.main(['--input', str(self.input),
                                         '--output', str(self.output)]), 0)
        self.assertEqual(self.input.read_bytes(), self.base)
        self.assertEqual(self.output.read_bytes(),
                         probe.diagnostic_archive(self.base, WRAPPER))

    def test_existing_output_and_same_input_are_never_overwritten(self):
        self.output.write_bytes(b'keep this')
        with self.assertRaises(FileExistsError):
            probe.build(self.input, self.output)
        self.assertEqual(self.output.read_bytes(), b'keep this')
        with self.assertRaisesRegex(ValueError, 'different paths'):
            probe.build(self.input, self.input)
        self.assertEqual(self.input.read_bytes(), self.base)

    def test_output_symlinks_and_hardlinks_are_not_followed(self):
        self.output.symlink_to(self.input)
        with self.assertRaises(ValueError):
            probe.build(self.input, self.output)
        self.output.unlink()
        os.link(self.input, self.output)
        with self.assertRaises(FileExistsError):
            probe.build(self.input, self.output)
        self.output.unlink()
        missing = self.output.with_name('does-not-exist')
        self.output.symlink_to(missing)
        with self.assertRaises(FileExistsError):
            probe.build(self.input, self.output)
        self.assertFalse(missing.exists())
        self.assertEqual(self.input.read_bytes(), self.base)

    def test_bad_input_does_not_create_output(self):
        self.input.write_bytes(b'not cpio')
        errors = io.StringIO()
        with redirect_stderr(errors), self.assertRaises(SystemExit) as error:
            probe.main(['--input', str(self.input), '--output', str(self.output)])
        self.assertEqual(error.exception.code, 1)
        self.assertIn('truncated', errors.getvalue())
        self.assertFalse(self.output.exists())
        self.assertEqual(self.input.read_bytes(), b'not cpio')

    def test_help_warns_about_opt_in_panic_and_cancellation(self):
        output = io.StringIO()
        with redirect_stdout(output), self.assertRaises(SystemExit) as result:
            probe.main(['--help'])
        self.assertEqual(result.exception.code, 0)
        for text in ('WARNING', 'PANICS', 'pocketboot.probe', 'panic=-1',
                     '/run/tbx304f-probe.keep', 'oem shell:'):
            self.assertIn(text, output.getvalue())


class WrapperSafetyTests(unittest.TestCase):
    def test_shell_syntax(self):
        shell = shutil.which('sh')
        if shell is None:
            self.skipTest('no host sh for syntax check')
        result = subprocess.run([shell, '-n', str(probe.WRAPPER)],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_non_pid1_refusal_before_any_diagnostic_action(self):
        shell = shutil.which('sh')
        if shell is None:
            self.skipTest('no host sh for refusal check')
        # A normal child process only. No namespaces, sourcing, root privileges,
        # device mocks or execution of any code after the early PID1 guard.
        result = subprocess.run([shell, '-x', str(probe.WRAPPER)],
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 1)
        self.assertIn('refusing to run', result.stderr)
        self.assertEqual(result.stdout, '')
        for forbidden in ('+ probe', '+ exec', '/bin/busybox sleep',
                          '/dev/kmsg', '/proc/sysrq-trigger'):
            self.assertNotIn(forbidden, result.stderr)


if __name__ == '__main__':
    unittest.main()
