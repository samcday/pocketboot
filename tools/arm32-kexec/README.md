# ARM32 two-kernel kexec smoke test

Run from the repository root:

```sh
bash tools/arm32-kexec/run.sh /absolute/path/to/linux LLVM=-19
# Or use ARM hard-float GCC (the default):
bash tools/arm32-kexec/run.sh /absolute/path/to/linux
```

Supply a **local, clean Linux source tree** supporting ARM virt and the modern
zImage kernel-size extension. Nothing clones or downloads a kernel. LLVM 19
(`LLVM=-19`) or `arm-linux-gnueabihf-gcc` plus kernel build prerequisites are
required. Also install Rust's `armv7-unknown-linux-musleabihf` target,
`rust-lld`, `qemu-system-arm` with `virt,dtb-randomness=off` support,
Python 3, device-tree-compiler (`fdtput`), GNU make/coreutils, binutils (`nm`,
`readelf`) and the usual kernel host build tools. Cargo dependencies must be
cached or downloadable.

All build products, effective kernel configuration, DTBs, initramfs images and
serial logs live in `target/arm32-kexec-qemu/`. The supplied source is built
out of tree. `JOBS`, `CROSS_COMPILE`, `QEMU_SYSTEM_ARM` and `SMOKE_TIMEOUT`
(seconds **per QEMU invocation**, default 60) may be overridden. Additional
arguments after the source path are passed to kernel make.

To build in the CI container and run QEMU on the host, use `SMOKE_MODE=build`
for the first invocation and `SMOKE_MODE=run` for the second, sharing the
same checkout and `target/arm32-kexec-qemu/`. The default `SMOKE_MODE=all`
does both.

The standalone Cargo manifest builds `examples/arm32-kexec-smoke.rs` as a
small static ARMv7 musl PID 1 without building pocketboot's UI. It includes
the actual `src/kexec.rs`, `src/pe.rs` and `src/zboot.rs`: there is no substitute
loader or syscall shim. The checked-in Cargo lockfile, fixed kernel build
metadata and deterministic CPIO metadata make repeated builds reproducible
with the same source tree and tool versions; this does not pin the external
kernel or toolchain. Fixture DTB generation disables QEMU's random seeds;
normal guest boots retain QEMU's default entropy injection. The kernel
fragment uses `allnoconfig`, uniprocessor
ARM virt, PL011, kexec, external gzip initramfs and static ELF userspace.
The build also checks the loader's decompressor BSS/stack contract against
the compressed kernel's ELF symbols; the zImage size tag cannot establish
those bounds by itself. Run `check-decompressor.py` on any new destination
kernel build before treating it as covered by this first-pass loader.

## What passes

Both invocations use `-M virt -cpu cortex-a15 -smp 1 -m 256M -nographic`.
They boot the same kernel build twice with two **different** external initrds:

1. Source `/init` mounts proc, sysfs and devtmpfs and calls
   `KexecImage::new(...).load()` followed by `exec_loaded_image()`.
2. Destination `/init` requires a destination-only cmdline marker, the exact
   destination sentinel, no source kernel file, `/chosen/linux,booted-from-kexec`,
   and the live QEMU memory range (256 MiB at `0x40000000`).
3. It prints `ARM32-KEXEC-SMOKE: PASS case=fallback` or `case=supplied`
   and powers off. The host requires both the exact PASS line and successful
   QEMU termination within the timeout; panic, hang, failure or reset fails.

The fallback case supplies no DTB to the loader, exercising
`/sys/firmware/fdt`. The supplied case passes QEMU's DTB with `/memory/reg`
deliberately changed to 128 MiB at `0x50000000`; the destination must see
the real live-memory range and a supplied-only DT marker. Both destination
zImages have a **byte-appended stale DTB** with a separate poison marker and
wrong memory. `CONFIG_ARM_APPENDED_DTB=y` ensures the kernel would consume it
if the loader failed to strip it; neither destination may see that marker.
Host-side `fdtput` edits blobs and the guest checks the kernel's sysfs DT
view, avoiding a second FDT parser.

This is a privileged-in-guest smoke test, not hardware, SMP, old zImage,
raw Image, initrd-less, or malicious-input coverage. A green run proves the
two-kernel workflow in QEMU, not physical-device cache/coherency correctness.
