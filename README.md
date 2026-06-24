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
cargo xtask initrd qcom/sdm845-oneplus-fajita
```

This builds `target/aarch64-unknown-linux-musl/release/pocketboot` and writes a
device initrd-ready `newc` archive to
`target/initrd/<vendor>/<device>/pocketboot-initrd.cpio` with the binary
installed as `/init`.

When enabled by device config, the initrd also includes BusyBox built from the
official 1.38.0 source release with applet symlinks installed under `/bin`,
`/sbin`, `/usr/bin` and `/usr/sbin`. The known musl targets use the matching
`*-linux-musl*-gcc` wrapper by default; set `BUSYBOX_CC` or
`BUSYBOX_CROSS_COMPILE` to use another static libc-capable target C toolchain.
BusyBox is cached under `target/busybox`; use `cargo xtask busybox` to build it
without creating an initrd.

To build all configured pocketboot artifacts for a supported device, pass the
canonical arm64 DTB path without the `.dtb` suffix:

```sh
cargo xtask build qcom/msm8916-samsung-a5u-eur
cargo xtask build exynos/exynos7870-j7xelte
```

Pass `--kernel PATH` to use an existing kernel tree instead of materializing the
configured `[kernel-source]` tree:

```sh
cargo xtask build --kernel ./linux qcom/msm8916-samsung-a5u-eur
```

The kernel build uses `target/kernel/<vendor>/<device>` as `O=`, embeds a
per-device pocketboot initramfs from `target/initrd/<vendor>/<device>`, and builds
the configured kernel image target plus the inferred DTB. This also leaves normal
intermediate image prerequisites in the build output. Kernel configuration is assembled from
`configs/pocketboot.toml`, `configs/soc/<vendor>/<soc>.toml` and
`configs/device/<vendor>/<device>.toml`.

Pinned kernel sources can be described with an inherited `[kernel-source]` table
containing `remote` and `sha` fields. `cargo xtask kernel-build prepare-source
<vendor/device>` materializes that source under `target/kernel/src`.

The low-level build phases are composable:

```sh
cargo xtask kernel-build prepare-source qcom/msm8916-samsung-a5u-eur
cargo xtask kernel-build config qcom/msm8916-samsung-a5u-eur
cargo xtask kernel-build modules qcom/msm8916-samsung-a5u-eur
cargo xtask initrd qcom/msm8916-samsung-a5u-eur
cargo xtask kernel-build image qcom/msm8916-samsung-a5u-eur
```

Once the kernel is built, package those artifacts as an Android boot image:

```sh
cargo xtask bootimg qcom/sdm670-google-sargo
cargo xtask bootimg qcom/msm8916-samsung-a5u-eur
cargo xtask bootimg exynos/exynos7870-j7xelte
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
