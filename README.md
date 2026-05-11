# HASH256 OpenCL GPU Miner

Production-oriented Rust miner for HASH token proof-of-work on Ethereum mainnet contract `0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc`.

It supports standalone mining and a safer cluster architecture where rented GPU workers never hold the private key. The coordinator owns Ethereum transaction nonce management, validates solutions, persists transaction state to SQLite, and exposes HTTP work APIs.

## What this is

- Rust CLI binary: `hash256`
- OpenCL GPU miner with device listing, benchmarking, and self-test
- Full uint256 PoW nonce layout: high 192-bit structured prefix plus low 64-bit GPU counter
- Multi-RPC read/write pool with health checks and raw transaction fanout hooks
- EIP-1559 gas modes: cheap, balanced, turbo
- Coordinator/worker mode for one wallet across many GPUs safely
- SQLite transaction nonce log and allocation serialization

## Build

```bash
cargo build --release
```

Binary path:

```bash
./target/release/hash256
```

Windows binary path:

```powershell
.\target\release\hash256.exe
```

## CLI commands

```bash
hash256 devices
hash256 state
hash256 bench
hash256 verify --nonce 0x1234
hash256 mine
hash256 coordinator
hash256 worker
hash256 txs
hash256 doctor
```

## Standalone mode quickstart

Use standalone mode for one GPU or one VPS. This process stores the private key and submits transactions itself.

```bash
export PRIVATE_KEY=0xYOUR_PRIVATE_KEY
export RPC_URLS=https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY,https://rpc.ankr.com/eth
export RPC_SUBMIT_URLS=https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY,https://rpc.ankr.com/eth
export MODE=standalone
export GPU=1
export GPU_INDEX=0
export GPU_BATCH=134217728
export GAS_MODE=balanced
export TARGET_SAFETY_DIVISOR=4
./target/release/hash256 doctor
./target/release/hash256 devices
./target/release/hash256 state
./target/release/hash256 bench
./target/release/hash256 mine
```

## Cluster mode quickstart

Use cluster mode when many rented GPU VPS instances mine for one wallet.

### Coordinator / submitter

Only this machine has `PRIVATE_KEY`.

```bash
export MODE=coordinator
export PRIVATE_KEY_FILE=/secure/hash256.key
export RPC_URLS=https://rpc1.example,https://rpc2.example
export RPC_SUBMIT_URLS=https://rpc1.example,https://rpc3.example
export WORKER_AUTH_TOKEN=change-this-long-random-token
export COORDINATOR_BIND=0.0.0.0:8787
export SQLITE_PATH=hash256.sqlite
export GAS_MODE=balanced
chmod 600 .env /secure/hash256.key
./target/release/hash256 coordinator
```

### Worker

Workers do not have `PRIVATE_KEY`. They only receive miner address, challenge, target, effective target, epoch, and nonce prefix/ranges.

```bash
export MODE=worker
export COORDINATOR_URL=http://COORDINATOR_PRIVATE_IP:8787
export WORKER_AUTH_TOKEN=change-this-long-random-token
export MINER_ID=vps-rtx-5090-01
export GPU=1
export GPU_INDEX=0
export GPU_BATCH=134217728
./target/release/hash256 worker
```

## One wallet, many devices

HASH contract challenges are address-dependent: `getChallenge(miner_address)` returns work for the wallet address that will call `mine(uint256)`. If many machines mine for one wallet, all workers must mine the same wallet challenge. They must not use their own keys unless each worker is intentionally an independent wallet.

Cluster mode solves this:

- coordinator signs transactions and owns the wallet
- workers never store or see the private key
- coordinator centrally allocates Ethereum transaction nonces
- workers only search disjoint PoW nonce ranges

## Why workers should not hold the private key

Putting the same private key on many rented VPS machines is dangerous and causes transaction nonce races. Two workers can sign transactions with the same Ethereum account nonce, replacing or blocking each other. Cluster mode avoids both risks: workers mine, coordinator signs.

## Two nonce types

### PoW nonce

`pow_nonce` is the uint256 argument passed to `mine(uint256)`. It is hashed as:

```text
keccak256(challenge || pow_nonce_uint256_big_endian)
```

The miner uses structured full uint256 space:

```text
bits 255..240: version
bits 239..208: miner_id_hash32
bits 207..160: boot_random48
bits 159..128: device_id / worker_id
bits 127..64:  epoch_or_challenge_hash64
bits 63..0:    GPU counter
```

Formula:

```text
pow_nonce = prefix_high_192_bits << 64 | counter64
```

### Ethereum transaction nonce

`eth_tx_nonce` is the Ethereum account nonce used to order wallet transactions. It must be globally serialized per wallet. Only the coordinator allocates it in cluster mode, under a mutex, and persists allocations to SQLite.

## Recommended settings

### RTX 5060 Ti

```bash
GPU_BATCH=67108864
MINER_THREADS=4
```

or:

```bash
MINER_THREADS=8
```

### RTX 5090

```bash
GPU_BATCH=134217728
MINER_THREADS=8
```

or:

```bash
MINER_THREADS=16
```

### RTX 5090 aggressive

```bash
GPU_BATCH=268435456
```

### Gas modes

