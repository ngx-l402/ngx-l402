//! Lightning melt fee-reserve calculation and sat/msat conversion.
//!
//! When redeeming Cashu proofs to Lightning, a fee reserve is held back so the
//! melt's actual routing fee is covered. The reserve is a percentage of the
//! amount, floored at a configured minimum. Get this wrong and every redemption
//! over- or under-reserves, so the calculation is a pure function pinned by
//! tests.

/// Millisatoshis per satoshi.
pub const MSAT_PER_SAT: u64 = 1_000;

/// Convert satoshis to millisatoshis, saturating instead of wrapping.
///
/// Several inputs to the melt loop come from the mint — `min_amount` and the
/// fee `min` in its `/info` response, the `fee_reserve` in a melt quote. A
/// wrapping `sats * 1000` would let a hostile or buggy mint turn an absurd
/// value into a small one and move a threshold the operator relies on.
///
/// Saturating is the safe direction here: every caller compares the result
/// against a balance, and `u64::MAX` fails that comparison, so an implausible
/// input stops the redemption rather than silently permitting one.
pub fn sat_to_msat(sats: u64) -> u64 {
    sats.saturating_mul(MSAT_PER_SAT)
}

/// Compute the Lightning fee reserve, in millisatoshis, to hold back from a
/// redemption of `amount_msat`: `percent`% of the amount, but never less than
/// `min_reserve_msat`. Matches the truncating `as u64` behaviour of the
/// production melt loop exactly.
pub fn fee_reserve_msat(amount_msat: u64, percent: f64, min_reserve_msat: u64) -> u64 {
    let percentage_fee = ((amount_msat as f64) * (percent / 100.0)) as u64;
    percentage_fee.max(min_reserve_msat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_applies_above_floor() {
        // 1% of 1_000_000 msat = 10_000, well above the 1_000 floor.
        assert_eq!(fee_reserve_msat(1_000_000, 1.0, 1_000), 10_000);
    }

    #[test]
    fn floor_applies_when_percentage_is_smaller() {
        // 1% of 10_000 = 100, below the 5_000 floor -> floor wins.
        assert_eq!(fee_reserve_msat(10_000, 1.0, 5_000), 5_000);
    }

    #[test]
    fn zero_percent_yields_the_floor() {
        assert_eq!(fee_reserve_msat(1_000_000, 0.0, 2_000), 2_000);
    }

    #[test]
    fn zero_amount_yields_the_floor() {
        assert_eq!(fee_reserve_msat(0, 1.0, 1_500), 1_500);
    }

    /// Fractional percentages truncate (not round) to match `as u64`.
    #[test]
    fn fractional_percent_truncates() {
        // 0.1% of 12_345 = 12.345 -> truncated to 12.
        assert_eq!(fee_reserve_msat(12_345, 0.1, 0), 12);
    }

    #[test]
    fn handles_large_amounts() {
        // 1% of 1 BTC (in msat) = 1_000_000_000 msat; no overflow, exact.
        assert_eq!(fee_reserve_msat(100_000_000_000, 1.0, 0), 1_000_000_000);
    }

    #[test]
    fn sat_to_msat_converts_exactly() {
        assert_eq!(sat_to_msat(0), 0);
        assert_eq!(sat_to_msat(1), 1_000);
        // 21M BTC in sats — the whole supply still converts without saturating.
        assert_eq!(
            sat_to_msat(2_100_000_000_000_000),
            2_100_000_000_000_000_000
        );
    }

    /// A hostile mint returning an absurd amount must not wrap to a small
    /// number: that would move a threshold the operator relies on. Saturating
    /// leaves a value no balance can satisfy, so redemption stops instead.
    #[test]
    fn sat_to_msat_saturates_rather_than_wrapping() {
        assert_eq!(sat_to_msat(u64::MAX), u64::MAX);
        assert_eq!(sat_to_msat(u64::MAX / 2), u64::MAX);
        // The wrapping result would have been a small number; prove it is not.
        assert!(sat_to_msat(u64::MAX) > 1_000_000);
    }
}
