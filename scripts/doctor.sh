#!/usr/bin/env bash
set -euo pipefail
set -a
[ -f .env ] && . ./.env
set +a
clinfo || true
./target/release/hash256 devices || true
./target/release/hash256 doctor

