#!/usr/bin/env bash
# Worker supervisor: restart on crash with capped exponential backoff.
# Usage: setsid nohup ./scripts/run_worker_supervised.sh > worker.log 2>&1 < /dev/null & disown
set +e
cd "$(dirname "$0")/.."  # to /workspace/hash256
BACKOFF=2
MAX=60
RESTARTS=0
while true; do
  echo
  echo "============================================================="
  echo "[$(date -Iseconds)] supervisor: starting worker (restart #$RESTARTS)"
  echo "============================================================="
  export HTTP_PROXY=http://localhost:1055
  export HTTPS_PROXY=http://localhost:1055
  export ALL_PROXY=socks5://localhost:1055
  set -a; . ./.env; set +a
  ./target/release/hash256 worker
  CODE=$?
  RESTARTS=$((RESTARTS+1))
  echo
  echo "[$(date -Iseconds)] supervisor: worker exited code=$CODE, restart in ${BACKOFF}s..."
  sleep $BACKOFF
  BACKOFF=$((BACKOFF * 2))
  [ $BACKOFF -gt $MAX ] && BACKOFF=$MAX
done
