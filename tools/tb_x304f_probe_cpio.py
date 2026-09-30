#!/usr/bin/env python3
"""Append a lab-only TB-X304F diagnostic /init overlay without extracting files."""

import argparse
from pathlib import Path, PurePosixPath
import stat
import struct
import sys
from typing import NamedTuple


WRAPPER = Path(__file__).with_name('tb-x304f-probe-init.sh')
REAL_INIT = 'pocketboot.real-init'
HEADER_SIZE = 110


class Entry(NamedTuple):
    name: str
    mode: int
    uid: int
    gid: int
    nlink: int
    mtime: int
    data: bytes


def align4(size):
    return (size + 3) & ~3


def archive_name(raw):
    if not raw or raw[-1:] != b'\0' or b'\0' in raw[:-1]:
        raise ValueError('invalid newc filename')
    name = raw[:-1].decode('utf-8', errors='surrogateescape')
    if not name or '..' in name.split('/'):
        raise ValueError('empty or traversing newc filename')
    # /init and ./init identify the same initramfs member as init.
    return str(PurePosixPath('/' + name.lstrip('/'))).lstrip('/')


def parse_newc(data):
    """Read complete uncompressed newc archive(s), including each trailer.

    Multiple archives are legal in Linux initramfs. Accept them here so an
    already-wrapped archive is detected even when its saved init is in an
    appended overlay. No archive member is extracted to the host filesystem.
    """
    entries = []
    offset = 0
    while True:
        while True:
            if offset + HEADER_SIZE > len(data):
                raise ValueError('truncated newc header or missing trailer')
            header = data[offset:offset + HEADER_SIZE]
            if header[:6] != b'070701':
                raise ValueError('expected uncompressed newc magic 070701')
            if any(c not in b'0123456789abcdefABCDEF' for c in header[6:]):
                raise ValueError('invalid hexadecimal newc header')
            fields = [int(header[i:i + 8], 16) for i in range(6, HEADER_SIZE, 8)]
            (_, mode, uid, gid, nlink, mtime, size, _, _, _, _,
             namesize, check) = fields
            if namesize < 2 or check != 0:
                raise ValueError('invalid newc namesize or checksum')
            name_start = offset + HEADER_SIZE
            name_end = name_start + namesize
            data_start = align4(name_end)
            data_end = data_start + size
            next_offset = align4(data_end)
            if next_offset > len(data):
                raise ValueError('truncated newc name, data or alignment padding')
            raw_name = data[name_start:name_end]
            name = archive_name(raw_name)
            contents = data[data_start:data_end]
            offset = next_offset
            if raw_name == b'TRAILER!!!\0':
                if size != 0:
                    raise ValueError('newc trailer must have no data')
                break
            entries.append(Entry(name, mode, uid, gid, nlink, mtime, contents))

        # cpio tools may pad archives to a larger block size with NULs.
        while offset < len(data) and data[offset] == 0:
            offset += 1
        if offset == len(data):
            return entries
        if offset % 4:
            raise ValueError('unaligned concatenated newc archive')


def executable(entries, name):
    matches = [entry for entry in entries if entry.name == name]
    if len(matches) != 1:
        raise ValueError(f'expected exactly one /{name}, found {len(matches)}')
    entry = matches[0]
    if not stat.S_ISREG(entry.mode) or not entry.mode & 0o111:
        raise ValueError(f'/{name} must be a regular executable file')
    # Avoid aliasing the overwritten init or relying on hardlink data ordering.
    if entry.nlink != 1:
        raise ValueError(f'/{name} must not be hardlinked')
    if not entry.data:
        raise ValueError(f'/{name} must not be empty')
    return entry


def check_aarch64_elf(data, name):
    # Verify the executable's identity, not every ELF segment/section. Its bytes
    # are copied verbatim; this tool is not an ELF loader or a cross-linker.
    if (len(data) < 64 or data[:7] != b'\x7fELF\x02\x01\x01'
            or struct.unpack_from('<HHI', data, 16) not in
            ((2, 183, 1), (3, 183, 1))
            or struct.unpack_from('<H', data, 52)[0] != 64):
        raise ValueError(f'/{name} must be a little-endian ELF64 AArch64 executable')


