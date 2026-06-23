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

By default, the initrd also includes BusyBox built from the official 1.38.0
source release with applet symlinks installed under `/bin`, `/sbin`, `/usr/bin`
and `/usr/sbin`. The default target expects `aarch64-linux-musl-gcc`; set
`BUSYBOX_CC` or `BUSYBOX_CROSS_COMPILE` to use another static libc-capable target
C toolchain, or pass `--no-busybox` to build only the Rust `/init`. BusyBox is
cached under `target/busybox`; use `cargo xtask busybox` to build it without
creating an initrd.

If local cross-toolchains are getting in the way, pass `--podman` to run the
toolchain-heavy steps in the same image CI uses:

```sh
cargo xtask cpio --podman
cargo xtask busybox --podman
```

`xtask` first tries the published CI image
`ghcr.io/samcday/pocketboot-ci:df-<sha256(.github/Dockerfile)>`. If that exact
image is unavailable, it builds `localhost/pocketboot-ci:df-<hash>` locally from
`.github/Dockerfile`. Set `POCKETBOOT_PODMAN_IMAGE` to force a specific image, or
`POCKETBOOT_PODMAN_CACHE_FROM` to pass one or more Buildah cache repositories to
`podman build --cache-from`.

To build a pocketboot kernel for a supported device, pass the canonical arm64
DTB path without the `.dtb` suffix and a kernel tree:

```sh
cargo xtask kernel qcom/msm8916-samsung-a5u-eur ./linux
cargo xtask kernel exynos/exynos7870-j7xelte ~/tmp/linux-pocketboot-exynos7870
cargo xtask kernel --podman qcom/msm8916-samsung-a5u-eur
```

The kernel build uses `target/kernel/<vendor>/<device>` as `O=`, embeds a
per-device pocketboot initramfs from `target/cpio/<vendor>/<device>`, and builds
`Image.gz` plus the inferred DTB by default. This also leaves the uncompressed
`Image` prerequisite in the build output. Pass `--initrd PATH` to embed an
existing cpio archive. Kernel configuration is assembled from
`configs/pocketboot.toml`, `configs/soc/<vendor>/<soc>.toml` and
`configs/device/<vendor>/<device>.toml`.

Pinned kernel sources can be described with an inherited `[kernel-source]` table
containing `remote` and `sha` fields. `cargo xtask kernel-src <vendor/device>`
materializes that source under `target/kernel/src`. When `KERNEL_TREE` is omitted,
`cargo xtask kernel` fetches or updates the configured source automatically before
building, including when `--podman` is used.

Once the kernel is built, package those artifacts as an Android boot image:

```sh
cargo xtask bootimg qcom/sdm670-google-sargo
cargo xtask bootimg qcom/msm8916-samsung-a5u-eur
cargo xtask bootimg exynos/exynos7870-j7xelte
cargo xtask bootimg --podman exynos/exynos7870-j7xelte
```

Boot image packaging is device-specific and configured by the `[bootimg]` table
in `configs/device/<vendor>/<device>.toml`. The command requires an existing
kernel build and writes `target/kernel/<vendor>/<device>/boot.img` by default.
`kernel_image` selects the artifact under `arch/arm64/boot` to package and
defaults to `Image.gz`. Android boot header v2 DTB sections, legacy QCDT vendor
DT payloads, Samsung DTBH vendor DT payloads and legacy `Image.gz+dtb` appended
DTB payloads are supported.

To boot under QEMU, run:

```sh
cargo xtask qemu ./linux
```

This builds the `qemu/aarch64-virt` kernel target with its per-device embedded
initramfs. The guest starts a USB/IP vUDC server and forwards that server to
`127.0.0.1:3240` on the host. In another shell, attach the guest USB gadget to
the host before using `fastboot` or `adb`:

```sh
sudo modprobe vhci-hcd
sudo usbip attach -r 127.0.0.1 -d usbip-vudc.0
fastboot -i 0x1d6b devices
adb devices
```

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
