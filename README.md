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

### Kernel sources

Builds prefer an explicit `KERNEL_TREE` argument, then an attached Delta kernel
worktree, then the device's configured remote and pinned revision. Explicit and
attached kernels are used as-is, without applying the configured patch series.

Configured kernels share a Git cache under `$XDG_CACHE_HOME/pocketboot/kernels/`
(normally `$HOME/.cache/pocketboot/kernels/` on Linux), keyed by remote URL. A
missing revision is fetched shallowly once; cached revisions need no upstream
connection, including from new checkouts or after `cargo clean`. Cache access is
locked across processes, and fetched revisions are retained across pin changes.

Mutable sources still live under `target/kernel/src/` (or `CARGO_TARGET_DIR`).
Each checkout gets a self-contained shallow repository from the local cache,
without sharing source edits, patches, or indexes. This avoids repeated upstream
downloads, not the disk space for each checkout's source snapshot. The sources
also work after the cache is removed or the checkout is moved. Existing source
trees are preserved; a pin change refuses to overwrite uncommitted work. The
legacy top-level `kernel` repository, when present, is still used to supply
worktrees instead of the host cache.

### Download cache

BusyBox archives are shared across checkouts under
`$XDG_CACHE_HOME/pocketboot/downloads/` (normally `$HOME/.cache/pocketboot/downloads/`).
`cached-path` manages downloads and locking; SHA-256 verification runs before use.
Valid cache hits make no HTTP requests. A checksum mismatch triggers one refetch
before failing. Unpacked sources and build outputs stay under `target/` (or
`CARGO_TARGET_DIR`), so `cargo clean` preserves the download cache.

Dev docs will be forthcoming. For now, ask your favourite clanker for an explanation.

[LinuxBoot]: https://www.linuxboot.org/
