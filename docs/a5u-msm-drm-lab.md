# Samsung A5U MSM8916 DRM proof runner

`tools/a5u_msm_drm_lab.py` transiently boots one Pocketboot image from an
already-running Pocketboot userspace-fastboot instance, then retrieves dmesg
and a read-only DRM inventory through that same interface. It does not expose
flash, erase, reboot, continue, slot-selection, or partition operations.

This runner is separate from `crosshatch_drm_lab.py` because the Crosshatch
contract depends on SBU UART, S6E3HA8 command-mode milestones, DPU debugfs CRC,
and repeated ABL boots. None of those are appropriate proof requirements for
the A5U's MDP5, EA8061V video-mode path.

## Safety and starting state

The A5U must already be running Pocketboot and offering userspace fastboot.
Supply its serial explicitly; the runner never enumerates or auto-selects a
device. Before downloading the image it requires these exact getvars from the
explicitly addressed target:

- `product=pocketboot`
- `serialno=FASTBOOT_SERIAL`
- `compatible=samsung,a5u-eur`
- `is-userspace=yes`

It repeats all four checks after the new image boots. A mismatch fails closed.
The only state transition is standard `fastboot boot IMAGE`: Pocketboot stages
the image in memory and kexecs it. The runner never writes a phone partition.

Inspect the exact plan without requiring either path to exist and without
touching USB:

```sh
python3 tools/a5u_msm_drm_lab.py \
  --dry-run \
  --image target/a5u-msm-drm/probe-quiet-boot.img \
  --fastboot-serial A5_FASTBOOT_SERIAL
```

## One proof boot

The image must include `pocketboot.drm_page_flips=16` on its kernel command
line. Run:

```sh
python3 tools/a5u_msm_drm_lab.py \
  --image target/a5u-msm-drm/probe-quiet-boot.img \
  --fastboot-serial A5_FASTBOOT_SERIAL
```

After the booted Pocketboot fastboot gadget reappears, the runner uses `oem
dmesg` and `get_staged` to retrieve the kernel log. It waits for, and then
requires, all of the following:

- `POCKETBOOT_DRM_READY` for `/dev/dri/cardN`, `DSI-N`, and 720x1280;
- ordered `POCKETBOOT_DRM_PAGE_FLIP sequence=1` through `sequence=16` records;
- the exact `requested=16 completed=16` test result;
- no panic, Oops, lockup, or Pocketboot UI-thread exit.

It then sends a fixed read-only script through `fastboot stage`, which is an
in-memory download rather than a partition write, and invokes the short
`oem shell-staged` command. Staging is necessary because Pocketboot fastboot
command packets are limited to 64 bytes. The script only reads `/proc/cmdline`,
`/sys/kernel/debug/dri/*/name`, and the DRM card/DSI connector attributes in
sysfs. The host requires an enabled, connected DSI connector advertising
720x1280 to belong to the same card whose DRM driver name is exactly `msm` or
`msm-kms`; the ready marker must identify that same card and connector. Any
non-MSM DRM card, including `simpledrm`, fails the proof.

Each run creates a new timestamped directory under
`target/a5u-msm-drm-lab/` containing:

- `host.log`: timestamped commands, identity checks, and fastboot output;
- `dmesg.txt`: the complete staged kernel log used for marker assertions;
- `drm-inventory.tsv`: the staged read-only inventory;
- `drm-inventory.sh`: the exact read-only script staged in target RAM;
- `summary.json`: image SHA-256, both identities, DRM topology, and proof
  result.

This proves native MSM MDP5 KMS, the DSI/panel scanout path at 720x1280, and
completed event-driven page flips. It does not prove Adreno/GPU acceleration,
full panel-rail power cycling, or hardware CRC. MDP5 does not provide the DPU
CRC interface used by the Crosshatch lab.
