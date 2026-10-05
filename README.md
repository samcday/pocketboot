# pocketboot

[LinuxBoot][] for pocket computers.

pocketboot builds a small Linux kernel plus Rust `/init` that fits in an
Android boot image, brings up enough hardware to find a real OS, then kexecs
into it.

## Quickstart

There will be prebuilt binaries available.

To build yourself:

```sh
rustup target add aarch64-unknown-linux-musl

# Build a boot.img for a supported device
cargo xtask build
```

(You will need a bunch of undocumented cross-compile toolchain deps, sorry about that)

### Download cache

BusyBox archives are shared across checkouts under
`$XDG_CACHE_HOME/pocketboot/downloads/` (normally `$HOME/.cache/pocketboot/downloads/`).
`cached-path` manages downloads and locking; SHA-256 verification runs before use.
Valid cache hits make no HTTP requests. A checksum mismatch triggers one refetch
before failing. Unpacked sources and build outputs stay under `target/` (or
`CARGO_TARGET_DIR`), so `cargo clean` preserves the download cache.

Dev docs will be forthcoming. For now, ask your favourite clanker for an explanation.

[LinuxBoot]: https://www.linuxboot.org/
