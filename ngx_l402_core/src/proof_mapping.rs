//! Lifetime of a Cashu proof-to-LNURL mapping in Redis.
//!
//! Multi-tenant redemption records which LNURL address each received proof
//! belongs to. A mapping that expires before its redemption cycle runs sends
//! that tenant's proofs to the default address, so the lifetime follows the
//! redemption interval instead of being fixed.

const DAY: u64 = 86_400;

/// Seconds a proof-to-LNURL mapping should live, given the redemption interval
/// (`0` when redemption is off).
///
/// Twenty intervals, floored at a day and capped at thirty days so abandoned
/// mappings still expire, but never less than two intervals: past about 1.5
/// days the cap alone eats into that margin, and past 30 days it would expire a
/// mapping before its cycle.
pub fn proof_mapping_ttl_secs(redemption_interval_secs: u64) -> u64 {
    redemption_interval_secs
        .saturating_mul(20)
        .clamp(DAY, 30 * DAY)
        .max(redemption_interval_secs.saturating_mul(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_or_disabled_intervals_get_a_day() {
        assert_eq!(proof_mapping_ttl_secs(0), DAY);
        // Twenty hourly cycles is 20h, under the floor.
        assert_eq!(proof_mapping_ttl_secs(3600), DAY);
    }

    #[test]
    fn typical_intervals_get_twenty_cycles() {
        assert_eq!(proof_mapping_ttl_secs(6 * 3600), 5 * DAY);
        assert_eq!(proof_mapping_ttl_secs(DAY), 20 * DAY);
    }

    #[test]
    fn the_cap_never_undercuts_two_intervals() {
        // 1.5 days reaches the cap exactly.
        assert_eq!(proof_mapping_ttl_secs(DAY * 3 / 2), 30 * DAY);
        assert_eq!(proof_mapping_ttl_secs(10 * DAY), 30 * DAY);
        assert_eq!(proof_mapping_ttl_secs(20 * DAY), 40 * DAY);
        assert_eq!(proof_mapping_ttl_secs(u64::MAX), u64::MAX);
    }
}
