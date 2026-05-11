#!/usr/bin/env bash
# =============================================================================
# hash256 worker auto-setup untuk VPS GPU rental (vast.ai / runpod / ubuntu).
# Cara pakai (di mesin VPS):
#
#   curl -fsSL https://raw.githubusercontent.com/ryuken25/hash256/main/scripts/setup_worker.sh | bash
#
# Skrip ini:
#   1. Install dependency (git, build tools, OpenCL ICD loader, screen)
#   2. Install + start Tailscale, minta auth link
#   3. Test koneksi ke coordinator (Tailscale IP laptop)
#   4. Install Rust + clone repo + cargo build --release
#   5. Auto-detect GPU model + jumlah → set GPU_BATCH dan GPU_INDICES
#   6. Generate .env worker (TANPA private key — ini yang aman di VPS sewa)
#   7. Run worker di screen session "hash256-worker"
# =============================================================================
set -euo pipefail

# ---- Konfigurasi yang biasanya di-edit (atau override via env saat curl) ----
COORDINATOR_URL="${COORDINATOR_URL:-http://100.121.79.5:8787}"
WORKER_AUTH_TOKEN="${WORKER_AUTH_TOKEN:-aku-atar-ganteng-2026-hash256}"
MINER_ADDRESS="${MINER_ADDRESS:-0xF2641c957BF8f7c35f1019ACC49Ec9732B3B825d}"
REPO_URL="${REPO_URL:-https://github.com/ryuken25/hash256.git}"
WORK_DIR="${WORK_DIR:-/workspace}"

# Fallback kalau /workspace tidak ada (bukan vast.ai-style image)
[ ! -d "$WORK_DIR" ] && WORK_DIR="$HOME"

echo "============================================================="
echo " hash256 worker auto-setup"
echo " coordinator : $COORDINATOR_URL"
echo " miner addr  : $MINER_ADDRESS"
echo " work dir    : $WORK_DIR"
echo "============================================================="

# ---- 1. Install paket dasar -------------------------------------------------
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y \
  git curl ca-certificates build-essential pkg-config libssl-dev \
  clinfo ocl-icd-opencl-dev ocl-icd-libopencl1 \
  screen tmux nano jq iproute2

# ---- 2. Tailscale -----------------------------------------------------------
if ! command -v tailscale >/dev/null 2>&1; then
  echo "[setup] installing tailscale..."
  curl -fsSL https://tailscale.com/install.sh | sh
fi

systemctl start tailscaled 2>/dev/null || true
if ! pgrep -x tailscaled >/dev/null 2>&1; then
  mkdir -p /var/lib/tailscale /var/run/tailscale
  nohup tailscaled \
    --state=/var/lib/tailscale/tailscaled.state \
    --socket=/var/run/tailscale/tailscaled.sock \
    > /tmp/tailscaled.log 2>&1 &
  sleep 3
fi

echo ""
echo "[setup] tailscale up — kalau muncul auth URL, buka di browser laptop dan approve device ini."
tailscale up --accept-dns=false || true
echo ""
echo "[setup] tailscale status:"
tailscale status || true

# ---- 3. Test koneksi ke coordinator -----------------------------------------
echo ""
echo "[setup] test koneksi /health ke coordinator..."
if curl --max-time 5 -sSf "$COORDINATOR_URL/health" >/dev/null; then
  echo "  OK — coordinator merespons"
else
  echo "  WARNING: belum bisa connect ke $COORDINATOR_URL"
  echo "  - pastikan coordinator laptop sudah jalan: hash256 coordinator"
  echo "  - cek tailscale status di kedua sisi"
  echo "  - skrip lanjut, tapi worker akan retry register sampai coordinator hidup"
fi

# ---- 4. Rust + repo ---------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  echo "[setup] installing Rust..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env"

mkdir -p "$WORK_DIR"
cd "$WORK_DIR"
if [ -d hash256/.git ]; then
  echo "[setup] hash256 repo sudah ada, git pull..."
  cd hash256
  git fetch --all
  git reset --hard origin/main
else
  echo "[setup] cloning $REPO_URL..."
  git clone "$REPO_URL"
  cd hash256
fi

echo "[setup] cargo build --release (ini bisa makan 3-8 menit pertama kali)..."
cargo build --release

if [ ! -x ./target/release/hash256 ]; then
  echo "ERROR: binary tidak ke-build"
  ls -lah ./target/release || true
  exit 1
