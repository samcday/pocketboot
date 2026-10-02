# MSM8939 Pocketboot kernel patches

> The Xiaomi Mi 4i is currently booted through lk2nd, which installs and owns
> the spin table, so the ferrari configuration does not reference `0001` and
> builds with `CONFIG_ARM64_SPIN_TABLE_KEXEC=n`. The patch below is retained
> for when pocketpreboot parking is re-enabled.

`0001-arm64-pocketboot-spin-table-kexec.patch` applies to the configured Linux
base `45add32603ee4aa28979ad6ec70c10b14af4ac29` (`ferrari/lkml` on
`https://github.com/pem120/linux.git`, Linux 7.3-rc4). It is the MSM8916 parking
series rebased for two clusters of four Cortex-A53s.

The ABI is unchanged in layout but indexed densely instead of by raw MPIDR:
Aff1 selects the cluster, Aff0 the core, so `index = Aff1 * 4 + Aff0` and the
resident descriptor CPU count is `num_possible_cpus()` (4 for MSM8916, 8 for
MSM8939). The primary is logical CPU0, i.e. whichever CPU the firmware boots
on: MPIDR 0 on MSM8916 and MPIDR 0x100 (big-cluster core 0) on MSM8939. Its
slot is simply never entered by the resident, and slot 0 (MPIDR 0) is a normal
secondary on MSM8939. The generated `cpu-release-addr` and the kernel's
`pb_slot()` agree on the dense index, and the resident trampoline computes it
while its caches are off.

`0002-usb-chipidea-msm-enable-sg-bounce.patch` is copied unchanged from the
MSM8916 series. `0003-iommu-qcom-kexec-context.patch` combines the two IOMMU
fixes that still apply (ignore disabled secure contexts, invalidate a newly
programmed context). The FunctionFS reset-work backport and the inverted fault
handler check are already upstream in this base and are intentionally absent.

The IOMMU invalidation remains after context programming, matching the exercised
MSM8916 patch. It is not proof that an active DMA master cannot use a stale
translation between enable and invalidation. Simply hoisting it before the loop
is not equivalent: the loop restores secure configuration and disables the old
context, which could otherwise refill translations after an early invalidation.
Master quiescence and a safe disable/invalidate/enable ordering remain a
hardware-validation concern; do not treat the current patch as that proof.

Enable `CONFIG_ARM64_SPIN_TABLE_KEXEC=y` in a 4 KiB-page arm64 kernel with
`ARCH_QCOM`, `HOTPLUG_CPU`, and `KEXEC_CORE`. See the
[MSM8916 patch README](../msm8916/README.md) for the parking contract,
`arch_kexec_pre_shutdown()` ordering, and cache/shutdown review; the
`pocketboot,spin-table-v1` reservation and `PBSDIAG1`/`PBSPIN01` tags are
identical.

These patches apply to the pristine pinned kernel, but the MSM8939 path has no
hardware validation yet. The MSM8916 record in
[`docs/msm8916-validation.md`](../../../docs/msm8916-validation.md) describes the
method and the tests that still pass unchanged for the shared code.
