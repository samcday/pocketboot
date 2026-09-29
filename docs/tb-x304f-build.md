# TB-X304F experimental PocketBoot build

This is an **LK2nd-first diagnostic target**, not direct-stock-boot support or
a hardware-validated Linux port. The initial goal is PocketBoot `/init`,
USB gadget diagnostics and a firmware framebuffer. No flashing is required by
the build, and no hardware commands are included here.

## Build

```sh
cargo test -p xtask
cargo xtask kernel-src qcom/msm8917-lenovo-tbx304x
cargo xtask build qcom/msm8917-lenovo-tbx304x
```

The canonical ID deliberately matches the existing board-port DT filename
`qcom/msm8917-lenovo-tbx304x.dtb` and `lenovo,tbx304x` compatible. This initial
PocketBoot trial targets the Wi-Fi TB-X304F/APQ8017; the family name does not
claim PocketBoot validation on TB-X304L or other variants.

The [device config](../configs/device/qcom/msm8917-lenovo-tbx304x.toml) pins
[`pem120/linux-msm89x7`, `lenovo-tbx304`](https://github.com/pem120/linux-msm89x7/tree/a51b91b503307d35902447dd1f90db765372cf3b)
at `a51b91b503307d35902447dd1f90db765372cf3b` (Linux 7.0.9).
`xtask` fetches it beneath
`target/kernel/src/msm8917/msm8917-lenovo-tbx304x/` and applies the two
[maintained patches](../patches/kernel/msm8917/README.md). A second invocation
recognizes the applied series. It does not reset conflicting source edits.

Requirements are the normal PocketBoot Rust dependencies and installed
`aarch64-unknown-linux-musl` target, GNU AArch64 cross compiler/binutils,
host C build tools, make, flex and bison. No in-kernel Rust toolchain is needed:
this target uses a text DRM panic screen rather than the baseline Rust QR
encoder. PocketBoot `/init` remains Rust, with BusyBox included.

Outputs (relative to the workspace, unless `CARGO_TARGET_DIR` is set):

```text
target/kernel/qcom/msm8917-lenovo-tbx304x/boot.img
target/kernel/qcom/msm8917-lenovo-tbx304x/arch/arm64/boot/Image
target/kernel/qcom/msm8917-lenovo-tbx304x/arch/arm64/boot/Image.gz
target/kernel/qcom/msm8917-lenovo-tbx304x/arch/arm64/boot/dts/qcom/msm8917-lenovo-tbx304x.dtb
target/kernel/qcom/msm8917-lenovo-tbx304x/.config
target/cpio/qcom/msm8917-lenovo-tbx304x/pocketboot-initrd.cpio
```

## Source compatibility

The [board DTS](https://github.com/pem120/linux-msm89x7/blob/a51b91b503307d35902447dd1f90db765372cf3b/arch/arm64/boot/dts/qcom/msm8917-lenovo-tbx304x.dts)
includes `msm8917.dtsi`, `pm8937.dtsi`, `pmi8950.dtsi` and
`msm8937-qdsp6.dtsi`. The last file supplies audio routing only.
`msm8917.dtsi` is a complete SoC description, not an alias of MSM8916 or
MSM8953. The [SoC config](../configs/soc/qcom/msm8917.toml) follows its
actual drivers:

| Path in the source | Built-in driver/config |
| --- | --- |
| Four Cortex-A53 CPUs, MPIDRs `0x100`–`0x103`; `enable-method = "psci"` | ARM64 PSCI, SMC conduit; no spin-table patch or preboot shim |
| `qcom,gcc-msm8917`, `qcom,msm8917-pinctrl` | `MSM_GCC_8917`, `PINCTRL_MSM8917` (dedicated drivers in this fork) |
| SMEM/TCSR, SMD RPM clocks, domains and PM8937 regulator votes | `QCOM_SMEM`, `HWSPINLOCK_QCOM`, `RPMSG_QCOM_SMD`, `QCOM_SMD_RPM`, `QCOM_CLK_SMD_RPM`, `QCOM_RPMPD`, `REGULATOR_QCOM_SMD_RPM` |
| PM8937/PMI8950 SPMI | PMIC arbiter, MFD and PMIC pinctrl |
| eMMC/SD `qcom,sdhci-msm-v4` | `MMC_SDHCI_MSM`, TLMM and RPM supply dependencies |
| USB `qcom,ci-hdrc` | ChipIdea MSM glue and UDC, not DWC3 |
| USB `qcom,usb-hs-28nm-femtophy` | `PHY_QCOM_USB_HS_28NM`, not the older `PHY_QCOM_USB_HS` |

Only verified selection metadata is added for the F: APQ8017 ID 307, revision
0, alongside the existing MSM8917 ID 303. Board ID remains `<0x1000b 0>`.
The other stock multi-platform IDs are not copied without evidence.

RAM size stays a bootloader-filled `memory@80000000` placeholder. The source
reserves QSEE at `0x85b00000` (8 MiB), SMEM at `0x86300000` (1 MiB), a
4 MiB region at `0x86400000`, RMTFS at `0x92100000` (1.5 MiB) and board
ramoops at `0x8ee00000` (512 KiB). ADSP (17 MiB) and WCNSS (7 MiB) have
dynamic no-map reservations in the `0x86800000`–`0x8e800000` allocation
window; MPSS, GPS, MBA and Venus reservations remain disabled. These inherited
regions are retained conservatively, not certified against F firmware.
No modem, audio, Wi-Fi or GPU drivers are enabled for the initial target.

## LK2nd handoff

Android header v0, page size 2048, payload `Image.gz` followed by the DTB,
built-in initramfs, no external ramdisk/QCDT. The header uses base `0x80000000`,
kernel offset `0x80000`, tags offset `0x3400000` and ramdisk offset `0x3600000`.
These are LK2nd msm8952 ARM64 defaults, **not** the stock ARM32 offsets.

In [LK2nd `517bb38`](https://github.com/msm8916-mainline/lk2nd/tree/517bb38a409d4ac982f09e83a046e4dc71be5029),
`project/msm8952.mk` enables `ABOOT_IGNORE_BOOT_HEADER_ADDRS`.
`app/aboot/aboot.c:update_ker_tags_rdisk_addr()` uses the decompressed ARM64
Image's `text_offset` relative to a 2 MiB-aligned base. The locally built
Image has offset zero, so the expected entry placement is `0x80000000`,
not the nominal header address `0x80080000`.

The current diagnostic command line is:

```text
console=tty0 loglevel=8 ignore_loglevel lk2nd.pass-simplefb lk2nd.pass-ramoops clk_ignore_unused pd_ignore_unused regulator_ignore_unused
```

LK2nd passes the boot image command line through `boot_linux()` to
`update_device_tree()` and its registered handlers.
`lk2nd/display/simplefb.c` reads `lk2nd.pass-simplefb`, adds a no-map
`cont-splash@...` reservation and creates/enables the simple-framebuffer
with the live base, dimensions, stride and format. The static board
framebuffer/reservation is removed to avoid duplicate reservations and native
DSI PLL dependencies. No resolution, rotation or RAM size is imposed here.
The packaged DT therefore has **no framebuffer until LK2nd updates it**.
Capturing that final handed-off DT is still a hardware validation step.

The three `ignore_unused` flags temporarily preserve firmware-owned display
clocks, power domains and rails without native DRM/DSI taking ownership.
They are diagnostic policy, not a power-management solution. USB is explicitly
peripheral-only and its charger/extcon and host-VBUS dependencies are removed;
this target does not manage charging or provide USB host mode.

`console=tty0` is paired with `DRM_FBDEV_EMULATION`,
`DRM_CLIENT_DEFAULT_FBDEV` and `FRAMEBUFFER_CONSOLE` so ordinary kernel text
can appear after simpledrm probes. The initial image omitted this client:
simpledrm and DRM panic support alone do not provide an fbcon console. This
does not expose failures before framebuffer initialization or guarantee USB
bring-up, but avoids mistaking an unchanged splash for a kernel that never ran.

`PSTORE_CONSOLE` and the bare `lk2nd.pass-ramoops` token select retained
kernel-console logging in LK2nd's own mapped scratch-end region. On this unit
that is `0xbff80000`, size 512 KiB, with 8 KiB dump records, a 256 KiB console,
and ECC disabled. LK2nd rewrites the board's original `0x8ee00000` ramoops
reservation during handoff; the packaged DT still contains the original
placeholder. Do not add `=zap`, which clears the retained region.
See the bring-up record for source bounds, capture commands and retention
limits. This does not provide logs before the kernel pstore console registers.

## Validation and remaining work

Local validation completed with Rust 1.96.0 and AArch64 GCC 16.2.1:

- `cargo test -p xtask`: 54 tests passed, including the target contract.
- `cargo fmt --all --check`, `git diff --check`: passed.
- `cargo xtask build` lists the target.
- `cargo xtask kernel-src ...` twice: clean application, then idempotent.
- `cargo xtask build qcom/msm8917-lenovo-tbx304x`: built PocketBoot, BusyBox,
  kernel, DTB and the initial 4,734,976-byte `boot.img`, before enabling fbcon.
- Binary inspection verified the v0 header, exact gzip+DTB payload, ARM64
  header, APQ8017/SKU5 metadata, PSCI, peripheral USB and retained reservations.
  All requested enabled Kconfig values survived the strict merge.

The first config merge failed because this kernel's strict merge script
rejects `# CONFIG_FOO is not set` for symbols omitted from the final config.
Target-local `{ raw = "n" }` settings use its supported equivalent form;
neither the shared builder nor kernel runtime was changed to bypass checks.
The successful build had three existing PocketBoot Rust warnings
(`libc::time_t` twice, unused `continue_then`), not kernel/DT build errors.

Still unvalidated: Linux entry, PSCI/SMP, gadget enumeration, storage I/O,
live framebuffer handoff, retention of the inherited ramoops region and kexec.
This kernel also lacks the existing MSM8916 series' ChipIdea SG-bounce and
FunctionFS reset-work fixes; large fastboot uploads and gadget teardown must
not be assumed reliable. Touch, native panel/GPU, Wi-Fi and audio are deferred.
Panic/reboot is not an established unattended recovery path.

The first hardware trial was accepted by LK2nd, but no tablet USB interface
appeared within 45 seconds. Linux entry and userspace startup are not yet
confirmed. See the [bring-up record](tb-x304f-bringup.md) for the observed
result, bootloader image, recovery constraints, and next diagnostic checkpoint.

A console-enabled candidate was then rebuilt using the same initramfs and
unchanged DTB: 4,784,128 bytes, SHA-256
`c707c18b92da380fdc2bfe9158c7c10e33cc09acda09957da186a52fdd03124b`.
The generated config confirms fbdev emulation, the fbdev default DRM client,
fbcon, and their fbdev-core dependency are built in. Gzip/Image/DTB inspection
and the 54 build-tool tests passed again. LK2nd accepted this second candidate,
but no USB appeared within 45 seconds and the screen was black when checked
later. This is not a demonstrated USB fix.

`PSTORE_CONSOLE` is enabled for the retained-log profile. The bounded raw
exporter was exercised before the trial, but the kernel rebuild does not
verify Linux entry or RAM retention. The latest hardware state and
logging-region checks belong in the linked bring-up record.

The shared-region image is 4,784,128 bytes, SHA-256
`c0da6ec5c7c287b94a0e1e814e0f1517da6e091fd75bba3e9c0a43e738bd24d2`.
The built-in frontend, bare handoff flag, gzip/Image/DTB layout and mainline DT
classification were checked. LK2nd accepted the image, but USB again remained
absent for 45 seconds. Post-reset log retrieval and retention are pending.
