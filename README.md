# pocketboot

[LinuxBoot][] for pocket computers.

## Why?

Most Android devices have a fused bootloader that does the bare-minimum to
prepare the device to boot an Android kernel.

Mainline kernels, and the distros that make use of them, are different
enough from Android that a "shim" to bridge the divide between the two is
needed. Projects like [lk2nd][] and [u-boot][] have been traditionally used
for this purpose.

## How?

pocketboot is similar to LinuxBoot + [u-root][]: it's made up of two main
components:

 * A mainline kernel, built as minimally as possible for this pre-boot
   environment.
 * A minimal userspace to handle finding the real distro kernel, DTB and
   initrd, and chaining to that (via kexec).

On many devices, we're working in an extremely constrained environment. The
`boot` partition on msm8916 devices, for example, is 10MiB. We need to be
able to fit the kernel, FDT, and userspace in this tiny partition.

The userspace is implemented as a monolithic static Rust binary. This gives
us a rich environment to build a streamlined and user-friendly pre-boot
environment with all the bells and whistles.

## Building

The current beachhead is a tiny `/init` binary that mounts `/proc`, `/sys`,
`/dev` and `/run`, prints the block devices visible in `/sys/block`, and exits
so PID 1 death trips the kernel panic/reboot path.

```sh
cargo xtask cpio
```

This builds `target/aarch64-unknown-linux-musl/release/pocketboot` and writes an
initrd-ready `newc` archive to `target/pocketboot-initrd.cpio` with the binary
installed as `/init`.

To build a pocketboot kernel for a supported device, pass the canonical arm64
DTB path without the `.dtb` suffix and a kernel tree:

```sh
cargo xtask kernel qcom/msm8916-samsung-a5u-eur ./linux
```

The kernel build uses `target/kernel/<vendor>/<device>` as `O=`, embeds a fresh
pocketboot initramfs, and builds `Image.gz` plus the inferred DTB.

Once the kernel is built, package those artifacts as an Android boot image:

```sh
cargo xtask bootimg qcom/sdm670-google-sargo
cargo xtask bootimg qcom/msm8916-samsung-a5u-eur
```

Boot image packaging is device-specific and configured by
`configs/bootimg/<vendor>/<device>.toml`. The command requires an existing
kernel build and writes `target/kernel/<vendor>/<device>/boot.img` by default.
Both Android boot header v2 DTB sections and legacy QCDT vendor DT payloads are
supported.

Planned features:

 * Touch-enabled boot menu
 * A `fastbootd` server that supports `boot`/`flash`,`fetch` and useful
   recovery/diagnostic features (e.g `oem shell:'ls /dev'`).
 * (Maybe) a minimal `adbd` server for an even more ergonomic `adb shell`
   recovery environment.
 * Rich downstream boot support: extlinux, BLS (type #1 *and* #2), etc.

[LinuxBoot]: https://www.linuxboot.org/
[lk2nd]: https://github.com/msm8916-mainline/lk2nd
[u-boot]: https://u-boot.org/
[u-root]: https://github.com/u-root/u-root
