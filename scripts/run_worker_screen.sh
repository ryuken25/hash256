#!/usr/bin/env bash
set -euo pipefail
SESSION=${SESSION:-hash256-worker}
ENVFILE=${ENVFILE:-.env.worker}
[ ! -f "$ENVFILE" ] && ENVFILE=.env
echo "using env file: $ENVFILE"
screen -dmS "$SESSION" bash -lc "set -a; [ -f \"$ENVFILE\" ] && . ./\"$ENVFILE\"; set +a; ./target/release/hash256 worker 2>&1 | tee -a worker.log"
screen -ls

