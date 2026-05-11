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

# Detect whether /dev/net/tun is usable (vast.ai / runpod containers usually
# do not expose it, so we have to use Tailscale userspace networking with a
# local HTTP proxy that the worker reqwest client picks up via HTTP_PROXY).
USE_USERSPACE=0
if [ ! -e /dev/net/tun ]; then
  USE_USERSPACE=1
fi

# Kill any prior tailscaled that may have started without the right flags.
pkill -9 tailscaled 2>/dev/null || true
sleep 1
mkdir -p /var/lib/tailscale /var/run/tailscale

TS_FLAGS=(
  "--state=/var/lib/tailscale/tailscaled.state"
  "--socket=/var/run/tailscale/tailscaled.sock"
)
if [ "$USE_USERSPACE" = "1" ]; then
  echo "[setup] /dev/net/tun unavailable -> using Tailscale userspace networking + HTTP proxy on :1055"
  TS_FLAGS+=("--tun=userspace-networking")
  TS_FLAGS+=("--socks5-server=localhost:1055")
  TS_FLAGS+=("--outbound-http-proxy-listen=localhost:1055")
fi

setsid nohup tailscaled "${TS_FLAGS[@]}" > /tmp/tailscaled.log 2>&1 < /dev/null &
disown 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9 10; do
  [ -S /var/run/tailscale/tailscaled.sock ] && break
  sleep 1
done

if ! pgrep -x tailscaled >/dev/null 2>&1; then
  echo "ERROR: tailscaled belum jalan. Cek /tmp/tailscaled.log"
  tail -20 /tmp/tailscaled.log 2>/dev/null || true
fi

echo ""
if [ -n "${TS_AUTHKEY:-}" ]; then
  echo "[setup] tailscale up dengan auth key (non-interactive)"
  tailscale up --accept-dns=false --authkey="$TS_AUTHKEY" --hostname="$(hostname)" || true
else
  echo "[setup] tailscale up — kalau muncul auth URL, buka di browser laptop dan approve device ini."
  tailscale up --accept-dns=false || true
fi
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

# Default per device. Auto-tune will further adjust based on dispatch time.
GPU_BATCH=33554432
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

# Ensure NVIDIA OpenCL ICD vendor file exists. Some vast.ai images install
# pocl (CPU OpenCL) but never write /etc/OpenCL/vendors/nvidia.icd, so OpenCL
# apps see only the CPU device and ignore the GPU.
if command -v nvidia-smi >/dev/null 2>&1 && [ -f /usr/lib/x86_64-linux-gnu/libnvidia-opencl.so.1 ]; then
  mkdir -p /etc/OpenCL/vendors
  if [ ! -f /etc/OpenCL/vendors/nvidia.icd ]; then
    echo "[setup] writing /etc/OpenCL/vendors/nvidia.icd"
    echo "libnvidia-opencl.so.1" > /etc/OpenCL/vendors/nvidia.icd
  fi
  # If pocl ICD is also present and would dominate, move it aside so the GPU
  # platform shows up as the default (worker uses platform[0]).
  if [ -f /etc/OpenCL/vendors/pocl.icd ]; then
    mv /etc/OpenCL/vendors/pocl.icd /etc/OpenCL/vendors/pocl.icd.disabled
    echo "[setup] disabled /etc/OpenCL/vendors/pocl.icd (CPU OpenCL would override GPU)"
  fi
fi

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

# ---- 7. Run worker via nohup (more reliable than screen on vast.ai images) -
# Some vast.ai container images kill detached screen sessions after a while.
# nohup + setsid + disown gives us a process that survives the SSH session
# terminating. Logs go to worker.log; check with `tail -f worker.log`.
pkill -9 -f 'target/release/hash256 worker' 2>/dev/null || true
screen -S hash256-worker -X quit 2>/dev/null || true
sleep 1

PROXY_ENV=""
if [ "$USE_USERSPACE" = "1" ]; then
  PROXY_ENV="export HTTP_PROXY=http://localhost:1055; export HTTPS_PROXY=http://localhost:1055; export ALL_PROXY=socks5://localhost:1055;"
  echo "[setup] worker akan pakai HTTP proxy localhost:1055 (Tailscale userspace mode)"
fi

cd "$WORK_DIR/hash256"
chmod +x scripts/*.sh 2>/dev/null
# Spawn under supervisor so it auto-restarts on crash.
setsid nohup bash scripts/run_worker_supervised.sh > "$WORK_DIR/hash256/worker.log" 2>&1 < /dev/null &
disown 2>/dev/null || true
sleep 6

WORKER_PID=$(pgrep -x hash256 | head -1)
SUP_PID=$(pgrep -f run_worker_supervised | head -1)
if [ -n "$WORKER_PID" ]; then
  echo ""
  echo "[setup] worker running (pid=$WORKER_PID, supervisor=$SUP_PID)"
  echo "    tail log  :  tail -f $WORK_DIR/hash256/worker.log"
  echo "    stop      :  pkill -f run_worker_supervised; pkill -x hash256"
  echo "    re-run    :  bash $WORK_DIR/hash256/scripts/setup_worker.sh"
  echo ""
  echo "[setup] last 6 lines of worker log:"
  tail -6 "$WORK_DIR/hash256/worker.log"
else
  echo "[setup] WARNING: worker process not found after 6s. Check $WORK_DIR/hash256/worker.log"
  tail -20 "$WORK_DIR/hash256/worker.log"
fi
