use std::sync::Arc;

use alloy::primitives::{Address, B256, U256};
use hash256::{
    config::{parse_urls, AppConfig, GasMode, Mode},
    contract::{hash_below_target, hash_nonce, mine_calldata, WorkState},
    gas::{bump_eip1559, plan_gas, GasPlan},
    metrics::Metrics,
    nonce_space::{pow_nonce_from_parts, NonceAllocator, NoncePrefix},
    rpc_pool::parse_rpc_urls,
    submitter::{SolutionSubmit, Submitter},
};

#[test]
fn cpu_keccak_hash_nonce_known_manual_encoding() {
    let c = B256::from([0x22u8; 32]);
    let n = U256::from(1u64);
    let h = hash_nonce(&c, n);
    let mut bytes = [0u8; 64];
    bytes[..32].copy_from_slice(c.as_slice());
    bytes[63] = 1;
    assert_eq!(h, alloy::primitives::keccak256(bytes));
}

#[test]
fn uint256_nonce_prefix_encoding_big_endian() {
    let prefix = NoncePrefix {
        high192: [0x55u8; 24],
    };
    let nonce = pow_nonce_from_parts(prefix, 0xaabb_ccdd_eeff_0011);
    let b = nonce.to_be_bytes::<32>();
    assert_eq!(&b[0..24], &[0x55u8; 24]);
    assert_eq!(&b[24..32], &0xaabb_ccdd_eeff_0011u64.to_be_bytes());
}

#[test]
fn target_compare_strict_less_than() {
    assert!(hash_below_target(&B256::from([0u8; 32]), U256::from(1u64)));
    assert!(!hash_below_target(&B256::from([0u8; 32]), U256::ZERO));
}

#[test]
fn nonce_prefix_allocation_uniqueness() {
    let c = B256::from([9u8; 32]);
    let a = NonceAllocator::with_parts(1, 2, 3, 4, 5, &c).prefix();
    let b = NonceAllocator::with_parts(1, 2, 3, 5, 5, &c).prefix();
    assert_ne!(a, b);
}

#[test]
fn nonce_prefix_changes_on_epoch_rebind() {
    let c1 = B256::from([1u8; 32]);
    let c2 = B256::from([2u8; 32]);
    let mut a = NonceAllocator::with_parts(1, 2, 3, 4, 5, &c1);
    let p1 = a.prefix();
    a.next_batch(10_000);
    assert!(a.current_counter() >= 10_000);
    a.rebind_epoch(6, &c2);
    assert_eq!(a.current_counter(), 0);
    assert_ne!(a.prefix(), p1);
}

#[test]
fn rpc_url_parsing() {
    assert_eq!(parse_rpc_urls("a,b; c d"), vec!["a", "b", "c", "d"]);
    assert_eq!(parse_urls("x;y"), vec!["x", "y"]);
}

#[test]
fn gas_fee_calculation_and_bump() {
    let cfg = fake_cfg();
    let p = plan_gas(&cfg, Some(100), Some(100_000));
    assert_eq!(p.gas_limit, 200_000);
    assert_eq!(p.max_priority_fee_per_gas, 200_000_000);
    let b = bump_eip1559(
        GasPlan {
            gas_limit: 1,
            max_priority_fee_per_gas: 100,
            max_fee_per_gas: 1000,
        },
        15,
        10_000,
    );
    assert_eq!(b.max_priority_fee_per_gas, 115);
}

#[test]
fn calldata_for_mine_uint256() {
    let data = mine_calldata(U256::from(1));
    assert_eq!(data.len(), 36);
    assert_eq!(&data[32..36], &[0, 0, 0, 1]);
}

#[tokio::test]
async fn stale_solution_rejection() {
    let cfg = fake_cfg();
    let rpc = hash256::rpc_pool::RpcPool::new(
        vec!["http://127.0.0.1:1".into()],
        vec!["http://127.0.0.1:1".into()],
        1,
        1,
        1,
    )
    .unwrap();
    let metrics = Arc::new(Metrics::default());
    let submitter = Submitter::new(cfg, rpc, None, None, Address::ZERO, metrics);
    let state = WorkState {
        miner_address: Address::ZERO,
        block_number: 1,
        epoch: 2,
        challenge: B256::from([1u8; 32]),
        target: U256::MAX,
        effective_target: U256::MAX,
        blocks_left: 1,
    };
    let sol = SolutionSubmit {
        worker_id: "w".into(),
        device_id: 0,
        epoch: 1,
        challenge: state.challenge,
        nonce: U256::ZERO,
        hash: hash_nonce(&state.challenge, U256::ZERO),
        attempts: 1,
        hashrate: 1.0,
        found_at_block: 1,
    };
    assert!(submitter.validate_solution(&sol, &state).await.is_err());
}

#[test]
fn eip1559_tx_building_plan_has_type2_fields() {
    let cfg = fake_cfg();
    let p = plan_gas(&cfg, None, Some(200_000));
    assert!(p.max_fee_per_gas >= p.max_priority_fee_per_gas);
    assert_eq!(p.gas_limit, 240_000);
}

fn fake_cfg() -> AppConfig {
    AppConfig {
        mode: Mode::Standalone,
        private_key: None,
        miner_address: Some(Address::ZERO),
        rpc_urls: vec!["http://localhost".into()],
        rpc_submit_urls: vec!["http://localhost".into()],
        chain_id: 1,
        contract: Address::ZERO,
        gas_mode: GasMode::Balanced,
        priority_gwei: 0.2,
        max_fee_gwei_cap: 1.0,
        gas_limit_override: None,
        gas_limit_cap: 300_000,
        fee_bump_percent: 15,
        pending_replace_after_blocks: 2,
        target_safety_divisor: 4,
        state_check_interval_ms: 500,
        print_interval_sec: 5,
        gpu: false,
        gpu_indices: vec![0],
        gpu_batch: 1024,
        auto_tune_batch: true,
        local_size: 256,
        miner_id: "test".into(),
        coordinator_url: "http://localhost:8787".into(),
        worker_auth_token: None,
        coordinator_bind: "127.0.0.1:8787".into(),
        sqlite_path: ":memory:".into(),
        rpc_health_interval_sec: 10,
        rpc_timeout_sec: 1,
        rpc_max_block_lag: 1,
        metrics_bind: "127.0.0.1:9898".into(),
        enable_parallel_pending_tx: false,
        auto_tune_target_ms: 250,
        late_epoch_priority_gwei: 0.0,
        late_epoch_threshold_blocks: 5,
        skip_simulate_after_success: false,
    }
}
