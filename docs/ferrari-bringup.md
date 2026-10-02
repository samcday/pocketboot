# Xiaomi Mi 4i (Ferrari) bring-up

The device configuration originates from pem120's pocketboot
[`msm8939-ferrari` commit][port]. Its SoC config pins pem120/linux
[`ferrari/lkml` at `45add32603ee4aa28979ad6ec70c10b14af4ac29`][kernel]
and applies the checked-in MSM8939 USB SG-bounce and IOMMU patches.

The current image is intended to boot **through lk2nd**: an Android v0 boot
image containing `Image.gz` with an appended DTB and built-in pocketboot
initramfs. It does not enable pocketpreboot or the experimental MSM8939
CPU-parking kernel patch. Preserve this bring-up baseline until the replacement
SMP path has its own evidence.

## Build and retrieve

Local build, with the existing project toolchain prerequisites:

```sh
cargo xtask build qcom/msm8939-xiaomi-ferrari
```

The standard CI matrix automatically includes the checked-in device config.
After a successful run for the desired PR head, download the artifact
`bootimg-qcom-msm8939-xiaomi-ferrari`:

```sh
gh run download <run-id> --repo samcday/pocketboot \
    --name bootimg-qcom-msm8939-xiaomi-ferrari
sha256sum boot.img
```

Record the PR head SHA, run ID, and image hash with hardware results.
The local output is
`target/kernel/qcom/msm8939-xiaomi-ferrari/boot.img`; an unrelated `boot.img`
in the repository root is not this build's output.

## Known kexec blocker

There are two source-confirmed barriers in the current no-preboot configuration:
userspace first rejects the unowned lk2nd table with
`live spin-table CPUs have no owned parking contract`. Independently, the
kernel also lacks the CPU-shutdown support needed to perform a safe handoff.

The current DT uses lk2nd-owned `spin-table` for eight CPUs. In the pinned
kernel, the spin-table CPU operations do not provide `cpu_die`; with more than
one possible CPU, `cpus_are_stuck_in_kernel()` returns true.
If reached, `machine_kexec_prepare()` rejects the legacy kexec load with
`-EBUSY` ("Can't kexec: CPUs are stuck in the kernel").

This is a source-confirmed limitation of the current configuration, not a
claim of a new hardware test. `maxcpus=1` alone does not fix it: possible CPUs
and online CPUs are different. Do not remove the kernel's safety check to
force a handoff. Safe CPU shutdown/parking, second-kernel startup and retained
memory ownership need a focused follow-up. The experimental parking code
retained in this tree is not evidence that that path works on Ferrari.
Track the investigation in [pem120/linux#3](https://github.com/pem120/linux/issues/3).

The userspace DTB graft supports the experimental eight-CPU contract, including
dense release slots and physical boot CPU 0x100. That only removes the old
four-core-only loader restriction when a valid pocketboot-owned parking contract
already exists. It does not adopt lk2nd's resident memory, enable the shim/kernel
patch, prove CPU parking, or bypass either ownership or kernel safety checks.

## Hardware acceptance

CI compilation and host parser tests cannot establish these results:

1. The exact image reaches pocketboot with display, touch, USB and eMMC.
2. The actual postmarketOS boot filesystem is discovered; its
   [extlinux config](extlinux.md) resolves to the intended kernel, initrd and DTB.
3. After the CPU-handoff blocker is addressed, kexec load succeeds and the target
   kernel reaches userspace with working SMP and devices.

Keep observations for these stages separate. Collect existing logs before
rebooting or replacing a working image; coordinate device-changing tests with
the device owner.

[port]: https://github.com/pem120/pocketboot/commit/d3f4602418a03af4266128feb0f95f028e45fa1b
[kernel]: https://github.com/pem120/linux/tree/45add32603ee4aa28979ad6ec70c10b14af4ac29
