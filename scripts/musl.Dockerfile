# Keep the Rust version aligned with rust-toolchain.toml.
FROM rust:1.98.1-alpine@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f

RUN apk add --no-cache build-base cmake git perl pkgconf \
    && rustup component add rustfmt clippy

WORKDIR /work
