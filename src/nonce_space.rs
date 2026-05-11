use alloy::primitives::{keccak256, B256, U256};
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NoncePrefix {
    pub high192: [u8; 24],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NonceAllocator {
    version: u16,
    miner_hash32: u32,
    boot_random48: u64,
    worker_device32: u32,
    epoch_hash64: u64,
    next_counter: u64,
}

impl NonceAllocator {
    pub fn new(miner_id: &str, device_id: u32, epoch: u64, challenge: &B256) -> Self {
        let digest = keccak256(miner_id.as_bytes());
        let miner_hash32 = u32::from_be_bytes(digest.as_slice()[0..4].try_into().unwrap());
        let mut rng = rand::thread_rng();
        let boot_random48 = rng.next_u64() & 0x0000_FFFF_FFFF_FFFF;
        Self::with_parts(1, miner_hash32, boot_random48, device_id, epoch, challenge)
    }

    pub fn with_parts(
        version: u16,
        miner_hash32: u32,
        boot_random48: u64,
        worker_device32: u32,
        epoch: u64,
        challenge: &B256,
    ) -> Self {
        let epoch_hash64 = epoch_challenge_hash64(epoch, challenge);
        Self {
            version,
            miner_hash32,
            boot_random48: boot_random48 & 0x0000_FFFF_FFFF_FFFF,
            worker_device32,
            epoch_hash64,
            next_counter: 0,
        }
    }

    pub fn prefix(&self) -> NoncePrefix {
        let mut b = [0u8; 24];
        b[0..2].copy_from_slice(&self.version.to_be_bytes());
        b[2..6].copy_from_slice(&self.miner_hash32.to_be_bytes());
        let boot = self.boot_random48.to_be_bytes();
        b[6..12].copy_from_slice(&boot[2..8]);
        b[12..16].copy_from_slice(&self.worker_device32.to_be_bytes());
        b[16..24].copy_from_slice(&self.epoch_hash64.to_be_bytes());
        NoncePrefix { high192: b }
    }

    pub fn next_batch(&mut self, batch_size: u64) -> NonceBatch {
        let base_counter64 = self.next_counter;
        self.next_counter = self.next_counter.wrapping_add(batch_size);
        NonceBatch {
            prefix: self.prefix(),
            base_counter64,
            batch_size,
        }
    }

    pub fn epoch_changed(&self, epoch: u64, challenge: &B256) -> bool {
        self.epoch_hash64 != epoch_challenge_hash64(epoch, challenge)
    }

    pub fn rebind_epoch(&mut self, epoch: u64, challenge: &B256) {
        let new_hash = epoch_challenge_hash64(epoch, challenge);
        if new_hash != self.epoch_hash64 {
            self.epoch_hash64 = new_hash;
            self.next_counter = 0;
        }
    }

    pub fn current_counter(&self) -> u64 {
        self.next_counter
    }

    pub fn set_counter(&mut self, c: u64) {
        self.next_counter = c;
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct NonceBatch {
    pub prefix: NoncePrefix,
    pub base_counter64: u64,
    pub batch_size: u64,
}

impl NonceBatch {
    pub fn pow_nonce(&self, offset: u64) -> U256 {
        pow_nonce_from_parts(self.prefix, self.base_counter64.wrapping_add(offset))
    }
}

pub fn pow_nonce_from_parts(prefix: NoncePrefix, counter64: u64) -> U256 {
    let mut b = [0u8; 32];
    b[..24].copy_from_slice(&prefix.high192);
    b[24..].copy_from_slice(&counter64.to_be_bytes());
    U256::from_be_bytes(b)
}

pub fn epoch_challenge_hash64(epoch: u64, challenge: &B256) -> u64 {
    let mut input = [0u8; 40];
    input[..8].copy_from_slice(&epoch.to_be_bytes());
    input[8..].copy_from_slice(challenge.as_slice());
    let digest = keccak256(input);
    u64::from_be_bytes(digest.as_slice()[0..8].try_into().unwrap())
}

pub fn prefix_hex(prefix: NoncePrefix) -> String {
    format!("0x{}", hex::encode(prefix.high192))
}

pub fn parse_prefix_hex(raw: &str) -> eyre::Result<NoncePrefix> {
    let s = raw.trim_start_matches("0x");
    let bytes = hex::decode(s)?;
    if bytes.len() != 24 {
        eyre::bail!("nonce_prefix_high_192 must be 24 bytes");
    }
    let mut high192 = [0u8; 24];
    high192.copy_from_slice(&bytes);
    Ok(NoncePrefix { high192 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pow_nonce_uses_full_uint256_space() {
        let p = NoncePrefix {
            high192: [0xabu8; 24],
        };
        let n = pow_nonce_from_parts(p, 0x1122_3344_5566_7788);
        let b = n.to_be_bytes::<32>();
        assert_eq!(&b[..24], &[0xabu8; 24]);
        assert_eq!(&b[24..], &0x1122_3344_5566_7788u64.to_be_bytes());
    }

    #[test]
    fn prefix_allocation_unique_by_device() {
        let c = B256::from([1u8; 32]);
        let a = NonceAllocator::with_parts(1, 2, 3, 4, 5, &c).prefix();
        let b = NonceAllocator::with_parts(1, 2, 3, 5, 5, &c).prefix();
        assert_ne!(a, b);
    }
}
