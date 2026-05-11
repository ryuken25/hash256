use alloy::primitives::U256;

use crate::config::{AppConfig, GasMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasPlan {
    pub gas_limit: u64,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
}

pub fn gwei_to_wei(gwei: f64) -> u128 {
    (gwei * 1_000_000_000.0).round() as u128
}

pub fn plan_gas(cfg: &AppConfig, base_fee_per_gas: Option<u128>, estimate: Option<u64>) -> GasPlan {
    let gas_limit = cfg.gas_limit_override.unwrap_or_else(|| {
        let estimated = estimate.unwrap_or(200_000);
        (((estimated as f64) * 1.20).ceil() as u64).clamp(200_000, cfg.gas_limit_cap)
    });
    let priority = gwei_to_wei(cfg.priority_gwei);
    let cap = gwei_to_wei(cfg.max_fee_gwei_cap);
    let base_plus_tip = base_fee_per_gas.unwrap_or(0).saturating_add(priority);
    GasPlan {
        gas_limit: gas_limit.min(cfg.gas_limit_cap),
        max_priority_fee_per_gas: priority,
        max_fee_per_gas: cap.max(base_plus_tip).min(cap),
    }
}

pub fn bump_eip1559(old: GasPlan, bump_percent: u64, max_fee_cap_wei: u128) -> GasPlan {
    let mul = 100u128 + bump_percent as u128;
    let bumped_priority = old.max_priority_fee_per_gas.saturating_mul(mul) / 100u128;
    let bumped_max = old.max_fee_per_gas.saturating_mul(mul) / 100u128;
    GasPlan {
        gas_limit: old.gas_limit,
        max_priority_fee_per_gas: bumped_priority,
        max_fee_per_gas: bumped_max.min(max_fee_cap_wei).max(bumped_priority),
    }
}

pub fn defaults_for(mode: GasMode) -> (f64, f64) {
    mode.defaults()
}

#[allow(dead_code)]
pub fn u256_to_u128_saturating(v: U256) -> u128 {
    if v > U256::from(u128::MAX) {
        u128::MAX
    } else {
        v.to::<u128>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_modes_match_requested_defaults() {
        assert_eq!(defaults_for(GasMode::Cheap), (0.02, 0.5));
        assert_eq!(defaults_for(GasMode::Balanced), (0.2, 1.0));
        assert_eq!(defaults_for(GasMode::Turbo), (2.5, 5.0));
    }

    #[test]
    fn bump_increases_fees() {
        let old = GasPlan {
            gas_limit: 200_000,
            max_priority_fee_per_gas: 100,
            max_fee_per_gas: 1000,
        };
        let bumped = bump_eip1559(old, 15, 10_000);
        assert_eq!(bumped.max_priority_fee_per_gas, 115);
        assert_eq!(bumped.max_fee_per_gas, 1150);
    }
}
