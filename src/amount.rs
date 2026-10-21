// src/amount.rs
// Shared monetary bounds: reject impossible values instead of wrapping or saturating.

use anyhow::{ensure, Result};

pub const MAX_MSAT: i64 = 21_000_000 * 100_000_000 * 1000;

pub fn from_msat(value: u64) -> Option<i64> {
    // `try_from` checks the unsigned-to-signed conversion; `as` would wrap.
    let value = i64::try_from(value).ok()?;
    (value > 0 && value <= MAX_MSAT).then_some(value)
}

pub fn from_sats(value: u64) -> Option<i64> {
    from_msat(value.checked_mul(1000)?)
}

pub fn reserve_total(amount: i64, fee: i64) -> Result<i64> {
    ensure!(amount > 0 && amount <= MAX_MSAT, "invalid payment amount");
    ensure!(fee >= 0, "negative payment fee");
    let total = amount.checked_add(fee).filter(|n| *n <= MAX_MSAT);
    total.ok_or_else(|| anyhow::anyhow!("payment amount plus fee exceeds monetary bounds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_overflow_and_impossible_amounts() {
        assert_eq!(from_sats(1), Some(1000));
        assert_eq!(from_msat(MAX_MSAT as u64), Some(MAX_MSAT));
        for value in [0, MAX_MSAT as u64 + 1, i64::MAX as u64, u64::MAX] {
            assert_eq!(from_msat(value), None);
        }
        for value in [0, 9_223_372_036_854_776, u64::MAX] {
            assert_eq!(from_sats(value), None);
        }
        assert_eq!(reserve_total(1000, 5000).unwrap(), 6000);
        for (amount, fee) in [(0, 0), (-1, 0), (1, -1), (i64::MAX, 5000), (MAX_MSAT, 1)] {
            assert!(reserve_total(amount, fee).is_err());
        }
    }
}
