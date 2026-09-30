# ARM32 kexec bring-up

Initial ARMv7 handoff is implemented and tested with two-kernel QEMU boots.
[Expressltexx](samsung-expressltexx.md#hardware-validation) has also completed
two consecutive Pocketboot-to-Pocketboot handoffs on hardware. Expressatt
uses a different kernel tree and remains independently unvalidated.

## Supported first-pass contract

- Little-endian ARMv7 Linux DT boot through legacy `kexec_load`.
- A RAM-loaded `zImage` with the modern six-word kernel-size extension,
  `TEXT_OFFSET=0x8000`, and a 64 KiB decompressor heap. Raw `Image`/`Image.gz`
  and older size-less zImages are rejected rather than guessed. The existing
  payload-preparation path can unwrap a gzip-wrapped **zImage**.
- A destination kernel built for the same platform, using the normal Linux
  DT-aware `AUTO_ZRELADDR` decompressor, or a fixed `ZRELADDR` matching the
  lowest RAM bank plus `TEXT_OFFSET`. For a fixed-address build, verify that
  linker symbol in `arch/arm/boot/compressed/vmlinux`; its value is not in
  the zImage size tag. The DT and `/proc/iomem` must agree on the lowest RAM
  bank, aligned to 2 MiB. Crash-kernel usable-memory overrides and
  nested/disabled memory banks are not supported.
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
2. Removes any appended DTB and adds a zero tail. Otherwise
   `ARM_APPENDED_DTB` can ignore the newly patched DTB in `r2`.
3. Protects destination kernel BSS and the entire relocated decompressor,
   heap, stack, and alignment area. Relocation-code headroom is bounded by
   another full zImage length; decompressor BSS uses the validated limit
   above. The kernel segment itself contains only the file and zero tail,
   not a zero-filled copy of this whole workspace.
4. Grafts live memory into a supplied DTB and retains live firmware
   reservations in its reserve map. `/proc/iomem` is not a complete record
   of memblock reservations. Mismatched DT root cell widths are rejected.
5. Replaces `/chosen/bootargs` and initrd bounds, using the root address-cell
   width. Old initrd properties are removed when no initrd is supplied.
6. Places non-overlapping page-aligned segments outside all known reserved
   ranges. The DTB is last because the ARM kernel selects the last segment
   beginning with FDT magic for `r2`.

Qualcomm's current ARM32 SMP operations lack `cpu_kill`; the initial loader
kernels therefore use `CONFIG_SMP=n`. Supporting SMP in the destination OS
does not remove that first-kernel constraint.

## Reproduce validation

```sh
cargo test --manifest-path tools/arm32-kexec/Cargo.toml --example arm32-kexec-smoke
bash tools/arm32-kexec/run.sh /path/to/linux LLVM=-19
```

See [the smoke harness](../tools/arm32-kexec/README.md) for dependencies and
separate container-build/host-QEMU modes. Both live-DTB and supplied-DTB
cases require a new kernel, new command line, destination-only initrd
sentinel, corrected RAM map, and rejection of an appended poison DTB.
Serial evidence and build identities are attached to the implementation
commit in `refs/notes/evidence`, not copied into this guide.

Emulator success does not validate Krait cache/coherency, secondary CPUs,
Qualcomm display/USB shutdown, or the physical boot chain. Begin hardware
testing with UART, a recovery path, and a known matching destination zImage;
do not replace a working bootloader just because the emulator passed.
