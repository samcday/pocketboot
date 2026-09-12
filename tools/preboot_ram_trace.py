#!/usr/bin/env python3
"""Read/decode Pocketpreboot's optional triplicated RAM trace after a reset.

Read mode requires an explicit lk2nd serial, compatible and physical address
obtained from the exact shim ELF plus its load address. Only getvar and debug
readq commands are sent. Firmware can corrupt retained RAM: console text is
accepted only with its FNV-1a checksum, while header disagreement is reported.
"""

import argparse
import json
from pathlib import Path
import re
import struct
import subprocess

PAGE = 4096
TEXT = 128
FIELDS = ("stage", "length", "fdt", "current_el", "sctlr", "esr", "elr", "far", "text_checksum")


def checksum(data):
    value = 0xcbf29ce484222325
    for byte in data:
        value = ((value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    return value


def decode(data):
    data = bytes(data)
    page_bytes, remainder = divmod(len(data), 3)
    if remainder or not TEXT <= page_bytes <= PAGE or page_bytes % 8:
        raise ValueError("trace must contain three equal, 8-byte-aligned page prefixes of 128–4096 bytes")
    pages = [data[i * page_bytes:(i + 1) * page_bytes] for i in range(3)]
    majority = bytes((a & b) | (a & c) | (b & c) for a, b, c in zip(*pages))
    bitwise_or = bytes(a | b | c for a, b, c in zip(*pages))
    candidates = [("majority", majority), *[(f"copy-{i}", p) for i, p in enumerate(pages)],
                  ("bitwise-or", bitwise_or)]
    valid = []
    for name, page in candidates:
        if page[:8] != b"PBTRACE1":
            continue
        length = struct.unpack_from("<Q", page, 16)[0]
        if length > page_bytes - TEXT:
            continue
        text = page[TEXT:TEXT + length]
        if checksum(text) == struct.unpack_from("<Q", page, 72)[0]:
            valid.append((name, page, text))
    if not valid:
        raise ValueError("no trace copy or reconstructed text has a valid header, length and checksum")
    if len({entry[2] for entry in valid}) != 1:
        raise ValueError("ambiguous checksum-valid text after recovery")
    name, selected, text = valid[0]
    header = {}
    disagreement = {}
    for index, field in enumerate(FIELDS, 1):
        offset = index * 8
        values = [struct.unpack_from("<Q", page, offset)[0] for page in pages]
        header[field] = hex(struct.unpack_from("<Q", selected, offset)[0])
        if len(set(values)) != 1:
            disagreement[field] = [hex(value) for value in values]
    result = {"selected_recovery": name, "checksum_valid_recoveries": [v[0] for v in valid],
              "header": header, "header_copy_disagreement": disagreement,
              "header_caution": "Text checksum does not authenticate header fields; inspect disagreement.",
              "text": text.decode("utf-8", errors="replace")}
    return result


def read_device(serial, compatible, address, output, page_bytes=PAGE):
    if address % PAGE or not 0x80000000 <= address < 0x100000000 - 3 * PAGE:
        raise ValueError("expected a page-aligned trace address in MSM8916 DRAM")
    with (output / "fastboot.log").open("xb") as log:
        def fb(args):
            command = ["fastboot", "-s", serial, *args]
            log.write((repr(command) + "\n").encode())
            log.flush()
            try:
                result = subprocess.run(command, capture_output=True, timeout=10)
            except subprocess.TimeoutExpired as error:
                log.write((error.stdout or b"") + (error.stderr or b"") + b"TIMEOUT\n")
                log.flush()
                raise
            text = (result.stdout + result.stderr).decode(errors="replace")
            log.write(text.encode())
            log.flush()
            if result.returncode:
                raise RuntimeError(text)
            return text

        for key, expected in [("product", "lk2nd-msm8916"), ("serialno", serial),
                              ("lk2nd:compatible", compatible)]:
            if f"{key}: {expected}\n" not in fb(["getvar", key]):
                raise RuntimeError(f"unexpected device identity: {key}")
        data = bytearray()
        with (output / "trace.bin").open("xb") as raw:
            for offset in (page * PAGE + word for page in range(3) for word in range(0, page_bytes, 8)):
                text = fb(["oem", "debug", "readq", hex(address + offset)])
                match = re.search(r"\(bootloader\) (0x[0-9a-fA-F]+|0)\s", text)
                if not match:
                    raise RuntimeError(f"unrecognized register read: {text}")
                word = int(match[1], 0).to_bytes(8, "little")
                data.extend(word)
                raw.write(word)
    return bytes(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--input", type=Path, help="existing raw 12 KiB trace, without device access")
    source.add_argument("--serial", help="explicit lk2nd fastboot serial")
    parser.add_argument("--compatible", help="required board compatible in device-read mode")
    parser.add_argument("--address", type=lambda value: int(value, 0), help="trace physical address")
    parser.add_argument("--page-bytes", type=int, default=PAGE,
                        help="bytes to read from each page (128–4096, multiple of 8)")
    parser.add_argument("--output", type=Path, required=True, help="new evidence directory")
    args = parser.parse_args()
    if args.serial and (not args.compatible or args.address is None):
        parser.error("device-read mode requires --compatible and --address")
    if not TEXT <= args.page_bytes <= PAGE or args.page_bytes % 8:
        parser.error("--page-bytes must be a multiple of 8 from 128 to 4096")
    args.output.mkdir(parents=True, exist_ok=False)
    data = args.input.read_bytes() if args.input else read_device(
        args.serial, args.compatible, args.address, args.output, args.page_bytes)
    result = decode(data)
    (args.output / "decoded.json").write_text(json.dumps(result, indent=2) + "\n")
    (args.output / "console.log").write_text(result["text"])
    print(json.dumps({key: value for key, value in result.items() if key != "text"}, indent=2))
    print(result["text"])


if __name__ == "__main__":
    main()
