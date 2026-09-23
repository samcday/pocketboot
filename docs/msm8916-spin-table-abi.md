# MSM8916 resident spin-table ABI, version 1

This is Pocketboot's experimental contract between pocketpreboot, the running
kernel, its kexec loader and the next kernel. It is not a general CPU power-off
interface. Hardware acceptance requires non-PSCI ACC/SCM startup, useful work on
all four CPUs, acknowledged parking, and useful work on all four CPUs after
kexec. A DB410c PSCI baseline alone does not establish that result.

## Ownership and device tree

One enabled child of `/reserved-memory` has
`compatible = "pocketboot,spin-table-v1"`, a single `reg` describing exactly one
4096-byte, 4096-byte-aligned page, and `no-map`. It must not be reusable or
overlap another reservation owner. MSM8916 builds use `0x854ff000` when that
page is available in the board's memory map.

Version 1 supports the four Cortex-A53 CPUs with MPIDRs 0, 1, 2 and 3. CPU0 is
the boot and reboot CPU. Each enabled CPU has `enable-method = "spin-table"`
and a 64-bit big-endian FDT `cpu-release-addr` pointing to its slot below.
PSCI CPU startup, CPU PSCI power domains and CPU idle-state references are
removed from the handed-off description; unrelated power domains remain.
The versioned binding fixes the other offsets, so no redundant per-CPU
physical-address properties are needed.

The loader must preserve this live contract even when booting with an external
DTB: match CPUs by MPIDR, carry the reservation and release addresses, reserve
the page from kexec placement, and keep it reserved from the incoming kernel's
allocator. Malformed or conflicting contracts stop the handoff.

## Page layout

All offsets are relative to the reservation base. Descriptor and slot integers
are little-endian; FDT integers remain big-endian.

| Offset | Contents |
| --- | --- |
| `0x000` | Branch to the resident entry at `0x100` |
| `0x080` | Eight ASCII bytes `spin-tab`, the legacy identification marker |
| `0x088` | Legacy shared release address, unused and zero |
| `0x090` | Eight ASCII bytes `PBSPIN01` |
| `0x098` | u32 ABI version: 1 |
| `0x09c` | u32 page size: 4096 |
| `0x0a0` | u32 entry offset: `0x100` |
| `0x0a4` | u32 slot array offset: `0x400` |
| `0x0a8` | u32 slot stride: `0x80` |
| `0x0ac` | u32 CPU count: 4 |
| `0x0b0` | Optional eight-byte diagnostic tag `PBSDIAG1` |
| `0x100` | Immutable, position-independent resident parking code |
| `0x400 + MPIDR * 0x80` | u64 release address, primary-owned |
| slot + `0x08` | u64 request generation, primary-owned |
| slot + `0x40` | u64 acknowledgement generation, secondary-owned |
| slot + `0x48` | u64 `CurrentEL` on resident entry (4 for EL1, 8 for EL2) |
| slot + `0x50` | Optional u64 raw `CPUECTLR_EL1`, valid only with `PBSDIAG1` |

Request/release and acknowledgement occupy separate 64-byte Cortex-A53 cache
lines. Neither marker makes an occupied page safe to overwrite: an lk2nd
trampoline or parked resident CPUs may be executing it. Cold startup reclaims
an occupied page only while every secondary's `APCS_CPU_PWR_CTL` shows reset
asserted and `CORE_PWRD_UP` clear, and refuses otherwise. Page contents are
not trusted either way; reuse without reset requires the handoff below.
Neither an outgoing nor incoming kernel writes the resident code.

`PBSDIAG1` is an optional extension in previously unused space; the v1 geometry
and mandatory fields are unchanged. A tagged resident publishes both its
exception level and CPUECTLR snapshot before the acknowledgment. Its release
path requires Cortex-A53 `CPUECTLR.SMPEN` (bit 6) to be set. Preboot reads this
register but does not write it: firmware controls write permission. Older v1
pages without the tag have no defined value at slot+`0x50`; consumers must not
interpret that padding as a valid snapshot.

## Startup and parking handshake

Cold startup installs and synchronizes the resident code, initializes each
secondary slot with release=0, request=1 and ack=0, then sets the Qualcomm SCM
boot address and performs the ACC power/reset sequence. It must observe every
secondary's matching acknowledgement and matching exception level before
releasing Linux with a patched FDT. This path does not invoke PSCI. The initial
kernel parking implementation supports EL1; recording EL2 does not imply that
EL2 kexec parking has been implemented.

For kexec the primary clears a secondary's release address and advances its
nonzero request generation, making both visible before the CPU leaves the
kernel. Generation overflow is an error. Each secondary completes its final
cache maintenance and enters resident code with address translation and data
caching disabled. Only resident code publishes `ack=request`, orders the write
with `dsb`, signals an event, and waits for a nonzero release address. It zeroes
x0-x3 before branching to the next kernel's secondary entry.

The primary must observe acknowledgement through a mapping/cache-maintenance
scheme that sees the secondary's physical writes. `READ_ONCE` on a stale
cached line is insufficient. Every requested acknowledgement must match before
relocation. A failure after CPU teardown must halt handoff rather than warn
and overwrite memory a secondary might still execute.

Ordinary CPU hotplug and crash kexec are initially rejected for this contract.
Eligibility and CPU0 reboot affinity must be checked before device/CPU teardown.
The reservation, code and acknowledgements must not refer to outgoing-kernel
storage, and final cache cleaning must not be followed by stack writes in that
storage.

## Entering pocketpreboot again

A packaged kexec destination may enter pocketpreboot before its kernel. It may
reuse the resident page only when all four incoming CPU nodes already describe
the matching v1 spin-table contract, CPU0's release remains zero, and each
secondary has release=0, a nonzero request, ack=request, and the primary's
exception level. Tagged snapshots must also show SMPEN. Mixed CPU methods,
unknown descriptors, active release slots and stale acknowledgments are errors.

This path validates and rewrites the destination FDT but leaves the resident
page untouched and does not call SCM or access ACC. A raw-start input following
a bootloader reset is not equivalent to this explicit acknowledged handoff.
DRAM largely survives such a reset, so cold startup often finds its previous
page intact and reclaims it under the ACC rule above.
