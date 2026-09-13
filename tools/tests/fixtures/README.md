# A5U ramoops regression fixture

`a5u-controlled-panic.bin.gz` is a 23,203-byte compressed fixture derived from
the [A5 controlled SysRq panic capture](https://github.com/samcday/pocketboot/blob/bf5d70026af96f97a87c0d300b4bf91e0a0b22e5/docs/evidence/a5u-smp-2026-09-11/ramoops/controlled-panic/ramoops.bin).
The original 512 KiB capture remains in the experiment archive.

The fixture preserves every region header, every occupied 128-byte data block
(including the last block's padding), their original 64-byte parity blocks,
and every header's parity. Unused data and unused parity blocks are zeroed.
Every decoded record and its correction counts were compared with the original.
No log text or active ECC symbols were regenerated. Gzip uses level 9 and mtime 0.

SHA-256:

- Original capture: `650623fddb12e21d93216a37385c0f287b4fbf548687806a8e046c8ef00ac14b`
- Decompressed fixture: `dcea9aa907361a4ac83be7829ffdb526c7b91a4c985f4cf983aa30080a1d722e`

`test_ramoops_decode.py` expands the fixture to the original 512 KiB layout
and exercises compressed kernel logs, header/data/parity ECC corrections,
uncorrectable errors and truncated captures. See
[validation prerequisites](../../../docs/msm8916-validation.md#reproduce-the-checks)
for the pinned Linux codec source and test command.
