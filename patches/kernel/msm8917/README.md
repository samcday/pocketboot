# TB-X304X experimental kernel patches

Base: [`pem120/linux-msm89x7`](https://github.com/pem120/linux-msm89x7/tree/a51b91b503307d35902447dd1f90db765372cf3b),
commit `a51b91b503307d35902447dd1f90db765372cf3b`, branch `lenovo-tbx304`.
Only the `msm8917-lenovo-tbx304x` device config selects this series.

1. `0001-arm64-dts-qcom-tbx304x-apq8017.patch` adds the measured APQ8017
   bootloader selection ID, preserving the source board's family identity.
2. `0002-arm64-dts-qcom-tbx304x-pocketboot.patch` is PocketBoot-only policy:
   let LK2nd supply the actual simplefb node/reservation and make USB
   peripheral-only with an explicit role-switch API instead of charger/extcon
   dependencies. PocketBoot opts into device role for this target. This is not
   intended as a general-purpose upstream board change.
3. `0003-arm64-dts-qcom-msm8917-rpm-mailbox.patch` moves RPM IPC from the
   legacy APCS syscon path to mailbox channel 0, preserving offset 8/bit 0.
   This avoids a dependency on the CPU PLL provider and explicit PLL gating
   through the syscon regmap; it does not enable CPU frequency scaling.
4. `0004-clk-qcom-apcs-msm8916-use-clock-device.patch` gets the PLL input
   from the clock device's firmware node, supporting both the dedicated child
   and the legacy mailbox-node layout.
5. `0005-arm64-dts-qcom-msm8917-apcs-clock-child.patch` moves the unchanged
   APCS clock inputs and provider into that child and updates CPU phandles.
   RPMCC can then depend on the mailbox without the mailbox depending on RPMCC.
   CPU PLL, APCS mux and CPU-frequency drivers remain disabled for bring-up.
6. `0006-pmdomain-qcom-rpmpd-select-opp.patch` selects the OPP framework
   required by RPMPD performance-state tables. Without it, the minimal kernel
   reaches RPM regulator registration but RPMPD fails with `-EOPNOTSUPP`.
   This does not enable CPU frequency scaling.
7. `0007-driver-core-honor-builtin-probe-timeout.patch` honors an explicit
   deferred-probe timeout with modules disabled, so asynchronously created
   RPM power-domain providers can appear before SDHCI gives up. The default
   zero-timeout and module-enabled behavior remain unchanged.

Apply through `cargo xtask kernel-src qcom/msm8917-lenovo-tbx304x`; do not
manually modify another kernel checkout. `xtask` applies the ordered series
atomically and recognizes an already-applied series. Changed patches or base
revisions need a fresh generated source tree or deliberate reconciliation;
the builder never resets conflicting source edits.

See [build instructions and compatibility limits](../../../docs/tb-x304f-build.md).
Compilation and DT inspection passed. The
[normal-image trials](../../../docs/tb-x304f-bringup.md#verified-normal-usb-and-emmc-boot)
confirm Linux, all four CPUs, simpledrm/fbcon registration, PocketBoot USB
fastboot and automatic eMMC discovery. A small read-only GPT transfer matched
the original backup. Visible display output, large transfers, writes and
kexec remain separate validation work.
