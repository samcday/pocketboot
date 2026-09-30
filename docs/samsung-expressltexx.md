# Samsung Galaxy Express GT-I8730 (Expressltexx)

This is an **experimental bring-up image**, not a hardware-validated
replacement bootloader. ARM32 handoff is developed separately; see
[the ARM32 kexec bring-up](arm32-kexec.md) for its current scope and evidence.
Expressatt uses a different kernel tree and is a separate device port.

## Source and build

The device config pins Sam's MSM8930/PM8917 integration tree to the Linux
revision recorded by
[`samsung-expressltexx-mainlining`](https://github.com/samcday/samsung-expressltexx-mainlining).
This restores automated builds of the earlier pocketboot port; it does not
claim complete upstream Linux support.

Use the toolchain from `.github/Dockerfile`, including the ARMv7 musl Rust
target and the ARM hard-float tools/headers used for BusyBox:

```sh
rustup target add armv7-unknown-linux-musleabihf
cargo xtask build qcom/msm8930-samsung-expressltexx
```

The output is `target/kernel/qcom/msm8930-samsung-expressltexx/boot.img`.
CI discovers the pinned target automatically and publishes
`bootimg-qcom-msm8930-samsung-expressltexx`.
The normal pocketboot config fragments are used, not the integration tree's
full `qcom_defconfig`. Pocketboot and BusyBox are built into the initramfs.

## Boot and driver choices

- Android v0, 2048-byte pages, `zImage` plus
  `qcom-msm8930-samsung-expressltexx.dtb`. Stock aboot supplies ATAGS; the ARM
  decompressor finds the appended DTB and merges the bootloader's RAM map.
- Base `0x80200000`, kernel offset `0x8000`, ramdisk offset `0x02200000`,
  tags offset `0x02000000`. Preserve the one-byte external ramdisk placeholder
  found in the hardware-booted stock wrapper; the real initramfs is built in.
  No ARM64 pocketpreboot shim.
- Supply an explicit UART command line. With an empty boot-image command line,
  stock aboot injects `mem=100M console=null`, overriding the firmware RAM map
  and exposing SMEM as normal RAM. Do not add a synthetic `mem=` limit.
- Retain the existing simpledrm path using the framebuffer described by
  Sam's DT. This requires a bootloader-initialized display; this port does
  not switch to native DRM.
- Storage, USB gadget, GPIO/power buttons, maXTouch, and touchkeys are built
  in. PM8917's direct L31 rail powers maXTouch; it is not an RPM regulator.
- The loader kernel uses `CONFIG_SMP=n` because Qualcomm's ARM32 SMP ops lack
  `cpu_kill`. No Wi-Fi, modem, or GPU firmware is bundled. Charging is not
  enabled or validated.

## Hardware validation

Before testing, establish the current stock-aboot/lk2nd/U-Boot chain,
partition limits, and recovery method. Do not overwrite a working bootloader
as the first test. The mainlining workspace uses a separate `Image.gz` path
for lk2nd; do not assume this stock-style `zImage` is interchangeable.

Record the bootloader version, kernel/DTB identity, UART output through
`/init`, display refresh, touch/key events, eMMC/SD enumeration, USB debug
operation, and return to the previous boot chain. Validate kernel handoff,
initrd/cmdline delivery, and destination CPUs separately. A cartkit UART jig
may be available, but its connection and device profile need confirmation
before use.
