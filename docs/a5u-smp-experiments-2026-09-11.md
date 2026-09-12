# Samsung A5U SMP hardware validation, 2026-09-11

Status: cold startup and three consecutive kexec handoffs now pass useful work
on all four CPUs and shared-memory coherency tests. The third handoff re-enters
the configured Pocketpreboot wrapper. Panic reboot and ramoops recovery work
without UART. A subsequent IOMMU fix also removes recurring display faults
across three further handoffs; physical screen confirmation after that fix is
pending. The sequence below retains earlier failures and the diagnostics that
led to the working runs.

## Identified starting state

The explicitly addressed fastboot serial was `cd0ee037`, with
`product=pocketboot`, `compatible=samsung,a5u-eur`, and `is-userspace=yes`.
The running kernel was Linux 7.1.0-rc6+, built on 2026-07-23. Only CPU0 was
online. Its log shows failed PSCI startup of each of CPUs 1–3 with error `-95`;
all four live DT CPU nodes used `enable-method = "psci"`.

The Pixel `99NAY1AZG1` and DB410c `bc72e60` were also attached. No boot, flash,
or reboot command in this experiment targeted either of them.

## Restored lk2nd

A bootloader reboot returned directly to the old Pocketboot. With the user's
explicit authorization to restore lk2nd to boot, the entire 13 MiB boot
partition (`/dev/mmcblk0p16`) was backed up before writing. The backup SHA256 is
`3b8f02233a869d2722270ca8882cef01455364280e85570bbe5f2dcbad571125`.

The local MSM8916 lk2nd image was 421,904 bytes, SHA256
`a138dda3dbfbc5614bc4f2fa0302c9e22dc819daf4cb448856b03268c166e8f8`.
Pocketboot wrote this image at the beginning of boot. A full readback verified
the exact image bytes and confirmed every following byte was unchanged.
The full before/after images remain in `target/a5u-smp-lab/20260911/`.

After reboot, lk2nd reported:

- `product=lk2nd-msm8916`
- `lk2nd:compatible=samsung,a5u-eur`
- `lk2nd:model=Samsung Galaxy A5U (SM-A500FU)`
- `lk2nd:version=22.0-next-e9799da44-20260621`

Its boot log confirms that this existing local build forces fastboot at boot.
It also includes the debug register-read interface and the custom CPU
enable-method check used to leave startup to Pocketpreboot.

## First temporary boot

The A5 build's boot image matched the earlier coherency build manifest:
`02a44672a842d90b7386fbf747c87eee768ea9e6baacf5e3803fc54bfaa1f9e0`.
The lab copy adds only `pocketboot.lab=a5-cold` in the Android header command
line; all payload bytes remain unchanged. The lab image SHA256 is
`388f62be503b46409416ac69e13cd74d11c71ad09841b802d1edbde74b6fd8e5`.
Its four input CPU nodes use `pocketboot,msm8916-acc` and include valid ACC
phandles. The resident page reservation is at `0x854ff000`.

lk2nd acknowledged the 13,797,392-byte download and boot request. The A5 then
disappeared from USB and did not re-enumerate during observation. There is no
captured new kernel log, CPU workload result, or handoff result yet.

The user was asked to report the screen state and force a restart with USB
connected. The next step is to inspect any retained resident descriptor and
per-CPU slots using lk2nd's read-only debug interface. Retention across that
reset is not assumed. Further instrumentation or UART may be needed to locate
an earlier failure.

Once the first kernel runs, require the same four pinned arithmetic workers,
shared-buffer ring and migration checks used on DB410c. Repeat them after
bare-kernel kexec and packaged-Pocketpreboot re-entry. Prepare those destination
images from the A5's live DT, preserving its firmware-provided memory layout;
the static input DT has a zero-sized memory placeholder.

Raw logs, the original live DT and manifests are preserved under
`docs/evidence/a5u-smp-2026-09-11/`, with `SHA256SUMS`. Large boot images and
partition backups are retained locally under `target/a5u-smp-lab/20260911/`.

## UART-free diagnostic iteration

The user observed the unchanged lk2nd menu with unresponsive buttons, then
restarted the phone. All queried resident descriptor and slot words were zero
after that reset. This does not distinguish an early stop from reset-time
memory loss.

