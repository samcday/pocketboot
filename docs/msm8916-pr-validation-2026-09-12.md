# MSM8916 PR preparation checks, 2026-09-12

This records the checks performed when preparing the SMP/kexec work for review.
No phone or development board was booted, flashed, or otherwise operated during
PR preparation. Hardware claims refer to the recorded September 11 runs.

## Host and emulated checks

| Check | Result |
| --- | --- |
| Pocketboot host tests | 177 passed |
| Pocketpreboot host tests with `soc-msm8916` | 15 passed |
| xtask tests | 49 passed |
| ARM64 kexec tests executed through `qemu-aarch64` | 22 passed |
| Python tool tests, including real ramoops capture/ECC corruption cases | 50 passed |
| QEMU resident/startup harness | SMPEN-clear refusal and relocated Rust/FDT startup passed |
| Freestanding release builds | A5U and existing Exynos7870/J7xelte features passed |
| Kernel patch application | All six patches apply together to pristine `d87323486b79a31b9b25eb6fe30f1b503f142ff2`; reverse check matches the tested source |
| Formatting, whitespace, evidence hashes | Passed |

The initial workspace test run caught an old overlay assertion that matched
an IOMMU parent's path inside its disabled secure-context children. It now
matches the exact parent override; all 49 xtask tests pass. Formatting changes
are confined to the new RAM trace and boot-image test code. Neither cleanup
changes the tested SMP or IOMMU behavior.

Reproduction commands, with the normal cross-build prerequisites installed:

```sh
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

The ramoops tests require the configured kernel source at
`target/kernel/src/msm8916`: `cargo xtask kernel-src qcom/msm8916-samsung-a5u-eur`
prepares it and applies the configured patch series. They compile the host
codec against that kernel's Reed–Solomon implementation. Cargo's offline
commands assume dependencies have already been downloaded.

## Hardware evidence and remaining scope

- [DB410c evidence](msm8916-smp-experiments-2026-09-11.md): raw ACC/SCM startup,
  four working CPUs, three consecutive handoffs including preboot re-entry,
  and a 300.8-second coherency/workload soak.
- [A5U evidence](a5u-smp-experiments-2026-09-11.md): the same cold/handoff/soak
  milestone on build #51; automatic panic reboot and ramoops recovery;
  subsequent #56 handoffs with four working CPUs and zero display IOMMU faults.
  One initial display underrun remains. Explicit physical screen confirmation
  after the IOMMU fix is not recorded.
- GT510 configuration is included, but no GT510 hardware success is claimed.
  Later A5 USB/IOMMU fixes have not been revalidated on DB410c or wider MSM devices.
- V1 requires four Cortex-A53 CPUs at EL1, 4 KiB kernel pages and CPU0 as the
  reboot CPU. Ordinary CPU hotplug, crash kexec and EL2 parking are unsupported.
  Firmware must establish SMPEN. An occupied resident page after a bootloader
  reset is deliberately rejected unless it is a valid acknowledged re-entry.
- The tested uncompressed A5 image is larger than the 13 MiB boot partition;
  it has been used through temporary boot/kexec. Packaging for installation
  remains follow-up work.

Raw logs and fixtures are in `docs/evidence/`, with checksums. Full boot images,
partition backups and generated build trees remain local under `target/`.
