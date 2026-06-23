FROM rust:1-trixie@sha256:4fd8406017c992f7b8ab55a2f99a1d56aeb1d7ecd255850dfa04239a88601f73

ENV DEBIAN_FRONTEND=noninteractive

RUN set -eux; \
    dpkg --add-architecture arm64; \
    apt-get update; \
    apt-get install --no-install-recommends -y \
        bc \
        binutils-aarch64-linux-gnu \
        bison \
        ca-certificates \
        ccache \
        clang \
        flex \
        gcc-aarch64-linux-gnu \
        git \
        libclang-19-dev \
        libelf-dev \
        libfontconfig1-dev \
        libssl-dev \
        lld \
        linux-libc-dev-arm64-cross \
        make \
        musl-dev:arm64 \
        llvm \
        pkg-config \
        python3; \
    rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    rustup target add aarch64-unknown-linux-musl aarch64-unknown-none armv7-unknown-linux-musleabihf; \
    rustup component add rust-src rustfmt; \
    cargo install --locked bindgen-cli; \
    rustc --version; \
    rustfmt --version; \
    bindgen --version; \
    printf 'typedef int bindgen_smoke;\n' > /tmp/bindgen-smoke.h; \
    LIBCLANG_PATH=/usr/lib/llvm-19/lib bindgen /tmp/bindgen-smoke.h > /tmp/bindgen-smoke.rs; \
    rm /tmp/bindgen-smoke.h /tmp/bindgen-smoke.rs

ENV CARGO_TERM_COLOR=always \
    BUSYBOX_CC=aarch64-linux-musl-gcc \
    BUSYBOX_CFLAGS="-idirafter /usr/aarch64-linux-gnu/include" \
    BUSYBOX_CROSS_COMPILE=aarch64-linux-gnu- \
    LIBCLANG_PATH=/usr/lib/llvm-19/lib
