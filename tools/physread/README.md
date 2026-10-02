# physread

Dump a physical address range through `/dev/mem`. The exact bytes go to
stdout, diagnostics and errors only to stderr. It never writes memory, never
reads outside the request, and has no default address.

## Build

From the repository root:

```sh
cargo build --release --locked --manifest-path tools/physread/Cargo.toml
```

Static ARMv7 musl binary for the device (`armv7-unknown-linux-musleabihf`
plus `rust-lld`; the repository's `.cargo/config.toml` already selects the
linker):

```sh
cargo build --release --locked --manifest-path tools/physread/Cargo.toml \
    --target armv7-unknown-linux-musleabihf
```

Unit tests are host-only and never open `/dev/mem`:

```sh
cargo test --locked --manifest-path tools/physread/Cargo.toml
```

## Run

```sh
physread ADDRESS LENGTH > dump.bin
```

`ADDRESS` and `LENGTH` are byte counts: hexadecimal with a `0x`/`0X` prefix, or
decimal. Both must be multiples of 4 because the range is read as aligned
32-bit words; `LENGTH` must be nonzero. The mapping covers only the page(s)
that contain the request, each word is read once, and output preserves memory
byte order. Only use this access width where the hardware permits it.

`/dev/mem` requires `CONFIG_DEVMEM=y`, device-file read permission and
`CAP_SYS_RAWIO` (normally root). Kernel lockdown and architecture-specific
`CONFIG_STRICT_DEVMEM`/`CONFIG_IO_STRICT_DEVMEM` policy can further restrict
access. Strict devmem on ARM can permit unclaimed non-RAM while denying RAM;
do not disable it merely because the target is ROM. Kernel permission does
not bypass a SoC bus firewall.

A range the kernel refuses fails in `mmap` on stderr with exit status 1;
a bad command line exits 2. Output can be partial after an error or fault:
check its length and the command's status before treating it as a dump.

## Caveats

- A read-only *mapping* is a kernel permission, not hardware protection.
  Reading a physical range that no device answers, or a device that is not
  clocked, can cause `SIGBUS`, a kernel fault, a hang or a device reset.
  `physread` installs no signal handler and cannot make such a read safe.
- Reads are single 32-bit words taken in order, so a value can change, tear or
  disagree with neighbours while the dump runs; nothing here synchronises with
  other bus masters or invalidates caches.
