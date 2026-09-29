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

Apply through `cargo xtask kernel-src qcom/msm8917-lenovo-tbx304x`; do not
manually modify another kernel checkout. `xtask` applies the ordered series
atomically and recognizes an already-applied series. Changed patches or base
revisions need a fresh generated source tree or deliberate reconciliation;
the builder never resets conflicting source edits.

See [build instructions and compatibility limits](../../../docs/tb-x304f-build.md).
Compilation and DT inspection passed. The first hardware trial did not produce
a USB interface; Linux entry and userspace startup remain unconfirmed.
