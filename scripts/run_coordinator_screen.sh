#!/usr/bin/env bash
set -euo pipefail
SESSION=${SESSION:-hash256-coordinator}
screen -dmS "$SESSION" bash -lc 'set -a; [ -f .env ] && . ./.env; set +a; ./target/release/hash256 coordinator 2>&1 | tee -a coordinator.log'
screen -ls

