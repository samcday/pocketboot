# SHIFTphone 8 (otter): first PocketBoot trial

Handoff for Chris Vögel (`chri2`), using `qcom/qcm6490-shift-otter`.
This is an experimental RAM-boot image, **not yet hardware-tested**.
The otter PR (`astra/otter-build`) builds on [PR #38][extlinux-pr]
(`astra/extlinux`); exact build/test evidence is recorded in the otter PR.

## Preconditions and the DTBO trap

Use only Chris's **already unlocked, prepared test device**, with owner-confirmed
backups and an agreed, known recovery route. This is not a preparation guide:
do not unlock, flash, erase, change slots or modify vbmeta for this trial.

In [fastboop #118][dtbo-report], Chris reported that bootloader erase left DTBO
content intact; zeroing `dtbo_a` and `dtbo_b` from rooted Android let Linux boot,
but **stock Android no longer booted**. Cleared stock DTBO can break Android;
RAM boot does not undo that pre-existing state. Do not repeat that destructive
workaround. If the loader rejects this image (including
`No match found for Soc Dtb type`), stop and retain the host/UART logs.

The [reported bootloader identity][identity] is USB `18d1:d00d`, serial prefix
`SP8`, product `otter`; verify the actual attached device rather than assuming it.
Chris's distro log reports Linux 7.1.2, and his [2026-09-21 login report][login]
used `modprobe.blacklist=qcom_q6v5_pas`. Neither is a PocketBoot hardware result.

## What this image is trying to do

The [board config](../configs/device/qcom/qcm6490-shift-otter.toml#L1) pins
[sc7280-mainline/linux][kernel] at `v7.1.2-sc7280`,
commit `6f4b7109a64fcb63f2b367a6aa1a487e7ac5e15c`, with a local board DTS patch.
This matches Chris's reported kernel **release**, not a proven identical distro
commit or configuration. The source override is board-scoped.

- Display: simpledrm using an assumed bootloader framebuffer, 1080×2400 at
  `0xe1000000`, UI scale 3. Panel LDO12/13 are retained. This is untested;
  neither native KMS/GPU nor a touchscreen driver is included.
- Built-in UFS, USB2 gadget and power/volume-key drivers are intended for the
  diagnostic path; their presence is not proof that storage, USB or input works.
- The DTS disables PMIC GLINK and the QMP SuperSpeed PHY, sets USB to
  `peripheral`/`high-speed`, retains only the HS PHY and removes `usb-role-switch`.
  This experimental, untested fixed-role USB2 path is intended to avoid the
  [reported ADSP/GLINK orientation-triggered PHY reset][usb-report].
- No remoteproc/ADSP, PMIC GLINK/PD/charging, Wi-Fi, modem or Bluetooth support.
  `clk_ignore_unused pd_ignore_unused` are bring-up measures, not full power
  management. Charge beforehand; do not rely on charging while this image runs.
- UART diagnostics: `earlycon console=ttyMSM0,115200n8 pocketboot.log=debug loglevel=8`.
- Packaging: gzip `Image`, separate DTB, Android boot header v2, page size 4096,
  base zero, kernel offset `0x8000`; embedded initramfs, no separate Android ramdisk.

## Obtain the exact image

Record the otter PR URL, full PR head SHA, Actions run URL and image SHA-256
from the handoff before testing. Use that run's auto-discovered artifact
`bootimg-qcom-qcm6490-shift-otter`, extracting its `boot.img` into `otter/`.
Do not substitute an artifact from a different head or merely use “latest”.
Alternatively, from that exact source head:

```sh
cargo xtask kernel qcom/qcm6490-shift-otter
cargo xtask bootimg qcom/qcm6490-shift-otter
mkdir -p otter
cp target/kernel/qcom/qcm6490-shift-otter/boot.img otter/boot.img
sha256sum otter/boot.img
```

For a downloaded image, also run `sha256sum otter/boot.img` and compare against
the handoff. For a local build, report its own hash and build logs.

## First test: observe and collect, do not boot an OS

1. Confirm the preconditions above with the owner, charge the phone, use a known
   cable and start capture on Chris's existing debug UART (115200 8N1).
   Record host fastboot output and USB enumeration events as well.
2. In the bootloader, use `fastboot devices` and
   `fastboot -s SERIAL getvar product` to confirm the target. Replace `SERIAL`
   with its actual serial. Only after those checks, RAM-boot:

   ```sh
   fastboot -s SERIAL boot otter/boot.img
   ```

3. Wait and observe the display and USB independently: note the time to any UI,
   blank/frozen display, disconnect, re-enumeration or reset. A blank display
   alone does not prove a dead kernel. Check `fastboot devices` again; the
   PocketBoot gadget's serial may differ. Do not assume the original bootloader
   endpoint is PocketBoot.
4. If PocketBoot fastboot is available, substitute its serial below. Each OEM
   shell command replaces the staged output: fetch it before the next command.
   These are PocketBoot diagnostics, not stock bootloader OEM commands.

   ```sh
   fastboot -s SERIAL oem 'shell:dmesg'
   fastboot -s SERIAL get_staged otter-dmesg.txt
   fastboot -s SERIAL oem 'shell:cat /proc/partitions'
   fastboot -s SERIAL get_staged otter-partitions.txt
   fastboot -s SERIAL oem 'shell:cat /proc/cmdline'
   fastboot -s SERIAL get_staged otter-cmdline.txt
   ```

5. If the screen stays blank and no PocketBoot USB appears, stop and use the
   agreed recovery route. Also stop on a loader rejection or repeated resets;
   keep logs rather than changing partitions to make the image boot.

Do not select an OS, use `fastboot continue` or flash anything in this trial.
Report image/head/run identifiers, UART and host logs, staged files, display
observations, USB stability and recovery outcome. Redact device serials and
other private identifiers before posting logs; keep an unredacted local copy.

Kexec is a separate later milestone, after boot, USB, storage, input and recovery
are proven. #38 supplies extlinux `fdtdir`, path and symlink resolution, but does
not guarantee discovery of an existing pmOS partition; see
[extlinux discovery limits](extlinux.md#extlinux-boot-entries).

[extlinux-pr]: https://github.com/samcday/pocketboot/pull/38
[kernel]: https://github.com/sc7280-mainline/linux/tree/6f4b7109a64fcb63f2b367a6aa1a487e7ac5e15c
[identity]: https://github.com/samcday/fastboop/issues/118#issuecomment-4518486214
[dtbo-report]: https://github.com/samcday/fastboop/issues/118#issuecomment-5728684630
[usb-report]: https://github.com/samcday/fastboop/issues/118#issuecomment-5737064822
[login]: https://github.com/samcday/fastboop/issues/118#issuecomment-5756287819
