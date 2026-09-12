# MSM8916 Pocketboot kernel patches

`0001-arm64-pocketboot-spin-table-kexec.patch` applies to the configured Linux
base `d87323486b79a31b9b25eb6fe30f1b503f142ff2`. It replaces the earlier WIP
parking approach; do not stack commit `45e2439d9631` underneath it.

The `[kernel-source].patches` list in the SoC/device configuration names patch
files relative to the Pocketboot workspace root, in application order. The
build reads the series once and submits it to Git as one combined stream;
dependent patches that touch the same file are supported. A repeated build
recognizes the already-applied series. Conflicting or partially applied series
fail without resetting source edits. Patch paths cannot escape the workspace,
and Git rejects changes outside the kernel source. Updating the base revision
or replacing/removing a previously applied series requires a fresh source tree
or deliberate reconciliation of that tree; this mechanism does not clean it.

Enable `CONFIG_ARM64_SPIN_TABLE_KEXEC=y` in a 4 KiB-page arm64 kernel with
`ARCH_QCOM`, `HOTPLUG_CPU`, and `KEXEC_CORE`. Ordinary PSCI systems retain their
existing CPU operations. Spin-table kexec is accepted only with the explicit
`pocketboot,spin-table-v1` reservation and four online Cortex-A53 CPUs booted
at EL1, with logical CPU0 / MPIDR 0 selected for reboot.

The kernel validates the immutable resident descriptor, CPU release addresses,
and the 4 KiB aligned `no-map` reservation. It never installs or rewrites parking
code. The executing kernel and every destination DT must preserve that code and
reservation. Normal kexec increments a request epoch and clears the release
address for each secondary, then cleans the release cache line. A stackless
identity-mapped exit disables data caching, cleans and invalidates the A53
private L1 data cache, disables translation, and enters the resident trampoline.
Shared L2 and coherency remain active for the other cores. The
resident trampoline must acknowledge the new epoch before polling for the next
kernel. The primary invalidates each whole dedicated acknowledgement line on
poll, and panics if a secondary fails to acknowledge. A final check precedes
relocation. Kexec segments overlapping the reservation are rejected.

A small optional architecture hook revalidates this contract when kexec is
executed, before device teardown. Ordinary CPU hotplug, suspend via CPU
hotplug, crash-kexec, EL2 entry, and arbitrary reboot CPUs are unsupported.
Changing reboot CPU after preflight is caught by a final fail-closed check.
This is a powered, coherent CPU parking protocol, not a CPU power-off protocol.

## A5 transport and display fixes

The configured series also includes the fixes needed to exercise this handoff
on Samsung A5U:

| Patch | Purpose |
| --- | --- |
| `0002` | Enable the existing ChipIdea SG-tail bounce/16 KiB request workaround, preventing large FunctionFS uploads from failing with `-EINVAL`. |
| `0003` | Backport upstream `3137b243c93982fe3460335e12f9247739766e10`, initializing FunctionFS reset work before any teardown path can cancel it. |
| `0004` | Ignore disabled secure IOMMU contexts when deciding whether to allocate a firmware-owned page table. The A5 overlay disables unused camera/video contexts and retains non-secure display. |
| `0005` | Correct the inverted fault-handler result check so unhandled IOMMU faults are logged. |
| `0006` | Invalidate the newly programmed IOMMU context and synchronize completion, discarding translations retained across kexec. |

These later patches were tested on A5U. They are not part of the earlier DB410c
hardware run; wider MSM device coverage remains open. The A5 final build passes
three consecutive handoffs including configured preboot re-entry, with useful
work on four CPUs and zero display IOMMU faults. One initial display underrun
remains. See the [A5 experiment record](../../../docs/a5u-smp-experiments-2026-09-11.md)
for the distinction between software checks and physical screen confirmation.

## Validation on 2026-09-11

The patch was developed in an isolated checkout under
`/tmp/pocketboot-linux-spintable-v1`; the old dirty kernel tree was preserved.
GCC cross-compilation uses the existing A5 kernel configuration, with the
embedded initramfs path cleared for object verification:

```
make O=/tmp/pocketboot-linux-spintable-build ARCH=arm64 \
  CROSS_COMPILE=aarch64-linux-gnu- olddefconfig
make O=/tmp/pocketboot-linux-spintable-build ARCH=arm64 \
  CROSS_COMPILE=aarch64-linux-gnu- -j8 \
  arch/arm64/kernel/smp_spin_table.o \
  arch/arm64/kernel/smp_spin_table_exit.o \
  arch/arm64/kernel/machine_kexec.o arch/arm64/kernel/process.o \
  kernel/kexec_core.o
```

The exit object has no relocations, no stack accesses, no calls, no data stores,
and no PSCI/HVC/SMC instructions. Its disassembly contains the set/way clean,
barriers, MMU/cache disable, zeroing x0-x3, and branch to the resident entry.
All changed objects were compiled with the feature enabled; all applicable
changed C objects were also compiled with the feature disabled.
Object compilation and source/disassembly inspection do not establish hardware
cache correctness or four-core handoff success. Those require captured boot/parking evidence
and repeated live kexec cycles, including failure cases.

Subsequent DB410c testing supplied that positive hardware evidence: raw
SCM/ACC startup and three consecutive kexecs, with CPU1–3 acknowledging epochs
2, 3 and 4 before relocation. All four CPUs passed measured computation,
40,000 shared-buffer transfers and 4,000 checked migrations in every kernel.
The third destination reentered pocketpreboot and safely reused the resident
page. Load-only CPU-topology and reservation-overlap fixtures were rejected.
This does not claim hardware testing of a forced parking timeout, crash paths,
EL2 or other boards. See the [experiment record](../../../docs/msm8916-smp-experiments-2026-09-11.md).

## Cache and shutdown review

The private-cache sequence follows the individual-core shutdown sequence in
Arm's [Cortex-A53 TRM, DDI0500J](https://documentation-service.arm.com/static/6040c321ee937942ba301626),
section “Individual core shutdown mode”. It uses the first two operations
(clear SCTLR.C, clean/invalidate L1), but keeps SMPEN and power on. The TRM's
cluster shutdown procedure would require the other cores to have stopped
before touching shared L2; this kernel does not perform cluster shutdown.

The live reserved page has one persistent Normal-WB mapping and no linear
mapping. Temporary spin-table release mappings use the same attributes.
The kernel never writes an acknowledgement cache line: it only invalidates
and reads the whole dedicated 64-byte line. On A53, invalidate-by-VA can be
implemented as clean/invalidate; cached copies of acknowledgement lines are
always clean, so it cannot write a stale dirty acknowledgement over the
resident writer. Request/release values have a separate cache line that the
kernel explicitly cleans to PoC. The resident writer publishes entry EL
and the epoch with its caches disabled and barriers before the primary's
acceptance of the acknowledgement.

`arch_kexec_pre_shutdown()` runs before `liveupdate_reboot()`, device teardown,
and `migrate_to_reboot_cpu()`. It may run on any CPU; it validates that the
configured reboot CPU is CPU0 and every required core remains online.
Ordinary CPU-offline requests fail, so they cannot invalidate this condition.
The generic path then migrates to CPU0, temporarily disables CPU hotplug,
shuts down syscore devices, and reenables CPU hotplug for machine shutdown.
A second check on the executing CPU authorizes parking only at that point.
A concurrent change to the reboot-CPU setting after preflight is caught there
and halts handoff. Generic cleanup treats a cpu_kill error as a warning, hence
the parking timeout must panic rather than return an error.
