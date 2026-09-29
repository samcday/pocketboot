# Ferrari owned CPU-parking experiment

This branch enables **unvalidated experimental CPU startup and handoff** for
the Xiaomi Mi 4i (MSM8939). It stacks above the
[recovery layer](https://github.com/samcday/pocketboot/pull/42), leaving the
[no-preboot baseline](https://github.com/samcday/pocketboot/pull/37) available.
Successful compilation is not permission to flash it. Coordinate transient
boot tests with the device owner after the memory-map gate below.

## What changes

- The Android v0 image still carries a gzip kernel envelope and appended DTB,
  but the envelope now starts with pocketpreboot at `0x80080000`. Its inner
  Linux Image starts on a 2 MiB physical boundary after the shim's full runtime
  footprint, including BSS.
- All eight input CPU nodes use `pocketboot,msm8939-acc`. This tells lk2nd not
  to install its own spin table. Preboot performs the cold startup, owns the
  resident page, then rewrites the runtime CPU nodes to standard `spin-table`.
  Do not use `lk2nd.spin-table=force` with this experiment.
- The kernel applies parking patch `0001` before the USB/IOMMU patches and
  enables `CONFIG_ARM64_SPIN_TABLE_KEXEC`. The ordinary four-core MSM8916
  configuration remains separate.
- The candidate resident page is `[0x854ff000, 0x85500000)`, reserved `no-map`.
  Dense slots use `Aff1 * 4 + Aff0` for `0..3, 0x100..0x103`. The physical
  primary is `0x100` (slot 4), not physical zero; Linux logical CPU0 is still
  the primary. DTB `boot_cpuid_phys` is metadata, not a measurement of MPIDR.
- The source kernel logs at `pocketboot.log=info` and requests a one-second
  reboot on panic. Ramoops is passed without `=zap`. Pre-Linux failures and
  hard hangs are not automatically recovered by either setting.

The contract still rejects unowned holding pens, invalid topology, unsafe
memory placement, missing acknowledgements, and unsupported shutdown modes.
Normal kexec requires every CPU online and the handoff on logical CPU0.
Ordinary hotplug, suspend and crash-kexec are not supported by this contract.

`panic=1` is intentional for this experiment, not a guarantee of retained
evidence or a loop-free recovery. Whether reset preserves ramoops and returns
to lk2nd fastboot depends on the installed firmware and boot policy; neither
has been validated for this image. Before the first transient test, agree on
how to interrupt a repeated panic/reboot and retrieve logs. Do not install the
experiment as the default boot image to find out.

## Hardware gate: inspect the actual incoming memory map

The candidate page lies between the source DTS framebuffer reservation ending
at `0x85000000` and firmware reservations starting at `0x86000000`. The earlier
Ferrari Linux log also places these ranges there. Neither replaces a complete
live DTB and its FDT reservation map.

Before testing this image, collect these from the **working baseline**, without
changing its memory or clearing logs:

```sh
adb -s <serial> pull /sys/firmware/fdt ferrari-live.dtb
adb -s <serial> shell cat /proc/iomem > ferrari-iomem.txt
dtc -I dtb -O dts -o ferrari-live.dts ferrari-live.dtb
```

Confirm that the page is real DRAM and does not overlap any reservation,
including FDT `/memreserve/` entries, disabled firmware carveouts, framebuffer,
ramoops, or boot payloads. Preboot also validates RAM bounds and overlaps before
writing. That check is a fail-closed diagnostic, not a substitute for reviewing
the address before the first UART-free test.

## Validate and identify the artifact

```sh
cargo xtask build qcom/msm8939-xiaomi-ferrari
sha256sum target/kernel/qcom/msm8939-xiaomi-ferrari/boot.img
```

CI uses the usual `bootimg-qcom-msm8939-xiaomi-ferrari` artifact name. Always
record its branch, commit, run ID and **extracted boot.img** SHA-256; GitHub's
artifact archive digest is not the image hash.

Offline package inspection must verify:

1. Android header placement and outer ARM64 header resolve to `0x80080000`.
2. The decompressed envelope contains the expected inner Image at its aligned
   offset, outside the shim's runtime/BSS range.
3. The appended input DTB has exactly eight custom CPU methods and exactly one
   non-overlapping 4 KiB resident reservation.

Dense `cpu-release-addr` slots are produced by **executing** preboot; they are
not expected in the input DTB. Host tests and the existing four-core resident
QEMU harness are regression evidence, not two-cluster hardware acceptance.

## Device acceptance, one boundary at a time

After agreeing on a recoverable transient-boot procedure with the owner:

1. Reach pocketboot with eight CPUs online, display, touch, USB and storage.
   Capture the runtime DTB and full boot log; check the owned descriptor,
   reservation, CPU methods and all eight dense release addresses.
2. Select the existing postmarketOS entry. A rejected load must display its
   error and leave recovery available; collect it before changing anything.
3. If handoff succeeds, verify the destination kernel reaches userspace with
   all eight CPUs and working devices. Boot success alone is not an SMP
   coherency test.
4. Repeated handoffs and cross-cluster computation need their own validation.
   Kexec **back out of** a destination OS also requires that OS to support
   the parking contract; a stock spin-table kernel cannot safely do that.

Use [recovery diagnostics](recovery.md) to collect failures. Target-DTB ramoops
layout preservation is still separate work, so retain the raw lk2nd recovery
route and do not assume every destination-kernel crash is captured.

## Taking over from an existing lk2nd holding pen

This experiment does not implement runtime takeover. A kernel already booted
through lk2nd could in principle install a **new, separately reserved resident
page**, then move all its secondaries there with acknowledged parking before
kexec. That requires an installer and publication protocol. Relabelling the
old table or changing only the next DTB proves neither ownership nor CPU
quiescence. Keep that work separate from this cold-start experiment.
