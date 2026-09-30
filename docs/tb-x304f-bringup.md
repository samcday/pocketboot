# Lenovo Tab 4 10 TB-X304F bring-up

The experimental profile now has repeatable PocketBoot RAM-boots, working USB
fastboot diagnostics, verified eMMC reads and owner-confirmed visible UI.
This is a bring-up record, not a fully validated tablet port or a PocketFed
installation.

## Current status

- Stock fastboot confirms unlocked; secure boot remains enabled and critical
  partitions remain locked.
- LK2nd RAM-boots with working USB commands, NT35521S display handoff, logs
  and screenshots. Its device-scoped IMEM build can return to stock fastboot.
  No replacement image is installed in `boot` or `aboot`.
- At the owner's explicit request, **only `boot` was erased** after verifying
  the original 64 MiB backup. A normal reboot now falls back to stock fastboot
  in about 2.1 seconds. Recovery, aboot and GPT were not changed.
- The normal image **boots PocketBoot with USB and eMMC automatically**.
  Three RAM-boots verified `product=pocketboot`, `is-userspace=yes` and
  `compatible=lenovo,tbx304x` in 2.062, 2.338 and 1.612 seconds from the host's
  Linux boot invocation. No diagnostic wrapper or manual driver re-probe is
  present in this image.
- All 48 GPT partitions appear on the 14.6 GiB HS400 eMMC. A 17408-byte
  read of the primary GPT through Linux and USB matched the original EDL
  backup byte-for-byte. This validates small read-only transfers, not large
  uploads, writes or kexec.
- Linux starts all four CPUs and PocketBoot reports its 800x1280 DRM UI ready.
  The owner confirmed visible framebuffer output after preserving the
  firmware-enabled LCD rail during fixed-regulator probe. Touch is not yet
  working; native display, charging, Wi-Fi and audio are not validated.
- The target now passes `panic=-1`. With `boot` empty, one complete unattended
  panic cycle returned to verified stock fastboot in 15.5 seconds from the
  Linux boot command; lk2nd reloaded and the crash log was captured by
  16.7 seconds. This covers the observed panic, not arbitrary hard hangs.
- Stock Android previously stopped at its startup/decryption password.
  It cannot boot while `boot` is empty; no factory reset has been issued.
  Preserve the private original images before cleaning `target/` or removing
  the worktree.
- Normal PocketBoot reboot returned to stock fastboot in 4.041 and
  6.301 seconds in the repeat trials. The tablet was left running PocketBoot.
  Fastboot and ADB shells work, and USB remained configured at high speed
  after more than 15 minutes. No additional partition was written during
  these diagnostics.

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

This is the pre-unlock baseline. The disabled Android OEM-unlock control
corresponds to unlock ability being disabled in the bootloader.

## Authorized unlock attempt

The owner has authorized unlocking this expendable development unit, including
data loss and bricking risk. That authorization covers the EDL workaround;
preserving original partitions and verifying the edit remain technical
requirements, not additional consent gates.

The first attempts on 2026-09-29 established:

| Operation | Result |
| --- | --- |
| Stock `fastboot flashing unlock` | Rejected: `oem unlock is not allowed` |
| Stock `fastboot oem edl` | Rejected: `unknown command` |
| Stock `fastboot oem reboot-edl` | Rejected: `unknown command` |
| Stock `fastboot reboot` | Returned to Android, USB `17ef:7bc7` |

All fastboot commands selected the tablet's serial explicitly. Lock-state
queries after the rejected normal unlock still showed locked and untampered.
After the owner enabled USB debugging and authorized the host, stock Android
provided the following identity:

| Source | Result |
| --- | --- |
| Android model/build | TB-X304F, `TB-X304F_S001016_190329_ROW`, Android 8.1.0 |
| `soc0/machine`, `soc0/soc_id` | `APQ8017`, `307` |
| Stock DT `qcom,msm-id` | `<303 0 307 0 308 0 309 0>` |
| Stock DT `qcom,board-id` | `<0x1000b 0>` |
| Selected panel in DT bootargs | `qcom,mdss_dsi_nt35521s_wxga_video` |

Serial-selected `adb reboot edl` successfully entered `05c6:9008` on the same
physical USB port. A Sahara-only query matched both the HWID and full signing
root hash of the programmer below; the programmer was then accepted.

Before writing, the full `devinfo`, `aboot`, `abootbak`, `boot`, `recovery`,
`config`, and `misc` partitions were saved. Two independent `devinfo` reads
matched; both GPT copies were saved and their header/table CRCs verified.
Original files and SHA-256 manifests are retained read-only under the ignored,
host-local `target/tb-x304f-lab/original-partitions/`. These backups are not in
the PR and must be preserved before removing this worktree or cleaning
`target/`; they have not been copied to a separate recovery location.

The original `devinfo` began with `ANDROID-BOOT!` and had DWORD values
`0, 0, 0, 1` at offsets `0x10`, `0x14`, `0x18`, and `0x1c`. The staged image
changed **only byte `0x10`, from `0` to `1`**. It left `0x18` unchanged instead
of copying the second edit in the guide. An immediate pre-write read still
matched the original; after writing the 1 MiB `devinfo` image, a full readback
matched the staged image byte-for-byte. No boot, recovery, bootloader, config,
or GPT partition was written.

After the Firehose reset attempt the tablet returned as Android USB
`17ef:7bc7`, without ADB. The reset command also logged a default-sector-size
mismatch before USB disconnected. The owner observed **"To start Android,
enter your password"**, despite not having set a password. This is the
post-unlock condition described in the guide, not an ADB authorization prompt.
Stock recovery's factory data reset is the documented route back to usable
Android; it has deliberately not been performed for this Linux bootstrap.

After the owner forced the tablet back to stock fastboot, the bootloader
confirmed `Device unlocked: true`, `Device critical unlocked: false`, and
`Device tampered: false`. Its getvars returned `unlocked=yes`, `secure=yes`,
and still `get_unlock_ability=0`. The disabled unlock-ability setting is not
the current lock state. This confirms the one-byte edit worked on this unit,
without changing the critical-unlock flag or writing bootloader partitions.

