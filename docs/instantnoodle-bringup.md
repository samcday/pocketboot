# OnePlus 8 (instantnoodle): experimental pocketboot

This target is for **build validation and supervised RAM-boot experiments**.
It has not been boot-tested on a OnePlus 8. A successful CI job is not a claim
that its bootloader accepts the image, USB enumerates, or kexec works.
Do not use it on the OnePlus 8 Pro (`instantnoodlep`) or 8T (`kebab`).

## Sources and scope

- Kernel and board DTS:
  [Xo666/mainline-instantnoodle at 408762bfe17332622560658f394f9cdbb1c9902e](https://github.com/Xo666/mainline-instantnoodle/tree/408762bfe17332622560658f394f9cdbb1c9902e).
  This is an out-of-tree, Linux 6.16.7-based board bring-up tree, not upstream
  OnePlus 8 support. The pin belongs to the device config, not every SM8250
  device. The DTB is `qcom/sm8250-oneplus-instantnoodle.dtb`.
- Android boot-image geometry:
  [LineageOS SM8250 BoardConfigCommon.mk at d772af0971b01a6b4f9b0d265a9476df494ef280](https://github.com/LineageOS/android_device_oneplus_sm8250-common/blob/d772af0971b01a6b4f9b0d265a9476df494ef280/BoardConfigCommon.mk),
  imported by
  [instantnoodle BoardConfig.mk at 78968fcd1ba37e69436b1855c99a7e3da8738162](https://github.com/LineageOS/android_device_oneplus_instantnoodle/blob/78968fcd1ba37e69436b1855c99a7e3da8738162/BoardConfig.mk).
  These specify header v2, a 4096-byte page, base zero, an uncompressed `Image`,
  and a separate DTB section. The unspecified offsets use the Android
  mkbootimg defaults. The documented boot partition size is 96 MiB; this is
  an image-size reference, **not** a verified fastboot download limit.

Pocketboot embeds its initramfs in the kernel, so the boot image has no
separate Android ramdisk. The minimal configuration enables UFS, USB gadget,
power/volume keys, the S6SY761 touchscreen, and simpledrm. It deliberately
does not copy the large `op8_defconfig` or enable telephony, cameras, Wi-Fi,
or native MSM display/GPU drivers. This kernel runs pocketboot; it is not a
smoo-root distro kernel and does not need smoo's ublk configuration.

The board DTS describes a 1080x2400 framebuffer at `0x9c000000`. Its validity
on the tester's firmware is an **unverified assumption**. The overlay forces
USB peripheral mode and retains the three panel power rails while simpledrm
uses that buffer. `clk_ignore_unused pd_ignore_unused` preserves firmware
clock/power-domain state for this first experiment. These are bring-up
measures, not a complete panel, Type-C, charging, or power-management solution.
Even a working screen would not establish suspend/resume or native display
takeover.

## Build or download

With the toolchain used by `.github/Dockerfile`:

```sh
cargo xtask kernel qcom/sm8250-oneplus-instantnoodle
cargo xtask bootimg qcom/sm8250-oneplus-instantnoodle
```

The result is:

```text
target/kernel/qcom/sm8250-oneplus-instantnoodle/boot.img
```

The ordinary CI matrix discovers this target from its configuration and
uploads `bootimg-qcom-sm8250-oneplus-instantnoodle`. On the draft PR, select
a CI run for the exact commit being tested and check that this device's job
succeeded. Download the artifact from that run, or use:

```sh
gh run download RUN_ID --repo samcday/pocketboot \
  --name bootimg-qcom-sm8250-oneplus-instantnoodle \
  --dir instantnoodle
```

Record the PR commit, CI run URL, and `sha256sum instantnoodle/boot.img`
with the test report. The hash identifies the tested file; it is not a
substitute for choosing the intended CI run.

## First hardware test

Before attempting a boot:

1. Confirm the exact model is an `instantnoodle`, its installed firmware,
   and its existing bootloader-unlock state. This guide does not unlock the
   device: unlocking can wipe data and is a separate decision.
2. Back up important data and know how to return this particular device to
   its original bootloader/OS. Do not start with an irreplaceable daily driver.
3. Use a charged battery, a known-good data cable, and only the intended
   phone connected. Record `fastboot getvar product` and
   `fastboot getvar current-slot`, omitting serial numbers from public logs.

For an already-unlocked device whose owner has agreed to the experiment,
the only initial boot operation is:

```sh
fastboot boot instantnoodle/boot.img
```

Do **not** flash the image, erase `dtbo`/`vendor_boot`/`vbmeta`, change slots,
or disable verified boot to work around a rejection. Record the exact
fastboot response and stop instead. The OnePlus 6T's slot behaviour is not
assumed to describe this phone.

If Linux starts, first check whether the display changes and the USB gadget
enumerates. A black screen does not necessarily mean the kernel failed.
Once connected to **pocketboot's** fastboot implementation, collect:

```sh
fastboot oem 'shell:dmesg'
fastboot get_staged instantnoodle-dmesg.txt
fastboot oem 'shell:cat /proc/partitions; cat /proc/cmdline'
fastboot get_staged instantnoodle-system.txt
```

Record display, button/touch, UFS, and USB observations separately. Test cable
orientation and reconnect behaviour explicitly before relying on USB recovery.
Keep the first trial short; charging, thermals and idle power are not validated.
Do not use pocketboot's flash commands or select another OS as part of this
first test. RAM-loading this image does not make arbitrary later commands
or a chainloaded OS non-mutating.

Kexec into a separately agreed test kernel is a later milestone, after the
initial boot, logging, and return to the original OS have been demonstrated.

## Evidence needed before calling this boot-tested

- Exact model/firmware, source/PR commits, artifact hash and CI run.
- Bootloader acceptance versus actual Linux startup.
- Dated kernel log, including the selected DT model/compatible, UFS and UDC.
- Display output and input tested independently of USB.
- Return to the original OS without partition or slot changes.
- A separate result for kexec; successfully running `/init` does not prove it.
