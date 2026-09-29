from contextlib import redirect_stderr, redirect_stdout
import gzip
import importlib.util
import io
import json
from pathlib import Path
import struct
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('ramoops', ROOT / 'tools/ramoops_decode.py')
ramoops = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ramoops)


class RamoopsCLITests(unittest.TestCase):
    def test_no_ecc_needs_neither_kernel_nor_native_codec(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            raw = bytearray(0x80000)
            message = b'kernel console test\n'
            struct.pack_into('<III', raw, 0x40000, 0x43474244,
                             len(message), len(message))
            raw[0x4000c:0x4000c + len(message)] = message
            source = root / 'capture.raw'
            source.write_bytes(raw)
            output = root / 'decoded'
            argv = ['ramoops_decode', '--input', str(source),
                    '--output', str(output), '--ecc', '0']
            with mock.patch.object(sys, 'argv', argv), \
                    mock.patch.object(ramoops, 'Codec') as codec, \
                    redirect_stdout(io.StringIO()):
                ramoops.main()
            codec.assert_not_called()
            self.assertEqual((output / 'console.log').read_bytes(), message)
            result = json.loads((output / 'decoded.json').read_text())
            self.assertEqual(result['ecc_size'], 0)
            self.assertNotIn('error', result['regions']['console'])
            self.assertEqual(source.read_bytes(), raw)

    def test_ecc_requires_kernel_before_creating_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'decoded'
            argv = ['ramoops_decode', '--input', 'unused.raw',
                    '--output', str(output)]
            errors = io.StringIO()
            with mock.patch.object(sys, 'argv', argv), \
                    mock.patch.object(ramoops, 'Codec') as codec, \
                    redirect_stderr(errors), self.assertRaises(SystemExit) as error:
                ramoops.main()
            self.assertEqual(error.exception.code, 2)
            self.assertIn('--kernel is required', errors.getvalue())
            self.assertFalse(output.exists())
            codec.assert_not_called()

    def test_ecc_still_uses_the_supplied_kernel_codec(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / 'capture.raw'
            source.write_bytes(b'capture')
            output = root / 'decoded'
            kernel = root / 'kernel'
            argv = ['ramoops_decode', '--input', str(source),
                    '--output', str(output), '--kernel', str(kernel)]
            with mock.patch.object(sys, 'argv', argv), \
                    mock.patch.object(ramoops, 'Codec') as codec, \
                    mock.patch.object(ramoops, 'decode_capture', return_value={}) as decode, \
                    redirect_stdout(io.StringIO()):
                ramoops.main()
            codec.assert_called_once()
            self.assertEqual(codec.call_args.args[0], kernel)
            decode.assert_called_once_with(b'capture', 64, codec.return_value, output)


class RamoopsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.codec = ramoops.Codec(ROOT / 'target/kernel/src/msm8916', cls.temp.name)
        fixture = Path(__file__).with_name('fixtures') / 'a5u-controlled-panic.bin.gz'
        cls.raw = gzip.decompress(fixture.read_bytes())

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_real_kernel_panic_and_compression(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            result = ramoops.decode_capture(self.raw, 64, self.codec, out)
            self.assertIn(b'Kernel panic - not syncing: sysrq triggered crash',
                          (out / 'dmesg-0.log').read_bytes())
            self.assertIn(b'Rebooting in 5 seconds', (out / 'console.log').read_bytes())
            self.assertFalse(any('error' in zone for zone in result['regions'].values()))

    def test_real_kernel_ecc_recovers_header_data_and_parity(self):
        zone = self.raw[:0x2000]
        expected, _ = ramoops.decode_zone(zone, 64, self.codec)
        damaged = bytearray(zone)
        for offset in [0, 4, 8, 12, 18, 35, 0x1fff]:
            damaged[offset] ^= 0xa5
        actual, info = ramoops.decode_zone(damaged, 64, self.codec)
        self.assertEqual(actual, expected)
        self.assertEqual(info['corrected_symbols'], 7)
        self.assertEqual(info['uncorrectable_blocks'], [])

    def test_uncorrectable_block_is_exposed(self):
        damaged = bytearray(self.raw[:0x2000])
        for offset in range(12, 77):
            damaged[offset] ^= 0xa5
        _, info = ramoops.decode_zone(damaged, 64, self.codec)
        self.assertIn(0, info['uncorrectable_blocks'])

    def test_truncated_capture_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError):
                ramoops.decode_capture(self.raw[:-1], 64, self.codec, Path(tmp))


if __name__ == '__main__':
    unittest.main()
