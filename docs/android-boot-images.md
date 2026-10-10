# Android boot-image kernel preparation

Fastboot `boot` and `oem kexec-load` parse the Android image and prepare its
kernel before attempting kexec. Filesystem boot entries use the same kernel
preparation and DTB-selection rule.

For a top-level gzip kernel section, pocketboot supports:

- One or more valid gzip members, concatenated into the kernel payload.
- Optionally, one v17-format DTB immediately after the last member, followed
  by zero padding if needed. The DTB is separated from the kernel and validated:
  header/version, declared size, block bounds/alignment/overlap, reservation-map
  termination, and tree structure.

This includes the Ferrari Android v0 format produced by the packager:
`gzip(Image) + DTB`, or `gzip(pocketpreboot envelope) + DTB`. Preparation retains
the entire decompressed envelope, including its inner kernel and padded runtime
footprint; the shim's `image_size` is not the size of that combined payload.

An explicit DTB from the Android header/vendor table or filesystem boot entry
takes precedence over a valid appended DTB. Only when neither exists does
pocketboot fall back to the live DTB. A malformed trailer is an error even when
an explicit DTB was supplied.

Gzip checksum/size errors, truncated members, unknown trailers, malformed DTBs
and concatenated DTB sets are rejected. The loader neither scans arbitrary bytes
for FDT magic nor silently chooses a board from multiple appended trees. This
does not add raw uncompressed Image+DTB splitting or change PE/zboot extraction.

## Verification

The host tests cover ordinary and multi-member gzip, corrupt/truncated streams,
DTB selection and malformed trailers, and Android v0/v2 extraction. A synthetic
preboot envelope checks that neither the inner Image nor its BSS padding is
discarded.

An ignored integration test can check a real `boot.img` against independently
extracted kernel and DTB files without invoking kexec:

```sh
POCKETBOOT_TEST_BOOT_IMAGE=/path/to/boot.img \
POCKETBOOT_TEST_KERNEL=/path/to/decompressed-kernel \
POCKETBOOT_TEST_DTB=/path/to/appended.dtb \
  cargo test -p pocketboot inspect_supplied_android_image -- --ignored --nocapture
```

Successful parsing does not establish a successful device handoff. The outgoing
kernel must still support safe CPU/device shutdown, and a preboot reentry needs
its valid owned resident contract. No flashing or direct-OEM boot test is implied.
