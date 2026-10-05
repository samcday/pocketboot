# Samsung Galaxy Express SGH-I437 (Expressatt)

This is an **experimental bring-up image**, not a hardware-validated
replacement bootloader. ARM32 handoff is developed separately; see
[the ARM32 kexec bring-up](arm32-kexec.md) for its current scope and evidence.
Expressltexx uses a different kernel tree and is a separate device port.

## Source and build

The device config pins
[LogicalErzor's MSM8960/PM8921 integration tree](https://codeberg.org/LogicalErzor/linux/src/branch/expressatt),
not an upstream Linux release. Boot packaging follows
[QuickDirtyBoot's Expressatt recipe](https://codeberg.org/LogicalErzor/QuickDirtyBoot/src/commit/b3846d7dadf6dc439973c613ce5a577cceff98fb/device/samsung/expressatt/device.config)
and [pmaports deviceinfo](https://codeberg.org/LogicalErzor/pmaports/src/commit/95e3c7bbdb500a50445ea77fa763c0071fea0dd8/device/testing/device-samsung-expressatt/deviceinfo).

Use the toolchain from `.github/Dockerfile`, including the ARMv7 musl Rust
target and the ARM hard-float tools/headers used for BusyBox:

```sh
rustup target add armv7-unknown-linux-musleabihf
cargo xtask build qcom/msm8960-samsung-expressatt
```

The output is `target/kernel/qcom/msm8960-samsung-expressatt/boot.img`.
CI discovers the pinned target automatically and publishes
`bootimg-qcom-msm8960-samsung-expressatt`.
The normal pocketboot config fragments are used, not the integration tree's
full `qcom_defconfig`. Pocketboot and BusyBox are built into the initramfs.

## Boot and driver choices

- Android v0, 2048-byte pages, `zImage` plus
  `qcom-msm8960-samsung-expressatt.dtb`. Stock aboot supplies ATAGS; the ARM
  decompressor finds the appended DTB and merges the bootloader's RAM map.
- Base `0x80200000`, kernel offset `0x8000`, ramdisk offset `0x01500000`,
  tags offset `0x100`. The existing one-byte external-ramdisk placeholder
  preserves QuickDirtyBoot's nonempty-ramdisk convention. No ARM64 shim.
- The DT has no simple framebuffer. Build the local MDP4, MSM8960 DSI PHY,
  MSM IOMMU, RPM interconnect, and Magnachip AMS452GP32 panel drivers. The
  overlay disables Adreno and its IOMMU, and selects USB peripheral mode.
  The software-rendered UI needs no GPU firmware.
- Storage, USB gadget, GPIO/power buttons, maXTouch, and TC360 touchkeys are
  built in. The loader kernel uses `CONFIG_SMP=n` because Qualcomm's ARM32
  SMP ops lack `cpu_kill`. No Wi-Fi, modem, or GPU firmware is bundled.
  Charging is not enabled or validated.

## Hardware validation

Before testing, establish the current boot chain, partition limits, and
recovery method. Do not overwrite a working bootloader as the first test.
The pmaports configuration names `system` as its fastboot kernel partition;
that is not evidence that `fastboot flash boot` is safe.

Record the bootloader version, kernel/DTB identity, UART output through
`/init`, native display refresh, touch/key events, eMMC/SD enumeration, USB
debug operation, and return to the previous boot chain. Validate kernel
handoff, initrd/cmdline delivery, and destination CPUs separately.
