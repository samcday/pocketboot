# Optional Pocketpreboot RAM trace

The `debug-ram-trace` feature records primary entry, exceptions and MSM8916
console text in the shim's own RAM footprint. It adds no persistent writes and
does not change the resident spin-table ABI. It is off in normal builds.
With the feature disabled, the A5 preboot binary remains byte-for-byte equal
to the previously tested DB410c/A5 artifact (`5f87eb4bd5ee2b9591c26f645cb132adca48f9cc8fd4166d25681fbc642de561`).

Build a separate lab shim and preserve its ELF before another build replaces it:

```sh
CARGO_PROFILE_RELEASE_STRIP=none cargo xtask preboot qcom/msm8916-samsung-a5u-eur \
  --features debug-ram-trace --output /tmp/preboot-trace.bin
cp target/aarch64-unknown-none/release/pocketpreboot /tmp/preboot-trace.elf
llvm-nm -n /tmp/preboot-trace.elf | rg pocketpreboot_ram_trace
```

The physical trace address is the ELF symbol value plus the actual shim load
address. Never assume it stays constant across builds. The September 11 A5
corrected packed probe has symbol offset `0x8c000`, load address `0x80008000`,
and physical trace address `0x80094000`. Before the packaging fix, lk2nd honored
the shim's zero ARM64 text offset and actually loaded it at `0x80000000`;
that earlier trace was at `0x8008c000`, not the initially assumed address.
The image header includes all 12 KiB of trace storage.
Packaging still places the inner kernel at the next 2 MiB boundary after the
shim's full runtime footprint.

After a failed boot and a restart into lk2nd, collect only from the explicitly
identified board:

```sh
python3 tools/preboot_ram_trace.py --serial cd0ee037 \
  --compatible samsung,a5u-eur --address 0x80094000 \
  --output target/a5-trace-read-NEW
```

This sends only identity getvars and `oem debug readq` requests. It saves the
raw bytes and command log before decoding. A raw dump can also be decoded
offline with `--input PATH --output NEW_DIRECTORY`.

For a short log, `--page-bytes 256` reads only the first 256 bytes of each page,
reducing fastboot traffic. The saved binary concatenates those three actual
prefixes; the decoder derives their equal length and rejects text extending
beyond the captured bytes. It does not invent unread trailing data. An A5
full-page collection stalled partway through; a host USB connection reset
recovered lk2nd without rebooting the phone, then the compact capture succeeded.

## Format and limits

Three identical 4096-byte pages contain `PBTRACE1` at offset zero. Following
little-endian u64 fields are stage, text length, entry FDT address, CurrentEL,
entry SCTLR, ESR, ELR, FAR, and an FNV-1a checksum of the text. Text starts at
offset 128, with capacity 3968 bytes; excess output is dropped. Entry assembly
initializes the buffer before enabling FP/SIMD or entering Rust. It remains
outside the BSS clearing range. The exception assembly records fault registers
before attempting the Rust/UART report.

Stages are 1 (assembly entry), 2 (relocations and BSS ready), 3 (Rust main),
4 (about to initialize the DT-selected UART), 5 (UART initialization returned),
`0xe001` (exception), and `0x1000` (about to enter the kernel). Stages are coarse;
console text narrows progress further. This buffer is not a kernel reservation
and must not be assumed to survive successful kernel memory allocation.

The A5's software reboot preserved most bits in reserved-RAM control words but
cleared some, so raw retention is unreliable. That observation does not prove
corruption during normal operation. The decoder tries individual copies,
bitwise majority and bitwise OR, accepting console text only if magic, bounds
and its checksum validate. It rejects differing checksum-valid texts. Header
fields have no checksum: disagreements are explicitly recorded, and recovered
register values must be interpreted accordingly. Complete loss, correlated
corruption or a reset that overwrites the region still requires another means
of observation, such as UART.

Six host tests cover intact/compact logs, independent bit loss, unrecoverable
common corruption, invalid length and header disagreement. QEMU execution of the exact
instrumented shim produced three identical copies of its expected zero-SMPEN
failure log; decoding validated the checksum. The ordinary resident and PIE
startup QEMU checks also pass. The A5 hardware trace was subsequently recovered
using bitwise OR, and its full 66-byte console text matched the checksum. It
reported `pocketpreboot: bad payload`, locating the header/padding mismatch
before secondary startup. Header-copy disagreements remain recorded separately.