## Read-only preflight

Keep each tablet's serial and USB-port mapping in host-local lab notes, not in
the public repository. Explicitly select the intended tablet for every
command, even when only one tablet appears connected. Do not select the first
device in an enumeration: the lab also contains other fastboot targets.

With `SERIAL` set to the intended tablet's fastboot serial:

```sh
(
    : "${SERIAL:?Set SERIAL to the intended tablet's fastboot serial}"
    preflight_status=0
    for variable in product secure unlocked max-download-size \
        partition-size:boot partition-size:recovery partition-size:aboot \
        partition-size:devinfo partition-size:config partition-size:frp
    do
        timeout 5s fastboot -s "$SERIAL" getvar "$variable" || preflight_status=1
    done
    timeout 5s fastboot -s "$SERIAL" oem device-info || preflight_status=1
    timeout 5s fastboot -s "$SERIAL" flashing get_unlock_ability || preflight_status=1
    exit "$preflight_status"
)
```

These are queries only. Record failures and empty fields rather than guessing
their meaning. A failed query does not skip the remaining diagnostics; the
subshell returns nonzero if any query fails. `MSM8917` is a generic platform
identity, not enough to tell two tablets apart or prove the exact Lenovo model.

Lenovo's [platform specification][lenovo-spec] identifies the Wi-Fi TB-X304F
as **APQ8017** and the LTE TB-X304L as **MSM8917** (Snapdragon 425, four
Cortex-A53 cores). Do not select an LTE programmer, firmware image, or board
DT from the stock fastboot product string.

## Unlock gate

The [TB-X304F/L greyed-out OEM-unlock guide][unlock-guide] describes a direct
EDL/Firehose `devinfo` rewrite, **not** a normal user-confirmed fastboot unlock.
The linked archived BLUnlocker scripts independently confirm the partition
operations: [dump_devinfo.bat][devinfo-read] uses `emmcdl -d devinfo` to read
the partition into a host file; [unlock.bat][devinfo-write] uses
`emmcdl -b devinfo devinfo.img` to write that file back. The write script prints
"Bootloader Unlocked" without checking the command's exit status.

Although the live XDA page returned a web challenge, its [archived
illustration][devinfo-illustration] was recovered. It circles the bytes at
offsets **`0x10` and `0x18`**, both set to **`0x01`**, in a file starting with
`ANDROID-BOOT!`. The image SHA-256 is
`08854b18206856a5dcc9690625c72235f6e428bad68ff3e46bf422f61c03fe14`.

This verifies what the guide depicts, not every firmware's partition layout.
In particular, do not label `0x18` as "critical unlocked": known LK
layouts use that position for different fields. Inspect the original dump
and preserve all unrelated bytes rather than transplanting the guide's data
or invoking a generic unlock helper that can choose a different partition.

The guide offers model-specific Firehose attachments; this session used the
stock-package programmer described below. Restoring the original partition
has not been tested. The scripts contain no
explicit data-wipe or fuse operation, but the guide describes a possible
recovery data-format step if the modified device boots to a password prompt.
Treat all user data as at risk.

Before using this workaround:

1. Obtain the owner's explicit permission for bootloader unlocking and the
   possible factory reset; back up anything they want to keep.
2. Verify the full procedure and its applicability to the exact model and
   firmware. A search snippet or a similarly named Lenovo tablet is not
   sufficient.
3. Identify the exact TB-X304F-compatible programmer and recovery firmware.
   Establish a verified readback path and preserve a byte-identical original
   `devinfo` backup before proposing edits. Keep device-unique data and raw
   backups private.
4. Ensure the owner's authorization covers the low-level partition write;
   honor authorization already given for this operation. Do not erase a
   protection partition, overwrite bootloader stages, or transplant another
   tablet's partition image as an experiment.
5. Re-query the lock state after the unlock/reset. Do not infer success from
   the host command's exit status alone.

The one-byte write was followed by a successful stock-fastboot unlock check
on this unit; recovery of stock Android remains a separate step.
Unlocking the bootloader is not the same operation as disabling secure-boot
fuses; do not attempt fuse changes.

### Accepted programmer

