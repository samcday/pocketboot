# Evidence inventory

The checked-in evidence is deliberately small and redacted. The raw captures
included a device serial, provisioned network addresses, PARTUUIDs, hardware
identifiers, and unrelated Android runtime output, so they do not belong in a
potentially public Git repository.

Raw files verified in `/tmp` while preparing this handover:

| Capture | Raw SHA-256 | Bytes |
| --- | --- | ---: |
| Final TTL (`pocketboot-crosshatch-live-20260714.log`) | `73243c4c0558c0cf4a50adfdb7f9071e857b9ccb245c9ccde2f1d79bd628ad9d` | 498,221 |
| Final pre-handoff dmesg (`pocketboot-precontinue-dmesg-20260714.txt`) | `3c1e5022926fd40b1d74df665f17dd2a5b5cf4399d6aa6158c7ed70f43645fd2` | 38,830 |
| Final Pocketboot cmdline (`pocketboot-precontinue-cmdline-20260714.txt`) | `ed135ab84b8d17c7204205db18e41a3915d77057b74d3dbb143aad56853bd237` | 1,905 |
| First-attempt UART (`crosshatch-stockos-wip-attempt-20260713.uart.log`) | `e26dfd5b1fbb677bd31884558a4777f66e390d0cf1387771ce82f7cb0ac74529` | 211,094 |

Two additional first-attempt hashes were recorded during the live session, but
the files were no longer present when this handover was written:

| Capture | Recorded raw SHA-256 | Bytes |
| --- | --- | ---: |
| First-attempt dmesg | `32d609e58e3d19cf59da5a049e0652c939a8fdd5effc46fec6653c66fa1834d2` | 40,893 |
| First-attempt cmdline | `8f5419a9d30864cc1a9b7e379327841ab3cb57d794c3202ce568f5b56f140027` | 1,905 |

`/tmp` is ephemeral. These hashes establish provenance only if the originals
are separately retained. The hashes in `SHA256SUMS` apply to the curated files,
not to the raw captures above.

Curated excerpts:

- [2026-07-13-lz4-rejection.txt](2026-07-13-lz4-rejection.txt)
- [2026-07-14-kexec-watchdog-reset.txt](2026-07-14-kexec-watchdog-reset.txt)