Software-reboot control tests wrote and immediately verified isolated words in
the unused resident-page tail. After reboot, most bits remained but some ones
had become zero. All test words were then cleared. Raw captures retain both
the written and observed values. This establishes unreliable RAM retention
across that reset, not faulty RAM during normal operation.

An optional `debug-ram-trace` shim now records entry, exception registers and
console text in three copies with a text checksum. The normal feature-disabled
binary is byte-for-byte identical to the original build. Decoder fault tests,
15 preboot host tests, ordinary QEMU startup/resident tests and an actual
instrumented QEMU log capture pass. See [RAM trace details](preboot-ram-trace.md).

The instrumented A5 image has SHA256
`c965e21a4a920ed6a8e1b3ff789a162d8c7ad976106e8952e09258e7887bd6d7`.
It preserves the previous kernel payload and uses `pocketboot.lab=a5-ram-trace`.
lk2nd accepted this temporary boot, but the A5 again did not return on USB
during observation. The initially assumed trace address `0x80094000` was wrong
for that image, as diagnosed below.

## Recovered failure and load-header fix

The initially collected 12 KiB at `0x80094000` were all zero. Inspection of
lk2nd's `update_ker_tags_rdisk_addr()` and the built configuration showed that
it ignores Android placement addresses and honors the ARM64 Image text offset.
Our raw shim advertised zero, so lk2nd placed it at `0x80000000`, although the
packager padded the inner kernel for a load address of `0x80008000`. The inner
kernel was therefore 32 KiB before preboot's computed payload address.

At the actual trace address, `0x8008c000`, a full-page read stalled after 6984
bytes. Resetting only the host USB connection recovered lk2nd without a phone
reset. Reading 256-byte prefixes of all three pages then recovered the complete
66-byte console text using bitwise OR, with a matching FNV-1a checksum:

```text
msm8916 preboot
pocketpreboot
pocketpreboot: bad payload
```

The original newline bytes and header-copy disagreements are retained in the
raw capture and decoded JSON. UART initialization had returned. This failure
occurred before the SMP preparation path.

`xtask` now writes the configured load offset within a 2 MiB region into the
packaged shim's ARM64 header. For A5 it is `0x8000`; lk2nd then loads the shim
at `0x80008000`, and the existing `0x1f8000` file padding correctly places the
kernel at `0x80200000`. The raw standalone shim is unchanged. This also makes
the declared layout consistent for the legacy kexec loader.

All 13 boot-image tests pass, including the reproduced A5 placement case,
an aligned DB410c case and the Exynos placement case. The corrected wrapped
image also passed its payload check under QEMU, stopping at QEMU's expected
zero-SMPEN precondition rather than `bad payload`.

The default A5 boot image was regenerated. Its SHA256 is
`c731bf649c62d3bce8166ed7d831d1569fb06aebe8ba14e8249d3a3cee09d7c7`.

## Successful cold SMP proof

The corrected instrumented image SHA256 is
`968643c8566bb7259fdc6797d3f2246d79fc87acb7678c5b69a2c7e843cd4d0c`.
It entered Pocketboot with marker `pocketboot.lab=a5-header-fix-ram`.
Its live DT has CPU0–3 using `spin-table`, with release addresses
`0x854ff400`, `0x854ff480`, `0x854ff500`, `0x854ff580`; PSCI is disabled.
Linux reported the owned persistent page and all four CPUs booted.

The identity-gated phase runner passed:

- Four distinct pinned CPU workers, each computing 64 MiB of generated data
  and returning hash `292d74d0d0222325` after about 0.504 CPU seconds.
- 40,000 verified transfers of a shared 2 KiB buffer across all four CPUs,
  completing in 136 ms.
- 4,000 verified CPU migrations, completing in 85 ms.

Captures are in `proof/cold/`. This is hardware evidence of useful coherent SMP
in Pocketboot, rather than merely an online CPU count.

## First kexec attempt and current limit

A5-specific destination images were prepared from its captured live DT,
preserving firmware memory ranges and serial identity. The initial destination
CPU methods are PSCI and the resident reservation is removed, deliberately
requiring Pocketboot to graft its active spin-table contract. The first two
images contain the bare A5 kernel; the planned third contains the exact
corrected normal preboot/kernel section, including its `0x8000` text offset.
The earlier generated zero-offset third image is retained but is not part of
the selected sequence. No DB410c kernel or DTB is used.