The XDA TB-X304F programmer attachment is ID `5156731`; the archived page
lists it, but the ZIP itself was not recovered. Instead, the programmer was
extracted from [this third-party mirror's TB-X304F firmware package][stock-zip]:

- Archive: `TB-X304F_S001017_2211021443_Q21000_ROW_GB.zip`.
- Member: `TB-X304F_S001017_2211021443_Q21000_ROW_GB/image/prog_emmc_firehose_8917_ddr.mbn`.
- Size: 375580 bytes; SHA-256:
  `9dadea461c392fb993831b5af3cfcfe7067ab7caef9919b5269887a01ad85ea2`.
- Parsed certificate HWID: `000550E100000000` (APQ8017), OEM/model `0000`.
- Parsed root-certificate hash:
  `92242cf8f6fad111a0b0e2aef2fceb6932ac73d2451037cdc6059da3a4f6dd9d`.

HTTP range reads retrieved the ZIP directory and this member; its ZIP CRC
was checked. The whole archive was not downloaded or hash-verified, and the
mirror is not a Lenovo-origin download. On this unit, Sahara reported exactly
the HWID and full PK hash above, and successfully loaded this programmer.
Firehose reported eMMC with 512-byte sectors and 30535680 total sectors.

The host's installed `edl` launcher failed because its Python package was
missing. Running `python3 ~/src/edl/edl.py --help` from the source checkout at
`51e11022455d26bcf0b8305b930c474e9b3c81ad` worked, as did its offline
`fhloaderparse.py`. Neither the checkout nor the system installation was
modified. Run tools from an ignored local artifact directory and pass
`PYTHONDONTWRITEBYTECODE=1` when using an unattached source checkout.

Do not trust that client's `gpt` success message alone: at this revision the
file-writing statements are commented out, so it says "Dumped" without
creating the files. This session instead read sectors explicitly, then
checked both GPT header CRCs, both entry-array CRCs, and matching entries.
The verified files are `gpt/gpt-main-with-mbr.bin` and
`gpt/gpt-backup-full.bin` in the private backup directory. The backup reserves
32 sectors for its entry array even though the 48 entries occupy only 12;
derive its starting LBA from the backup header, not from entry count alone.

## Existing lk2nd support

Source audit on 2026-09-29 found a useful distinction between the local
checkout, the latest release, and upstream development:

- The inspected local `~/src/lk2nd` checkout at `4a88d4cc9` has no TB-X304
  device definition.
- Upstream [commit 517bb38][lk2nd-tbx304x] adds **Lenovo Tab 4 10x (TB-X304x)**
  in `lk2nd/device/dts/msm8952/msm8917-qrd-sku5.dts`, built by the
  `lk2nd-msm8952` target. Its root uses `qcom,msm-id = <QCOM_ID_MSM8917 0>`
  and `qcom,board-id = <QCOM_BOARD_ID(QRD, 1, 0) 0>`. Its board definition
  selects `lenovo,tbx304x`, the `msm8917-lenovo-tbx304x` kernel DTB, and
  NT35521S, NT35521S-BOE, or JD9364-BOE panel variants.
- [Release 23.1][lk2nd-release] predates that addition. Its generic
  `lk2nd-msm8952.img` is not evidence of TB-X304 support: it lacks the QRD SKU5
  DTB. The MSM8917 MTP DTB is not a substitute for the tablet's QRD selection.
- The earlier [PR #664][lk2nd-pr] was closed during cleanup of unrelated
  formatting changes, not a final rejection of device support; the clean
  addition landed later.

The stock identity now confirms that the panel/board match, but upstream's
root SoC list omits APQ8017. The small
[bootstrap patch](../patches/lk2nd/tb-x304f-apq8017.patch) adds APQ8017 while
retaining MSM8917; no other lk2nd source changes were needed for the initial
bootstrap. The addition was present from the first tested build; an
unmodified control image has not been tested. This is positive evidence for
the patched image, not an observed failure of the unmodified image or proof
that this unit differs from the original device-support author's tablet.

A candidate was built from a source archive of commit `517bb38`, under
`target/tb-x304f-lab/`, leaving `~/src/lk2nd` untouched. Apply the bootstrap
patch with `patch -p1` in that extracted source, then build there:

```sh
make -j16 TOOLCHAIN_PREFIX=arm-none-eabi- \
    LK2ND_DTBS=msm8917-qrd-sku5.dtb \
    LK2ND_FORCE_FASTBOOT=1 DEBUG_FBCON=1 \
    LK2ND_VERSION=517bb38-tbx304f-lab lk2nd-msm8952
```

The output `build-lk2nd-msm8952/lk2nd.img` is 301072 bytes, SHA-256
`394ae757926f15bd38e2901c29033075ae76ba870cf523250787947371b6889a`.
Its appended DT was checked for SoC IDs `<303 0 307 0>` and board ID
`<0x1000b 0>`, and checked to be present inside the Android boot image.
The DT filter takes the basename for this target: prefixing it with
`msm8952/` silently filtered out every appended DT in an earlier build.

The forced-fastboot build is for **transient bootstrap**, not installation.
Never flash its `emmc_appsboot.mbn` output over the signed stock `aboot`.

### Verified transient boot

Stock fastboot accepted this image using `fastboot boot`. The host's download
and boot command completed in about 0.024 seconds. Retrieved lk2nd logs show
device detection at 80 ms, framebuffer detection at 100 ms, and fastboot
processing commands at 600 ms; these are lk2nd's own startup timestamps, not
a measured complete boot-cycle duration.

The running image reported:

| Getvar | Result |
| --- | --- |
| `product` | `lk2nd-msm8952` |
| `lk2nd:version` | `517bb38-tbx304f-lab` |
| `lk2nd:model` | `Lenovo Tab 4 10 (TB-X304X)` |
| `lk2nd:compatible` | `lenovo,tbx304x` |
| `lk2nd:panel` | `qcom,mdss_dsi_nt35521s_wxga_video` |

The `TB-X304X` label comes from the reused family definition; the unit was
independently identified as a TB-X304F. Use **`lk2nd:version`**, not
`version-bootloader` or `lk2nd-version`, to identify this build. An initial
watcher checked the wrong variable and timed out even though lk2nd had booted.

The stock-initialized framebuffer is 800x1280 RGB888, base `0x90001000`,
stride 2400. USB screenshot capture succeeded. The handed-in DT reports
two 1 GiB RAM banks at `0x80000000` and `0xc0000000`.

Once `product=lk2nd-msm8952` is confirmed on the explicitly selected device,
retrieve diagnostics without writing a partition:

```sh
(
    : "${SERIAL:?Set SERIAL to the intended tablet's fastboot serial}"
    : "${OUT:?Set OUT to a private host-local artifact directory}"
    umask 077
    mkdir -p "$OUT" || exit
    timeout 5s fastboot -s "$SERIAL" getvar lk2nd:version || exit
    timeout 5s fastboot -s "$SERIAL" oem log || exit
    timeout 10s fastboot -s "$SERIAL" get_staged "$OUT/lk2nd.log" || exit
    timeout 5s fastboot -s "$SERIAL" oem screenshot || exit
    timeout 15s fastboot -s "$SERIAL" get_staged "$OUT/lk2nd.ppm"
)
```

The framebuffer-debug build draws logs over its menu. A quieter candidate
was also built with `DEBUG_FBCON=0` and
`LK2ND_VERSION=517bb38-tbx304f-lab-quiet`, SHA-256
`68454f94e33c5af86e7ac241a91c194c35df4f6a5cc53c63cf0033fc64efef86`;
it has not been booted yet.

The subsequent return-path test accepted `fastboot reboot bootloader` but
landed in stock Android (`17ef:7bc7`, no ADB), not stock fastboot. Do not
describe the loop as unattended or assume that command is a recovery path
on this firmware.

### Quiet build and IMEM restart experiment

The [restart-protocol patch](../patches/lk2nd/tb-x304f-imem-restart.patch)
makes `USE_PON_REBOOT_REG` overridable while preserving the default value of
`1` for other builds. The Lenovo kernel's [restart path][lenovo-restart]
writes the `0x77665500` bootloader cookie to IMEM. Its
[board PON description][lenovo-pon] deletes `qcom,store-hard-reset-reason`,
selecting the warm-reset path rather than relying on a retained PON reason.
That differs from this lk2nd target's original PON/hard-reset configuration.
This is source evidence for a candidate fix, not verification of stock
aboot's mode-selection implementation.

After applying both lk2nd patches to the same source archive, build:

```sh
make -j16 TOOLCHAIN_PREFIX=arm-none-eabi- \
    LK2ND_DTBS=msm8917-qrd-sku5.dtb \
    LK2ND_FORCE_FASTBOOT=1 DEBUG_FBCON=0 USE_PON_REBOOT_REG=0 \
    LK2ND_VERSION=517bb38-tbx304f-lab-imem lk2nd-msm8952
```

Both compile-time cases were checked: without an override the generated
config still sets `USE_PON_REBOOT_REG=1`; this invocation sets it to `0`.
The resulting image is 299024 bytes, SHA-256
`132ce182d877b42c1645c84f6108202039736b253b9d543929ca257ef2f98d1d`.
It RAM-booted from stock fastboot and returned the expected product, version,
and panel in 0.849 seconds from the host's boot invocation. Its screenshot
shows a readable menu without debug text drawn over it.

LK2nd reported `MPIDR=0x80000100` and `MIDR=0x410fd034`, consistent with
the board port's boot CPU. A later controlled `fastboot reboot bootloader`
test returned to stock `product=MSM8917` in **2.08 seconds**, with unlocked
state preserved. The same lk2nd image then RAM-booted again successfully.
This verifies the requested mode transition from a running lk2nd, not Linux
panic handling or recovery from an arbitrary hang.

A preceding `oem debug readl 0x8ee00000` probe reset lk2nd and returned to
stock fastboot without capturing any RAM-log data. Do not repeat it: that
command directly dereferences an lk2nd virtual address; a Linux DT physical
reservation does not establish an lk2nd mapping. The reset alone is not
evidence that the Linux reserved region is protected or invalid. Use the
supported `oem ramoops` path with a matching handoff instead.

## Kernel and boot-image prerequisites

The lk2nd addition is a bootloader port, not a Linux board port. Upstream Linux
has [MSM8917 SoC support][linux-msm8917], but this audit did not find a
TB-X304F board DT in upstream Linux or a TB-X304F-specific postmarketOS device
profile. PocketBoot now has experimental SoC/device configurations using
the separate board-port source below.

An existing mainline-oriented board port was subsequently found in
[`pem120/linux-msm89x7`][pem120-dts], branch `lenovo-tbx304`, commit
`a51b91b503307d35902447dd1f90db765372cf3b`. It provides
`msm8917-lenovo-tbx304x.dts` for the MSM8917 TB-X304L/X, not yet a validated
APQ8017/TB-X304F target. This is the starting point to adapt and test rather
than writing a board port from scratch. The
[downstream TBX304 kernel DTS][downstream-dts] remains a stock reference.

Also distinguish [lk2nd's 512 KiB partition offset][lk2nd-boot] from an Android
boot-header kernel load offset: the former reserves the start of `boot` for
lk2nd when it is installed persistently. It is not a PocketBoot kernel load
address. Derive this tablet's image layout from verified sources, not another
Qualcomm device's configuration.

## First PocketBoot trial

The [experimental build target](tb-x304f-build.md) is now
`qcom/msm8917-lenovo-tbx304x`. Its 4,734,976-byte image has SHA-256
`54d72e13b8f4d57bf9d751f446fa9ed6f9f22f820bf171074edc12f04fcdff36`.
It packages the pinned kernel with built-in PocketBoot/BusyBox initramfs,
APQ8017 selection metadata, peripheral-only USB, and LK2nd-supplied simplefb.

From the quiet IMEM lk2nd build, the first `fastboot boot` returned `OKAY`
for download (0.148 s) and boot (0.230 s), total 0.381 s. Afterward no USB
device appeared on the tablet's physical port within 45 seconds. There was
no PocketBoot fastboot/ADB/ACM identity to query. A host boot acknowledgement
does not establish Linux entry or userspace startup; neither was confirmed
for this first image.
The owner observed a black screen later, without watching the handoff, and
returned it to fastboot with the buttons. No image was flashed and no factory
reset was performed.

Static comparison against the stock image found no proven CPU/PSCI, GIC,
packaging, or USB-config defect. It did identify a diagnostic gap: the first
image requested `console=tty0` but had no framebuffer console. A blank or
unchanged screen would therefore not prove that Linux never ran.

The next candidate enables the fbdev DRM client and framebuffer console
without changing the DTB or initramfs. It is 4,784,128 bytes, SHA-256
`c707c18b92da380fdc2bfe9158c7c10e33cc09acda09957da186a52fdd03124b`.
Build, generated-config, and image-layout checks passed. Its second trial
was accepted by lk2nd in 0.383 seconds but again produced no tablet USB
within 45 seconds; the owner again saw a black screen later and returned it
to fastboot. The enabled console only helps after simpledrm probes, so this
does not locate the failure before or after Linux entry.

Instead of depending on a live view of the screen, the next logging profile
enables `PSTORE_CONSOLE`. The rebuilt config and kernel include that frontend.
Its region must be shared with lk2nd's supported ramoops exporter; RAM-log
retention through the actual button-reset sequence was unverified when this
candidate was built. The recovered evidence below now demonstrates it for one
trial.

### Shared-region RAM logging trial

The source audit explains why directly reading `0x8ee00000` from lk2nd was
not a safe retrieval method: that aligned physical RAM address lies outside
this build's mapped virtual ranges. An lk2nd translation fault is consistent
with the reset, not proof that the physical region is unusable by Linux.

The supported exporter instead uses the end of scratch:

- Scratch base `0xa0100000`, size `0x1ff00000` (511 MiB), ending at
  `0xc0000000`. The live `max-download-size` query matched this size.
- The final 512 KiB is `0xbff80000` through `0xbfffffff`, inside lk2nd's
  identity-mapped scratch area.
- The first 256 KiB contains 32 dump records of 8 KiB; the last 256 KiB is
  the console. ECC is zero in this lk2nd build.
- This is separate from the small image downloads, kernel/tags, framebuffer
  and inherited firmware reservations. A sufficiently large download can
  still overwrite it; the advertised maximum does not protect the log tail.

The current image enables `PSTORE_CONSOLE` and passes the **bare**
`lk2nd.pass-ramoops` flag. LK2nd uses it to rewrite the existing kernel
ramoops node to this shared region. Never use `lk2nd.pass-ramoops=zap` when
preserving evidence: it clears the window. The kernel must still reach
ramoops/console registration, and the final handed-off DT is not yet captured.

The fixed-size `oem ramoops raw` export was tested before the trial: it
returned exactly 524288 bytes, with no valid pre-trial records, and lk2nd
remained responsive. The raw baseline and decoder metadata are private.
Use this bounded raw export rather than `oem ramoops console`, which trusts
the record's stored size, or the source's FIXME-marked dump decoder.

The shared-region image is 4,784,128 bytes, SHA-256
`c0da6ec5c7c287b94a0e1e814e0f1517da6e091fd75bba3e9c0a43e738bd24d2`.
LK2nd accepted its download and boot in 0.384 seconds; there was still no
tablet USB interface within 45 seconds. On 2026-09-30 the owner reported a
blank screen with possibly lit backlight, uncertain in bright ambient light,
and a return to fastboot. Backlight alone would not establish Linux progress:
it could remain enabled from the bootloader. The post-reset capture below
provides the first direct evidence of this kernel and userspace running.

After button recovery into stock fastboot, RAM-boot only the known lk2nd
image, verify its identity, and retrieve **before another kernel boot**.
For the verified lk2nd image, run from this repository:

```sh
(
    : "${SERIAL:?Set SERIAL to the intended tablet's fastboot serial}"
    umask 077
    mkdir -p target/tb-x304f-lab || exit
    OUT="$(mktemp -d target/tb-x304f-lab/ramoops.XXXXXX)" || exit
    timeout 5s fastboot -s "$SERIAL" oem ramoops raw || exit
    timeout 15s fastboot -s "$SERIAL" get_staged "$OUT/ramoops.raw" || exit
    python3 tools/ramoops_decode.py \
        --input "$OUT/ramoops.raw" --output "$OUT/decoded" --ecc 0 || exit
    printf 'Private capture: %s\n' "$OUT"
)
```

`--ecc 0` needs neither a kernel tree nor a host C compiler. Inspect
`decoded.json` for invalid records rather than equating CLI success with a
valid kernel log. Preserve the raw file and compare against the pre-trial
baseline and expected kernel version. Power loss, reset behavior, cache state
or later bootloader activity can destroy evidence; an empty capture does not
prove that Linux never ran. `oem log` is only the current lk2nd session's log
and cannot recover its pre-reset handoff messages.

### Recovered boot evidence (2026-09-30)

After USB contact was restored, stock fastboot still reported unlocked. Only
the known quiet IMEM lk2nd image was RAM-booted, with its SHA-256, version,
board compatible and panel verified. Its bounded exporter returned all
524288 bytes before any further kernel boot. The raw file is retained
read-only under the private `target/tb-x304f-lab/ramoops-shared/` directory,
SHA-256 `c78edcd6635a8b746cc55000cb8bf045e20db9538bd4ab11eaf47c84d4330216`.

The ECC0 decoder accepted all 33 region headers. It recovered an 18241-byte
console and two compressed dmesg records; the other 30 dump records were
empty. This differs from the pre-trial baseline, which had no valid headers.
The console identifies Linux `7.0.9+ #2`, the expected build time and the
shared-ramoops command line. Selected evidence:

```text
[    0.005453] smp: Brought up 1 node, 4 CPUs
[    0.023263] ramoops: using 0x80000@0xbff80000, ecc: 0
[    0.139817] simple-framebuffer 90001000.framebuffer: [drm] fb0: simpledrmdrmfb frame buffer device
[    0.199061] Run /init as init process
[    5.210396]  WARN pocketboot: local flash settle timed out elapsed_ms=5003 disks=0 partitions=0 events=0 snapshot_changes=0 snapshot=none
[   11.282308] ERROR pocketboot::gadget: USB gadget failed error=Custom { kind: NotFound, error: "no USB device controller (UDC) available" }
[   12.322267] Kernel panic - not syncing: Attempted to kill init! exitcode=0x00000000
```

This establishes successful kernel handoff, SMP, framebuffer-driver
registration, RAM-console logging and PocketBoot userspace execution, not a
working display or USB gadget. The absence of a registered UDC is a
device-side problem to investigate separately from intermittent cable contact.
Storage also remained undiscovered. Check their driver/provider dependencies
before changing boot addresses or the CPU handoff.

Two FunctionFS unmount warnings pass through `ffs_fs_kill_sb()` and
`cancel_work_sync()` during gadget cleanup. They are separate from the missing
controller; the capture does not establish that they caused its absence.
The final panic is PID 1 exiting, not evidence of an early kernel-entry fault.
LK2nd remained responsive after retrieval. No subsequent kernel was booted
as part of this capture.

### RPM mailbox follow-up

The recovered log also contains RPM SMD-edge errors:
`failed to get regmap from syscon: -517`. The legacy `qcom,ipc` path tries to
acquire the APCS node's first clock, the CPU PLL, whose driver is absent.
Enabling that driver alone is not a suitable workaround: the syscon regmap
would explicitly enable/disable it during register accesses. USB and SDHCI
supplies are RPM-managed; enabling the unrelated direct-SPMI regulator driver
would not provide them either. Both config-only candidates were withheld
before hardware use; see the [source analysis](tb-x304f-build.md).

The third maintained kernel patch instead replaces RPM's
`qcom,ipc = <&apcs 8 0>` with `mboxes = <&apcs 0>`. The already-enabled APCS
mailbox driver preserves offset 8/bit 0 using its own clockless regmap.
Its provider may still be delayed by firmware device-link dependencies, so
this is not a guarantee of successful RPM/USB/storage probing.

The three-patch series applied cleanly and a second source invocation was
idempotent. All 54 build-tool tests, formatting and whitespace checks passed.
The rebuilt `7.0.9+ #5` image is 4,784,128 bytes, SHA-256
`36f0efdbd774586f79cd3591cf178cf1838bad98f8cf3eb3657840f651816b7d`.
Decompiling its DTB and the previous image's DTB confirmed that the IPC
property is the only DT change. The initramfs and command line are unchanged;
CPU PLL, APCS mux, CPU frequency scaling and direct-SPMI regulator drivers
remain disabled.

After verifying the known IMEM lk2nd identity and the preserved prior capture,
one transient `fastboot boot` trial returned `OKAY` in 0.384 seconds.
No USB device reappeared on the tablet's port during 45.4 seconds of
observation. No image was flashed and no automatic second kernel attempt
was made.

The subsequent 512 KiB capture has SHA-256
`77a291516370e02fcb62d9f51025ecbe43e51d46d0c731ac971952aee6a5a445`.
Its 18399-byte console identifies the expected `7.0.9+ #5` build and shows
`/init`, zero discovered disks, no UDC and an init-exit panic at 12.442 seconds.
The old syscon-regmap error is absent, but that alone does not prove successful
RPM initialization. All 33 region headers are valid; `dmesg-1` is byte-identical
to the prior trial's record and must not be mistaken for a new crash.

### Automatic panic recovery with an empty boot partition

The first four attempts omitted `panic=`, leaving the kernel's default
`panic=0` indefinite wait. The owner requested `panic=-1` so a panic requests
immediate reboot. The target config and its contract test now require exactly
that setting. This is not a watchdog for arbitrary hangs.

The repackaged image is 4,784,128 bytes, SHA-256
`b77a860331ba78b4aa735222e4eb96cb23d70367816f74c9c9af25cbea9d5ea7`.
Every byte outside the command-line fields matches the mailbox trial image;
kernel, DTB and initramfs are unchanged. All 54 build-tool tests, formatting
and image checks passed. Before changing any partition, this image reset
automatically and returned to stock Android USB at 30.417 seconds, without
ADB or fastboot. After manual recovery, its RAM window had no valid records.
Thus `panic=-1` alone did not establish a useful unattended diagnostic loop.

The owner then explicitly authorized erasing **only `boot`** to test stock
aboot's invalid-boot fallback. Before issuing the erase, the original
`boot`, `recovery`, `aboot` and `abootbak` backups were rehashed against their
read-only manifest. The original boot image is 67108864 bytes, SHA-256
`a1480a980d664213c67d98b89e69d406b11cebee1b81ab046b4bc61cd7f85db9`.
The command was issued from verified **stock** fastboot, not lk2nd, after
checking the target serial/USB port, unlock state and 64 MiB partition size.
`fastboot erase boot` returned `OKAY`; recovery, aboot, GPT and other partitions
were not written. A normal `fastboot reboot` returned to stock fastboot in
2.17 seconds, still unlocked, untampered and critical-locked.

The same panic-enabled image was then tested once more:

1. RAM-boot the known IMEM lk2nd image, verify it, and RAM-boot PocketBoot.
2. Linux reaches the same no-UDC/init-exit panic at 12.454 seconds.
3. Without buttons, stock fastboot returns and is verified at **15.501 seconds**
   from the host's Linux boot invocation.
4. RAM-boot lk2nd again and retrieve the bounded RAM window by **16.729 seconds**.
   The console contains `panic=-1`, the expected kernel and the current panic.

The post-reset raw capture is 524288 bytes, SHA-256
`783e3a6fd91f9d524f656ffd7938d7243307728796ef67f48056bc2778c7f005`.
All 33 headers decode, with a 17489-byte console and two compressed dmesg
records. LK2nd remains responsive after retrieval. This verifies one complete
panic/reboot/log-retrieval cycle with `boot` empty; USB/storage bring-up and
recovery from non-panic hangs remain separate work.

The original boot backup remains private and read-only at
`target/tb-x304f-lab/original-partitions/boot.img`. Preserve it outside this
worktree before any cleanup. Restoring it would require explicitly flashing
it through stock fastboot and would remove this empty-boot fallback; restoration
has not been performed or tested. The pre-existing Android decryption/password
issue is separate from restoring the boot image.

### Deferred-probe evidence

Adding only `deferred_probe_timeout=5` exposed the outstanding suppliers before
the init-exit panic. Kernel, DTB and initramfs bytes were unchanged. The
4,784,128-byte image has SHA-256
`25b8a7b1a86ebaa9a371c7c4a08efeec0319358feea7d8546e1338f8f408f58d`.
The automatic loop returned to stock fastboot in 15.52 seconds and completed
RAM-log retrieval by 16.74 seconds. All 33 record headers decode; the private
raw capture has SHA-256
`1740df71a496921d785ea29a15084a6475cb97f613ac5f5d9c7688c786b727d0`.

The log identifies the actual dependency loop, rather than a guessed missing
regulator driver:

```text
[    5.347368] platform remoteproc: deferred probe pending: qcom-rpm-proc: Failed to register smd-edge
[    5.374193] platform b011000.mailbox: deferred probe pending: platform: wait for supplier /remoteproc/smd-edge/rpm-requests/clock-controller
[    5.401534] platform 6c000.phy: deferred probe pending: platform: wait for supplier /remoteproc/smd-edge/rpm-requests/regulators-0/l13
[    5.429437] platform 78db000.usb: deferred probe pending: platform: supplier 6c000.phy not ready
[    5.443715] platform 7824900.mmc: deferred probe pending: platform: wait for supplier /remoteproc/smd-edge/rpm-requests/regulators-0/l5
[    5.471880] platform 7864900.mmc: deferred probe pending: platform: wait for supplier /remoteproc/smd-edge/rpm-requests/regulators-0/l12
```

RPM needs APCS mailbox channel 0; APCS's declared `ref` input points back to
RPM's clock controller. RPM-managed supplies then block the USB PHY and both
SDHCI controllers. The next change must address this specific relationship,
not globally disable firmware dependency checking. LK2nd remained responsive
after retrieving this trial's evidence.

### APCS clock-child trial

The binding-supported follow-up separates the mailbox from its clock
controller instead of replacing the RPM reference with an unverified fixed-XO
input. Clock inputs, names and `#clock-cells` move to a `clock-controller`
child; all four CPU clock phandles follow it. The mailbox driver creates the
clock platform device with that child's firmware node, retaining the old
mailbox firmware node for legacy layouts. A companion patch makes the clock
driver's PLL lookup use its own device, not the parent; the shared register map
and existing clock name remain on the parent.

Source review found no blockers. The five-patch series applies cleanly and
idempotently; all 54 build-tool tests, formatting and whitespace checks pass.
The compiled DT passes the targeted `qcom,apcs-kpss-global` schema. Binary
checks confirm that the clock child preserves the exact provider phandles and
specifiers, CPU consumers point to the child and RPM still uses mailbox
channel 0. The clock driver was compiled explicitly as an object, but remains
disabled and unlinked in the trial kernel; CPU PLL and frequency scaling
remain disabled too.

The 4,784,128-byte candidate has SHA-256
`99d1a1dec947cab96f3759db0e1012b90179007ba20e3d06c781b1dd500cbf41`.
Kernel image, config, initramfs and command line are byte-identical to the
deferred-probe diagnostic trial. Only the DT changes in the boot payload.

LK2nd accepted one RAM-only boot in 0.384 seconds. No tablet fastboot
interface appeared during 60.05 seconds; a subsequent host check found
neither ADB nor a USB device on the tablet's physical port. This differs from
the earlier 15.5-second panic return, but does not identify whether Linux
stalled or is running without host USB. The owner saw a black screen and
returned it to fastboot with the buttons. No additional partition write or
automatic second kernel trial was made.

The recovered raw capture has SHA-256
`daa042ec91605751d06c54596f32478078f09d6733de00975f32e100a3059b97`.
Its current console reaches `/init`, initializes RPM regulators and continues
through 31.715 seconds. Both compressed dmesg slots are byte-identical to the
previous trial and are stale, not evidence of a new panic. The new failure is
`genpd_provider cx: error -95: Failed to add OPP table for index 0`.
The built config lacks `PM_OPP`, and the pinned OPP helper therefore returns
`-EOPNOTSUPP`. The sixth patch selects that framework from RPMPD without
enabling CPU frequency scaling.

### Bounded USB and storage diagnostics

The separate [probe initramfs](tb-x304f-probe.md) preserves PocketBoot as PID 1,
records bounded read-only state and requests an explicitly marked diagnostic
panic unless cancelled. It is opt-in and not part of normal images.

With `PM_OPP=y` and `pocketboot.log=info`, the RPMPD error disappeared and
`ttyMSM0` registered. The first bounded trial positively logged
`USB gadget bound udc=ci_hdrc.0`; sysfs nevertheless stayed `not attached`
with speed `UNKNOWN`. No deferred devices or disks were listed. The
deliberate panic returned to fastboot at 36.621 seconds and log capture
completed by 37.836 seconds, without buttons.

A second controlled image exposed the existing USB role-switch API, then
requested device role through the switch whose canonical device parent matched
the UDC. PocketBoot USB appeared and passed product/userspace/compatible checks
plus an OEM-shell `uname -r` command at **5.471 seconds**.
After a later snapshot confirmed RPMPD bound, a one-time re-probe of the still
unbound `7824900.mmc` detected HS400 eMMC and all 48 partitions at 12.1 seconds.
The automatic diagnostic recovery completed and preserved the whole log.

These controls isolated two issues rather than serving as permanent workarounds:

- This charger/extcon-free bootstrap needs an explicit USB device-role request.
  The normal image now opts into `pocketboot.usb_role=device`; it writes only
  the unique role switch matching the UDC after binding. Other targets retain
  their existing behavior unless explicitly opted in.
- The built-in-only kernel gave up on the late RPM power-domain provider after
  initcalls, ignoring the requested positive probe timeout. The seventh patch
  honors that bounded wait. Default and negative built-in timeout behavior,
  and all module-enabled behavior, remain unchanged. No global module-loading
  or firmware-dependency bypass was added.

### Verified normal USB and eMMC boot

The ordinary `cargo xtask build qcom/msm8917-lenovo-tbx304x` path produced a
4,810,752-byte image, SHA-256
`45ad387ee315585badce7bee2a5e1792c7482bf907c998c1bf24a79f939710d4`.
Its initramfs contains the normal ELF `/init`, not the diagnostic wrapper;
there are no `pocketboot.probe*` flags or manual re-probe actions.

| Normal-image trial | Linux boot command to verified PocketBoot USB | eMMC |
| --- | --- | --- |
| 1 | 2.062 s | 30535680 sectors, 48 partitions |
| 2 | 2.338 s | 30535680 sectors, 48 partitions |
| 3 | 1.612 s | 30535680 sectors, 48 partitions |

These are host-side RAM-boot timings from lk2nd, not cold-power-on times.
All three trials retrieved a live kernel log over USB. The first also queried
the 64 MiB boot partition and read the first 34 sectors of `/dev/mmcblk0`
without writing it. The 17408-byte result matched the original GPT capture,
SHA-256 `d71622f41619be3ac060171252de68dee0a818c322fae22ebea6ce809f8b86f2`.

The later normal boots show eMMC enumeration and the device-role request
during startup, with local flash settling automatically. They no longer
contain the old OPP error, the SDHCI "assuming no driver" warning, or probe
wrapper markers. Normal `fastboot reboot` returned to stock fastboot in
4.041 and 6.301 seconds between trials; each then reloaded lk2nd and the
same PocketBoot image without buttons.

The final health check found 948.49 seconds of uptime with the UDC still
`configured` at `high-speed`. An explicitly selected ADB shell also returned
the expected kernel release. PocketBoot was left running after these checks.

Validation also includes 190 PocketBoot tests, 54 build-tool tests, 15 probe
archive tests, ShellCheck, formatting, image/config/DT checks and independent
source review. The actual deferred-probe helper was compiled into a host
truth-table check for modules on/off, pre/post-initcalls and timeout -1/0/5.
Small USB reads work; large transfers, writes, kexec, PocketFed and physical
display output were not validated by these three trials. The following
single-change trial establishes visible display output separately.

### Verified visible framebuffer

With USB working, a read-only live inspection distinguished successful KMS
setup from an actually powered panel. The CRTC and plane were active, the
MDSS power domain and display clock gates were on, and panel reset GPIO60
was high. However, `lcd_3v3` was disabled with zero users and its active-high
enable GPIO46 was low.

The pinned fixed-regulator driver requests output-low unless the regulator
has `regulator-boot-on`. `regulator_ignore_unused` skips later unused-rail
cleanup, not this probe-time action. The native DSI panel, normally the rail's
consumer, is deliberately absent from the firmware-framebuffer profile.
Patch 0002 now adds only `regulator-boot-on` to this supply; its fixed 3.3 V
value and GPIO polarity are unchanged. No raw GPIO writes, `always-on`
constraint, backlight adjustment or native display driver was added.

The 4,810,752-byte candidate has SHA-256
`0e69d2e2e346d87032d60ef5a3cac999aa41aff8a001af2eac1554509f7792dd`.
Kernel, config, initramfs and command line were byte-identical to the prior
normal image; the DT's sole semantic change was that boot-on property.
The fixed-regulator schema check, including its inherited regulator schema,
passed without diagnostics.

After rebooting through stock fastboot and LK2nd to reinitialize the panel,
PocketBoot USB returned in 2.386 seconds. Live state showed `lcd_3v3` enabled
at 3.3 V, GPIO46 high and eMMC still present. **The owner then confirmed a
working PocketBoot framebuffer.** This is physical confirmation, not merely
the `POCKETBOOT_DRM_READY` marker.

Touch remains the next checkpoint: the live input list contains only the
power, reset and GPIO-key devices, with no I2C adapters. The current image
does not enable the QUP I2C or declared Goodix touchscreen driver. This does
not yet identify which touchscreen variant is fitted.

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

Reuse the parallel PocketBoot work rather than building another recovery
path: [PR #44][menu-pr] provides volume-down/menu entry and
[PR #42][recovery-pr] provides failed-load recovery and retained pstore
diagnostics. Both were draft at this audit; #42 is stacked on the extlinux
work, not directly on `main`. Neither establishes this tablet's key input,
USB re-enumeration, or ramoops memory layout, and neither guarantees recovery
from a hard hang.

`~/src/lk2nd` can be inspected without attaching it to a Delta thread. Attach
it before making changes there; prefer existing device support over a new
fork. A reusable agent skill can follow once the device-control and recovery
contract has actually been tested.

[unlock-guide]: https://xdaforums.com/t/guide-unlock-bootloader-of-lenovo-tab-4-10-with-oem-unlock-greyed-out-tb-x304f-l-and-other-qcom-tablets.4201857/
[lenovo-spec]: https://psref.lenovo.com/syspool/Sys/PDF/Lenovo_Tablets/TAB4_10/TAB4_10_Spec.PDF
[devinfo-read]: https://github.com/Naveen3Singh/BLUnlocker/blob/45a1e187764e18bd2ce7fadfc57e00bf40f457d3/dump_devinfo.bat
[devinfo-write]: https://github.com/Naveen3Singh/BLUnlocker/blob/45a1e187764e18bd2ce7fadfc57e00bf40f457d3/unlock.bat
[devinfo-illustration]: https://web.archive.org/web/20250206102708id_/https://xdaforums.com/attachments/1607766281870-png.5154947/
[stock-zip]: https://mirrors-obs-2.lolinet.com/firmware/lenowow/2017/Tab_4_10/TB-X304F/TB-X304F_S001017_2211021443_Q21000_ROW_GB.zip
[lk2nd]: https://github.com/msm8916-mainline/lk2nd
[lk2nd-tbx304x]: https://github.com/msm8916-mainline/lk2nd/commit/517bb38a409d4ac982f09e83a046e4dc71be5029
[lk2nd-release]: https://github.com/msm8916-mainline/lk2nd/releases/tag/23.1
[lk2nd-pr]: https://github.com/msm8916-mainline/lk2nd/pull/664
[lk2nd-boot]: https://github.com/msm8916-mainline/lk2nd/blob/main/Documentation/boot.md
[linux-msm8917]: https://github.com/torvalds/linux/blob/master/arch/arm64/boot/dts/qcom/msm8917.dtsi
[downstream-dts]: https://github.com/lenovo-devs/android_kernel_lenovo_msm8953/blob/lineage-16.0-tbx304/arch/arm/boot/dts/qcom/tbx304-msm8917-pmi8937-qrd-sku5.dts
[pem120-dts]: https://github.com/pem120/linux-msm89x7/blob/a51b91b503307d35902447dd1f90db765372cf3b/arch/arm64/boot/dts/qcom/msm8917-lenovo-tbx304x.dts
[lenovo-restart]: https://github.com/lenovo-devs/android_kernel_lenovo_msm8953/blob/lineage-16.0-tbx304/drivers/power/reset/msm-poweroff.c
[lenovo-pon]: https://github.com/lenovo-devs/android_kernel_lenovo_msm8953/blob/lineage-16.0-tbx304/arch/arm/boot/dts/qcom/tbx304/tbx304-msm-pm8937.dtsi
[lab-relay]: https://github.com/samcday/skills/pull/1
[menu-pr]: https://github.com/samcday/pocketboot/pull/44
[recovery-pr]: https://github.com/samcday/pocketboot/pull/42
