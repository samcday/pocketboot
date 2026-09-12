# DB410c bring-up baseline, 2026-09-10

This records the initial access check. See the [September 11 experiment log](msm8916-smp-experiments-2026-09-11.md)
for the subsequent four-core baseline and raw SCM/ACC work. Live testing found
that the running U-Boot uses `0x93400000` as its download buffer, and that the
host fastboot client wraps a FIT passed to `boot` in an Android image. Use
`fastboot stage` followed by UART `bootm 93400000` for these lab FITs.

The board reaches U-Boot and fastboot. Host USB queries and live UART receive
were verified. No reset, kernel boot, flash, erase, or persistent bootloader
environment write was performed in these checks.

## Boot evidence supplied by Sam

The power-on UART trace reports:

```text
QC_IMAGE_VERSION_STRING=BOOT.BF.3.0-00288
IMAGE_VARIANT_STRING=HAAAANAZA
OEM_IMAGE_VERSION_STRING=CRM
CDT version:3,Platform ID:24,Major ID:1,Minor ID:0,Subtype:0
DDR Frequency, 400 MHz
U-Boot 2026.07-rc4-00430-gb89b7bb08426-dirty (Jun 12 2026 - 23:01:13 +1000)
Qualcomm-DragonBoard 410C
DRAM:  1 GiB
Core:  152 devices, 24 uclasses, devicetree: separate
MMC:   mmc@7824900: 0, mmc@7864900: 1
Loading Environment from MMC... ** Read outside partition 2
Reading from MMC(0)... *** Warning - bad CRC, using default environment
In:    serial@78b0000
Out:   serial@78b0000
Err:   serial@78b0000
U-Boot loaded from XBL
Net:   No ethernet found.
```

This proves bootloader startup and DRAM detection, not Linux SMP or kexec.
The persisted environment was not loaded; U-Boot used defaults. Investigate
that separately if persistent environment storage is needed. Do not use
`saveenv` merely to suppress the warning.

## Live access checks

- Host UART: `/dev/ttyUSB0`; FTDI FT232R adapter with a stable link under
  `/dev/serial/by-id/`. The execution sandbox hides the host device nodes;
  approved host execution can access them.
- Fastboot `product`: `sbc`.
- Fastboot `version-bootloader` matches the supplied UART banner.
- Fastboot `max-download-size`: `0x07000000` (112 MiB).
- No process held the UART when checked. Its host settings had returned to
  9600 baud/canonical mode; configured 115200 baud, 8N1, raw mode, no flow
  control for capture. This does not identify the cause of earlier UART trouble.
- A benign unrecognized fastboot command produced a 41-byte U-Boot diagnostic
  on UART, verifying live receive while fastboot remained available.
  This was not a successful OEM command invocation.

Generated probe logs are under
`target/db410c-lab/access-probe-2026-09-10/{uart,fastboot}.log`.
UART transmission, external hard-reset recovery, and autonomous recovery
from a hung kernel have not yet been demonstrated.

Sam subsequently confirmed that he can manually power-cycle the nearby board
when needed and authorized Magic SysRq for kernel recovery. A relay is an
optional improvement, not a prerequisite for starting the baseline tests.
The shared Pocketboot configuration already enables `MAGIC_SYSRQ`,
`MAGIC_SYSRQ_SERIAL`, and the reboot command mask (`0x80`).

## Ordinary PSCI baseline image

Pocketboot already has `configs/device/qcom/apq8016-sbc.toml`, inheriting
the MSM8916 kernel source/config. No confidently suitable DB410c image was
found in the inspected main and relevant archived build trees.

Build with `cargo xtask kernel qcom/apq8016-sbc` (optionally selecting an
explicit local kernel source). This produces a kernel with embedded
Pocketboot initramfs and the board DTB under
`target/kernel/qcom/apq8016-sbc/arch/arm64/boot/`. The default requested
kernel output is `Image.gz`; uncompressed `Image` is also a build prerequisite.

The locally available U-Boot commit `b89b7bb08426` enables FIT in its DB410c
defconfig. Its fastboot boot path invokes `bootm` at the download buffer,
unless overridden by `fastboot_bootcmd`. The configured buffer begins at
`0x91000000`. The running build is dirty, so inspect live environment and
memory information before treating these source defaults as final.

Package the uncompressed ARM64 Image and APQ8016 board DTB into a FIT, after
checking Image header entry/placement requirements and available memory.
Then use an identity-selected `fastboot stage` and UART `bootm` for transient loading. The
current DB410c config has no Android `[bootimg]` table; `xtask build` would
fail at that packaging step. A FIT avoids introducing unrelated Android
boot-image defaults.

First establish four CPUs under ordinary PSCI. The subsequent goal is raw
SCM/ACC startup in pocketpreboot and the reserved parking handoff described in
[the SMP audit](msm8916-smp-status-2026-09-08.md). Both kernels in that experiment
must use spin-table CPU operations. The ordinary PSCI baseline does not validate
the A5's ACC+SCM startup path.