Cheap:

```bash
GAS_MODE=cheap
PRIORITY_GWEI=0.02
MAX_FEE_GWEI_CAP=0.5
```

Balanced:

```bash
GAS_MODE=balanced
PRIORITY_GWEI=0.2
MAX_FEE_GWEI_CAP=1.0
```

Turbo:

```bash
GAS_MODE=turbo
PRIORITY_GWEI=2.5
MAX_FEE_GWEI_CAP=5.0
```

Recommended gas limit:

```bash
GAS_LIMIT_OVERRIDE=200000
GAS_LIMIT_CAP=300000
```

Do not use `50000000` gas for `eth_call` or `eth_estimateGas`.

## Vast.ai Ubuntu instructions

```bash
apt update
apt install -y git curl build-essential pkg-config libssl-dev clinfo ocl-icd-opencl-dev screen tmux nano
curl https://sh.rustup.rs -sSf | sh -s -- -y
. "$HOME/.cargo/env"
git clone https://github.com/ryuken25/hash256.git
cd hash256
cargo build --release
./target/release/hash256 devices
./target/release/hash256 doctor
```

Run coordinator in `screen`:

```bash
screen -S hash256-coordinator
./target/release/hash256 coordinator
```

Run worker in `tmux`:

```bash
tmux new -s hash256-worker
./target/release/hash256 worker
```

Detach:

```text
screen: Ctrl+A then D
tmux:   Ctrl+B then D
```

## Security warning

Do not put the same private key on many rented VPS machines. Use coordinator mode if using one wallet across many devices. Keep `.env` and key files private:

```bash
chmod 600 .env /secure/hash256.key
```

Use private networking or firewall rules for the coordinator. Bind to `127.0.0.1:8787` by default; use `0.0.0.0:8787` only when explicitly secured with `WORKER_AUTH_TOKEN` and private network access.

## Environment variables

See `.env.example` for the full list. Key variables:

```bash
PRIVATE_KEY=0x...
PRIVATE_KEY_FILE=/secure/key
MINER_ADDRESS=0x...
RPC_URLS=https://rpc1,https://rpc2
RPC_SUBMIT_URLS=https://rpc1,https://rpc2
CHAIN_ID=1
CONTRACT=0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc
GPU=1
GPU_INDEX=0
GPU_INDICES=0,1,2,3
GPU_BATCH=134217728
AUTO_TUNE_BATCH=true
LOCAL_SIZE=256
TARGET_SAFETY_DIVISOR=4
STATE_CHECK_INTERVAL_MS=500
PRINT_INTERVAL_SEC=5
MINER_ID=my-vps-1
GAS_MODE=balanced
PRIORITY_GWEI=0.2
MAX_FEE_GWEI_CAP=1.0
GAS_LIMIT_OVERRIDE=200000
GAS_LIMIT_CAP=300000
FEE_BUMP_PERCENT=15
PENDING_REPLACE_AFTER_BLOCKS=2
MODE=standalone
COORDINATOR_URL=http://x.x.x.x:8787
WORKER_AUTH_TOKEN=...
COORDINATOR_BIND=0.0.0.0:8787
SQLITE_PATH=hash256.sqlite
RUST_LOG=info
JSON_LOGS=false
METRICS_BIND=0.0.0.0:9898
```

## Troubleshooting

### GPU not detected

Run:

```bash
clinfo
./target/release/hash256 devices
```

Install OpenCL ICD packages and NVIDIA drivers. On Ubuntu:

```bash
apt install -y ocl-icd-opencl-dev clinfo
```

### OpenCL `clinfo` does not show NVIDIA

The NVIDIA driver or OpenCL ICD is missing. Reinstall the proper NVIDIA driver image for the VPS and verify `nvidia-smi`.

### Fallback to CPU

Set:

```bash
GPU=0
```

Then run:

```bash
./target/release/hash256 bench
```

### `nonce too low`

Another process used the wallet transaction nonce. Stop duplicate standalone miners for the same wallet. In cluster mode, only the coordinator should sign.

### `replacement transaction underpriced`

Increase:

```bash
FEE_BUMP_PERCENT=20
GAS_MODE=turbo
```

### `insufficient funds` during `eth_call`

Use explicit reasonable gas caps:

```bash
GAS_LIMIT_OVERRIDE=200000
GAS_LIMIT_CAP=300000
```

Do not set huge gas limits.

### Stale challenge

Increase RPC quality, reduce batch size, or increase polling speed:

```bash
GPU_BATCH=67108864
STATE_CHECK_INTERVAL_MS=250
TARGET_SAFETY_DIVISOR=4
```

### Hash above target

The solution was found against an old target or bad local verification. Run `hash256 verify --nonce <nonce>` and check GPU self-test.

### RPC lag

Use multiple URLs:

```bash
RPC_URLS=https://rpc1,https://rpc2,https://rpc3
RPC_MAX_BLOCK_LAG=2
```

### Pending transaction stuck

Use `hash256 txs` to inspect SQLite records. Raise gas mode or fee bump. Avoid running multiple submitters for one wallet.

## Development

```bash
cargo fmt
cargo test
cargo build --release
```

## License

MIT. Mining and transaction submission are at your own risk.