The first image's fastboot boot request succeeded. Host USB logs show a new
Pocketboot device with serial `cd0ee037` after transient address/descriptor
errors. All four identity getvars subsequently matched. However, the first
workload-install transfer failed, and the host recorded a USB disconnect at
17:26:17 Australia/Sydney. No destination command-line marker, live DT or CPU
workload result was captured. Generations two and three were not attempted.
The user subsequently confirmed a blank screen and a manual reset to lk2nd.
No panic log exists for that original attempt, so its exact cause cannot be
assigned retrospectively. Later instrumented runs reproduced USB and display
failures independently, as described below.

The normal uncompressed boot image is 13,797,392 bytes, exceeding the A5's
13,631,488-byte boot partition; it was only booted from RAM. lk2nd remains the
installed boot image. A future persistent Pocketboot installation needs an
image that fits, including lk2nd's 512 KiB prefix if that layout is retained.

## Proven panic recovery and the USB failure it exposed

The A5 overlay now reserves `0x9ff80000..0x9fffffff`, the 512 KiB window
downloaded by this lk2nd build's `oem ramoops raw`. It contains 32 8 KiB panic
records and a 256 KiB console region. `CONFIG_PSTORE_CONSOLE=y` enables
continuous logging. ECC uses 64 parity bytes per 128-byte data block; available
payload is 5364 bytes per panic record and 174708 bytes for the console.
All test images use `panic=5 oops=panic`, and the kexec fixture generator now
defaults to those settings. It does not enable panic-on-warning.

A deliberate `echo c > /proc/sysrq-trigger` produced a panic on CPU2 and
automatically returned to lk2nd. The full raw window was downloaded in one
transfer. Both the console, including `Rebooting in 5 seconds..`, and the
compressed panic backtrace were recovered with zero ECC corrections.
The offline decoder uses the pinned kernel's Reed-Solomon implementation and
raw DEFLATE format. Four tests cover the real hardware panic record, injected
header/data/parity corruption, uncorrectable blocks and truncated captures.

The next cold run exposed an unrelated failure when uploading the 41054-byte
workload installer:

```text
ci_hdrc ci_hdrc.0: not page aligned sg buffer
fastboot download failed ... Invalid argument
fastboot server fatal transport error ... timeout waiting for exact AIO transfer
Kernel panic - not syncing: Attempted to kill init! exitcode=0x00000000
```

The device automatically returned to lk2nd and supplied the complete log.
The MSM ChipIdea glue now enables its existing `CI_HDRC_HAS_SHORT_PKT_LIMIT`
workaround, including repair of problematic scatter-gather tails before DMA
mapping. This also enforces its existing 16 KiB request limit; the fastboot
AIO library already splits requests at 16 KiB. Wider MSM platform and other
gadget-function coverage remain untested. The separate upstream FunctionFS
fix `3137b243c93982fe3460335e12f9247739766e10` is backported to remove the
uninitialized-work cleanup warnings.

With the USB fix, the original installer and all CPU/coherency tests passed.
Later, 17 exact upload/download round trips passed at sizes from 1 byte to
1048593 bytes, including 4095/4096/4097, 16383/16384/16385, 41054, and
65535/65536/65537 bytes.

## Kexec display failure and unused secure IOMMU contexts

The first kexec with reliable USB entered the expected destination and started
all four CPUs, then failed during display probing. The firmware rejected
`qcom_scm_iommu_secure_ptbl_init()` with `-EINVAL`; repeated display cleanup
then faulted in `destroy_workqueue()` via `mdp5_kms_destroy()`. The panic
automatically rebooted to lk2nd. This reset did corrupt RAM: ECC recovered the
console header and many blocks, including the final backtrace, but some blocks
and the compressed dumps were uncorrectable. The decoder retains and labels
the partial console as damaged instead of presenting it as a complete log.

