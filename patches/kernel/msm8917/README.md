# TB-X304X experimental kernel patches

Base: [`pem120/linux-msm89x7`](https://github.com/pem120/linux-msm89x7/tree/a51b91b503307d35902447dd1f90db765372cf3b),
commit `a51b91b503307d35902447dd1f90db765372cf3b`, branch `lenovo-tbx304`.
Only the `msm8917-lenovo-tbx304x` device config selects this series.

1. `0001-arm64-dts-qcom-tbx304x-apq8017.patch` adds the measured APQ8017
   bootloader selection ID, preserving the source board's family identity.
2. `0002-arm64-dts-qcom-tbx304x-pocketboot.patch` is PocketBoot-only policy:
   let LK2nd supply the actual simplefb node/reservation and make USB
   peripheral-only without charger/extcon dependencies. It is not intended
   as a general-purpose upstream board change.
3. `0003-arm64-dts-qcom-msm8917-rpm-mailbox.patch` moves RPM IPC from the
   legacy APCS syscon path to mailbox channel 0, preserving offset 8/bit 0.
   This avoids a dependency on the CPU PLL provider and explicit PLL gating
   through the syscon regmap; it does not enable CPU frequency scaling.

Apply through `cargo xtask kernel-src qcom/msm8917-lenovo-tbx304x`; do not
manually modify another kernel checkout. `xtask` applies the ordered series
atomically and recognizes an already-applied series. Changed patches or base
revisions need a fresh generated source tree or deliberate reconciliation;
the builder never resets conflicting source edits.

See [build instructions and compatibility limits](../../../docs/tb-x304f-build.md).
Compilation and DT inspection passed. The shared-ramoops trial's
[recovered log](../../../docs/tb-x304f-bringup.md#recovered-boot-evidence-2026-09-30)
confirms Linux, all four CPUs, simpledrm/fbcon registration and PocketBoot
`/init` startup. That image had no UDC or discovered disks and eventually
panicked after PID 1 exited; a usable display/USB/storage session is not yet
established.
