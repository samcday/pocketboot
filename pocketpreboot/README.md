# MSM8916 preboot

For UART-free lab diagnostics, see the optional
[triplicated RAM trace](../docs/preboot-ram-trace.md).
Once Linux starts, [A5 ramoops recovery](../docs/a5u-ramoops.md) preserves
kernel logs across failures. [A5 hardware validation](../docs/msm8916-validation.md)
now covers cold startup and three consecutive handoffs, including re-entry.

The packager sets the shim's ARM64 `text_offset` to match its configured load
address within a 2 MiB region. This must agree with the inner-kernel padding:
lk2nd and kexec honor that field even when the Android header names a different
address. The raw standalone shim retains offset zero.

The A5U and GT510 boot-image configurations prepend this shim to the uncompressed
arm64 Image. Their DT overlays reserve a 4 KiB page at `0x854ff000` and label all
four CPUs with `enable-method = "pocketboot,msm8916-acc"`. That input DT must boot
through pocketpreboot: the shim starts CPUs using Qualcomm SCM `SET_BOOT_ADDR_MC`
and the per-core ACC power sequence, waits for their resident acknowledgments,
then changes every CPU to `spin-table` before entering Linux.

The custom input method prevents lk2nd's automatic SMP spin-table setup from
starting the secondaries first. Local lk2nd checks for `psci` or `spin-table` in
`lk2nd/smp/spin-table/spin-table.c:check_cpus()`. Do not combine this image with
`lk2nd.spin-table=force`, which bypasses that check. An occupied page is always
rejected on the cold startup path. Preboot never overwrites possibly executing
resident code.

A packaged image entered through kexec can reuse the page. This requires all
four incoming CPU nodes to use `spin-table` with their exact v1 release slots,
the matching descriptor, zero release words, and each secondary's nonzero
request to match its acknowledgment and exception level. Reentry performs no
SCM calls, ACC operations, or writes to the resident page. A bootloader reset
that restores the raw startup DT does not meet this contract and is rejected.

The resident format is specified in [the versioned parking ABI](../docs/msm8916-spin-table-abi.md).
The assembly reports each secondary's exception level before publishing its
acknowledgment. Mixed exception levels fail startup. Both primary and secondary
entries require the MMU and data cache disabled. Instruction cache may be enabled.
The firmware must provide a working architectural counter for ACC delays.

New resident pages contain the optional `PBSDIAG1` tag at descriptor offset
`0xb0`. This means slot offset `0x50` contains a little-endian CPUECTLR_EL1
snapshot taken before the acknowledgment. Preboot prints these snapshots and
requires Cortex-A53 SMPEN (bit 6) on CPU0 and newly started secondaries. On
reentry it checks secondary snapshots only when the tag is present; old v1
pages have no diagnostic field. The resident also refuses to release a CPU
whose snapshot has SMPEN clear. No path writes CPUECTLR: higher firmware owns
its access permissions and must establish coherency before Linux enables caches.

The shim is linked as a position-independent executable. Its assembly entry
applies `R_AARCH64_RELATIVE` relocations before Rust accesses pointer tables,
enables FP/SIMD, and installs exception diagnostics on a separate stack. The
raw image must include the dynamic relocation table; no ELF loader is needed.

UART output uses the FDT `stdout-path`/alias to locate UARTDM v1.4. It preserves
firmware pin routing, clock and baud configuration, and initializes only the
transmitter's single-character mode. It does not program the board's USB/UART
MUIC switch. SCM probes SMC64 and SMC32, packs extended arguments to match, and
preserves Qualcomm's x6 cookie across interrupted calls. Unsupported conventions,
firmware service errors, and missing secondary acknowledgments stop the boot.

CPU PSCI power domains and idle-state references are removed; unrelated CPU
power-domain tuples and their names are retained. V1 uses WFI idle rather than
adding another firmware power-collapse/resume path.

Host checks and a freestanding build require no network access:

```sh
cargo test --offline -p pocketpreboot --features soc-msm8916
cargo build --offline --release --target aarch64-unknown-none -p pocketpreboot \
  --features device-msm8916-samsung-a5u-eur
```

Host tests cover FDT parsing/rewriting, ABI geometry and rejection, CPU topology,
memory overlaps, the lk2nd bypass method, guarded resident reentry, and SCM
argument encoding. They do not
simulate secure firmware, ACC hardware, UART routing, or cache coherency.

With QEMU and the GNU AArch64 assembler/linker installed, run:

```sh
bash pocketpreboot/tests/qemu/run.sh
```

The resident test launches three emulated secondaries. If the CPU model
reports SMPEN set, it checks two parking generations and zero entry registers
on both releases. QEMU's Cortex-A53 currently reports SMPEN clear; in that case
the test verifies all three snapshots and that release is refused. It does
not emulate hardware coherency or bypass the production precondition. The startup
test runs the real assembly entry and linker script, checking relocated Rust
pointer tables and the preserved FDT argument. QEMU uses its own PSCI service
to instantiate test CPUs; this does not test the production ACC/SCM path.
