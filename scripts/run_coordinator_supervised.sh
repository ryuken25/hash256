#!/usr/bin/env bash
# Coordinator supervisor: restart on crash with capped exponential backoff.
# Usage: setsid nohup ./scripts/run_coordinator_supervised.sh > coordinator.log 2>&1 < /dev/null & disown
set +e
cd "$(dirname "$0")/.."  # to /workspace/hash256
BACKOFF=2
MAX=60
RESTARTS=0
while true; do
  echo
  echo "============================================================="
  echo "[$(date -Iseconds)] supervisor: starting coordinator (restart #$RESTARTS)"
  echo "============================================================="
  set -a; . ./.env; set +a
  ./target/release/hash256 coordinator
  CODE=$?
  RESTARTS=$((RESTARTS+1))
  echo
  echo "[$(date -Iseconds)] supervisor: coordinator exited code=$CODE, restart in ${BACKOFF}s..."
  sleep $BACKOFF
  BACKOFF=$((BACKOFF * 2))
  [ $BACKOFF -gt $MAX ] && BACKOFF=$MAX
done
