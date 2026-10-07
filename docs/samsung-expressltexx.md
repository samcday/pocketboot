# Samsung Galaxy Express GT-I8730 (Expressltexx)

This is an **experimental development image** for booting pocketboot's kernel
and `/init` on the device. **ARM32 kexec is not supported yet**: booting another
OS, including via `fastboot boot`, is not part of this target's supported scope.
Expressatt uses a different kernel tree and is a separate device port.

## Source and build

The device config pins Sam's MSM8930/PM8917 integration tree to
`3bc07d096bddd33218b8c13b9fa30ffdaba3c050`, the Linux revision used by
[`samsung-expressltexx-mainlining`](https://github.com/samcday/samsung-expressltexx-mainlining).
This restores automated builds of the earlier pocketboot port; it does not
claim complete upstream Linux support.

Use the toolchain from `.github/Dockerfile`, including the ARMv7 musl Rust
target and the ARM hard-float tools/headers used for BusyBox:

```sh
rustup target add armv7-unknown-linux-musleabihf
cargo xtask build qcom/msm8930-samsung-expressltexx
```

The output is
`${CARGO_TARGET_DIR:-target}/kernel/qcom/msm8930-samsung-expressltexx/boot.img`.
CI discovers the pinned target automatically and publishes the artifact
`bootimg-qcom-msm8930-samsung-expressltexx`.
The normal pocketboot config fragments are used, not the integration tree's
full `qcom_defconfig`. Pocketboot and BusyBox are built into the initramfs.

## Cold-boot contract

- Android v0, 2048-byte pages, `zImage` plus the appended
  `qcom-msm8930-samsung-expressltexx.dtb`. Stock aboot supplies ATAGS; the ARM
  decompressor finds the appended DTB and merges the bootloader's RAM map.
- Base `0x80200000`, kernel offset `0x8000`, ramdisk offset `0x02200000`,
  tags offset `0x02000000`. Preserve the one-byte external ramdisk placeholder
  in the hardware-booted wrapper; the real initramfs is built in.
  No ARM64 pocketpreboot shim.
- Supply an explicit UART command line. With an empty boot-image command line,
  stock aboot injects `mem=100M console=null`, overriding the firmware RAM map
  and exposing SMEM as normal RAM. Do not add a synthetic `mem=` limit.
- Disable `ARCH_MULTIPLATFORM` and `AUTO_ZRELADDR`, set
  `PHYS_OFFSET=0x80200000`, and retain `ARM_PATCH_PHYS_VIRT`.
  The resulting `ZRELADDR=0x80208000` keeps decompression and its initial page
  tables out of SMEM. The automatic path skips its early RAM check with an
  appended DTB and otherwise chooses `0x80008000`, before ATAG conversion.
- Preserve the uniprocessor configuration used for the recorded cold boots.
  Retain simpledrm, which requires a bootloader-initialized framebuffer; this
  port does not switch to native DRM.
- Storage, USB gadget, GPIO/power buttons, maXTouch, and touchkeys are built in.
  PM8917's direct L31 rail powers maXTouch; it is not an RPM regulator.
  No Wi-Fi, modem, or GPU firmware is bundled. Charging is not enabled or
  validated.

## Validation and limitations

The device configuration comes from the cold-boot fixes in
[#48](https://github.com/samcday/pocketboot/pull/48). On 2026-09-30, stock aboot
cold-booted the fixed-address kernel after a BOOT-only flash and byte-for-byte
readback. Kernel code was at `0x80208000`, SoC ID was 116, USB debug interfaces
worked, and eMMC and input devices enumerated.

That evidence predates this device-only extraction onto current `main`; it is
not a fresh hardware test of every subsequent build. The old branch also
tested experimental Pocketboot-to-Pocketboot handoffs, but those ARM32 loader
changes are deliberately not included here. Detailed logs and image identities
are preserved in Git notes:

```sh
git fetch origin refs/notes/evidence:refs/notes/evidence
git notes --ref=evidence show cf2ebd6e790b13a1eef86a9d02c8e2caf9cb76ce
```

The recorded checks do **not** establish visual display/touch-event behavior,
external SD, physical UART/earlycon output, rollback to the backed-up stock
BOOT image, or Expressatt support. The UART command line was checked for its
effect on aboot's RAM-map override, not as a physical serial console.

Before testing, establish the current stock-aboot/lk2nd/U-Boot chain, partition
limits, and recovery method. Do not overwrite a working bootloader as the first
test. The mainlining workspace uses a separate `Image.gz` path for lk2nd; do
not assume this stock-style `zImage` is interchangeable.