fi

# ---- 5. Auto-detect GPU -----------------------------------------------------
GPU_COUNT=1
GPU_NAME="unknown"
if command -v nvidia-smi >/dev/null 2>&1; then
  GPU_COUNT=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | grep -c . || echo 1)
  GPU_NAME=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -n 1 || echo unknown)
fi
[ "$GPU_COUNT" -lt 1 ] && GPU_COUNT=1

# Default per device
GPU_BATCH=67108864
case "$(echo "$GPU_NAME" | tr '[:upper:]' '[:lower:]')" in
  *5090*)         GPU_BATCH=134217728 ;;
  *5080*)         GPU_BATCH=67108864 ;;
  *5070*)         GPU_BATCH=67108864 ;;
  *5060*)         GPU_BATCH=67108864 ;;
  *4090*)         GPU_BATCH=134217728 ;;
  *4080*)         GPU_BATCH=67108864 ;;
  *4070*)         GPU_BATCH=33554432 ;;
  *3090*)         GPU_BATCH=67108864 ;;
  *3080*)         GPU_BATCH=33554432 ;;
  *3070*)         GPU_BATCH=33554432 ;;
  *3060*)         GPU_BATCH=16777216 ;;
  *a100*|*h100*)  GPU_BATCH=268435456 ;;
esac

# Build "0,1,2,..." sampai GPU_COUNT
GPU_INDICES=$(seq -s, 0 $((GPU_COUNT-1)))

# OpenCL self-check
echo ""
echo "[setup] OpenCL devices:"
./target/release/hash256 devices || true

# ---- 6. Generate .env worker ------------------------------------------------
HOSTNAME_ID="$(hostname || echo vps)-$(date +%s)"

cat > .env <<EOF
# Auto-generated by setup_worker.sh on $(date -Iseconds)
MODE=worker

# coordinator (Tailscale)
COORDINATOR_URL=$COORDINATOR_URL
WORKER_AUTH_TOKEN=$WORKER_AUTH_TOKEN

# wallet — alamat saja, TIDAK pernah private key di worker
MINER_ADDRESS=$MINER_ADDRESS
MINER_ID=$HOSTNAME_ID

# chain (worker hanya butuh address contract buat verifikasi lokal)
CONTRACT=0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc
CHAIN_ID=1

# RPC placeholder — worker sebenarnya tidak query RPC, semua state via /work
RPC_URLS=https://eth.llamarpc.com
RPC_SUBMIT_URLS=https://eth.llamarpc.com

# GPU — auto-detected
GPU=1
GPU_INDICES=$GPU_INDICES
GPU_BATCH=$GPU_BATCH
AUTO_TUNE_BATCH=true
AUTO_TUNE_TARGET_MS=250
LOCAL_SIZE=256

# safety + cadence
TARGET_SAFETY_DIVISOR=4
STATE_CHECK_INTERVAL_MS=500
PRINT_INTERVAL_SEC=5

RUST_LOG=info
JSON_LOGS=false
METRICS_BIND=127.0.0.1:9898
EOF
chmod 600 .env

echo ""
echo "============================================================="
echo " worker config:"
echo "   GPU_NAME      = $GPU_NAME"
echo "   GPU_COUNT     = $GPU_COUNT"
echo "   GPU_INDICES   = $GPU_INDICES"
echo "   GPU_BATCH     = $GPU_BATCH"
echo "   MINER_ID      = $HOSTNAME_ID"
echo "   COORDINATOR   = $COORDINATOR_URL"
echo "============================================================="

# ---- 7. Run worker di screen ------------------------------------------------
# Kill session lama kalau ada supaya bisa re-run skrip dengan aman.
screen -S hash256-worker -X quit 2>/dev/null || true

screen -dmS hash256-worker bash -lc \
  "cd $WORK_DIR/hash256 && set -a && . ./.env && set +a && ./target/release/hash256 worker 2>&1 | tee -a worker.log"

sleep 1
screen -ls || true

echo ""
echo "[setup] worker started in screen 'hash256-worker'."
echo "    lihat log live :  screen -r hash256-worker     (detach: Ctrl+A D)"
echo "    tail file log  :  tail -f $WORK_DIR/hash256/worker.log"
echo "    stop worker    :  screen -S hash256-worker -X quit"
echo "    re-run setup   :  bash $WORK_DIR/hash256/scripts/setup_worker.sh"
