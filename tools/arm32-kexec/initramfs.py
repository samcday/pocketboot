#!/usr/bin/env python3
"""Build deterministic newc/gzip initramfs images, including /dev/console as nonroot."""

import gzip
from pathlib import Path
import stat
import sys


def archive(files):
    result = bytearray()

    def entry(name, mode, data=b"", rdev=(0, 0)):
        encoded = name.encode() + b"\0"
        fields = (
            1, mode, 0, 0, 1, 0, len(data), 0, 0, *rdev, len(encoded), 0
        )
        result.extend(b"070701" + "".join(f"{x:08x}" for x in fields).encode())
        result.extend(encoded)
        result.extend(b"\0" * (-len(result) % 4))
        result.extend(data)
        result.extend(b"\0" * (-len(result) % 4))

    for directory in ("dev", "proc", "sys"):
        entry(directory, stat.S_IFDIR | 0o755)
    entry("dev/console", stat.S_IFCHR | 0o600, rdev=(5, 1))
    for name, mode, data in files:
        entry(name, stat.S_IFREG | mode, data)
    entry("TRAILER!!!", 0)
    result.extend(b"\0" * (-len(result) % 512))
    return gzip.compress(bytes(result), mtime=0)


def main():
    binary, kernel, dtb, output = map(Path, sys.argv[1:])
    init = ("init", 0o755, binary.read_bytes())
    destination = archive([init, ("sentinel", 0o644, b"pocketboot destination initramfs\n")])
    (output / "destination.cpio.gz").write_bytes(destination)
    source = archive([
        init,
        ("destination.zImage", 0o644, kernel.read_bytes()),
        ("destination.cpio.gz", 0o644, destination),
        ("supplied.dtb", 0o644, dtb.read_bytes()),
    ])
    (output / "source.cpio.gz").write_bytes(source)


if __name__ == "__main__":
    main()
