//! The `l402_invoice_rate_limit` directive: parsing its value, and deriving the
//! Redis key its counter lives under.

/// Build the Redis key for one (client, route) invoice-rate bucket.
///
/// Both components are hashed, so the key length is fixed no matter what the
/// client sends. Hashing only the path — as this once did — left the address
/// half unbounded, and a multi-kilobyte header became a multi-kilobyte Redis
/// key, one per distinct value.
///
/// The two are separated by a NUL before hashing. Concatenating them directly
/// would make `("ab", "c")` and `("a", "bc")` collide into one bucket, letting
/// a crafted path share (and exhaust) another client's limit.
pub fn invoice_rate_limit_key(client: &str, path: &str) -> String {
    use cdk::secp256k1::hashes::{sha256, Hash};

    let mut buf = Vec::with_capacity(client.len() + path.len() + 1);
    buf.extend_from_slice(client.as_bytes());
    buf.push(0);
    buf.extend_from_slice(path.as_bytes());

    format!("l402:invoice_rate:{}", sha256::Hash::hash(&buf))
}

/// Parse a rate-limit directive value into `(max_requests, window_seconds)`.
/// Accepts `"<N>r/s"`, `"<N>r/m"`, `"<N>r/h"`, or a bare `"<N>"` (defaulting to a
/// 60-second window). Returns `None` on a malformed value — callers treat that
/// as "no limit configured", so a parse bug must be caught here rather than
/// silently disabling rate limiting.
pub fn parse_rate_limit(val: &str) -> Option<(u32, u64)> {
    let val = val.trim();
    if let Some(n) = val.strip_suffix("r/m") {
        n.trim().parse::<u32>().ok().map(|c| (c, 60))
    } else if let Some(n) = val.strip_suffix("r/h") {
        n.trim().parse::<u32>().ok().map(|c| (c, 3600))
    } else if let Some(n) = val.strip_suffix("r/s") {
        n.trim().parse::<u32>().ok().map(|c| (c, 1))
    } else {
        val.parse::<u32>().ok().map(|c| (c, 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_per_second_minute_hour() {
        assert_eq!(parse_rate_limit("5r/s"), Some((5, 1)));
        assert_eq!(parse_rate_limit("100r/m"), Some((100, 60)));
        assert_eq!(parse_rate_limit("1000r/h"), Some((1000, 3600)));
    }

    #[test]
    fn bare_number_defaults_to_per_minute() {
        assert_eq!(parse_rate_limit("30"), Some((30, 60)));
    }

    #[test]
    fn tolerates_surrounding_and_inner_whitespace() {
        assert_eq!(parse_rate_limit("  10 r/s "), Some((10, 1)));
    }

    /// A malformed value must return None (not a wrong limit) — callers read
    /// None as "unlimited", so this is the line between safe and silently broken.
    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_rate_limit(""), None);
        assert_eq!(parse_rate_limit("abc"), None);
        assert_eq!(parse_rate_limit("r/s"), None);
        assert_eq!(parse_rate_limit("-5r/m"), None); // u32 rejects negative
    }

    #[test]
    fn key_is_stable_for_the_same_inputs() {
        assert_eq!(
            invoice_rate_limit_key("1.2.3.4", "/protected"),
            invoice_rate_limit_key("1.2.3.4", "/protected")
        );
    }

    #[test]
    fn key_separates_clients_and_routes() {
        let a = invoice_rate_limit_key("1.2.3.4", "/protected");
        assert_ne!(a, invoice_rate_limit_key("1.2.3.5", "/protected"));
        assert_ne!(a, invoice_rate_limit_key("1.2.3.4", "/other"));
    }

    /// Without a separator, ("ab", "c") and ("a", "bc") hash identically and
    /// share one bucket — a crafted path could then exhaust another client's
    /// limit.
    #[test]
    fn key_is_unambiguous_across_the_boundary() {
        assert_ne!(
            invoice_rate_limit_key("ab", "c"),
            invoice_rate_limit_key("a", "bc")
        );
    }

    /// The whole point of hashing: an attacker-supplied component of any length
    /// yields a fixed-size key, so Redis memory per bucket is bounded.
    #[test]
    fn key_length_is_fixed_regardless_of_input_size() {
        let short = invoice_rate_limit_key("1.2.3.4", "/a");
        let huge = invoice_rate_limit_key(&"x".repeat(8192), &"y".repeat(8192));
        assert_eq!(short.len(), huge.len());
        assert!(short.starts_with("l402:invoice_rate:"));
    }
}
