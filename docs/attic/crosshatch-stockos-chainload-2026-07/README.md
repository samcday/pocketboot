# Crosshatch stockOS chainload — attic handover

Status: archived WIP, 2026-07-14. This is not a supported Pocketboot feature.

This experiment asked whether an ephemerally booted Pocketboot could sit
between Google ABL and either removable-media mainline Linux or the installed
stock Android slot. It successfully reconstructed and verified the stock boot
payload through `kexec_load` and reached ARM64 `machine_kexec`, but the target
stock kernel never executed visibly. The SoC reset into UEFI with ABL reset
reason 8, then stock Android cold-booted normally with boot reason `watchdog`.

The historical implementation remains intact at:

```text
Base/main:        84be0aa061681df6721d8483206d73d88bbab6e3
Branch:           wip/crosshatch-stockos-chainload
Experimental tip: 75c59278e35294655ec6e32837f753e032e188c9
Attic tag:        attic/crosshatch-stockos-chainload-2026-07-14
Branch diff:      88 files, 23,177 insertions, 782 deletions
```

The branch and annotated tag are local unless deliberately published. For
off-machine durability, push a private ref or create an encrypted/private Git
bundle. Do not publish captured fixtures without reviewing them for device
serials and other identifiers.

## Original goal

Make Pocketboot a no-flash Crosshatch intermediary: safely sink ABL's selected
DTBO, retain its `dtbo_idx` receipt, keep removable-media mainline boot working,
and AVB-aware chainload the active stock Android slot with its exact base-DTB,
DTBO, ramdisk, and command-line state. Fail closed, preserve UART diagnostics,
and never write flash.

Conceptually:

```text
XBL/ABL -> ephemeral Pocketboot
              |-- removable-media/mainline boot
              `-- active stock slot
                    |-- snapshot partitions read-only
                    |-- verify the requested AVB graph
                    |-- rebuild boot-v2 kernel/ramdisk/DTB
                    |-- replay ABL DTBO and command-line receipts
                    `-- legacy ARM64 kexec
```

This was a deliberately tiny, ABL-shaped second-stage loader. It was never a
replacement for Qualcomm platform initialization or secure boot from reset.

## Experimental history

The nine commits are intentionally loud WIP checkpoints. Intermediate commits
are not guaranteed to build independently; notably, the diagnostics checkpoint
uses staged-payload cleanup introduced by a later checkpoint.

| Commit | Purpose |
| --- | --- |
| `42b460ab201b8ec930334c2ef8956ab9cb4e5942` | Avoid recursive Fedora ccache wrappers. |
| `db6228f5db4b460a76500f7109bebd3c26e571c5` | Vendor a read-only libavb verification scaffold. |
| `87a5945260a01d4acdd6ec0ec52e710e86184768` | Reconstruct boot-v2, DTBO, and stock artifacts from ABL receipts. |
| `4077354aa7df94994223d3c4befbef295b2361cd` | Add the custom ARM64 legacy-kexec loader and Android DT handoff. |
| `0330c99f781ec28c92e045a90df944fe3b97b6dc` | Prepare the active stock slot from read-only snapshots. |
| `eb3513f6cee9fda3c2de858b01a10cf1e5e95d77` | Build and validate Crosshatch's 121-symbol DTBO shield. |
| `908ecbb42a62ed774f59b202717bc7aaf9edcd4f` | Bound fastboot dmesg diagnostics and fail closed. |
| `bdea9e6af139c2d9ce8a641f7401f7a5bf1ebe28` | Integrate the no-flash stock boot coordinator. |
| `75c59278e35294655ec6e32837f753e032e188c9` | Strictly decode modern Android LZ4 frames. |

Vendored libavb came from AOSP `external/avb` commit:

```text
4e48849c766dcbe8ff8623509f7f8d0f5f8f04dc
```

## Live test baseline

The test unit was already unlocked; the experiment did not alter that state.

```text
Product:       crosshatch
Fingerprint:   google/crosshatch/crosshatch:12/SP1A.210812.015/7679548:user/release-keys
Bootloader:    b1c1-0.4-7617406
Slot:          _a, successful
Boot state:    unlocked / orange
Verity:        enforcing
dtb_idx:       0
dtbo_idx:      13
VBMeta digest: 68cd5529bb07077fee41e72d9e4bf35e89cb61be1488e08f37681a94e87f2c26
```

ABL selected this live hardware identity:

```text
msm-id:   0x141 / 0x20001
board-id: 0x11505 / 0
```

## What was learned

