#!/usr/bin/env sh
# Rebuild the committed fixture modules.
#
#   sh build.sh
#
# Needs the wasm32-wasip1 target (`rustup target add wasm32-wasip1`).
set -eu
cd "$(dirname "$0")"
CARGO_TARGET_DIR=target cargo build --release --target wasm32-wasip1
mkdir -p wasm
for name in echo runaway memory_hog env_probe fs_probe; do
    cp "target/wasm32-wasip1/release/$name.wasm" "wasm/$name.wasm"
done
