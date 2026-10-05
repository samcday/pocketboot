# Agent instructions

Before the first pocketboot or kernel build in a Delta session, ensure `.localkernel` contains the attached kernel worktree's absolute path, or remove it if no kernel is attached; recheck before building after machine or attachment changes.

Build: `cargo xtask build <vendor/device>`; omit the device to list choices.
Sargo (Pixel 3a): `cargo xtask build qcom/sdm670-google-sargo`.
Output: `${CARGO_TARGET_DIR:-target}/kernel/<vendor/device>/boot.img`.