- ABL may supply an external stock ramdisk when Pocketboot's boot header says
  `ramdisk_size=0`. The experimental packager therefore put Pocketboot's cpio
  explicitly in the boot-image ramdisk field.
- ABL's `androidboot.dtb_idx` indexes the image ABL actually loaded. Its numeric
  position is not portable from Pocketboot's DTB table to stock `boot_a`. The
  branch selects the unique stock-compatible base DTB and retains the original
  index only as diagnostics.
- `androidboot.dtbo_idx=13` was the ordered Crosshatch overlay receipt.
- ABL's observed overlay semantics differ slightly from ordinary libfdt: it
  copies selected root identity properties and does not retain overlay-added
  symbols.
- Crosshatch's shield redirected 121 external fixup symbols into
  `/masked-devices`; all 14 entries in Google's pinned DTBO table were
  host-validated, and the installed test image booted with entry 13 applied.
- The manifest's source hash is provenance, not a live comparison against the
  phone. A future OTA could add a fixup or a `target-path` fragment and must be
  re-audited.
- Stock's reconstructed command line needed the stock-DT-only prefix
  `rcupdate.rcu_expedited=1`, followed by ABL's receipt.
- Google's kernel section starts with modern LZ4 frame magic `04 22 4d 18`.
  Pocketboot initially recognized only Linux's legacy LZ4 magic `02 21 4c 18`.
- The final LZ4 decoder requires a complete EndMark and every advertised
  checksum, rejects trailing or concatenated frames, caps decompressed output
  at 512 MiB, and bounds allocation growth.

## Live attempts

### First serious attempt: format rejection

```text
Boot image SHA-256: 0e1c0bd666ccd974e2bb2cf4253064f6fc3df17fd12a299c021c3bb1b9f5c637
Boot image size:    7,118,848 bytes
```

ABL accepted the image through ephemeral `fastboot boot`. Pocketboot matched
the requested stock AVB signatures and payload hashes to ABL's receipt, but
`fastboot continue` failed before loading segments:

```text
load stock Android slot a: kernel is not a raw arm64 Image
```

Pocketboot restored its fastboot recovery surface. No flash, erase, format, or
slot mutation occurred.

### Modern-LZ4 oracle

Official source:

```text
https://android.googlesource.com/device/google/crosshatch-kernel/+/refs/tags/android-12.0.0_r1/Image.lz4
```

```text
Compressed SHA-256: e0591a31e27d5f2ddec4aa6844ea1850db35894761f48615f727a39a5bfb9dac
Compressed size:    19,835,242 bytes
Magic:              04 22 4d 18

Decoded SHA-256:    331001c3367dfe918e3d91814f7a549386a7140606bcfb4a65b3c65070635163
Decoded size:       49,014,808 bytes
text_offset:        0x80000
image_size:         0x37f5000
flags:              0x0a
Kernel:             4.9.270-g862f51bac900-ab7613625
```

### Final attempt: warm-handoff boundary

```text
Boot image SHA-256: ba31e468ef8d5485330bf4279b70be1f23ad3623ee8543b0a6bee5f795a90667
Boot image size:    7,139,328 bytes
```

Host verification at the experimental tip:

```text
Default runtime:       232 passed, 2 ignored
Receipt-mode runtime:  197 passed, 2 ignored
AVB scaffold:          11 passed in each configuration
xtask:                 29 passed
DTBO mask:             121 targets
Independent review:    no blocker
```

The decisive sequence was:

```text
stock AVB signatures and payload hashes match ABL's receipt
...
arm-smmu 15000000.iommu: disabling translation
kexec_core: Starting new kernel
psci: CPU1 killed
...
psci: CPU7 killed
Bye!

UEFI Start
...
ABL version: b1c1-0.4-7617406
Reset REASON 8
Booting from slot (a)
```

The target stock kernel emitted no first-instruction or early-console output
from the direct handoff. The subsequent cold boot reported
`androidboot.bootreason=watchdog`, and there was no pstore evidence of a target
kernel panic.

Do not turn that into “the watchdog caused the failure.” What is proven is that
firmware classified the resulting reset as reason 8 and Android called the next
boot a watchdog boot. The initiating fault remains unknown.

A FunctionFS teardown warning appeared shortly before both handoffs. It may be
incidental, but it remains in the curated evidence because it is one of the few
repeatable events immediately before `machine_kexec`.

## Remaining technical boundary

The unresolved work is the mainline ARM64/Qualcomm warm-handoff boundary:

