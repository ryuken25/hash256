# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Rust CLI (`hash256`) for HASH token PoW mining on Ethereum mainnet contract `0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc`. The contract's `getChallenge(miner)` is **address-dependent**, so a single wallet mined by many machines must serialize on one address — that constraint drives the entire architecture.

## Common commands

```bash
cargo build --release              # release binary at target/release/hash256[.exe]
cargo test                          # all tests (lib + tests/core.rs)
cargo test --no-default-features    # run tests without OpenCL backend
cargo test --test core <name>       # single integration test
cargo fmt
cargo build --no-default-features   # CPU-only build (no OpenCL)
```

Runtime subcommands (auto-selected from `MODE` env var when omitted):
`devices | state | bench | verify --nonce 0x... | mine | coordinator | worker | txs | doctor`

`AppConfig::from_env()` runs **before** `Cli::parse()` in [src/main.rs](src/main.rs#L43), so `.env` is loaded for every subcommand. Tests rely on `fake_cfg()` in [tests/core.rs](tests/core.rs#L143) — keep its fields in sync when adding to `AppConfig`.

## Architecture

Three runtime modes share the same binary, selected by `MODE`:

- **Standalone** ([standalone_mine](src/main.rs)): one process holds the private key, runs a [StateWatcher](src/state_watcher.rs) background poller, spawns one OS thread per `GPU_INDICES` entry that runs [GpuMiner::mine_until](src/gpu/mod.rs), and pushes solutions through an `mpsc` channel into the [Submitter](src/submitter.rs).
- **Coordinator** ([CoordinatorApp](src/coordinator.rs)): owns the private key and the Ethereum tx nonce. Exposes axum HTTP at `COORDINATOR_BIND` with `/register`, `/work`, `/solution`, `/health`, `/metrics`. Bearer-auths workers with `WORKER_AUTH_TOKEN`. Spawns receipt watcher + replacement loop via [Submitter](src/submitter.rs).
- **Worker** ([run_worker](src/worker.rs)): registers with coordinator, runs one OS thread per device that polls `/work`, mines until `MineOutcome::Found` or stale, and posts back to `/solution`. Never sees the private key. Stale detection: a background task polls `/work` and bumps an `AtomicU64` token that the GPU loop checks between dispatches.

### Two distinct nonces — do not conflate

- **PoW nonce** = full uint256 passed to `mine(uint256)`. Layout in [nonce_space.rs](src/nonce_space.rs): high 192 bits are a structured prefix `version(16) | miner_hash32(32) | boot_random48(48) | worker_device_id(32) | epoch_challenge_hash(64)`; low 64 bits are the GPU search counter. `NonceAllocator::rebind_epoch` resets the counter and re-derives the epoch hash whenever the watcher reports a new epoch/challenge.
- **Ethereum tx nonce** = per-account transaction order. Allocated only by [TxNonceManager](src/tx_nonce_manager.rs) under a `tokio::Mutex`, seeded from `max(pending, latest, sqlite)` and persisted to SQLite. Cluster mode exists primarily to keep this serialized on one machine.

### Hash convention

`keccak256(challenge_bytes32 || pow_nonce_be_bytes32)` — see [contract::hash_nonce](src/contract.rs). The OpenCL kernel must match exactly; CPU verification ([proof_is_valid](src/contract.rs)) is the reference, and `GpuMiner::self_test` re-checks GPU output against `cpu_hash` before returning a solution. Active kernel is [src/gpu/kernel.cl](src/gpu/kernel.cl) (uint256 prefix + counter). The standalone `src/keccak_kernel.cl` is the older single-nonce kernel and is unused.

### State watcher → mining abort

[StateWatcher](src/state_watcher.rs) exposes:
- `subscribe()` — `watch::Receiver<Option<WorkState>>` for tasks that want change notifications.
- `current()` — synchronous snapshot of the latest state (used inside OS-thread mining loops).
- `token()` — `Arc<AtomicU64>` that increments on every challenge/epoch change.

`GpuMiner::mine_until` accepts an optional `(token, snapshot)` pair and aborts between dispatches when the token diverges from the snapshot. This is the primary stale-prevention mechanism.

### Submitter pipeline

[Submitter::submit_solution](src/submitter.rs) does, in order:
1. `validate_solution` (epoch/challenge/hash/target).
2. Re-fetch `getChallenge` to catch race between mining and signing.
3. Acquire `pending_lock` unless `ENABLE_PARALLEL_PENDING_TX=true` (one pending tx per wallet by default).
4. `eth_feeHistory` for base fee → `plan_gas` (EIP-1559 with `priority_gwei` / `max_fee_gwei_cap`).
5. `eth_call` simulate with explicit `gas=200000`.
6. `eth_estimateGas` (capped by `GAS_LIMIT_CAP`) → `plan_gas` again with the estimate.
7. `TxNonceManager::allocate` (mutex + SQLite persist).
8. Sign EIP-1559 with `alloy::consensus::TxEip1559` + `PrivateKeySigner::sign_transaction_sync`.
9. `RpcPool::broadcast_raw_tx` fans out to **all healthy** submit RPCs in parallel; "already known" counts as success.
10. Persist tx hash.

Two background loops (spawned in both standalone and coordinator):
- `receipt_watcher_loop`: polls `eth_getTransactionReceipt` for each pending row → marks `success`/`failed`.
- `replacement_loop`: any tx older than `PENDING_REPLACE_AFTER_BLOCKS` is re-signed with `bump_eip1559(FEE_BUMP_PERCENT)` (capped at `MAX_FEE_GWEI_CAP`) and re-broadcast on the same eth nonce.

### RPC pool

[RpcPool](src/rpc_pool.rs) maintains separate **read** and **submit** endpoint lists with periodic health checks (`spawn_health_loop`). Endpoints are sorted by `(unhealthy_first_then_latency)`. URLs lagging by more than `RPC_MAX_BLOCK_LAG` blocks behind the freshest are marked unhealthy. `broadcast_raw_tx` fans out to all healthy submit endpoints. API keys in URLs are redacted via `redact_url` in logs/health output. Useful methods: `mining_state`, `get_challenge`, `fee_history`, `estimate_gas`, `tx_receipt`.

### Effective target safety divisor

Workers mine against `target / TARGET_SAFETY_DIVISOR` (default 4) so the nonce stays below the on-chain target through small difficulty bumps. `Submitter::validate_solution` re-checks against both effective and real target.

### Persistence

SQLite via `rusqlite` (bundled). Schema in [Db::migrate](src/db.rs): `meta(key,value)` for `next_nonce` checkpoint, `txs` for allocated tx records (status: `pending`/`success`/`failed`/`replaced`/`cancelled`/`stale`). Best-effort `ALTER TABLE` runs cover schema upgrades for older DBs. Connection is a single `Arc<Mutex<Connection>>` — calls are sync and block whichever tokio worker invokes them; throughput is fine for submission rates.

### Metrics

[Metrics](src/metrics.rs) is a Prometheus-style counter set + per-device hashrate map. Both standalone and coordinator spawn `serve_metrics(METRICS_BIND, ...)` which exposes `/metrics` on a separate axum server. Counters update via `AtomicU64::fetch_add`; the per-device map uses `std::sync::Mutex` so it can be written from the OS-thread mining loop.

## Conventions to preserve

- All errors flow through `eyre::Result`. The `errors` module re-exports `eyre::Report`.
- New env vars: add to `AppConfig`, parse via the `env_s` / `env_parse` / `env_bool` helpers in [config.rs](src/config.rs), document in `.env.example` and README, **and** add to `fake_cfg()` in [tests/core.rs](tests/core.rs).
- New CLI subcommands: add to [cli::Command](src/cli.rs) and dispatch in `main`.
- The OpenCL kernel ([src/gpu/kernel.cl](src/gpu/kernel.cl)) is `include_str!`'d at compile time. Kernel arg order is fixed in `mine_prefix` / `dispatch_once` — keep host-side `Kernel::builder().arg(...)` chain and the kernel signature in lockstep.
- The OpenCL backend is gated behind the `gpu` Cargo feature (default-on). CPU-only builds use `--no-default-features` and fall through to a much slower CPU path in standalone/worker.
- Standalone mining lives in OS threads (`std::thread::spawn`), not tokio tasks — `GpuMiner::mine_until` blocks. Don't call `Handle::current()` or async APIs from inside those threads; use sync APIs (`metrics.record_device`, `mpsc::UnboundedSender::send`, `watcher.current()`).