def newc_entry(name, mode, contents, *, ino, uid=0, gid=0, mtime=0):
    encoded_name = name.encode('ascii') + b'\0'
    fields = (ino, mode, uid, gid, 1, mtime, len(contents),
              0, 0, 0, 0, len(encoded_name), 0)
    if any(value < 0 or value > 0xffffffff for value in fields):
        raise ValueError('newc field exceeds 32 bits')
    result = b'070701' + ''.join(f'{value:08x}' for value in fields).encode('ascii')
    result += encoded_name
    result += b'\0' * (align4(len(result)) - len(result))
    result += contents
    return result + b'\0' * (align4(len(result)) - len(result))


def diagnostic_archive(base, wrapper):
    entries = parse_newc(base)
    if any(entry.name == REAL_INIT or entry.name.startswith(REAL_INIT + '/')
           for entry in entries):
        raise ValueError('already-wrapped input: /pocketboot.real-init exists')
    init = executable(entries, 'init')
    busybox = executable(entries, 'bin/busybox')
    if any(entry.name == 'bin' and not stat.S_ISDIR(entry.mode)
           for entry in entries):
        raise ValueError('/bin must be a directory, not an alias')
    for executable_entry in (init, busybox):
        check_aarch64_elf(executable_entry.data, executable_entry.name)
    if not wrapper.startswith(b'#!/bin/busybox sh\n'):
        raise ValueError('unexpected diagnostic wrapper interpreter')

    # Keep the original archive, trailer and padding byte-for-byte. Linux
    # unpacks the second archive afterward, replacing /init with the wrapper.
    overlay = newc_entry(REAL_INIT, init.mode, init.data, ino=1,
                         uid=init.uid, gid=init.gid, mtime=init.mtime)
    overlay += newc_entry('init', stat.S_IFREG | 0o755, wrapper, ino=2,
                          mtime=init.mtime)
    overlay += newc_entry('TRAILER!!!', 0, b'', ino=3)
    return base + b'\0' * (align4(len(base)) - len(base)) + overlay


def build(input_path, output_path):
    input_path, output_path = Path(input_path), Path(output_path)
    if input_path.resolve() == output_path.resolve():
        raise ValueError('input and output must be different paths')
    base = input_path.read_bytes()
    result = diagnostic_archive(base, WRAPPER.read_bytes())
    # O_EXCL also refuses hardlink aliases, dangling symlinks and racing writers.
    # Validate everything before creating any output.
    with output_path.open('xb') as output:
        output.write(result)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description='Build a separate, opt-in TB-X304F diagnostic initramfs.',
        epilog=(
            'WARNING: this lab wrapper intentionally PANICS about 30s after its '
            'guards pass (lenovo,tbx304x, pocketboot.probe, panic=-1). A verified '
            'PocketBoot USB session can cancel via oem shell: by creating '
            '/run/tbx304f-probe.keep before the deadline. Optional '
            'pocketboot.probe-role-device requests device role on the matched '
            'ci_hdrc.0 role switch after the first snapshot; '
            'pocketboot.probe-reprobe-mmc retries only unbound 7824900.mmc after '
            'RPMPD is bound and the second snapshot is logged. This is not recovery '
            'from a hung kernel. Do not replace a normal boot artifact.'))
    parser.add_argument('--input', type=Path, required=True,
                        help='existing uncompressed newc base (never modified)')
    parser.add_argument('--output', type=Path, required=True,
                        help='new diagnostic cpio path; must not already exist')
    args = parser.parse_args(argv)
    try:
        build(args.input, args.output)
    except (OSError, ValueError) as error:
        parser.exit(1, f'{parser.prog}: {error}\n')
    print(f'Wrote lab-only diagnostic archive: {args.output}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
