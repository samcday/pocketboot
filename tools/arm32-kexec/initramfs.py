#!/usr/bin/env python3
"""Build deterministic newc/gzip initramfs images, including /dev/console as nonroot.

Writes the destination initrd plus one source initrd per smoke case. Each case
spec is CASE=KERNEL[:DTB]: the destination kernel payload that case must load and,
for the supplied-DTB case, the explicit DTB fixture. Paths with ':' are unsupported.
"""

import argparse
import gzip
from pathlib import Path
import stat

# Keep in sync with SENTINEL in tools/arm32-kexec/src/main.rs.
SENTINEL = b"pocketboot destination initramfs\n"


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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path, help="PID 1 binary to install as /init")
    parser.add_argument("outdir", type=Path, help="directory for the initramfs images")
    parser.add_argument(
        "cases",
        metavar="CASE=KERNEL[:DTB]",
        nargs="+",
        help="source initrd to build: /destination.zImage payload and optional /supplied.dtb fixture",
    )
    args = parser.parse_args()

    init = ("init", 0o755, args.binary.read_bytes())
    destination = archive([init, ("sentinel", 0o644, SENTINEL)])
    (args.outdir / "destination.cpio.gz").write_bytes(destination)

    for spec in args.cases:
        name, _, paths = spec.partition("=")
        kernel, _, dtb = paths.partition(":")
        if not name or not kernel:
            parser.error(f"case spec must look like CASE=KERNEL[:DTB]: {spec}")
        files = [
            init,
            ("destination.zImage", 0o644, Path(kernel).read_bytes()),
            ("destination.cpio.gz", 0o644, destination),
        ]
        if dtb:
            files.append(("supplied.dtb", 0o644, Path(dtb).read_bytes()))
        (args.outdir / f"source-{name}.cpio.gz").write_bytes(archive(files))


if __name__ == "__main__":
    main()
