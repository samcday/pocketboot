# Recovering from a failed boot

## A rejected load should leave recovery running

When a UI-selected or default boot attempt returns, pocketboot reports the
error and resets the menu's busy state so another entry can be selected.
An unexpectedly returning successful boot action is also treated as a failure:
a successful handoff does not return to userspace.

A UI load failure leaves the existing USB service running. A returned fastboot
action, failed default boot after `fastboot continue`, or USB service failure
restarts fastboot and ADB after a one-second asynchronous delay. The host may
need to wait for USB re-enumeration. This does not rescue an attempt that hangs,
panics, or has already torn down the running kernel.

## Logging

Set `pocketboot.log=info` on the **pocketboot kernel's** command line for boot
discovery, payload selection and load diagnostics. `debug` adds detail;
`trace` also includes high-volume UI timing. The default is `warn`. This is
separate from Linux's `loglevel` and from the command line in an extlinux entry,
which belongs to the destination OS.

Fatal startup errors and Rust panics make a best-effort direct `/dev/kmsg`
write even with `pocketboot.log=off`. Each emergency record is bounded to
512 bytes; longer messages retain a prefix marked ` [truncated]`. The console
fallback is retained. An unavailable kernel log or a pre-Linux failure cannot
be fixed by this userspace hook.

## Retrieve previous-boot pstore records

At startup, before UI, getty and USB services, pocketboot mounts pstore
read-only at `/sys/fs/pstore` and copies regular records into an immutable
snapshot at `/run/pocketboot/pstore`. Filenames and bytes are preserved; no
source record is erased. A failed copy does not publish a partial snapshot,
and an existing snapshot is never replaced.

Capture is deliberately all-or-nothing for the exposed record set. Here
"best-effort" means a capture error cannot prevent recovery startup; it does
not mean publishing an incomplete snapshot. If capture fails, the originals
remain under `/sys/fs/pstore` and individual records can still be pulled from
there. Compressed `.enc.z` records are copied as opaque bytes, not decompressed
or discarded by pocketboot.

Once ADB is available, use the serial of the intended device:

```sh
adb -s <serial> pull /run/pocketboot/pstore ./previous-boot-pstore
```

There is no snapshot directory if no regular records were available. Capture
errors are logged and do not stop recovery startup. The snapshot lives in
volatile `/run`: retrieve it before another reboot. Do not delete the source
pstore files as part of collection.

Requirements and limits:

- The kernel needs pstore and the relevant backend. Ramoops additionally needs
  a correctly reserved memory region and layout; a console record needs
  `CONFIG_PSTORE_CONSOLE` and a nonzero `console-size`.
- The failed kernel, firmware reboot path and recovery kernel must preserve
  and agree on that memory/layout. This snapshot feature **does not yet graft
  the live ramoops reservation into a destination OS DTB**.
- Atomic publication uses `renameat2(RENAME_NOREPLACE)`: Linux 3.15 provides
  the syscall, with tmpfs support since 3.17 (ramfs since 4.9). Pocketboot
  normally mounts `/run` as tmpfs. An unsupported publication operation leaves
  the originals intact and reports capture failure.
- `panic=1` requests reboot one second after a kernel panic; a negative value
  requests immediate reboot. Neither recovers an ordinary hard lockup or
  guarantees that the reboot firmware returns to pocketboot.
- Do not clear retained RAM on the recovery path. On Ferrari, the current
  configuration uses `lk2nd.pass-ramoops` **without `=zap`**. This is not a
  universal setting: the [A5 ECC layout](a5u-ramoops.md) must not use that
  lk2nd fixup because it replaces the configured ECC size.

Pstore copying, coordinator retries and menu reset have automated coverage.
Real USB re-enumeration and previous-boot ADB retrieval still need device
validation. Record the pocketboot commit, CI run, image SHA-256 and exact
command lines with any hardware result.
