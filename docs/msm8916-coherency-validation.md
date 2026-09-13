# MSM8916 shared-memory coherency diagnostic

`tools/smp-coherency.c` tests properties that the arithmetic-only `smp-work`
workload does not cover. Four processes pinned to CPUs 0–3 pass one 2 KiB buffer
around a token ring. Each receiver verifies every 64-bit word against the
previous sequence, writes the next pattern, and publishes an acquire/release
atomic token on a separate cache line. Default execution performs 10,000 rounds
per CPU: 40,000 handoffs in total.

After all workers report success and exit successfully, the parent verifies
the final token and buffer. It then writes a buffer on CPU3 and repeatedly
migrates itself through CPU0, CPU1, CPU2 and CPU3, checking the previous pattern
after every move. This second phase performs 4,000 migrations by default.
Only the parent prints results. The program writes anonymous RAM, not devices
or persistent storage.

## Build and run

From the repository root:

```sh
aarch64-linux-musl-gcc -O2 -static -std=c11 -Wall -Wextra -Werror \
  -o /tmp/smp-coherency-v1.unstripped tools/smp-coherency.c
cp /tmp/smp-coherency-v1.unstripped /tmp/smp-coherency-v1
aarch64-linux-gnu-strip --strip-all /tmp/smp-coherency-v1
sha256sum /tmp/smp-coherency-v1
```

The host-built artifact prepared on 2026-09-11 is a static AArch64 ELF of
67,448 bytes, SHA256
`c9f5b66ae145b757ae277a80e9fc746b582badb40b9af8cf2cf5cc2fce02a9f0`.

After placing that binary in target RAM, execute it from UART or a staged shell:

```sh
/tmp/smp-coherency-v1
# Equivalent explicit defaults:
/tmp/smp-coherency-v1 10000 1000 12
```

Arguments are ring rounds per CPU, migration rounds, and timeout seconds per
phase. Each phase has a **12-second default deadline**, leaving room inside
Pocketboot's 30-second staged-shell limit. The parent also bounds result waits
and worker reaping, kills remaining workers on failure, and rejects missing or
duplicate results. Userspace deadlines require the kernel scheduler and clock
to remain functional; they cannot recover a hung kernel.

## Acceptance

Require process exit status zero and all of these records:

- Header `SMP_COHERENCY_V1` with `phase_timeout_seconds=12`.
- Four distinct `phase=ring` CPU records, CPUs 0–3, each with `completed=10000`,
  matching `before`/`after` CPU IDs and `result=PASS`.
- Ring summary with `handoffs=40000`, `payload_bytes=2048` and `PASS`.
- Migration record with `completed=4000` and `result=PASS`.
- Final `SMP_COHERENCY_PASS all_four_cpus_shared_memory_and_migration`.

Missing output, a timeout, nonzero exit, or any failure record is a failure.
Capture the complete output separately before and after each kernel handoff;
a previous generation's result is not evidence for the next one. Passing this
bounded test establishes these exercised memory transfers, not long-duration
system stability or correctness on other boards.

## Host validation completed

Both `-O2` and `-Os` builds compile with warnings treated as errors and pass
their default runs under host `qemu-aarch64`. Native host execution also passes.
Disassembly at both optimization levels retains the 256-word payload load and
store loops and the ring's `LDAR`/`STLR`; volatile payload accesses prevent
removal of memory traffic, while the atomics provide interprocess ordering.

Temporary native fault-injection builds verified rejection of a flipped buffer
word, one worker exiting without a result, and one worker stopped indefinitely.
The stopped-worker case failed at its explicitly configured one-second
deadline. Invalid zero rounds are rejected with usage exit status 2.

These checks validate the diagnostic on the host. Subsequent DB410c hardware
execution passed with the exact binary above in the cold raw-SCM/ACC kernel
and after three consecutive kexecs, including packaged-preboot reentry. Each
phase completed 40,000 ring handoffs and 4,000 migrations. The ring took about
131–137 ms and migration about 85 ms in these runs. Captures are under
`target/db410c-lab/20260911/coherency-proof/{cold,generation-1,generation-2,generation-3}/coherency.log`.
See the [validation summary and archive](msm8916-validation.md) for provenance,
other checks and remaining limitations.
