# Crosshatch MSM DRM lab runner

`tools/crosshatch_drm_lab.py` boots a Pocketboot image ephemerally while
capturing the Pixel 3 XL's SBU debug UART. It is deliberately narrow:
the fastboot surface contains read-only `getvar` checks, `fastboot boot`, and
an optional identity-gated `fastboot reboot bootloader`. There is no flash,
erase, format, slot-selection, or continue operation.

Crosshatch has no EUD. The tested SBU UART setup is a passive USB-C breakout
with an FT232RL adapter rather than Google's USB-Cereal board. Crosshatch uses
3.3 V-class signalling: with the FT232 disconnected, its idle TX measured
2.936 V. An FT232 set to 1.8 V still captured readable target output, but its
TX was not recognized by Crosshatch; selecting 3.3 V restored a bidirectional
Pocketboot shell. Leave the adapter's VCC pin disconnected and never select
5 V. Correct connector orientation is also required.

Fastboot and ADB remain the normal control and recovery paths. Serial
BREAK/Magic SysRq support is available after bidirectional UART has been proven
with the actual adapter, level setting, and connector orientation in use.

## Downstream provenance

The local `aosp-msm` worktrees preserve the relevant Google 4.9 sources:

- `/var/home/sam/tmp/linux-aosp-crosshatch-4.9-android12` at
  `862f51bac900c4b8a6e31792369e588bb395b8a3`;
- `/var/home/sam/tmp/linux-aosp-crosshatch-4.9-pie-qpr3` at
  `695fa5606dabebeb8b10bfc7d145ffca8c65a450`;
- `/var/home/sam/tmp/linux-aosp-bluecross-4.9-pie-dr1` at
  `340a9aaf92bcb84a79e51a07c03e05ef2f9ea812`.

`arch/arm64/boot/dts/google/dsi-panel-s6e3ha8-dsc-wqhd-cmd.dtsi`
identifies the controller as S6E3HA8 and specifies single-DSI command mode,
1440x2960 at 60 Hz, 8 bpc/8 bpp DSC with 720x40 slices, the reset/DCS
sequence, and 5..1023 DCS brightness. The shared display file supplies GPIO6
reset, GPIO12 TE, L14 VDDI, L28 VCI, and SWIRE/AOD-controlled LAB. This
differs materially from mainline's Galaxy S9 video-mode variant, so the
mainline driver uses a separate board-qualified compatible.

Google's C1 include also enables downstream `qcom,null-insertion-enabled`.
The current mainline baseline deliberately omits it and has no timeout or
underflow across first light, bounded page flips, and DCS blanking. That proves
it is not required for those tests, not that every workload can omit it. A
future parity test should A/B it under sustained command-mode updates before
deciding whether to add an explicit mainline property.

The historical Pixel3Dev worktree at
`/var/home/sam/tmp/linux-pixel3dev-crosshatch-display` records an early
video-mode `panel-simple` attempt in `a63b305aeaff2`, immediately reverted by
`5be007227d794`. It is useful negative evidence, not an implementation base.

## Safety contract

The runner requires all three target inputs explicitly:

- `--image` names the boot image.
- `--fastboot-serial` names the Android fastboot device. The runner never
  enumerates or auto-selects a device.
- `--serial-port` names the SBU UART adapter's tty. Prefer its stable
  `/dev/serial/by-id/...` path; no tty path is built in or discovered by a
  shell glob.

Before every boot, `fastboot -s SERIAL getvar product` must equal `crosshatch`.
Changing `--expected-product` is an explicit opt-in override. By default the
runner also requires `getvar unlocked` to report yes; `--allow-locked` is an
explicit escape hatch for bootloaders without that variable.

Before the optional between-attempt reboot, the Pocketboot gadget must report
all three exact values: `product=pocketboot`, `serialno=SERIAL`, and
`compatible=google,crosshatch`. Only then does the runner issue the standard
`fastboot reboot bootloader` command. The next ABL appearance is independently
checked again as `product=crosshatch` and unlocked before another ephemeral
boot.

The tty must resolve to a character device and must not be the runner's own
terminal, `/dev/console`, `/dev/tty`, `/dev/tty0`, or `/dev/ptmx`.

Inspect a plan without touching fastboot, opening the tty, creating a log, or
requiring the paths to exist:

```sh
python3 tools/crosshatch_drm_lab.py \
  --dry-run \
  --image target/crosshatch/boot.img \
  --fastboot-serial PIXEL_FASTBOOT_SERIAL \
  --serial-port /dev/serial/by-id/USB_CEREAL_ADAPTER
```

## One panel attempt

The Crosshatch config adds `earlycon console=ttyMSM0,115200n8` for the
SBU debug UART plus `pocketboot.log=info loglevel=8 ignore_loglevel
drm.debug=0x6`. This records early boot, DRM driver, and KMS setup without
enabling the atomic/vblank debug classes, which are too noisy for repeated
flips.
Pocketboot emits these deterministic UART markers:

