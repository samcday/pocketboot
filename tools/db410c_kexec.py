#!/usr/bin/env python3
"""Prepare host-only MSM8916 kexec images and fastboot shell probes.

This tool never opens USB/UART or invokes fastboot. The Android v2 envelopes
contain the bare Linux Image and a separately supplied, initially PSCI DTB.
Pocketboot supplies the live spin-table contract when it loads each image.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shlex
import shutil
import struct
import subprocess

PAGE = 4096
PARKING = 0x854FF000


def run(*command, allow_failure=False):
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, check=False)
    if result.returncode and not allow_failure:
        raise RuntimeError(f"{shlex.join(command)}: {result.stderr.strip()}")
    return result.stdout.strip() if result.returncode == 0 else None


def prop(dtb, node, name, kind="s"):
    return run("fdtget", "-t", kind, str(dtb), node, name, allow_failure=True)


def children(dtb, node):
    return (run("fdtget", "-l", str(dtb), node, allow_failure=True) or "").splitlines()


def setprop(dtb, node, name, *values, kind="s"):
    run("fdtput", "-t", kind, str(dtb), node, name, *values)


def remove_prop(dtb, node, name):
    if prop(dtb, node, name, "bx") is not None:
        run("fdtput", "-d", str(dtb), node, name)


def remove_parking_memreserve(dtb):
    data = bytearray(dtb.read_bytes())
    cursor = struct.unpack_from(">I", data, 16)[0]
    start = cursor
    kept = bytearray()
    while True:
        address, size = struct.unpack_from(">QQ", data, cursor)
        cursor += 16
        if not address and not size:
            break
        if address < PARKING + PAGE and PARKING < address + size:
            if (address, size) != (PARKING, PAGE):
                raise ValueError("input reserve map has another owner overlapping parking")
        else:
            kept.extend(struct.pack(">QQ", address, size))
    kept.extend(bytes(16))
    data[start:cursor] = kept + bytes(cursor - start - len(kept))
    dtb.write_bytes(data)


def prepare_external_dtb(source, destination):
    shutil.copyfile(source, destination)
    cpus = {}
    for name in children(destination, "/cpus"):
        node = "/cpus/" + name
        if prop(destination, node, "device_type") != "cpu":
            continue
        reg = prop(destination, node, "reg", "x")
        if reg is None:
            raise ValueError(f"CPU {node} has no reg")
        mpidr = 0
        for cell in reg.split():
            mpidr = (mpidr << 32) | int(cell, 16)
        if mpidr in cpus:
            raise ValueError("duplicate CPU MPIDR")
        cpus[mpidr] = node
        setprop(destination, node, "enable-method", "psci")
        setprop(destination, node, "status", "okay")
        remove_prop(destination, node, "cpu-release-addr")
    if set(cpus) != set(range(4)):
        raise ValueError("DB410c probe requires CPU MPIDRs 0,1,2,3")
    for name in children(destination, "/reserved-memory"):
        node = "/reserved-memory/" + name
        compatible = prop(destination, node, "compatible") or ""
        if any(item.startswith("pocketboot,spin-table-") for item in compatible.split()):
            run("fdtput", "-r", str(destination), node)
    remove_parking_memreserve(destination)
    return cpus


def verify_boot_image(path, kernel, dtb):
    data = path.read_bytes()
    if data[:8] != b"ANDROID!":
        raise ValueError("mkbootimg output is not an Android boot image")
    kernel_size, = struct.unpack_from("<I", data, 8)
    page, version = struct.unpack_from("<II", data, 36)
    header_size, dtb_size = struct.unpack_from("<II", data, 1644)
    if (page, version, header_size) != (PAGE, 2, 1660):
        raise ValueError("unexpected Android v2 header layout")
    if kernel_size != len(kernel) or dtb_size != len(dtb):
        raise ValueError("wrong Android payload sizes")
    dtb_offset = PAGE + ((kernel_size + PAGE - 1) // PAGE) * PAGE
    if data[PAGE:PAGE + kernel_size] != kernel or data[dtb_offset:dtb_offset + dtb_size] != dtb:
        raise ValueError("Android v2 sections do not match prepared inputs")


def package(kernel_path, dtb_path, cmdline, output, mkbootimg):
    run(mkbootimg, "--header_version", "2", "--pagesize", str(PAGE),
        "--base", "0", "--kernel_offset", "0", "--dtb_offset", "0",
        "--kernel", str(kernel_path), "--dtb", str(dtb_path),
        "--cmdline", cmdline, "--output", str(output))
    verify_boot_image(output, kernel_path.read_bytes(), dtb_path.read_bytes())


def combine_reentry_preboot(shim_path, kernel):
    shim = shim_path.read_bytes()
    if len(shim) < 64 or shim[56:60] != b"ARMd":
        raise ValueError("reentry preboot is not an ARM64 Image")
    shim_offset, shim_size = struct.unpack_from("<QQ", shim, 8)
    kernel_offset, kernel_size = struct.unpack_from("<QQ", kernel, 8)
    prefix = (shim_size + 0x1fffff) & ~0x1fffff
    if shim_offset or kernel_offset or not shim_size or prefix != 0x200000 or len(shim) > prefix:
        raise ValueError("reentry layout requires zero text offsets and a 2 MiB shim prefix")
    combined = shim + bytes(prefix - len(shim)) + kernel
    # The outer image_size locates the payload, so retain it. The legacy loader
    # also reserves the full file length: include the inner kernel's BSS there.
    combined += bytes(prefix + max(kernel_size, len(kernel)) - len(combined))
    return combined, {"payload_offset": prefix, "inner_kernel_runtime_bytes": kernel_size,
                      "combined_runtime_bytes": len(combined), "outer_header_image_size": shim_size,
                      "padding_reason": "protect inner kernel BSS in legacy kexec allocator"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--dtb", type=Path, required=True)
    parser.add_argument("--cmdline-file", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True, help="new host artifact directory")
    parser.add_argument("--cc", default="aarch64-linux-musl-gcc")
    parser.add_argument("--mkbootimg", default="mkbootimg")
    parser.add_argument("--panic-timeout", type=int, default=5,
                        help="seconds before automatic panic reboot (default: 5)")
    parser.add_argument("--reentry-preboot", type=Path,
                        help="optionally prepare generation 3 with an explicitly reentry-capable shim")
    args = parser.parse_args()
    if args.panic_timeout < 1:
        parser.error("--panic-timeout must be positive")
    kernel = args.kernel.read_bytes()
    if len(kernel) < 64 or kernel[56:60] != b"ARMd" or struct.unpack_from("<Q", kernel, 16)[0] == 0:
        raise ValueError("--kernel must be a bare, uncompressed ARM64 Linux Image")
    cmdline = " ".join(word for word in args.cmdline_file.read_text().strip().split()
                       if not word.startswith(("pocketboot.lab=", "panic=", "oops=")))
    cmdline += f" panic={args.panic_timeout} oops=panic"
    if "\x00" in cmdline or len(cmdline.encode()) > 1400:
        raise ValueError("invalid or excessively long base cmdline")
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    next_kernel = output / "next.Image"
    next_kernel.write_bytes(kernel)
    external = output / "external-psci.dtb"
    cpus = prepare_external_dtb(args.dtb, external)
    for generation in (1, 2):
        package(next_kernel, external, f"{cmdline} pocketboot.lab=kexec-generation-{generation}",
                output / f"generation-{generation}.img", args.mkbootimg)
    bad_cpu = output / "negative-cpu3-disabled.dtb"
    shutil.copyfile(external, bad_cpu)
    setprop(bad_cpu, cpus[3], "status", "disabled")
    package(next_kernel, bad_cpu, cmdline, output / "negative-cpu3-disabled.img", args.mkbootimg)
    bad_reservation = output / "negative-parking-overlap.dtb"
    shutil.copyfile(external, bad_reservation)
    node = "/reserved-memory/negative-overlap@854ff000"
    run("fdtput", "-cp", str(bad_reservation), node)
    setprop(bad_reservation, node, "compatible", "pocketboot,negative-test-owner")
    setprop(bad_reservation, node, "no-map")
    address_cells = int(prop(bad_reservation, "/reserved-memory", "#address-cells", "x"), 16)
    size_cells = int(prop(bad_reservation, "/reserved-memory", "#size-cells", "x"), 16)
    if (address_cells, size_cells) != (2, 2):
        raise ValueError("the DB410c negative overlap fixture requires 2+2 cells")
    setprop(bad_reservation, node, "reg", "0", f"{PARKING:x}", "0", f"{PAGE:x}", kind="x")
    package(next_kernel, bad_reservation, cmdline, output / "negative-parking-overlap.img", args.mkbootimg)

    reentry_layout = None
    if args.reentry_preboot:
        combined, reentry_layout = combine_reentry_preboot(args.reentry_preboot, kernel)
        reentry_image = output / "generation-3-preboot.Image"
        reentry_image.write_bytes(combined)
        package(reentry_image, external, f"{cmdline} pocketboot.lab=kexec-generation-3-preboot",
                output / "generation-3-preboot.img", args.mkbootimg)

    workload_source = Path(__file__).resolve().with_name("smp-work.c")
    workload = output / "smp-work-v2"
    run(args.cc, "-Os", "-static", "-s", "-o", str(workload), str(workload_source))
    binary = workload.read_bytes()
    scripts = output / "workload-v2-install"
    scripts.mkdir()
    for index, offset in enumerate(range(0, len(binary), 8192)):
        chunk = binary[offset:offset + 8192]
        octal = "".join(f"\\0{byte:03o}" for byte in chunk)
        before = "mkdir -p /tmp/pb-smp-v2\n: > /tmp/pb-smp-v2/work\n" if index == 0 else ""
        after = (f"test \"$(wc -c < /tmp/pb-smp-v2/work)\" -eq {len(binary)}\n"
                 "chmod 700 /tmp/pb-smp-v2/work\n") if offset + len(chunk) == len(binary) else ""
        script = f"set -eu\n{before}printf '%b' '{octal}' >> /tmp/pb-smp-v2/work\n{after}"
        if len(script.encode()) > 65536:
            raise ValueError("workload transfer exceeds Pocketboot's staged shell limit")
        (scripts / f"{index:03d}.sh").write_text(script)
    (output / "snapshot-v2.sh").write_text('''set -eu
printf 'PB_SMP_SNAPSHOT_V2\\n'
uname -a
printf 'boot_id='; if test -r /proc/sys/kernel/random/boot_id; then cat /proc/sys/kernel/random/boot_id; else printf 'unavailable\\n'; fi
printf 'uptime='; cat /proc/uptime
printf 'cmdline='; cat /proc/cmdline
printf 'online='; cat /sys/devices/system/cpu/online
test "$(cat /sys/devices/system/cpu/online)" = '0-3'
for pb_cpu in /sys/firmware/devicetree/base/cpus/cpu@*; do
    printf 'cpu_node=%s reg=' "$pb_cpu"; hexdump -v -e '1/1 "%02x"' "$pb_cpu/reg"; printf '\\n'
    printf 'method='; tr '\\000' ' ' < "$pb_cpu/enable-method"; printf '\\n'
    if test -r "$pb_cpu/cpu-release-addr"; then
        printf 'release='; hexdump -v -e '1/1 "%02x"' "$pb_cpu/cpu-release-addr"; printf '\\n'
    fi
done
printf 'stat_before\\n'; cat /proc/stat
/tmp/pb-smp-v2/work
printf 'stat_after\\n'; cat /proc/stat
printf 'PB_SMP_SNAPSHOT_PASS\\n'
''')
    manifest = {"workload": {"version": 2, "binary": "smp-work-v2",
                              "install_scripts": "workload-v2-install/*.sh",
                              "snapshot": "snapshot-v2.sh", "target_path": "/tmp/pb-smp-v2/work",
                              "compiler_flags": ["-Os", "-static", "-s"],
                              "required_cpu_ids": [0, 1, 2, 3],
                              "minimum_cpu_seconds_exclusive": 0.02,
                              "expected_hash": "292d74d0d0222325"},
                "inputs": {name: {"path": str(path.resolve()), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                            for name, path in {"kernel": args.kernel, "dtb": args.dtb, "cmdline": args.cmdline_file,
                                               "workload_source": workload_source}.items()},
                "artifacts": {str(path.relative_to(output)): {"bytes": path.stat().st_size,
                              "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                              for path in sorted(output.rglob("*")) if path.is_file()}}
    if args.reentry_preboot:
        manifest["inputs"]["reentry_preboot"] = {
            "path": str(args.reentry_preboot.resolve()),
            "bytes": args.reentry_preboot.stat().st_size,
            "sha256": hashlib.sha256(args.reentry_preboot.read_bytes()).hexdigest()}
        manifest["generation_3_layout"] = reentry_layout
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Prepared host artifacts: {output}")
    serial = prop(external, "/", "serial-number") or "VERIFIED_SERIAL"
    print(f"First handoff: fastboot -s {shlex.quote(serial)} boot {shlex.quote(str(output / 'generation-1.img'))}")
    print(f"Second handoff: fastboot -s {shlex.quote(serial)} boot {shlex.quote(str(output / 'generation-2.img'))}")
    print("Use docs/db410c-kexec-validation.md for capture, workload transfer and load-only negative tests.")


if __name__ == "__main__":
    main()