- generic legacy `kexec_load` correctness on this SDM845 kernel;
- target entry/trampoline, cache, MMU, and exception-level state;
- Qualcomm watchdog or secure-firmware reset behavior;
- device, clock, SMMU, and USB quiescing;
- stock-kernel expectations not represented by the generic ARM64 boot contract.

The highest-value next experiment is mainline-to-mainline kexec on the same
Crosshatch. If that also resets after `Bye!`, the defect is generic to the
Pocketboot/SDM845 kexec path. If it succeeds, the remaining delta is specific
to the stock kernel handoff.

Before another stock attempt, expose segment physical addresses, ARM64 header
fields, and final DTB/initrd placement through a read-only diagnostic. Those
were logged at `info` and hidden by the live image's `warn` filter.

## Threat-model limits

- The tested Crosshatch was unlocked and orange before and after the run.
- `stock-abl-receipt` accepts the top-level vbmeta image's own non-empty embedded
  key. It verifies signatures, requested boot/DTBO hashes, and child-chain keys,
  but it does not independently pin Google's OEM root.
- All configured rollback floors are zero. Hardware anti-rollback is not
  reproduced, and no AFTL guarantee is provided.
- Matching ABL's vbmeta digest and command-line receipt is a same-boot
  continuity check, not an independent OEM trust decision.
- This does not recreate green locked boot, hardware-rooted attestation, or
  ABL's secure-boot measurements. Play Integrity and KeyMint were never reached
  or tested.
- Verification covers the signed graph needed for the requested `boot` and
  `dtbo` payloads; it is not verification of every stock-OS partition.
- The libavb FFI has no write callback and verifies immutable snapshots, which
  avoids reopening mutable storage after verification.
- The no-flash profile removes flash, erase, `set_active`, unrestricted shell,
  mass storage, and BusyBox paths. A privileged Linux kernel is still not a
  formal security boundary.

## What crossed back to main

Only generic or mainline-Crosshatch lessons were reimplemented on `main`:

| Main commit | Salvaged result |
| --- | --- |
| `71dcf5f` | Avoid recursive Fedora ccache wrappers. |
| `660331b` | Mount ext2/ext4 discovery filesystems with `noload`. |
| `86b2e70` | Explicit, fail-closed Crosshatch DTBO shield and packaged mainline identity. |
| `78e5fc8` | Bounded dmesg capture and stale fastboot-payload hygiene. |
| `eb3ebdd` | Preserve acknowledged fastboot actions across USB cleanup failure. |

The libavb scaffold, stock partition coordinator, boot-v2 reconstruction,
Android-specific kexec path, and modern-LZ4 support remain in the attic. The
mainline Crosshatch path builds `Image.gz`; those components add substantial
surface without advancing its immediate boot goals.

Potentially reusable but deliberately deferred pieces are menu generation IDs
for UI TOCTOU protection and target-DTB reserved-memory exclusion. Both deserve
fresh, device-independent designs rather than another partial cherry-pick.

## Restart recipe

Use a detached checkout so the attic history remains unchanged:

```sh
git switch --detach 75c59278e35294655ec6e32837f753e032e188c9

cargo fmt --all -- --check
cargo test --offline
cargo test --offline --features stock-abl-receipt
cargo test --offline --manifest-path xtask/Cargo.toml

env -u CROSS_COMPILE -u LLVM \
  CCACHE_DIR=/tmp/pocketboot-ccache \
  cargo run --offline --manifest-path xtask/Cargo.toml -- \
  build qcom/sdm845-google-crosshatch
```

For another hardware experiment:

1. Start from fully booted stock Android with ADB authorized and TTL at
   115200 8-N-1.
2. Record the active slot, bootloader version, and verified-boot state.
3. Start a file-backed UART capture before rebooting to ABL.
4. Positively verify product `crosshatch`, active slot, unlocked state, and ABL
   version.
5. Recheck the exact boot-image hash.
6. Use only ephemeral `fastboot boot`; never use `flash`, `erase`, `format`, or
   `set_active` in this experiment.
7. Capture Pocketboot's command line and dmesg before handoff.
8. Issue `fastboot continue` only after receipt verification succeeds and no
   preparation error is present.

## Evidence and session archaeology

See [evidence/README.md](evidence/README.md) for raw-capture hashes, redaction
policy, and the two checked-in excerpts.

User-supplied toasted Codex session ID:

```text
019f5a75-f5b8-7cd1-8bb0-e1a563572839
```

The handover is intended to stand alone; this ID is only for deeper local
archaeology if the transcript remains available.
