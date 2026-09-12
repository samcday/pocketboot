#!/usr/bin/env python3
"""Package a DB410c ARM64 Image/DTB and optional pocketpreboot into a lab FIT.

The caller supplies the prepared DTB, including memory/console information and
the v1 parking reservation for a non-PSCI test. This only writes host artifacts.
Transfer with `fastboot stage image.itb`, then use U-Boot `bootm $loadaddr`;
the Android fastboot client's `boot` command wraps non-Android files.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess


def image_header(data):
    if len(data) < 64 or data[56:60] != b"ARMd":
        raise ValueError("input is not an uncompressed ARM64 Image")
    offset, size = struct.unpack_from("<QQ", data, 8)
    if offset or not size:
        raise ValueError("this lab layout requires text_offset=0 and nonzero image_size")
    return size


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--dtb", required=True, type=Path)
    parser.add_argument("--preboot", type=Path)
    parser.add_argument("--output", required=True, type=Path,
                        help="new output directory")
    parser.add_argument("--mkimage", default="mkimage")
    args = parser.parse_args()
    kernel = args.kernel.read_bytes()
    kernel_size = image_header(kernel)
    kernel_load = 0x80200000
    load = kernel_load
    data = kernel
    if args.preboot:
        shim = args.preboot.read_bytes()
        shim_size = image_header(shim)
        load = 0x80000000
        offset = (shim_size + 0x1fffff) & ~0x1fffff
        if len(shim) > offset or load + offset != kernel_load:
            raise ValueError("preboot does not fit the chosen 2 MiB prefix")
        data = shim + bytes(offset - len(shim)) + kernel
    if kernel_load + max(kernel_size, len(kernel)) > 0x85000000:
        raise ValueError("kernel would overlap the lab DTB placement")
    # The outer preboot header describes only the shim, because its rounded
    # image_size locates the payload. Include the inner kernel's BSS footprint
    # in the file too so a later kexec allocator reserves the entire image.
    runtime_size = kernel_load - load + max(kernel_size, len(kernel))
    data += bytes(runtime_size - len(data))
    dtb = args.dtb.read_bytes()
    if len(dtb) < 40 or dtb[:4] != bytes.fromhex("d00dfeed"):
        raise ValueError("input DTB has an invalid header")
    total = struct.unpack_from(">I", dtb, 4)[0]
    if total > len(dtb) or len(dtb) > 0x100000:
        raise ValueError("DTB is truncated or too large for the lab layout")
    args.output.mkdir(parents=True, exist_ok=False)
    (args.output / "payload.Image").write_bytes(data)
    shutil.copyfile(args.dtb, args.output / "input.dtb")
    (args.output / "image.its").write_text(f'''/dts-v1/;
/ {{
    description = "Pocketboot DB410c lab";
    #address-cells = <1>;
    images {{
        kernel {{
            data = /incbin/("payload.Image");
            type = "kernel"; arch = "arm64"; os = "linux";
            compression = "none"; load = <0x{load:x}>; entry = <0x{load:x}>;
            hash {{ algo = "sha256"; }};
        }};
        fdt {{
            data = /incbin/("input.dtb");
            type = "flat_dt"; arch = "arm64"; compression = "none";
            load = <0x85000000>;
            hash {{ algo = "sha256"; }};
        }};
    }};
    configurations {{
        default = "db410c";
        db410c {{ kernel = "kernel"; fdt = "fdt"; }};
    }};
}};
''')
    subprocess.run([args.mkimage, "-f", "image.its", "image.itb"],
                   cwd=args.output, check=True)
    inputs = {"kernel": args.kernel, "dtb": args.dtb}
    if args.preboot:
        inputs["preboot"] = args.preboot
    manifest = {
        name: {"path": str(path.resolve()), "bytes": path.stat().st_size,
               "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        for name, path in inputs.items()
    }
    manifest["fit_sha256"] = hashlib.sha256((args.output / "image.itb").read_bytes()).hexdigest()
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
