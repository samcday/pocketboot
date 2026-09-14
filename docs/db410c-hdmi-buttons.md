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
