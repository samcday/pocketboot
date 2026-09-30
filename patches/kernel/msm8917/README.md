# TB-X304X experimental kernel patches

Base: [`pem120/linux-msm89x7`](https://github.com/pem120/linux-msm89x7/tree/a51b91b503307d35902447dd1f90db765372cf3b),
commit `a51b91b503307d35902447dd1f90db765372cf3b`, branch `lenovo-tbx304`.
Only the `msm8917-lenovo-tbx304x` device config selects this series.

1. `0001-arm64-dts-qcom-tbx304x-apq8017.patch` adds the measured APQ8017
   bootloader selection ID, preserving the source board's family identity.
2. `0002-arm64-dts-qcom-tbx304x-pocketboot.patch` is PocketBoot-only policy:
   let LK2nd supply the actual simplefb node/reservation, preserve its enabled
   LCD supply during fixed-regulator probe, and make USB peripheral-only with
   an explicit role-switch API instead of charger/extcon dependencies.
   The device-role request supplies software session-valid state: it is a
   bootstrap workaround, not an intrinsic requirement of peripheral mode.
   This is not intended as a general-purpose upstream board change.
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
   nonpositive-timeout and module-enabled behavior remain unchanged. This
   does not change the earlier no-modules fw_devlink/sync_state milestones.

Apply through `cargo xtask kernel-src qcom/msm8917-lenovo-tbx304x`; do not
manually modify another kernel checkout. `xtask` applies the ordered series
atomically and recognizes an already-applied series. Changed patches or base
revisions need a fresh generated source tree or deliberate reconciliation;
the builder never resets conflicting source edits.

Build with `cargo xtask build qcom/msm8917-lenovo-tbx304x`. The image targets
LK2nd's ARM64 handoff, not direct stock boot. It retains firmware-owned display
and CPU clock state; charging and complete peripheral power management are
out of scope. USB-role handling is still a workaround. Large transfers,
storage writes and kexec/PocketFed handoff remain unvalidated.

The bring-up record, image hashes, recovery constraints and retired lab
tools are preserved in `refs/notes/evidence` on the bring-up commits:

```sh
git fetch origin refs/notes/evidence:refs/notes/evidence
git log --notes=evidence -- configs/device/qcom/msm8917-lenovo-tbx304x.toml
```

Notes are separate from source history. Configure
`git config notes.rewriteRef refs/notes/evidence` in each checkout that rebases
or amends this work, and explicitly carry the relevant note onto a squash
merge commit before pushing `refs/notes/evidence`.
