# Lenovo Tab 4 10 TB-X304F bring-up

The first goal is a repeatable PocketBoot boot with a working USB diagnostic
path, before installing PocketFed or replacing Android. This is a bring-up
record, not a claim that PocketBoot supports this tablet yet.

## Verified starting state

Read-only queries against one stock TB-X304F on 2026-09-29 returned:

| Query | Result |
| --- | --- |
| `getvar product` | `MSM8917` |
| `getvar secure` | `yes` |
| `getvar unlocked` | `no` |
| `oem device-info` | Tampered, unlocked, and critical unlocked all `false` |
| `flashing get_unlock_ability` | `0` |
| `getvar max-download-size` | `0x1ff00000` (511 MiB) |
| `getvar partition-size:boot` | `0x4000000` (64 MiB) |
| `getvar partition-size:recovery` | `0x4000000` (64 MiB) |
| `getvar partition-size:aboot` | `0x100000` (1 MiB) |
| `getvar partition-size:devinfo` | `0x100000` (1 MiB) |
| `getvar partition-size:config` | `0x8000` (32 KiB) |

The bootloader version, display-panel field, and `partition-size:frp` were
empty. An empty value with an `OKAY` response is not evidence that a partition
is absent. These results do not identify the Android firmware version or the
partition layout well enough to authorize a write.

No unlock, reboot, boot, flash, erase, EDL transition, or partition readback
has been performed in this bring-up session. In particular, the disabled
Android OEM-unlock control corresponds to unlock ability being disabled in
the bootloader.

## Read-only preflight

Keep each tablet's serial and USB-port mapping in host-local lab notes, not in
the public repository. Explicitly select the intended tablet for every
command, even when only one tablet appears connected. Do not select the first
device in an enumeration: the lab also contains other fastboot targets.

With `SERIAL` set to the intended tablet's fastboot serial:

```sh
: "${SERIAL:?Set SERIAL to the intended tablet's fastboot serial}"
for variable in product secure unlocked max-download-size \
    partition-size:boot partition-size:recovery partition-size:aboot \
    partition-size:devinfo partition-size:config partition-size:frp
do
    timeout 5s fastboot -s "$SERIAL" getvar "$variable" || exit
done
timeout 5s fastboot -s "$SERIAL" oem device-info
timeout 5s fastboot -s "$SERIAL" flashing get_unlock_ability
```

These are queries only. Record failures and empty fields rather than guessing
their meaning. `MSM8917` is a generic platform identity, not enough to tell two
tablets apart or prove the exact Lenovo model.

## Unlock gate

The suggested starting reference is the
[TB-X304F/L greyed-out OEM-unlock guide][unlock-guide]. Before using any
workaround:

1. Obtain the owner's explicit permission for bootloader unlocking and the
   expected factory reset; back up anything they want to keep.
2. Verify the full procedure and its applicability to the exact model and
   firmware. A search snippet or a similarly named Lenovo tablet is not
   sufficient.
3. If the procedure involves Qualcomm EDL, identify the exact compatible
   programmer and recovery firmware. Establish and verify a readback path
   before proposing partition edits. Keep device-unique data and raw backups
   private.
4. Explain any proposed low-level write separately and get approval before
   executing it. Do not erase a protection partition, overwrite bootloader
   stages, or transplant another tablet's partition image as an experiment.
5. Re-query the lock state after the unlock/reset. Do not infer success from
   the host command's exit status alone.

There is no approved or device-validated unlock recipe in this document yet.
Unlocking the bootloader is not the same operation as disabling secure-boot
fuses; do not attempt fuse changes.

## First-boot milestones

Keep unlock/recovery, the first mainline boot, and the later PocketFed install
as separate steps:

1. **Recovery route:** verify physical recovery/fastboot entry and how to
   recover a hung boot before trying an experimental image. Preserve a way
   back to stock.
2. **Bootstrap:** inspect existing [lk2nd][] device support and its exact
   build/image requirements. Prefer a transient boot where the stock
   bootloader supports it; do not assume that `fastboot boot` works, or
   silently fall back to flashing `boot`.
3. **PocketBoot:** select the exact board DT and kernel source, add the minimal
   SoC/device configuration, and validate the boot-image layout. A related
   Qualcomm config is a reference, not evidence of compatibility.
4. **Observable boot:** require PocketBoot's USB fastboot identity
   (`product=pocketboot`, `is-userspace=yes`, and the expected board
   `compatible`), then retrieve a kernel log. A lit display alone does not
   prove a working kernel or a usable iteration loop.
5. **Handoff:** only after repeatable boots and recovery, validate kexec and
   then a PocketFed deployment. Preserve the fastboot recovery path while
   doing so.

For every trial, retain the source revision, build configuration, image
SHA-256, boot entry path, elapsed time, observed result, and recovery action.
Publish concise sanitized evidence, not personal device data or raw backups.

## Lab setup constraints

Start with an explicitly addressed USB connection and a person available to
operate the tablet's buttons. Record stock fastboot, Android/ADB, lk2nd,
PocketBoot, and EDL identities only as those modes are actually observed;
serials can differ between modes.

The [DB410c relay setup][lab-relay] switches external board power. This tablet
has an internal battery: switching USB VBUS is not a verified hard-reset
mechanism. Do not reuse the DB410c relay channel or assume any spare channel
is wired to the tablet. Button/reset automation needs a separately verified
wiring and recovery procedure.

`~/src/lk2nd` can be inspected without attaching it to a Delta thread. Attach
it before making changes there; prefer existing device support over a new
fork. A reusable agent skill can follow once the device-control and recovery
contract has actually been tested.

[unlock-guide]: https://xdaforums.com/t/guide-unlock-bootloader-of-lenovo-tab-4-10-with-oem-unlock-greyed-out-tb-x304f-l-and-other-qcom-tablets.4201857/
[lk2nd]: https://github.com/msm8916-mainline/lk2nd
[lab-relay]: https://github.com/samcday/skills/pull/1
