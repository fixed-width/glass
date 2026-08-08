#!/usr/bin/env bash
# Install everything glass's Linux backends need to run their *unit* tests, and fetch the sway
# bundle those tests launch.
#
# Four CI jobs need this identical setup — the in-diff mutation shards, the weekly sweep, `check`
# and `coverage` — because each runs `cargo test --workspace` or mutates a crate whose tests start
# a real display. Inlined four times, one copy ends up describing a package list it no longer
# installs, which had already happened once.
#
# The tests are deliberately not `#[ignore]`d: the mutation gate runs nextest, which skips ignored
# tests, so ignoring them would grade every mutant as caught.
set -euo pipefail

# xvfb: the private displays glass-x11's tests start. at-spi2-core: the one that starts a private
# a11y bus. The rest: the Mesa/GL stack and Xwayland the headless sway needs — the bundle resolves
# them from the host, so the GLVND loaders are required, not just libgl1-mesa-dri.
sudo apt-get update
sudo apt-get install -y \
    xvfb at-spi2-core \
    xwayland libegl1 libgl1 libgles2 libgbm1 libdrm2 libegl-mesa0 libgl1-mesa-dri

# Into the location glass's own discovery looks in, so nothing needs configuring. `pipefail` above
# is load-bearing: without it curl's status is discarded and a 404 or a truncated download is
# caught only if tar happens to choke on it.
bundle=https://github.com/fixed-width/sway-build/releases/download/bundle-latest/sway-bundle-linux-x86_64.tar.gz
mkdir -p ~/.local/share/glass/sway
curl -fsSL "$bundle" | tar -C ~/.local/share/glass/sway -xz
~/.local/share/glass/sway/bin/sway --version
