# ARM32 two-kernel kexec smoke test

Run from the repository root:

```sh
bash tools/arm32-kexec/run.sh /absolute/path/to/linux LLVM=1
# Or use ARM hard-float GCC (the default CROSS_COMPILE):
bash tools/arm32-kexec/run.sh /absolute/path/to/linux
```

Supply a **local, clean Linux source tree** supporting ARM virt and the modern
zImage kernel-size extension. Nothing clones or downloads a kernel. LLVM 19+
(`LLVM=1`, or `LLVM=-19` where only versioned tools are installed) or
`arm-linux-gnueabihf-gcc` plus kernel build prerequisites are required. Also
install Rust's `armv7-unknown-linux-musleabihf` target, `rust-lld`,
`qemu-system-arm` with `virt,dtb-randomness=off` support, Python 3,
device-tree-compiler (`fdtput`), gzip, GNU make/coreutils, binutils (`nm`,
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
does both. Run mode never touches the kernel source: it ignores both the source
path (which usually does not exist on the host) and any make arguments, and
refuses to start unless the build products it needs are already present.

## Layout and dependencies

The standalone Cargo manifest builds `tools/arm32-kexec/src/main.rs` as a
small static ARMv7 musl PID 1 without building pocketboot's UI, and the root
package no longer registers it as an example. It includes the actual
`src/kexec.rs`, `src/pe.rs` and `src/zboot.rs`: there is no substitute loader or
syscall shim. The checked-in Cargo lockfile, fixed kernel build metadata and
deterministic CPIO metadata make repeated builds reproducible with the same
source tree and tool versions; this does not pin the external kernel or
toolchain. Fixture DTB generation disables QEMU's random seeds; normal guest
boots retain QEMU's default entropy injection. The kernel fragment uses
`allnoconfig`, uniprocessor ARM virt, PL011, kexec, external gzip initramfs and
static ELF userspace. `CONFIG_EFI=y` makes every destination zImage an EFI-stub
(`MZ`/PE32) payload, so the harness covers production's payload classification
for that wrapper instead of rejecting it. The `appended` case gzip-wraps the
whole destination payload and requires `CONFIG_MEMFD_CREATE=y` for production's
memfd-backed preparation path. The build also checks the loader's
decompressor BSS/stack contract against the compressed kernel's ELF symbols; the
zImage size tag cannot establish those bounds by itself. Run
`check-decompressor.py` on any new destination kernel build before treating it
as covered by this first-pass loader.

The harness-side checks need no extra Rust dependencies:

```sh
cargo test --locked --manifest-path tools/arm32-kexec/Cargo.toml
```

That builds the PID 1 binary for the host and runs the unit tests of the
included loader, PE and zboot modules.

## What passes

Every invocation uses `-M virt -cpu cortex-a15 -smp 1 -m 256M -nographic` and
boots the same kernel twice with two **different** external initrds:

1. Source `/init` mounts proc, sysfs and devtmpfs, runs
   `prepare_kernel_payload(...)` exactly as production does, calls
   `KexecImage::new(...).load()` and then `exec_loaded_image()`.
2. Destination `/init` requires the destination-only initrd sentinel, no source
   kernel file, the loader-patched `/chosen`, the exact destination command line,
   the live 256 MiB memory map at `0x40000000`, and the DTB source that the case
   selects. It prints `ARM32-KEXEC-SMOKE: PASS case=...` and powers off. The host
   requires the exact PASS line, no `ARM32-KEXEC-SMOKE: FAIL` record and
   successful QEMU termination within the timeout; a guest FAIL powers the guest
   off, so a failure normally surfaces in seconds.

Three independent QEMU cases cover the DTB source rules:

| case | explicit DTB | appended DTB | DTB the loader must select |
| --- | --- | --- | --- |
| `fallback` | none | none | live `/sys/firmware/fdt` |
| `supplied` | `/supplied.dtb` | stale poison blob | the explicit DTB |
| `appended` | none | fixture blob | the appended DTB |

The fixtures are copies of QEMU's own DTB with `/memory@40000000` deliberately
changed to a stale map (`0x50000000`/128 MiB for supplied, `0x60000000`/128 MiB
for appended) and a `/chosen/pocketboot,dtb-source` marker naming the source.
The destination therefore verifies source choice, the loader's live-memory
graft, destination-only cmdline/initrd and `/chosen` patching in one view; no
second FDT parser exists on either side. Host-side `fdtput` edits blobs and the
guest reads the kernel's sysfs DT view.

`supplied` and `appended` reuse one destination payload built by appending the
`appended` fixture after the zImage `end` word, with an outer gzip wrapper for
the `appended` case. For `supplied` that blob is poison: the explicit DTB must
win. For `appended` it is the source of record: the loader must decompress the
payload, select its DTB, graft live RAM into it, patch `/chosen`, and still strip
it from the kernel segment, because `CONFIG_ARM_APPENDED_DTB=y` lets the
destination kernel consume a surviving appended DTB instead of the one passed
in r2.

Each case keeps its log in `target/arm32-kexec-qemu/<case>.log`. All three run
even when an earlier case fails, and the script fails once at the end with
`ARM32-KEXEC-SMOKE: FAIL <n>/3 cases: ...` instead of stopping at the first
failure.

Only the QEMU PID 1 may end the machine: the guest powers off when it can
identify itself as the smoke PID 1, and a normal host invocation never
reboots or powers off the host.

This is a privileged-in-guest smoke test, not hardware, SMP, old zImage,
raw Image, initrd-less, or malicious-input coverage. A green run proves the
two-kernel workflow in QEMU, not physical-device cache/coherency correctness.
