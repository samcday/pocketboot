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

### Compiler caches

Compiler caches reuse compilation results between builds and worktrees on the
same machine. Each worktree must keep its own build outputs. Do not point different
Delta threads at one shared `CARGO_TARGET_DIR`.

If `sccache` is on `PATH`, enable it in the shell that runs your builds:

```sh
export SCCACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/sccache"
export RUSTC_WRAPPER=sccache
```

`RUSTC_WRAPPER` covers Cargo's Rust compilations, including nested builds for
`/init` and preboot. `SCCACHE_DIR` also opts the kernel and BusyBox C builds into
sccache. Use your normal `cargo xtask build <vendor/device>` command afterward.
These exports affect the current shell, not other terminals or Delta agents.

PocketBoot does not configure remote cache storage. Existing settings in your
cache tools still apply. Use `sccache --show-stats` to see the active cache location.

If you prefer ccache for C compilation, set `CCACHE_DIR` instead:

```sh
export CCACHE_DIR="$(ccache --get-config cache_dir)"
```

When both cache directories are set, C builds use ccache. Rust still uses
`RUSTC_WRAPPER`, so ccache for C and sccache for Rust can run together.
If neither cache directory is set, PocketBoot does not add C compiler wrappers.
Unset a cache-directory variable to opt out. An empty value still counts as set.
Existing compiler wrappers on `PATH` still work.

The C wrappers preserve compiler selection. `BUSYBOX_CC` remains one executable
name or path, not a command such as `sccache gcc`. Explicit compiler paths bypass
the generated `PATH` wrappers. Compiler scripts are not themselves cached as
compiler binaries. A script can still call a wrapped real compiler from `PATH`.

Prefer the default `target/` directory, or the same relative `CARGO_TARGET_DIR`
in each worktree. sccache hashes `CARGO_*` environment values, so different
absolute target-directory values can prevent Rust cache hits.

Cache hits are not guaranteed for every build step. Rust dependencies can be
reused across worktrees, but some crate types and incremental compilations are
not cacheable. Build scripts, linking, archive creation, and boot-image assembly
still run as needed. Moving the kernel or BusyBox source directory can also
prevent compiler-cache hits.

Do not enable blanket `CCACHE_BASEDIR` or `SCCACHE_BASEDIRS` rewriting for this
workflow. Rewritten paths can change compiled file names or restore dependency
files that point into another worktree. Keep compiler checks and normal cache
correctness settings enabled.

To measure reuse, compare fresh worktrees with the same device, configuration,
toolchain, and build metadata. Record each build's time and cache statistics.
An unchanged incremental build measures build-system reuse, not compiler-cache
reuse. Do not clear a shared cache or reset its statistics for the comparison.

Dev docs will be forthcoming. For now, ask your favourite clanker for an explanation.

[LinuxBoot]: https://www.linuxboot.org/
