# TB-X304F bounded probe initramfs

**Lab-only: this wrapper deliberately panics a responsive kernel after a
deadline. Never install it persistently or use it as a normal boot image.**
Verify the intended tablet, a recovery route and the supported bounded RAM-log
export before trying it. It is not a watchdog for a hung kernel.

Normal `cargo xtask build qcom/msm8917-lenovo-tbx304x` does not use this
wrapper. The [normal image](tb-x304f-bringup.md#verified-normal-usb-and-emmc-boot)
now starts USB and eMMC without it.

## Build a separate diagnostic archive

From the repository root, after creating the ordinary target initramfs:

```sh
(
    set -eu
    umask 077
    mkdir -p target/tb-x304f-lab
    OUT="$(mktemp -d target/tb-x304f-lab/probe.XXXXXX)"
    python3 tools/tb_x304f_probe_cpio.py \
        --input target/cpio/qcom/msm8917-lenovo-tbx304x/pocketboot-initrd.cpio \
        --output "$OUT/probe.cpio"
    cargo xtask kernel qcom/msm8917-lenovo-tbx304x --initrd "$OUT/probe.cpio"
    cargo xtask bootimg qcom/msm8917-lenovo-tbx304x
    cp target/kernel/qcom/msm8917-lenovo-tbx304x/boot.img "$OUT/probe.img"
    printf 'Private diagnostic image: %s/probe.img\n' "$OUT"
)
```

The builder preserves the complete input archive and saves the original
AArch64 ELF as `/pocketboot.real-init` in an appended newc overlay. It rejects
malformed archives, unsuitable required members, already-wrapped input and
existing output paths. It does not extract files, flash a device or edit the
normal initramfs.

The kernel commands above do use the usual generated kernel/image directory.
Keep the separately named diagnostic copy, and run the normal full build again
before reusing its usual `boot.img` as an ordinary image.

## Explicit activation and recovery

Keep the target's normal command line and add the standalone token
`pocketboot.probe` to the separately staged diagnostic image. Retain exactly
`panic=-1` and bare `lk2nd.pass-ramoops`; never add `=zap`. Record the complete
effective command line and image hash, and RAM-boot only through the verified
lk2nd path. This guide does not authorize erasing another tablet's boot image.

The wrapper:

1. Refuses non-PID1 execution, starts diagnostic children, then execs the
   original PocketBoot ELF as PID1.
2. After about three seconds, requires exact `lenovo,tbx304x` compatibility,
   `pocketboot.probe` and an effective `panic=-1` before any `--` init arguments.
3. Arms an independent 30-second deadline, then records bounded snapshots of
   deferred devices, partitions, UDC state, gadget binding and software driver
   state into `/dev/kmsg`, prefixed `TBX304F-PROBE:`.
4. Mounts debugfs read-only if needed. It does not read arbitrary register
   files or block contents. Snapshot reads cannot delay the independent timer.
5. Unless cancelled, logs `intentional diagnostic panic` and writes `c` to the
   existing SysRq trigger. Do not misclassify this expected panic as a driver
   crash.

After independently verifying a usable PocketBoot USB session, the intended
cancellation command is:

```sh
: "${SERIAL:?Set SERIAL to the explicitly verified tablet}"
fastboot -s "$SERIAL" oem 'shell:touch /run/tbx304f-probe.keep'
```

It creates only a tmpfs marker. The recorded diagnostic trials deliberately
allowed the deadline to fire so both snapshots were retained; cancellation
itself has not been hardware-tested. Retrieve and preserve the bounded raw
RAM window before another kernel boot. See the
[recovery procedure](tb-x304f-bringup.md#automatic-panic-recovery-with-an-empty-boot-partition).

## Optional controlled interventions

These are off unless explicitly requested in addition to the three guards:

- `pocketboot.probe-role-device`: after the first snapshot, request device role
  through the `ci_hdrc.0` role switch only if its canonical device parent
  matches that UDC. The diagnostic DT must expose `usb-role-switch`.
- `pocketboot.probe-reprobe-mmc`: after the second snapshot, retry only the
  unbound `7824900.mmc` device, and only after its RPM power-domain driver is
  bound. It never unbinds a live controller or writes partition data.

These controls isolated the original faults; they are not substitutes for
normal startup fixes. The current normal profile already selects USB device
role and waits for RPM suppliers.

## Verification

The host-only suite has 15 tests covering archive preservation, malformed and
truncated input, AArch64 executable checks, overwrite refusal, shell syntax
and ordinary-process refusal. ShellCheck passes. The actual packaged BusyBox
applets and shell parsing were checked under `qemu-aarch64`.

On the tablet, the opt-in wrapper preserved PID1, logged the expected
snapshots, deliberately panicked, returned to stock fastboot and retained the
log. The optional role request enabled verified PocketBoot USB; the later
eMMC re-probe found all 48 GPT partitions. No additional partition writes
were part of those trials.