Pocketboot does not use the camera/video secure IOMMU contexts. The A5 overlay
now disables context banks `0x3000` and `0x5000`, retaining the display's
non-secure `0x4000` context. A small driver patch makes the secure-table
requirement check honor disabled children, consistent with device population.
This avoids allocating firmware-owned dynamic page tables that currently have
no kexec handoff protocol. No firmware error is ignored. The display driver
cleanup bug itself has not been fixed by this change.

## Three successful handoffs with the final build

Local run: `target/a5u-smp-lab/20260911/ramoops-iommu/`.
Kernel: Linux 7.1.0-rc6+ build `#51`, with the four configured local patches.
The frozen images retain A5 firmware memory ranges and serial identity.
The external DTs initially request PSCI and omit the resident page, requiring
the running loader to graft its live spin-table contract.

| Phase | Image entry | Four pinned workers | 40000 shared transfers | 4000 migrations |
| --- | --- | --- | --- | --- |
| Cold | Configured A5 Pocketpreboot | PASS | PASS | PASS |
| Generation 1 | Bare A5 kernel | PASS | PASS | PASS |
| Generation 2 | Bare A5 kernel | PASS | PASS | PASS |
| Generation 3 | Configured A5 Pocketpreboot re-entry | PASS | PASS | PASS |

Each worker processed 64 MiB of generated data with the expected hash and at
least 0.504 CPU seconds. All phases report `online=0-3`, the expected generation
marker, disabled PSCI, the same reserved resident page, and release addresses
`854ff400`, `854ff480`, `854ff500`, `854ff580`. Display KMS initializes in the
destination kernels. During the final five-minute soak, MDP interrupt error
`04000000` (interface underrun) recurred; this was not just a single startup
event. The final DRM inventory shows an enabled, connected 720x1280 DSI output,
an active CRTC and framebuffer 64 owned by `pocketboot-ui`. Those facts do not
prove correct visible output. The user subsequently confirmed a blank screen.

The third kernel's pstore filesystem also supplied generation 2's outgoing
console, with all three secondary parking acknowledgements at epoch 4 and
`Bye!`; the kernel reported no ECC errors for that saved console. Thus the
handoff evidence includes both outgoing parking and useful work after re-entry.

The final kernel then passed 21 more workload/coherency rounds over 300.8
seconds: 84 pinned 64 MiB worker runs, 840000 shared-buffer transfers and 84000
CPU migrations. No panic, Oops, CPU lockup or USB transport failure was recorded
during that soak. Recurring display underruns remain a separate limitation.

After panic resets, intact resident signatures can remain even though firmware
has stopped the CPUs. For two lab restarts only, host-side checks verified
lk2nd identity, the exact Pocketboot v1 descriptor, and all three ACC power
controls at `0x23` (POR reset and clamps asserted, powered-up flag clear), then
retired the two stale signature words before another raw startup. Register
checks were repeated before boot. The production preboot occupied-page guard
was not weakened. Subsequent bit-corrupting resets needed no signature writes.

The #51 normal uncompressed boot image was 13862928 bytes, SHA256
`0ee57bcc12b904581ecc3e5e6f752eb70680cd1f908d4d69294ec813d9931588`.
It remains RAM-booted because it exceeds the 13 MiB boot partition. lk2nd is
still installed. Passing these handoffs does not establish arbitrary external
kernel compatibility, ordinary CPU hotplug, crash kexec, or display recovery
on every boot.

## Display follow-up after the SMP milestone

The user confirmed the third kernel's screen was blank. A fourth, direct
bare-kernel handoff used `pocketboot.log=info pocketboot.drm_page_flips=16
drm.debug=0x6`, retaining panic recovery and the four-core contract. All 16
page flips completed and the native DSI output remained enabled, but underruns
also appeared in this direct handoff. This is not exclusive to preboot re-entry.

The existing configuration lacks a display interconnect path and the MSM8916
interconnect driver. An additional build tested a `mdp0-mem` path from
SNOC's MDP port to BIMC/EBI, with the corresponding driver enabled. This is
an investigation of the blank display, outside the proved #51 SMP run.

### Follow-up: hidden display IOMMU faults

The user confirmed a blank screen after the successful SMP sequence. An
interconnect trial enables the MSM8916 providers and adds `mdp0-mem`; its live
summary confirms a 6,400,000 kB/s peak display vote. This does not eliminate the
underruns. Bounded 16-flip tests consistently fault when framebuffer 64, IOVA
`0x732000`, is selected; framebuffers 62 and 63 have no corresponding underrun
in that short sequence. The final UI also selects framebuffer 64.

