# Samsung A5U MSM8916 DRM evidence, 2026-07-22

This checkpoint used identity-gated `fastboot boot` from Pocketboot userspace
fastboot. No partition was flashed or erased. The tested image SHA-256 was
`58f3620c34f91010d7a8fb0d0f7e7dcd4bd3dae5fda9f2d4d7945cb668975fa2`.

Local generated evidence:

- `target/a5u-msm-drm-lab/20260722T073541.811851Z/dmesg.txt`
  (`97fa3f3111e7fd667462445ff00c4585fea05f6658af042b25885d068c65b030`)
- `target/a5u-msm-drm-lab/20260722T073541.811851Z/drm-inventory-v2.tsv`
  (`5548aed2488b1aea371a58932a84bcf7402bb53de36b971514c7d9866d9fe5a1`)

Both the pre-boot and post-boot fastboot identity gates reported
`product=pocketboot`, `serialno=cd0ee037`,
`compatible=samsung,a5u-eur`, and `is-userspace=yes`.

## Native display path

The kernel attached the display controller to apps-IOMMU group 0, bound DSI,
initialized MDP5 v1.6, registered native MSM KMS, and discovered the panel
mode:

```text
platform 1a01000.display-controller: Adding to iommu group 0
msm_mdp 1a01000.display-controller: bound 1a98000.dsi
msm_mdp 1a01000.display-controller: [drm:mdp5_kms_init] MDP5 version v1.6
[drm] Initialized msm-kms 1.13.0 for 1a01000.display-controller on minor 0
[CONNECTOR:36:DSI-1] status updated from unknown to connected
Probed mode: "720x1280": 60 80117 720 800 896 1024 1280 1294 1296 1304
```

The corrected read-only live inventory tied the state to the same card:

```text
dri-name             /sys/kernel/debug/dri/0/name       msm-kms
card-driver          /sys/class/drm/card0               msm_mdp
connector-status     /sys/class/drm/card0-DSI-1         connected
connector-enabled    /sys/class/drm/card0-DSI-1         enabled
connector-mode       /sys/class/drm/card0-DSI-1         720x1280
```

There was no simpledrm card. DSI panel initialization transfers completed,
MDP5 enabled `720x1280` at 60 Hz, and Pocketboot reported:

```text
POCKETBOOT_DRM_READY path=/dev/dri/card0 connector=DSI-1 width=720 height=1280
POCKETBOOT_DRM_PAGE_FLIP sequence=1 remaining=15
...
POCKETBOOT_DRM_PAGE_FLIP sequence=16 remaining=0
POCKETBOOT_DRM_PAGE_FLIP_TEST_RESULT requested=16 completed=16
```

The 16 events were ordered and driven by MDP5 vblank completions across the
three framebuffer objects. The evidence contains no panic, Oops, lockup,
Pocketboot UI-thread exit, continuous DSI status error, or simpledrm probe.

## Known rough edges

The first modeset recorded one `INTF1_UNDER_RUN` (`0x04000000`) immediately
after enabling the interface. It did not repeat during the 16 completed flips.
The platform DT currently has no interconnect description, and MSM warns that
this may cause underflows; eliminating the initial event remains follow-up
work rather than a blocker to this checkpoint.

An earlier diagnostic image used `drm.debug=0x1ff`, `ignore_loglevel`, and the
A5U's synchronous 115200-baud serial console. Continuous DRM logging starved
USB and froze that lab instance. The successful image instead used bounded
page-flip markers and `drm.debug=0x6` without forcing debug messages onto the
console. The checked-in production cmdline contains only
`msm.skip_gpu=1 msm.separate_gpu_kms=1`.

The first generated inventory script also assumed `readlink -f`, which this
BusyBox build does not provide. The corrected live inventory above used plain
`readlink`; the checked-in runner includes that portability fix. The original
run's `summary.json` therefore remains `status=fail` at the collection step;
the saved dmesg and corrected `drm-inventory-v2.tsv` were subsequently
validated together with the current parser. That collector failure was not a
display-stack failure and is not represented as an end-to-end runner pass.
