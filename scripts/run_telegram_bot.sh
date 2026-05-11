#!/usr/bin/env bash
# Telegram bot supervisor.
set +e
cd "$(dirname "$0")/.."
BACKOFF=2
MAX=30
while true; do
  echo "[$(date -Iseconds)] starting telegram bot"
  python3 scripts/telegram_bot.py
  CODE=$?
  echo "[$(date -Iseconds)] bot exited code=$CODE, restart in ${BACKOFF}s..."
  sleep $BACKOFF
  BACKOFF=$((BACKOFF * 2))
  [ $BACKOFF -gt $MAX ] && BACKOFF=$MAX
done
