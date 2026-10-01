# Expressltexx PBL investigation

The first experiment is a read of a candidate boot-ROM address from a
diagnostic Pocketboot kernel. This is not an EDL programmer, a secure-boot
bypass, or a replacement for SBL1. No ROM extraction is claimed yet.

## Target and address evidence

The inspected GT-I8730 identifies as `samsung,expressltexx`, MSM8930,
SoC ID 116, revision 1.2. Its bootloader command line identifies
`I8730XWANE1`. Do not substitute Expressatt/MSM8960 images: those are
different devices even though some firmware source paths say `msm8960`.

The initial candidate is physical `0x00000000`, with a possible 128 KiB
window. This is a hypothesis for MSM8930, supported by:

- Qualcomm's [APQ8064 datasheet](https://www.qualcomm.com/content/dam/qcomm-martech/dm-assets/documents/snapdragon_600_apq_8064_data_sheet.pdf),
  section 3.1.1 and figure 4-1: ARM7 RPM is the primary boot processor,
  and its 128 KiB boot ROM is mapped at zero.
- A contemporaneous [SCH-I535/MSM8960 `viewmem` extraction report](https://xdaforums.com/t/r-d-unlock-bootloaders.1769411/post-30084661).
  This is related-chip evidence, not a provenanced MSM8930 binary.

Do not use a broad tool configuration's `0x00100000` ROM range blindly:
this handset's live RPM interface occupies `0x00108000–0x00108fff`.
Do not sweep MMIO looking for executable-looking bytes.

## Separate diagnostic build

The inspected Pocketboot kernel had neither `/dev/mem` nor module support.
The [Expressltexx handoff](samsung-expressltexx.md#hardware-validation)
already works without flashing another boot image. Keep the normal device
configuration unchanged and use a separate, ignored output directory.

Using the project's CI toolchain and the kernel revision pinned by the
device config, first build the ordinary image in the lab output:

```sh
device=qcom/msm8930-samsung-expressltexx
kernel=/absolute/path/to/the/pinned/linux/source
export CARGO_TARGET_DIR="$PWD/target/expressltexx-pbl"
cargo xtask build "$device" "$kernel"
out="$CARGO_TARGET_DIR/kernel/$device"
cp "$out/.config" "$out/config.before-pbl"
```

An already verified built-in initramfs can instead be supplied with
`--initrd /absolute/path/to/verified.cpio` to isolate the kernel change.
Record its hash; do not silently substitute another userspace build.

Change only the generated diagnostic config, then rebuild and repackage:

```sh
"$kernel/scripts/config" --file "$out/.config" \
    --enable DEVMEM --enable IKCONFIG --enable IKCONFIG_PROC \
    --set-str LOCALVERSION "-pbl"
make -C "$kernel" O="$out" ARCH=arm LLVM=-19 olddefconfig
make -C "$kernel" O="$out" ARCH=arm LLVM=-19 -j8 \
    zImage qcom/qcom-msm8930-samsung-expressltexx.dtb
cargo xtask bootimg "$device"
python3 tools/arm32-kexec/check-decompressor.py \
    "$out/arch/arm/boot/compressed/vmlinux"
```

Inspect the effective config: retain `SMP=n`, `PHYS_OFFSET=0x80200000`,
`AUTO_ZRELADDR=n` and `KEXEC=y`. Verify the decompressor's fixed
`zreladdr=0x80208000`, packaged DTB, kernel and initramfs identities.
Do not run a normal `cargo xtask build` against this modified output later:
its configuration stamp does not track manual `.config` edits.

## Read experiment

Use the [standalone read-only helper](../tools/physread/README.md). First
obtain approval for a RAM-only boot and a potentially faulting physical
read. Confirm the USB serial and the working persistent BOOT backup before
`fastboot boot`. This procedure does not write a flash partition.

After the handoff, verify the diagnostic kernel release, SoC ID, CPU 0-only
state, correct RAM map, `/chosen/linux,booted-from-kexec`, and `/dev/mem`.
Copy the helper into the RAM-backed `/run` directory.

The first request is **one aligned word**, `physread 0x0 4`. Read-only
does not mean crash-proof: a bus firewall or inaccessible address can
fault, hang or reset the phone. Stop on failure rather than trying
neighbouring addresses or changing security registers.

Pocketboot's raw ADB exec transport combines stdout and stderr and does
not carry a shell-v2 exit status. Stage the output, stderr and exit status
as separate RAM files, then retrieve them. For example, after setting
`serial` to the verified handset and installing `/run/physread`:

```sh
adb -s "$serial" exec-out \
    '/run/physread 0x0 4 >/run/rom-probe.bin 2>/run/rom-probe.err; echo $? >/run/rom-probe.status'
adb -s "$serial" exec-out 'cat /run/rom-probe.status /run/rom-probe.err'
adb -s "$serial" pull /run/rom-probe.bin first-word.bin
wc -c first-word.bin
```

Require remote status zero and exactly four output bytes. A successful
word read is not a dump: only then consider a page, followed by the
corroborated ROM window. Repeat full reads and compare sizes and hashes;
inspect vectors, instruction-set transitions and internal references.
Stable zeros or a plausible disassembly alone do not establish mask-ROM
provenance.

Keep raw dumps and build products ignored. Record commands, source/build
identities, addresses, lengths, hashes, relevant logs and interpretation
in `refs/notes/evidence` on the experiment commit. Never publish unrelated
RAM, EFS, modem-state or userdata as a supposed ROM dump.
