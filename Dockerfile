# Build + inspect image for glass-mcp, the MCP server.
#
# glass drives native GUI apps, which needs a display at run time — this image serves the stdio MCP
# surface (the protocol handshake and the tool list) for registry inspection (e.g. Glama) and
# headless use. Driving a real GUI app still needs a host with a display + the platform tools.
#
# The runtime binary links only glibc (libc / libm / libgcc_s — everything else is Rust/static), so a
# distroless/cc base is sufficient and keeps the image small with no shell or package manager.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# The reported version. The crate is pinned at 0.0.0 (releases are tag-driven), and a `docker build`
# has no git tag, so pass the release version in here — it feeds build.rs's tag path (GITHUB_REF_*)
# so `--version`, `doctor`, and the MCP handshake report it instead of 0.0.0. Keep it in sync with
# the current release (same discipline as server.json / the changelog).
ARG GLASS_VERSION=1.0.1
# The toolchain (a pinned nightly) is set by rust-toolchain.toml; cargo installs it automatically.
# --locked builds against the committed Cargo.lock. The glibc build needs no system dev packages
# (only the musl variant needs musl-tools), so there is nothing to apt-get here.
RUN GITHUB_REF_TYPE=tag GITHUB_REF_NAME="v${GLASS_VERSION}" \
    cargo build --release -p glass-mcp --locked

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/glass-mcp /usr/local/bin/glass-mcp
# No subcommand → glass-mcp serves MCP over stdio (its default transport).
ENTRYPOINT ["/usr/local/bin/glass-mcp"]
