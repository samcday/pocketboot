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

Dev docs will be forthcoming. For now, ask your favourite clanker for an explanation.

[LinuxBoot]: https://www.linuxboot.org/
