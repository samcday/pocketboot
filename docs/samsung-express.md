# Samsung Galaxy Express build targets

These are **experimental bring-up images**, not validated replacement
bootloaders. In particular, pocketboot's userspace `kexec_load` implementation
is currently ARM64-only: the ARMv7 builds cannot hand off to another OS through
the boot menu yet. A successful build does not establish a successful hardware
boot, display takeover, USB connection, or kexec.

## Devices and sources

| Target | Hardware | Kernel source |
| --- | --- | --- |
| `qcom/msm8930-samsung-expressltexx` | GT-I8730, MSM8930, PM8917 | Sam's `samsung-expressltexx` tree |
| `qcom/msm8960-samsung-expressatt` | SGH-I437, MSM8960, PM8921 | LogicalErzor's `expressatt` tree |

The device configs pin full commits from the respective integration trees.
Neither is a claim of complete upstream Linux support. The boards share much
of the Krait-era Qualcomm driver stack, but their DTBs, PMIC rails, GPIO
wiring, and boot parameters are not interchangeable.

The Expressltexx target restores automated builds of the earlier pocketboot
port, using the Linux revision recorded by
[`samsung-expressltexx-mainlining`](https://github.com/samcday/samsung-expressltexx-mainlining).
Expressatt follows LogicalErzor's
[kernel tree](https://codeberg.org/LogicalErzor/linux/src/branch/expressatt) and
[QuickDirtyBoot boot-image recipe](https://codeberg.org/LogicalErzor/QuickDirtyBoot/src/commit/b3846d7dadf6dc439973c613ce5a577cceff98fb/device/samsung/expressatt/device.config).
Its addresses are also recorded in
[pmaports deviceinfo](https://codeberg.org/LogicalErzor/pmaports/src/commit/95e3c7bbdb500a50445ea77fa763c0071fea0dd8/device/testing/device-samsung-expressatt/deviceinfo).

## Build

Use the toolchain from `.github/Dockerfile`, including the ARMv7 musl Rust
target and the ARM hard-float tools/headers used for BusyBox:

```sh
rustup target add armv7-unknown-linux-musleabihf
cargo xtask build qcom/msm8930-samsung-expressltexx
cargo xtask build qcom/msm8960-samsung-expressatt
```

`xtask` fetches the pinned sources and builds the normal pocketboot config
fragments, not the integration tree's full `qcom_defconfig`. Each image has
pocketboot and BusyBox built into the kernel's initramfs. Expressatt also uses
the existing one-byte boot-image ramdisk placeholder, preserving
QuickDirtyBoot's nonempty external-ramdisk convention.

Outputs:

```text
target/kernel/qcom/msm8930-samsung-expressltexx/boot.img
target/kernel/qcom/msm8960-samsung-expressatt/boot.img
```

Both targets are automatically included by `cargo xtask ci-matrix`. The CI
workflow publishes separate `bootimg-qcom-msm8930-samsung-expressltexx` and
`bootimg-qcom-msm8960-samsung-expressatt` artifacts.

## Boot and driver choices

Both images use an Android v0 header, 2048-byte pages, and `zImage` with their
own DTB appended. This preserves the stock-aboot ATAGS path: the ARM
decompressor finds the appended DTB and merges the bootloader's memory map
and command line. No ARM64 pocketpreboot shim is involved.

| Parameter | Expressltexx | Expressatt |
| --- | --- | --- |
| Base | `0x80200000` | `0x80200000` |
| Kernel offset | `0x00008000` | `0x00008000` |
| Ramdisk offset | `0x02200000` | `0x01500000` |
| Tags offset | `0x02000000` | `0x00000100` |
| DTB stem | `qcom-msm8930-samsung-expressltexx` | `qcom-msm8960-samsung-expressatt` |

- Both build storage, USB gadget, GPIO/power buttons, maXTouch, and touchkey
  drivers in. Expressltexx needs the local PM8917 direct-regulator driver for
  the touchscreen's L31 rail.
- Expressltexx retains the existing simpledrm path using the framebuffer
  described by Sam's DT. It relies on a bootloader-initialized display; this
  change does not switch it to native DRM.
- Expressatt's DT has no simple framebuffer. It builds the local MDP4,
  MSM8960 DSI PHY, MSM IOMMU, RPM interconnect, and Magnachip AMS452GP32 panel
  drivers. The pocketboot overlay disables the GPU and its IOMMU, and forces USB
  peripheral mode. The software-rendered UI does not need Adreno firmware.
  This native display path still needs a pocketboot hardware test.
- Both loader kernels deliberately disable SMP. Qualcomm's ARM32 SMP ops
  lack `cpu_kill`; with multiple possible CPUs, `machine_kexec_prepare()`
  rejects the handoff because the platform cannot safely hot-unplug them.
  Enabling `HOTPLUG_CPU` or merely offlining a CPU does not fix that.
  QuickDirtyBoot uses the same constraint. A future destination OS can still
  enable both CPUs; implementing pocketboot's ARM32 loader is separate work.
- No Wi-Fi, modem, or GPU firmware is bundled. Charging is not enabled or
  validated by these configs.

## Hardware validation still required

Do not blindly flash these images over a working bootloader. Establish the
device's current stock-aboot/lk2nd/U-Boot chain, image-size limit, partition
mapping, and recovery method first. In particular, Expressatt's pmaports
configuration names `system` as its fastboot kernel partition; that is not
evidence that `fastboot flash boot` is safe. The Expressltexx mainlining
workspace also uses a separate `Image.gz` path for lk2nd; do not assume these
stock-style `zImage` images are equivalent.

For each board, record:

1. Boot chain/version and kernel/DTB identity.
2. UART reaching `/init`, or a working USB debug connection.
3. Display refresh, touch coordinates, and GPIO/power key events.
4. eMMC and SD enumeration and OS discovery without modifying partitions.
5. USB fastboot/ADB operation and return to the previous boot chain.
6. After ARM32 loader support exists: a real kernel handoff with the live
   memory map preserved, initrd/cmdline delivered, and both destination CPUs
   checked.
