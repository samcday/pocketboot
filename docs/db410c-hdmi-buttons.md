# DragonBoard 410c HDMI and volume-button bring-up

The DB410c Pocketboot UI previously exited because the tiny lab kernel
registered no KMS card (`/dev/dri` absent) and exposed only one board button.
This is a kernel configuration gap, not a UI bug.

## Observed contract

- Lab kernel `target/kernel/qcom/apq8016-sbc/.config`: `CONFIG_DRM=y` with only
  the simple-framebuffer driver, **no `CONFIG_DRM_MSM`**, and
  `# CONFIG_DRM_I2C_ADV7511 is not set`. The DT display pipeline
  (`qcom,mdss`, `qcom,msm8916-mdp5`, `qcom,msm8916-dsi-ctrl`, `adi,adv7533`,
  `hdmi-connector`) is `okay`, but nothing binds it.
- `# CONFIG_INPUT_PM8941_PWRKEY is not set`: the board's Volume Down is the
  PM8916 PON RESIN key (`pm8916_resin` with `linux,code = <KEY_VOLUMEDOWN>`),
  so only the TLMM `gpio-keys` Volume Up appeared as an input device.
- The Pocketboot initramfs ships no loadable modules, so every display and
  input driver needed before userspace must be built in.

## Configuration change

`configs/device/qcom/apq8016-sbc.toml` adds, built in:

| Symbol | Purpose |
| --- | --- |
| `IOMMU_SUPPORT`, `QCOM_IOMMU` | IOMMU required by `DRM_MSM` and the display/GEM path |
| `DRM_MSM`, `DRM_MSM_MDP5`, `DRM_MSM_DSI`, `DRM_MSM_DSI_28NM_PHY` | MDP5 + DSI + 28 nm LP PHY for MSM8916 |
| `DRM_I2C_ADV7511` | On-board ADV7533 DSI-to-HDMI bridge (`adi,adv7533`) |
| `DRM_DISPLAY_CONNECTOR` | `/hdmi-out` (`hdmi-connector`) next bridge downstream of the ADV7533; without it `adv7511` probe defers forever and no DRM card appears |
| `DRM_SIMPLEDRM = false` | Let MSM own `card0` instead of a bootloader framebuffer |
| `INPUT_PM8941_PWRKEY` | PM8916 power and RESIN (Volume Down) keys |

The UI already maps `KEY_VOLUMEUP`/`KEY_VOLUMEDOWN` to menu navigation and
probes both bits from the input device, so no UI source change is needed; the
fix is only to make the kernel expose both keys.

## Build

```sh
cargo xtask kernel qcom/apq8016-sbc \
  --initrd target/cpio/qcom/apq8016-sbc/pocketboot-initrd.cpio
```

Reusing the retained initramfs keeps the userspace identity fixed while the
kernel configuration changes. The build writes the bare `Image`, the DTB and
the merged `.config` under `target/kernel/qcom/apq8016-sbc/`.

## Later hardware trial

The DB410c runs Pocketboot userspace fastboot, so the UI kernel can be tried as
a kexec destination, without a cold boot:

```sh
python3 tools/db410c_kexec.py \
  --kernel target/kernel/qcom/apq8016-sbc/arch/arm64/boot/Image \
  --dtb <retained external-psci.dtb> \
  --cmdline-file <cmdline> \
  --output <new directory>
fastboot -s <serial> boot <new directory>/generation-1.img
```

Expected on boot: `POCKETBOOT_DRM_READY` for an `msm`/`msm-kms` card with an
HDMI connector, both Volume Up and Volume Down input devices, and the UI
accepting physical button navigation. The existing spin-table handoff contract
is unchanged by this configuration, so the handoff acceptance still applies.
A cold FIT repackage is only needed for a cold boot; do not overwrite the
occupied parking page of a running instance.

## Hardware results (2026-09-16)

Cold-booting this configuration on a DragonBoard 410c through the lab FIT path
brought up the MSM DRM card and the `HDMI-A-1` connector, but the ADV7533
reported hot-plug detect low with an empty EDID from the attached USB HDMI
capture dongle, so the kernel supplied no modes and the UI exited with
`no DRM connector with modes found`. Forcing the connector on the kernel
command line fixed that without any code change:

```
video=HDMI-A-1:1280x720@60e
```

The trailing `e` sets `DRM_FORCE_ON`; a bare `video=HDMI-A-1:1280x720@60`
without it does not help because the disconnected early-exit runs before the
command-line mode is considered. With the forced mode the Pocketboot boot menu
rendered on the capture, the ADV7533 reported its TMDS PLL locked, and the
picture appeared whenever the sink's TMDS termination was sensed (ADV7511
status register `0x42` bit 5). Sinks that assert HPD and serve an EDID should
not need the override; sinks that do not, such as some capture dongles, do.
A follow-up could let the UI tolerate a mode-less connector by forcing it via
DRM debugfs or by programming a default mode itself.

Other findings from the same trials:

- Both volume-key input devices enumerate (`pm8941_resin` as Volume Down,
  `gpio-keys` as Volume Up). Physical navigation was not proven because the lab
  board's Volume Up switch (S3, TLMM gpio107) reads pressed permanently and the
  boot menu was empty; hardware navigation is a no-op with no boot entries and
  only the power menu reacts to volume keys.
- The Adreno `a300_pm4.fw` load errors are harmless here: `CONFIG_FW_LOADER` is
  off in this kernel, modeset is MDP5/DSI and independent of the GPU, and the UI
  renders on the CPU.
- Kexecing a kernel from a running instance of this UI image is unreliable
  (the destination died early where a cold boot of the same image was clean);
  that is tracked separately from the display configuration.
