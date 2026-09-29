#!/usr/bin/env python3
"""Check the ARM loader's unadvertised decompressor bounds against its build ELF."""

import re
import subprocess
import sys

elf = sys.argv[1]
symbols = {}
for line in subprocess.check_output(["nm", "-n", elf], text=True).splitlines():
    parts = line.split()
    if len(parts) == 3:
        symbols[parts[2]] = int(parts[0], 16)

gap = symbols["_end"] - symbols["_edata"]
if not 0 <= gap <= 64 * 1024:
    sys.exit(f"unsupported decompressor BSS/gap: {gap} bytes (limit 65536)")
if not symbols["_start"] <= symbols["restart"] < symbols["reloc_code_end"] <= symbols["_edata"]:
    sys.exit("relocation code lies outside the zImage file envelope")
sections = subprocess.check_output(["readelf", "-SW", elf], text=True)
stack = re.search(r"\]\s+\.stack\s+\S+\s+\S+\s+\S+\s+([0-9a-fA-F]+)", sections)
if stack is None or int(stack[1], 16) > 4096:
    sys.exit("missing or oversized decompressor stack (limit 4096)")
print(f"decompressor contract: BSS/gap={gap}, stack={int(stack[1], 16)} bytes; relocation code within image")
