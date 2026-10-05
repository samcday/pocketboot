pocketboot builds a Rust `/init` and wraps it in a lean mainline kernel.

Kernel source: explicit path, then attached Delta kernel (via `CDPATH`), then per device configuration.

Build: `cargo xtask build <vendor/device>`; omit the device to list choices.
Sargo (Pixel 3a): `cargo xtask build qcom/sdm670-google-sargo`.
Output: `${CARGO_TARGET_DIR:-target}/kernel/<vendor/device>/boot.img`.
