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

## A5 boot-image size

Samsung A5U uses `kernel_image = "Image.gz"` in its `[bootimg]` configuration. The
builder first places Pocketpreboot and the uncompressed kernel, including the
kernel's complete runtime footprint, then gzip-compresses that combined
payload before writing the Android image. This keeps the boot image within the
A5's 13 MiB partition without changing the preboot placement contract. The
preboot payload is always the uncompressed `Image`; `kernel_image = "Image"`
keeps the completed envelope uncompressed, while `"Image.gz"` compresses it.
Without preboot, `kernel_image` continues to select the existing kernel artifact.
Compression uses no filename or timestamp metadata.

The September 21 A5 trial RAM-booted an equivalent compressed resident of
about 5 MiB and repeated kexec with four CPUs online. That resident disabled
display probing to isolate a separate blank-panel/USB failure; compression
alone does not establish that the full A5 display path is ready for installation.

The first installed trial stopped in Pocketpreboot with `bad payload`, before
Linux started. The A5 configuration now uses the MSM8916 ARM64 load address
`0x80080000` consistently in the Android header and preboot padding, rather
than the ARM32 address `0x80008000`. The corrected compressed resident booted
from internal storage through Samsung ABL, brought all four CPUs online,
provided a UART shell, and discovered the SD boot entry. Payload failures
also report the running preboot address, expected kernel address, and observed
magic on UART.
