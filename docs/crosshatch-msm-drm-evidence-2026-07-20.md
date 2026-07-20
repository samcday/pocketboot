# Crosshatch MSM DRM evidence, 2026-07-20

This checkpoint used only identity-gated `fastboot boot` and ADB against
Crosshatch serial `8CCY15V1T`. No partition was flashed or erased. USB-Cereal
was receive-only.

## Five-boot run

Image SHA-256:
`d1b9b88d27338ecc88fab5432e4874ccd08fd6f99db0ad4a63a06c414e51e4dc`

Local generated artifacts:

- `target/crosshatch-drm-lab/20260720T080612.029781Z.uart.log`
- `target/crosshatch-drm-lab/20260720T080612.029781Z.summary.json`

All five attempts independently recorded exactly one panel-prepare marker,
one panel-enable marker, one successful DRM modeset marker, the exact
`requested=16 completed=16` result, and 16 completed page-flip markers. All
five also showed UFS `sda` through `sdf` and the Pocketboot USB gadget binding.

Aggregate scan of the UART log:

```text
panel prepared              5
exact 16/16 result          5
FunctionFS/workqueue WARN   0
missing-provider failure    0
DPU/MDP underflow           0
DSI timeout/transfer error  0
UFS sdf attached            5
USB gadget bound            5
```

The four between-attempt transitions each unbound the gadget and shut down DPU
cleanly before returning through ABL fastboot. The upstream FunctionFS fix
therefore removed the two teardown workqueue warnings seen before it.

## Build-#5 live blanking check

ADB still identified only the intended target as
`8CCY15V1T recovery usb:1-1.3.4.3.1 product:pocketboot`. Its kernel was:

```text
Linux (none) 7.2.0-rc1+ #5 SMP PREEMPT Mon Jul 20 18:05:20 AEST 2026 aarch64
```

Three one-second writes of brightness 0 followed by 512 produced six
successful DSI command transfers. The final sysfs brightness read back as 512:

```text
[  706.336034] POCKETBOOT_DRM_AUDIT_BEGIN build=5 image=d1b9b88d27338ecc
[  706.732011] [drm:dsi_cmds2buf_tx] ret=50
[  707.061700] POCKETBOOT_DRM_AUDIT_BLANK cycle=1
[  708.788287] [drm:dsi_cmds2buf_tx] ret=49
[  709.120924] POCKETBOOT_DRM_AUDIT_UNBLANK cycle=1
[  710.839950] [drm:dsi_cmds2buf_tx] ret=49
[  711.176790] POCKETBOOT_DRM_AUDIT_BLANK cycle=2
[  712.896017] [drm:dsi_cmds2buf_tx] ret=49
[  713.228969] POCKETBOOT_DRM_AUDIT_UNBLANK cycle=2
[  714.948035] [drm:dsi_cmds2buf_tx] ret=49
[  715.285044] POCKETBOOT_DRM_AUDIT_BLANK cycle=3
[  717.007973] [drm:dsi_cmds2buf_tx] ret=49
[  717.336741] POCKETBOOT_DRM_AUDIT_UNBLANK cycle=3
[  717.672763] POCKETBOOT_DRM_AUDIT_END brightness=512
```

The post-marker dmesg scan contained no DSI timeout or transfer failure and no
DPU/MDP underflow.

## Known warning

This is a WIP checkpoint, not a warning-clean kernel. Every successful boot
records two early DSI PLL lock failures followed by four CCF
disabled/unprepared imbalance warnings while orphaned DSI pixel clocks are
reparented. The five-boot log therefore contains 20 CCF WARN records. These
warnings did not correlate with panel, USB, or storage failure, but they remain
the next bounded clock-correctness task.
