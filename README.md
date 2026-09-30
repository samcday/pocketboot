# pocketboot

[LinuxBoot][] for pocket computers.

pocketboot builds a small Linux kernel plus Rust `/init` that fits in an
Android boot image, brings up enough hardware to find a real OS, then kexecs
into it.

pocketboot autoboots the first bootable entry it finds. For its boot menu and fastboot,
press and hold volume-down once the pocketboot kernel starts (holding it from power-on
usually enters the previous bootloader's fastboot instead), or add `pocketboot.menu` to
the kernel command line. It falls back to the menu when nothing is bootable or a boot fails.

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
