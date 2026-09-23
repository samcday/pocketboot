# MSM8916 SMP and kexec validation

Pocketpreboot starts the four Cortex-A53 cores through ACC/SCM. Pocketboot uses
all four, then returns the secondaries to the same reserved spin-table page
before kexec. See the [resident ABI](msm8916-spin-table-abi.md) and
[kernel patch notes](../patches/kernel/msm8916/README.md) for the contract.

## Hardware results

These results were recorded on September 11, 2026. Review preparation did not
boot or flash either device.

| Device / build | Recorded result |
| --- | --- |
| DB410c, non-PSCI path | Cold ACC/SCM startup and three consecutive kexecs, including preboot re-entry. All four CPUs passed measured computation, 40,000 shared-buffer handoffs and 4,000 migrations in every generation. A separate 21-round / 300.8-second soak passed. Invalid CPU topology and overlapping reservation were rejected before boot. |
| A5U, build #51 | The same cold-start, three-handoff and soak checks passed. Automatic panic reboot, raw ramoops recovery and 17 USB boundary transfers also passed. Display remained blank with recurring underruns. |
| A5U, build #56 | Initial kexec and three further handoffs, including configured preboot re-entry, passed the four-core checks and 16/16 display flips with zero IOMMU fault interrupts. Ramoops preserved the outgoing parking acknowledgements. One initial display underrun remained. |

The final #56 build has kexec evidence; its cold-start and soak coverage must
not be inferred from #51. Explicit physical screen confirmation after the
IOMMU fix is not recorded. GT510 has build coverage but no hardware validation.
The later A5 USB/IOMMU patches were not revalidated on DB410c or wider MSM hardware.

V1 requires four Cortex-A53 CPUs, EL1 kernel entry, 4 KiB pages, CPU0 for reboot
and firmware-established SMPEN. Ordinary hotplug, crash kexec and EL2 parking
are unsupported. A firmware reset is not a valid preboot re-entry; cold startup
reclaims a retained resident page only while ACC holds every secondary in
reset. The uncompressed A5 image exceeds its 13 MiB boot partition;
the recorded tests used temporary boot/kexec.

## Lessons retained from bring-up

- **Image placement:** the outer ARM64 `text_offset` must agree with the
  bootloader's placement. On A5, advertising zero moved the inner payload
  32 KiB early. The combined file must also cover the inner kernel's BSS,
  otherwise kexec can place its DTB or trampoline in that runtime memory.
- **Preboot execution:** compiler-generated pointer tables need PIE relocations,
  and generated memory routines may need FP/SIMD enabled before Rust entry.
- **CPU ownership:** an online mask is insufficient evidence. Measure useful
  work on each core and shared-memory transfers. Acknowledge parking from the
  resident code; never overwrite an occupied page a secondary could still
  execute. ACC reset state, not the page contents, decides that.
- **Coherency:** preserve firmware SMPEN and shared L2 while cleaning the
  departing core's private L1. QEMU exercised refusal when SMPEN was clear;
  the positive coherency evidence comes from hardware.
- **UART-free recovery:** `panic=5 oops=panic` and ECC-protected ramoops made
  failed A5 handoffs inspectable. Preserve the raw capture; imperfect reset
  retention can exceed the ECC correction bound.
- **USB and display:** short SG handling and early FunctionFS work initialization
  were prerequisites for reliable uploads. The blank display persisted with
  adequate bandwidth votes and valid software page-table entries. Logging
  unhandled IOMMU faults exposed the failure; invalidating the newly programmed
  context removed stale translations across kexec.

## Reproduce the checks

With the usual cross-build prerequisites and downloaded Cargo dependencies:

```sh
cargo xtask kernel-src qcom/msm8916-samsung-a5u-eur
cargo test --offline --workspace --features pocketpreboot/soc-msm8916
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64 \
  cargo test --offline -p pocketboot --target aarch64-unknown-linux-musl kexec::
python3 -m unittest discover -s tools/tests -v
bash pocketpreboot/tests/qemu/run.sh
cargo build --offline --release --target aarch64-unknown-none \
  -p pocketpreboot --features device-msm8916-samsung-a5u-eur
cargo build --offline --release --target aarch64-unknown-none \
  -p pocketpreboot --features device-exynos7870-j7xelte
cargo fmt --all -- --check
```

The Python ramoops tests compile a host codec from the pinned kernel's
Reed–Solomon source. Their [small capture fixture](../tools/tests/fixtures/README.md)
is self-contained; the experiment archive is not a test dependency.
Hardware procedures: [DB410c handoffs](db410c-kexec-validation.md),
[coherency workload](msm8916-coherency-validation.md),
[A5 recovery](a5u-ramoops.md), [preboot RAM trace](preboot-ram-trace.md).

## Full experiment archive

The original submission is retained on branch
`codex/msm8916-smp-evidence-2026-09-11`, pinned by commit
[`bf5d70026af96f97a87c0d300b4bf91e0a0b22e5`](https://github.com/samcday/pocketboot/tree/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5).
It preserves all 358 evidence files, both checksum manifests, the tested source
and the complete written investigation. These links are independent of the PR branch:

- [A5 experiment narrative](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/a5u-smp-experiments-2026-09-11.md)
  and [raw captures](https://github.com/samcday/pocketboot/tree/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/evidence/a5u-smp-2026-09-11).
- [DB410c experiment narrative](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/msm8916-smp-experiments-2026-09-11.md)
  and [raw captures](https://github.com/samcday/pocketboot/tree/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/evidence/msm8916-smp-2026-09-11).
- [Earlier recovery audit](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/msm8916-smp-status-2026-09-08.md),
  [DB410c access baseline](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/db410c-bringup-baseline-2026-09-10.md)
  and [original PR validation](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/msm8916-pr-validation-2026-09-12.md).

To extract and verify the captures without changing the checkout:

```sh
git fetch origin codex/msm8916-smp-evidence-2026-09-11
mkdir -p target/msm8916-evidence
git archive bf5d70026af96f97a87c0d300b4bf91e0a0b22e5 docs/evidence |
  tar -x -C target/msm8916-evidence
(cd target/msm8916-evidence/docs/evidence/a5u-smp-2026-09-11 && sha256sum -c SHA256SUMS)
(cd target/msm8916-evidence/docs/evidence/msm8916-smp-2026-09-11 && sha256sum -c SHA256SUMS)
```

Full boot images, partition backups and generated build trees were never part
of that commit; they remain local under `target/`.
