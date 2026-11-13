#!/usr/bin/env bash
# Build the static Linux artifact with a native musl C++ toolchain for OpenH264.
set -euo pipefail

repository=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$repository/target"}
mkdir -p "$target_dir"
target_dir=$(cd "$target_dir" && pwd)
mkdir -p "$target_dir/.musl-cargo"
docker_user="$(id -u):$(id -g)"
case "$(docker info --format '{{json .SecurityOptions}}')" in
    *name=rootless*) docker_user=0:0 ;;
esac

docker build --platform linux/amd64 \
    --file "$repository/scripts/musl.Dockerfile" \
    --tag glass-musl-builder:1.98.1 "$repository/scripts"

docker run --rm --platform linux/amd64 \
    --user "$docker_user" \
    --mount "type=bind,src=$repository,dst=/work,readonly" \
    --mount "type=bind,src=$target_dir,dst=/out" \
    --mount "type=bind,src=$target_dir/.musl-cargo,dst=/cargo-cache" \
    --env CARGO_HOME=/cargo-cache --env CARGO_TARGET_DIR=/out \
    --env CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
    glass-musl-builder:1.98.1 \
    cargo build --locked --target x86_64-unknown-linux-musl -p glass-mcp "$@"