- `S6E3HA8_CROSSHATCH_PREPARED` after the Crosshatch panel's regulator,
  reset, DCS initialization, and DSC PPS sequence succeeds;
- `S6E3HA8_CROSSHATCH_ENABLED` after its display-on command succeeds;
- `POCKETBOOT_DRM_READY` only after the first legacy CRTC modeset succeeds;
- `POCKETBOOT_DRM_PAGE_FLIP sequence=N` for each completed flip in the bounded
  startup lab run. The runner's default expression includes `sequence=`, so
  the surrounding test-start and test-result records cannot inflate the
  completion count. Later ordinary UI flips are deliberately silent at
  115200 baud.

The runner requires the two panel milestones and `POCKETBOOT_DRM_READY` on
every attempt. When `--pageflip-count N` is set, it also requires both `N`
individual sequence records and the exact final `requested=N completed=N`
result. They appear as separate requirements in dry-run output and in each
attempt's JSON marker counts. This prevents a modeset from being reported as a
pass when the DRM core has continued after a swallowed panel callback error,
or a partial bounded flip run from passing on incidental later flips.
Additional `--expect` expressions remain optional and cumulative.

The Crosshatch bring-up cmdline requests 16 bounded startup redraws with
`pocketboot.drm_page_flips=16`. Other devices default to zero, malformed values
are disabled, and the runtime hard cap is 64. The UI emits
`POCKETBOOT_DRM_PAGE_FLIP_TEST_START` and
`POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT` with requested/completed fields around
that run, so page flips do not depend on incidental battery or menu updates.

The first-light config deliberately leaves `REGULATOR_QCOM_LABIBB` disabled.
Downstream treats the panel's LAB rail as SWIRE/AOD-owned, while the mainline
LABIBB driver does not model that ownership. Binding it now would let late
regulator cleanup disable an otherwise unclaimed bootloader rail. Keep the LAB
rail at its bootloader state until SWIRE ownership and safe teardown are
represented explicitly.

Consequently, repeated ABL boots exercise DPU/DSI/DSC setup, panel reset, DCS
initialization, and command traffic, but they are not a proof of complete panel
rail power-off/power-on independence. L14 and L28 remain always-on and LAB
remains SWIRE/AOD-owned until that ownership has a safe mainline model.

For example, require the three built-in panel/modeset milestones and all 16
requested startup flips:

```sh
python3 tools/crosshatch_drm_lab.py \
  --image target/crosshatch/boot.img \
  --fastboot-serial PIXEL_FASTBOOT_SERIAL \
  --serial-port /dev/serial/by-id/USB_CEREAL_ADAPTER \
  --pageflip-count 16 \
  --timeout 180
```

`--expect` and `--failure-marker` accept Python regular expressions and may be
repeated. The built-in failure set catches panics, oopses, lockups, DPU/MDP
underflow, clocks stuck on during unused-clock cleanup, and the Pocketboot UI
exiting because `/dev/dri` never appeared. Use
`--no-default-failure-markers` only when collecting evidence past one of those
conditions. Each UART line and host action is written with a UTC timestamp
under `target/crosshatch-drm-lab/`; the adjacent JSON summary records every
expected/observed marker count.

Blank/unblank tests are opt-in and require an exact DCS-backlight sysfs path and
positive restore level:

```sh
  --blank-cycles 3 \
  --backlight-sysfs /sys/class/backlight/PANEL_BACKLIGHT \
  --unblank-brightness 512
```

After `POCKETBOOT_DRM_READY`, the runner first sends a short, echo-safe shell
probe and waits for its kmsg ACK. It then sends one independently parseable
command at a time: a backlight preflight followed by one blank/restore stage
per requested cycle. Every stage has a separate status barrier, and the next
stage is withheld until the previous ACK reports success. All physical shell
lines are limited to 512 UTF-8 bytes including the carriage return, well below
Pocketboot's 4096-byte BusyBox editing buffer.

The preflight verifies that the exact directory, `brightness`,
`max_brightness`, and `/dev/kmsg` are usable and checks the restore level
against `max_brightness`. Each cycle attempts the restore even when its blank
write or marker fails. Only successful writes emit `POCKETBOOT_DRM_BLANK` and
`POCKETBOOT_DRM_UNBLANK` to kmsg. A missing path, invalid limit, or failed write
emits `POCKETBOOT_DRM_LAB_ERROR`, which is a default failure marker. Hold times
default to one second and are configurable.

This validates the panel driver's backlight class and DCS brightness path. It
does **not** disable the CRTC or prove a full DRM connector DPMS cycle. The
runner deliberately does not guess a backlight name or use a wildcard. Generic
`--serial-command` arguments remain available for additional target-specific
experiments and are sent before the blanking stages. They use the same line
budget and ordered status barriers; each gets a carriage return for the
Pocketboot getty.

For null-insertion fact-finding, retain the full UART log and add failure
markers for the exact DSI/DPU messages seen in the current kernel. A successful
DCS init followed by repeated DPU underflow/DSI timeout or corrupt scanout is
useful evidence; the runner itself does not infer that null insertion is the
cause.

