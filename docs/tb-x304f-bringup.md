# Lenovo Tab 4 10 TB-X304F bring-up

The first goal is a repeatable PocketBoot boot with a working USB diagnostic
path, before installing PocketFed or replacing Android. This is a bring-up
record, not a claim that PocketBoot supports this tablet yet.

## Current status

- Stock fastboot confirms unlocked; secure boot remains enabled and critical
  partitions remain locked.
- A 301072-byte lk2nd image has booted transiently from stock fastboot, with
  USB commands, NT35521S display handoff, log retrieval, and screenshot
  capture working. Nothing was installed into `boot` or `aboot`.
- Stock Android now asks for its startup/decryption password. No factory
  reset has been issued; Android is not needed for the transient boot path.
- The original PON-mode lk2nd returned to Android instead of stock fastboot.
  The device-scoped IMEM build now returns to stock fastboot in 2.08 seconds.
  This is a working soft-reboot path from lk2nd, not recovery from a hung
  Linux kernel; button recovery is still needed for the latter.
- Both the initial PocketBoot image and its framebuffer-console follow-up
  were accepted by lk2nd, but neither produced tablet USB within 45 seconds.
  Both screens were black when checked later. Linux entry and userspace are
  unconfirmed.
- The shared-ramoops profile has also been attempted without USB appearing.
  Its bounded raw exporter and pre-trial baseline capture work; post-reset
  retrieval and actual RAM retention are still pending.

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
does not establish Linux entry or userspace startup; neither is confirmed.
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
retention through the actual button-reset sequence is not yet verified.

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
it could remain enabled from the bootloader. A post-reset capture is pending.

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
