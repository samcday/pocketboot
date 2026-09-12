#!/usr/bin/env python3
"""Decode an lk2nd 512 KiB raw ramoops capture with the pinned Linux RS codec.

Read-only offline tool. Preserve raw captures: ECC cannot guarantee recovery
outside its correction bound. Invalid records are reported, never silently used.
"""
import argparse
import ctypes
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import tempfile
import zlib


class Codec:
    def __init__(self, kernel, directory):
        source = Path(__file__).with_name('ramoops_rs.c')
        library = Path(directory) / 'ramoops-rs.so'
        subprocess.run(['cc', '-O2', '-shared', '-fPIC', '-Wall', '-Wextra',
                        '-I', str(Path(kernel) / 'lib/reed_solomon'),
                        str(source), '-o', str(library)], check=True)
        self.library = ctypes.CDLL(str(library))
        self.library.pb_rs.argtypes = [ctypes.c_void_p, ctypes.c_int,
                                      ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
        self.library.pb_rs.restype = ctypes.c_int

    def apply(self, data, parity, encode=False):
        buf = ctypes.create_string_buffer(bytes(data), len(data))
        par = ctypes.create_string_buffer(bytes(parity), len(parity))
        count = self.library.pb_rs(buf, len(data), par, len(parity), encode)
        if count < 0:
            raise ValueError(f'uncorrectable RS block ({count})')
        return buf.raw, par.raw, count


def decode_zone(raw, ecc, codec):
    capacity = len(raw) - 12
    blocks = (capacity - ecc + 128 + ecc - 1) // (128 + ecc) if ecc else 0
    capacity -= (blocks + 1) * ecc if ecc else 0
    parity_base = 12 + capacity
    header = raw[:12]
    corrected = 0
    if ecc:
        parity_header = parity_base + blocks * ecc
        header, _, corrected = codec.apply(header, raw[parity_header:parity_header + ecc])
    signature, start, size = struct.unpack('<III', header)
    if signature != 0x43474244 or size > capacity or start > size:
        raise ValueError(f'invalid header sig={signature:08x} start={start} size={size}')
    data = bytearray(raw[12:12 + capacity])
    bad_blocks = []
    if ecc:
        for offset in range(0, size, 128):
            end = min(offset + 128, capacity)
            parity_offset = parity_base + offset // 128 * ecc
            try:
                fixed, _, count = codec.apply(data[offset:end], raw[parity_offset:parity_offset + ecc])
                data[offset:end] = fixed
                corrected += count
            except ValueError:
                bad_blocks.append(offset)
    # Kernel persistent_ram_save_old copies [start:size], then [0:start].
    content = bytes(data[start:size] + data[:start])
    return content, dict(size=size, start=start, capacity=capacity,
                         corrected_symbols=corrected, uncorrectable_blocks=bad_blocks)


def decode_capture(raw, ecc, codec, output):
    if len(raw) != 0x80000:
        raise ValueError('expected the complete 512 KiB lk2nd ramoops raw window')
    records = {}
    regions = [(f'dmesg-{i}', i * 0x2000, 0x2000) for i in range(32)]
    regions.append(('console', 0x40000, 0x40000))
    for name, offset, size in regions:
        try:
            content, info = decode_zone(raw[offset:offset + size], ecc, codec)
            records[name] = info
            if not content:
                continue
            (output / f'{name}.bin').write_bytes(content)
            if info['uncorrectable_blocks']:
                # Retain bytes explicitly labelled damaged, without treating
                # compressed or corrupted data as a verified log.
                (output / f'{name}.damaged').write_bytes(content)
                continue
            if name.startswith('dmesg'):
                header, content = content.split(b'\n', 1)
                if not header.startswith(b'===='):
                    raise ValueError('missing ramoops timestamp')
                info['ramoops_header'] = header.decode('ascii')
                if header.endswith(b'-C'):
                    # Linux fs/pstore/platform.c stores a raw RFC1951 stream.
                    content = zlib.decompress(content, -zlib.MAX_WBITS)
            (output / f'{name}.log').write_bytes(content)
        except (ValueError, zlib.error) as error:
            records.setdefault(name, {})['error'] = str(error)
    result = {'raw_sha256': hashlib.sha256(raw).hexdigest(), 'ecc_size': ecc, 'regions': records}
    (output / 'decoded.json').write_text(json.dumps(result, indent=2) + '\n')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--ecc', type=int, default=64, choices=[0, 16, 32, 64])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory() as tmp:
        codec = Codec(args.kernel, tmp)
        result = decode_capture(args.input.read_bytes(), args.ecc, codec, args.output)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
