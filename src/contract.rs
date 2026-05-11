use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

sol! {
    #[sol(rpc)]
    contract HashToken {
        function currentDifficulty() external view returns (uint256);
        function totalMints() external view returns (uint256);
        function totalMiningMinted() external view returns (uint256);
        function genesisComplete() external view returns (bool);
        function getChallenge(address miner) external view returns (bytes32);
        function epochBlocksLeft() external view returns (uint256);
        function currentReward() external view returns (uint256);
        function miningState() external view returns (
            uint256 era,
            uint256 reward,
            uint256 difficulty,
            uint256 minted,
            uint256 remaining,
            uint256 epoch,
            uint256 epochBlocksLeft
        );
        function mine(uint256 nonce) external;
        function totalSupply() external view returns (uint256);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningStateView {
    pub era: U256,
    pub reward: U256,
    pub difficulty: U256,
    pub minted: U256,
    pub remaining: U256,
    pub epoch: U256,
    pub epoch_blocks_left: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkState {
    pub miner_address: Address,
    pub block_number: u64,
    pub epoch: u64,
    pub challenge: B256,
    pub target: U256,
    pub effective_target: U256,
    pub blocks_left: u64,
}

pub fn hash_nonce(challenge: &B256, pow_nonce: U256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(challenge.as_slice());
    buf[32..].copy_from_slice(&pow_nonce.to_be_bytes::<32>());
    keccak256(buf)
}

pub fn proof_is_valid(challenge: &B256, pow_nonce: U256, target: U256) -> bool {
    U256::from_be_bytes::<32>(hash_nonce(challenge, pow_nonce).0) < target
}

pub fn hash_below_target(hash: &B256, target: U256) -> bool {
    U256::from_be_bytes::<32>(hash.0) < target
}

pub fn mine_calldata(pow_nonce: U256) -> Vec<u8> {
    HashToken::mineCall { nonce: pow_nonce }.abi_encode()
}

pub fn hex_b256(v: &B256) -> String {
    format!("0x{}", hex::encode(v.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uint256_nonce_is_big_endian_in_hash_input() {
        let c = B256::from([0x11u8; 32]);
        let n = U256::from(0xdead_beefu64);
        let h1 = hash_nonce(&c, n);
        let mut manual = [0u8; 64];
        manual[..32].copy_from_slice(c.as_slice());
        manual[60..64].copy_from_slice(&0xdead_beefu32.to_be_bytes());
        assert_eq!(h1, keccak256(manual));
    }

    #[test]
    fn target_compare_is_strict() {
        let h = B256::from([0x00u8; 32]);
        assert!(hash_below_target(&h, U256::from(1u64)));
        assert!(!hash_below_target(&h, U256::ZERO));
    }

    #[test]
    fn calldata_selector_for_mine() {
        let data = mine_calldata(U256::from(7u64));
        assert_eq!(
            &data[..4],
            &keccak256("mine(uint256)".as_bytes()).as_slice()[..4]
        );
        assert_eq!(data.len(), 36);
    }
}
