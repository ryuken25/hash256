#!/usr/bin/env bash
set -euo pipefail
apt update
apt install -y git curl build-essential pkg-config libssl-dev clinfo ocl-icd-opencl-dev screen tmux nano
if ! command -v cargo >/dev/null 2>&1; then
  curl https://sh.rustup.rs -sSf | sh -s -- -y
  . "$HOME/.cargo/env"
fi
cargo build --release
./target/release/hash256 devices || true
./target/release/hash256 doctor || true