Use the first attempt to classify the failure before changing the host:

| Last successful milestone | First place to investigate |
| --- | --- |
| no `S6E3HA8_CROSSHATCH_PREPARED` | VDDI/VCI, reset, DCS init, or DSC PPS |
| prepared, not enabled | display-on DSI transfer |
| enabled, not `POCKETBOOT_DRM_READY` | DPU topology, connector state, or atomic modeset |
| ready, but flips fail with DPU/DSI timeout or underflow | command transfer, TE, then null insertion |
| prepared + enabled + ready + 16 flips, but dark | SWIRE/LAB state, physical reset/init, or panel hardware |

Do not enable null insertion for the baseline. If the UART/scanout evidence
puts it in scope, implement it as an explicit optional DT property and test the
mainline-normalized DSI register offset `0x2b0` with `BIT(0)` for virtual
channel 0. Downstream calls the physical register `0x2b4`; copying that offset
unchanged would miss mainline's 6G register-base normalization.

## Repeated attempts and Magic SysRq

Repeated runs require an explicit way back to fastboot:

```sh
python3 tools/crosshatch_drm_lab.py \
  --image target/crosshatch/boot.img \
  --fastboot-serial PIXEL_FASTBOOT_SERIAL \
  --serial-port /dev/serial/by-id/USB_CEREAL_ADAPTER \
  --attempts 3 \
  --pageflip-count 16 \
  --pocketboot-reboot-bootloader-between-attempts
```

This preferred path does not require UART input. Pocketboot implements exact
`reboot-bootloader` only when the kernel reboot-mode class advertises the
`bootloader` target; it fails before sending fastboot `OKAY` otherwise.

If serial input has been proven independently, use
`--sysrq-reboot-between-attempts`; alternatively, pass
`--between-attempt-command` for a proven target-side command that returns this
device to ABL fastboot. These three mechanisms are mutually exclusive. The
next attempt always waits for and re-verifies product and unlocked state.

The host configures 115200 8-N-1 with no flow control by default. A SysRq
request asserts BREAK with `TIOCSBRK` for 250 ms (falling back to
`tcsendbreak`), waits 100 ms, then writes one key. These timings are adjustable.
This assumes the SBU UART adapter and its host driver propagate BREAK and the
kernel UART is the active serial console. The original validated five-boot run
used the FT232 at 1.8 V, so it captured output but could not deliver input and
SysRq was not part of that run's control or recovery contract. Follow-up testing
at the measured-correct 3.3 V setting proved interactive UART input.

The Crosshatch lab config sets `MAGIC_SYSRQ_DEFAULT_ENABLE=0x88`: `0x08`
permits debug dumps and `0x80` permits emergency reboot/poweroff. Other SysRq
action classes remain disabled. `--sysrq-dump` accepts only `l`, `m`, `p`, `t`,
or `w`; recovery always ends with BREAK + `b`. SysRq-b is immediate and does
not sync filesystems, so recovery is opt-in and intended for this read-mostly
initramfs bring-up environment.

An emergency reboot may return to normal Android rather than ABL fastboot;
that is bootloader/reboot-reason behavior, not something the runner changes.
If fastboot-mode persistence is not proven, use a known
`--between-attempt-command` or arrange an external return to fastboot. The
runner times out instead of selecting another connected device.

## Build against the WIP kernel tree

The Pocketboot build command accepts the local kernel worktree explicitly:

```sh
cargo xtask build \
  qcom/sdm845-google-crosshatch \
  "$HOME/tmp/linux-pocketboot-sdm845" \
  --output target/crosshatch/boot.img
```

Continue to use the local-tree argument until the Crosshatch panel commits are
published and the pinned `[kernel-source]` SHA is advanced.

## Validated baseline and remaining clock warning

The image with SHA-256
`d1b9b88d27338ecc88fab5432e4874ccd08fd6f99db0ad4a63a06c414e51e4dc`
completed five consecutive identity-gated ephemeral boots. Every attempt
reached panel prepare/enable, a 1440x2960 command-mode DSC modeset, and exactly
16/16 bounded page flips. That five-boot UART run showed UFS `sda` through
`sdf`, gadget binding, and no provider-probe failure, FunctionFS workqueue
warning, DPU/MDP underflow, DSI timeout, or transfer failure.

A separate identity-checked ADB check against the fifth still-running image
completed three one-second DCS backlight blank/restore cycles and restored
brightness to 512. Its captured dmesg shows a successful DSI command transfer
for each transition and no runtime underflow or timeout. Taken together, this
is positive evidence that null insertion is not needed for the baseline.

The concise run and live-check record is in
`docs/crosshatch-msm-drm-evidence-2026-07-20.md`.

One warning class remains intentionally outside this pass: each successful
boot records two early DSI PLL lock attempts and four CCF
disabled/unprepared-balance warnings while DSI pixel-clock orphans are
reparented. They do not correlate with modeset failure, but the kernel is not
warning-clean. The next clock-focused pass should instrument the orphan clock
counts and fix that handoff without changing the proven panel command stream.
