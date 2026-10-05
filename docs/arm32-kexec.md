# ARM32 kexec bring-up

Initial ARMv7 handoff is implemented and tested with two-kernel QEMU boots.
[Expressltexx](samsung-expressltexx.md#hardware-validation) has also completed
two consecutive Pocketboot-to-Pocketboot handoffs on hardware. Expressatt
uses a different kernel tree and remains independently unvalidated.
Those phone runs predate the ultrareview fixes; this round revalidated the
loader in QEMU, not on hardware.

## Supported first-pass contract

- Little-endian ARMv7 Linux DT boot through legacy `kexec_load`.
- A RAM-loaded `zImage` with the modern six-word kernel-size extension,
  `TEXT_OFFSET=0x8000`, and a 64 KiB decompressor heap. Raw `Image`/`Image.gz`
  and older size-less zImages are rejected rather than guessed. The existing
  payload-preparation path accepts ARM EFI-stub (MZ/PE32) zImages and can
  unwrap a gzip-wrapped **zImage**.
- A destination kernel built for the same platform, using the normal Linux
  DT-aware `AUTO_ZRELADDR` decompressor or a fixed `ZRELADDR` matching the
  lowest RAM bank plus `0x8000`. For a fixed-address build, verify that
  linker symbol in `arch/arm/boot/compressed/vmlinux`; its value is not in
  the zImage size tag. For a RAM base not aligned to 128 MiB, automatic
  placement requires the DT-aware decompressor introduced in
  Linux v5.12 (or a verified backport). The six-word tag does **not** prove
  that capability: it is a destination-build contract, not a runtime check.
  The DT and `/proc/iomem` must agree on the lowest RAM bank, aligned to
  2 MiB. Crash-kernel usable-memory overrides and nested/disabled memory
  banks are not supported.
- All submitted segments and the decompressor workspace fit in the first
  128 MiB PC-derived boot window. Keeping initrd/DTB there is a deliberately
  conservative first-pass limit, not a general Linux ABI requirement:
  `/proc/iomem` alone cannot establish another kernel's lowmem boundary.
- The supported decompressor has at most 64 KiB between `_edata` and `_end`
  and a 4 KiB stack. **These bounds are not encoded in the six-word tag.**
  Validate a new destination build with:

  ```sh
  python3 tools/arm32-kexec/check-decompressor.py \
      /path/to/kernel-build/arch/arm/boot/compressed/vmlinux
  ```

The checked phone and QEMU gzip decompressors satisfy these BSS/stack bounds.
Do not infer support for arbitrary compression configurations from a valid zImage magic.
Broader support needs additional producer metadata or a separately validated
decompressor contract, not another guessed load address.

## Handoff and memory ownership

Linux's ARM kexec implementation relocates the submitted segments, shuts
down caches/MMU, and supplies `r0=0`, `r1=machine_arch_type`, and `r2=DTB`.
The syscall entry is the zImage itself. There is **no extra userspace ARM32
purgatory/trampoline** and no integrity-checking purgatory claim.

The loader:

1. Validates the image's declared length and bounded size-extension table.
2. Selects an explicit DTB first, otherwise a single validated appended
   DTB, otherwise the live DTB. Multiple appended trees are rejected unless
   an explicit DTB resolves the choice. It removes the appended blob from
   the kernel segment so `ARM_APPENDED_DTB` cannot override the patched `r2`.
3. Protects destination kernel BSS and the entire relocated decompressor,
   heap, stack, and alignment area. Relocation-code headroom is bounded by
   another full zImage length; decompressor BSS uses the validated limit
   above. The kernel segment's file data is borrowed, while its larger
   `memsz` asks Linux to zero-fill the workspace. Both original and relocated
   appended-DTB probes therefore see zero, not stale RAM containing FDT magic.
4. Grafts live memory into an explicit or appended DTB, preserving the live
   firmware serial when the destination omits it. It retains live firmware
   reservations in the header reserve map except where enabled destination
   `no-map` nodes already protect the region. Duplicating those regions in
   the header can defeat `no-map` on older kernels. Supplied header entries
   remain authoritative. Mismatched DT root cell widths are rejected.
5. Replaces `/chosen/bootargs` and initrd bounds. Initrd addresses are
   big-endian 64-bit values, independent of root address-cell width, matching
   Linux's kexec FDT writer. Old bounds are removed when no initrd is supplied.
6. Places non-overlapping page-aligned segments outside all known reserved
   ranges. The DTB is additionally 1 MiB-aligned and at most 2 MiB, fitting
   the ARM kernel's two early section mappings. It is last because the
   kernel selects the last segment beginning with FDT magic for `r2`.

`/proc/iomem` is not a complete record of memblock reservations. The loader
does not reclaim ambiguous live reserve-map entries, including reservations
that might belong to an old initrd. This can conservatively reject a tight
boot window rather than overwrite memory whose ownership is unknown.

Qualcomm's current ARM32 SMP operations lack `cpu_kill`; the initial loader
kernels must therefore use `CONFIG_SMP=n`. The Expressltexx and Expressatt
device PRs supply that configuration; this base PR does not claim that every
existing ARM32 device configuration is ready. In particular, msm8926 still
needs that bring-up work. Supporting SMP in the destination OS does not
remove the first-kernel constraint.

## Reproduce validation

```sh
cargo test --locked --manifest-path tools/arm32-kexec/Cargo.toml
bash tools/arm32-kexec/run.sh /path/to/linux LLVM=-19
```

See [the smoke harness](../tools/arm32-kexec/README.md) for dependencies and
separate container-build/host-QEMU modes. Live fallback, explicit-DTB and
appended-DTB cases exercise production EFI payload preparation and require
a new kernel, new command line, destination-only initrd sentinel, corrected
RAM map, and the selected DTB's marker. The explicit case must override the
appended tree rather than silently select it; the appended case also
gzip-wraps the complete payload to exercise decompression and memfd creation.
Serial evidence and build identities are attached to the implementation
commit in `refs/notes/evidence`, not copied into this guide.

Emulator success does not validate Krait cache/coherency, secondary CPUs,
Qualcomm display/USB shutdown, or the physical boot chain. Begin hardware
testing with UART, a recovery path, and a known matching destination zImage;
do not replace a working bootloader just because the emulator passed.