The #52 interrupt snapshot records 2,096,154 `qcom-iommu-fault` interrupts.
These were absent from dmesg because `qcom_iommu_fault()` tests the return of
`report_iommu_fault()` backwards. The MSM display callback returns `-ENOSYS`
after requesting a snapshot, and the driver prints only on zero. A logging-only
correction (#53) exposes `fsr=0x40000280`, `fsynr=0x23`, context bank 4, starting
at IOVA `0x732000`. Additional temporary physical-address diagnostics (#54)
show a level-3 translation fault (`fsr=0x40000202`) at the same IOVA, despite a
successful software lookup to physical `0x83003000`; TTBR0 reads
`0x4000082150000`, TCR `0x80802020`, TCR2 `0x38062`.

These captures establish a display DMA translation failure. The next experiment
below identifies stale IOMMU state as its cause. Interconnect support did not
resolve it. All these
boots still start four CPUs and complete the 16 page-flip events; those events
alone do not prove correct scanout. Raw captures and frozen images are in
`target/a5u-smp-lab/20260911/display-{icc,fault,phys}/`.


### Resolved recurring faults: invalidate the old IOMMU context

`qcom_iommu_init_domain()` installs a new TTBR using the context's fixed ASID,
but previously did not invalidate translations and intermediate table walks
left by the preceding kernel. The new `0006` patch calls the existing context
invalidation helper before publishing the new domain's page-table operations.
The helper issues `S1_TLBIASID` for the domain's contexts and waits for completion;
`attach_dev()` still holds the IOMMU's runtime-power reference at this point.

The #55 trial immediately removed the faults: all 16 flips completed, and the
IOMMU interrupt count remained zero. The #56 final build removes the extra
physical-address diagnostics and the unsuccessful interconnect experiment.
It retains the small `0005` unhandled-fault logging correction and `0006`
context invalidation. No UI rendering code changed.

The final #56 run passed the following checks at every stage:

| Stage | Entry | Four pinned CPU workers | Shared memory/migrations | Display flips | IOMMU fault IRQs |
| --- | --- | --- | --- | --- | --- |
| Initial | Bare-kernel kexec | PASS | PASS | 16/16 | 0 |
| Handoff 1 | Bare-kernel kexec | PASS | PASS | 16/16 | 0 |
| Handoff 2 | Bare-kernel kexec | PASS | PASS | 16/16 | 0 |
| Handoff 3 | Configured Pocketpreboot re-entry | PASS | PASS | 16/16 | 0 |

All four live DTs retain the verified A5 identity, 2 GiB RAM layout, spin-table
CPU methods and release slots, disabled PSCI, and ECC-protected ramoops.
A further workload run after 67 seconds uptime in the last kernel also passes
with zero IOMMU fault interrupts. Ramoops contains the preceding kernel's
CPU1–3 parking acknowledgements at epoch 13 followed by `Bye!`.

One initial `INTF1_UNDER_RUN` remains at interface enable on each boot, matching
the older July display checkpoint. The repeated per-buffer faults and steady
underrun stream are gone. **Physical confirmation of the panel's visible menu
is still pending**; DRM events and clean fault counters alone are not treated
as that confirmation.

The final normal uncompressed image is 13,862,928 bytes, SHA256
`0359533e3e1a7359c4925d117ddc337db9e10c9af6eaa5f41afd90c6e5b2e8ea`.
It still exceeds the 13 MiB boot partition and has not been flashed. lk2nd
remains installed. The device is left running `pocketboot.lab=display-final-3`.
The final #56 was tested through kexec; the cold ACC/SCM proof and five-minute
soak above belong to #51. No broader hardware coverage is implied.

Raw evidence, scripts, patch snapshots and source hashes are preserved in
[`evidence/a5u-smp-2026-09-11/display-final/`](evidence/a5u-smp-2026-09-11/display-final/).
The preceding failures are under `display-{icc,fault,phys}/`, and the first
successful trial is under `display-tlb/`. Large image payloads remain in the
matching directories beneath `target/a5u-smp-lab/20260911/`.
