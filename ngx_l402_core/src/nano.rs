//! Nano (XNO) payment verification — pure logic, no network.
//!
//! The module never moves funds. An unpaid request gets a 402 carrying a
//! challenge: an amount in raw (the route price plus a small per-challenge
//! tag) and an opaque challenge id. The client pays exactly that amount to the
//! operator's Nano address on its own, then retries with
//! `Authorization: Nano <challenge id>:<64-hex send block hash>`. The caller in
//! `lib.rs` checks the id with [`check_challenge`], asks a node for the block
//! (`block_info`) and hands the JSON reply to [`judge_block_info`], which
//! accepts it only when the block is a confirmed send to the operator for
//! exactly the challenge amount.
//!
//! Binding the payment to its challenge is what stops front-running: a send is
//! public on the ledger as soon as it is published, so a block hash alone is
//! not a secret. Only the client that received the 402 holds the challenge id,
//! and the id only redeems a block whose amount carries its own tag.
//!
//! The challenge id is stateless: an HMAC over the issue time, a random nonce,
//! the price and the request binding, under a key derived from `ROOT_KEY`, so
//! any worker can check it. The tag is derived from the nonce, so the id fixes
//! the amount and cannot be pointed at someone else's send.
//!
//! Prices stay in msat everywhere else in the module. The operator states a
//! fixed conversion (`NANO_RAW_PER_SAT`); with no rate there is no Nano offer,
//! so nothing here guesses an exchange rate.

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Nano's base32 alphabet (no `0`, `2`, `l`, `v`).
const ALPHABET: &[u8; 32] = b"13456789abcdefghijkmnopqrstuwxyz";

/// Characters after the prefix: 52 for the 256-bit key (4 pad bits) plus 8
/// for the 40-bit checksum.
const ADDRESS_BODY_LEN: usize = 60;

/// Header the 402 carries when Nano is offered.
pub const NANO_CHALLENGE_HEADER: &str = "X-Nano";

/// Header the 402 carries with the challenge id when Nano is offered.
pub const NANO_CHALLENGE_ID_HEADER: &str = "X-Nano-Challenge";

/// Authorization scheme a paying client retries with:
/// `Nano <challenge id>:<block hash>`.
pub const NANO_AUTH_SCHEME: &str = "Nano ";

/// Exclusive upper bound of the per-challenge tag added to the price, in raw.
/// 2^40 raw is about 10^-18 XNO, so the tag never changes what is paid in any
/// meaningful way, while two challenges for the same price share an amount
/// only with probability 2^-40.
pub const NANO_TAG_SPACE: u64 = 1 << 40;

/// Default lifetime of a challenge. A Nano send confirms in seconds.
pub const DEFAULT_NANO_CHALLENGE_TTL_SECS: u64 = 600;

/// Tolerated forward clock difference when checking an issue time, in seconds.
const CHALLENGE_CLOCK_SKEW_SECS: u64 = 5;

/// Why an operator-supplied Nano address was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidNanoAddress {
    Prefix,
    Length,
    Character,
    Padding,
    Checksum,
}

impl std::fmt::Display for InvalidNanoAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            InvalidNanoAddress::Prefix => "must start with nano_ or xrb_",
            InvalidNanoAddress::Length => "wrong length",
            InvalidNanoAddress::Character => "invalid character",
            InvalidNanoAddress::Padding => "non-zero padding bits",
            InvalidNanoAddress::Checksum => "checksum mismatch",
        };
        f.write_str(msg)
    }
}

fn checksum(public_key: &[u8; 32]) -> [u8; 5] {
    let mut out = [0u8; 5];
    // Blake2bVar accepts any output size from 1 to 64; 5 is always valid, and
    // `finalize_variable` fails only on a buffer of the wrong size, which `out`
    // is not. Both errors are therefore unreachable; an all-zero checksum
    // would simply never match a real address.
    if let Ok(mut hasher) = Blake2bVar::new(5) {
        hasher.update(public_key);
        let _ = hasher.finalize_variable(&mut out);
    }
    out.reverse();
    out
}

fn decode_base32(chars: &[u8], bits: usize) -> Result<Vec<u8>, InvalidNanoAddress> {
    // Collect the 5-bit groups as a bit string, then repack into bytes.
    let mut acc: Vec<bool> = Vec::with_capacity(chars.len() * 5);
    for c in chars {
        let v = ALPHABET
            .iter()
            .position(|a| a == c)
            .ok_or(InvalidNanoAddress::Character)?;
        for shift in (0..5).rev() {
            acc.push((v >> shift) & 1 == 1);
        }
    }
    let pad = acc.len() - bits;
    if acc[..pad].iter().any(|b| *b) {
        return Err(InvalidNanoAddress::Padding);
    }
    Ok(acc[pad..]
        .chunks(8)
        .map(|byte| byte.iter().fold(0u8, |a, b| (a << 1) | u8::from(*b)))
        .collect())
}

fn encode_base32(bytes: &[u8], pad_bits: usize) -> String {
    let mut bits: Vec<bool> = vec![false; pad_bits];
    for byte in bytes {
        for shift in (0..8).rev() {
            bits.push((byte >> shift) & 1 == 1);
        }
    }
    bits.chunks(5)
        .map(|g| {
            let idx = g.iter().fold(0usize, |a, b| (a << 1) | usize::from(*b));
            char::from(ALPHABET[idx])
        })
        .collect()
}

/// Validate a `nano_`/`xrb_` address and return its 32-byte public key.
pub fn parse_nano_address(address: &str) -> Result<[u8; 32], InvalidNanoAddress> {
    let body = address
        .strip_prefix("nano_")
        .or_else(|| address.strip_prefix("xrb_"))
        .ok_or(InvalidNanoAddress::Prefix)?;
    if body.len() != ADDRESS_BODY_LEN || !body.is_ascii() {
        return Err(InvalidNanoAddress::Length);
    }
    let (key_part, check_part) = body.as_bytes().split_at(52);
    let key_bytes = decode_base32(key_part, 256)?;
    let check_bytes = decode_base32(check_part, 40)?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);
    if checksum(&key)[..] != check_bytes[..] {
        return Err(InvalidNanoAddress::Checksum);
    }
    Ok(key)
}

/// Encode a public key as a `nano_` address.
pub fn encode_nano_address(public_key: &[u8; 32]) -> String {
    format!(
        "nano_{}{}",
        encode_base32(public_key, 4),
        encode_base32(&checksum(public_key), 0)
    )
}

/// Parse `NANO_RAW_PER_SAT`: a positive decimal integer of raw per sat.
pub fn parse_raw_per_sat(value: &str) -> Option<u128> {
    let v = value.trim();
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<u128>().ok().filter(|n| *n > 0)
}

/// Convert a price in msat to raw, rounding up so a fractional raw is never
/// waived. `None` for a non-positive price or on overflow.
pub fn msat_to_raw(amount_msat: i64, raw_per_sat: u128) -> Option<u128> {
    if amount_msat <= 0 || raw_per_sat == 0 {
        return None;
    }
    let scaled = (amount_msat as u128).checked_mul(raw_per_sat)?;
    Some(scaled.div_ceil(1000))
}

/// The `X-Nano` challenge value: `nano:<address>?amount=<raw>`.
pub fn nano_challenge(address: &str, amount_raw: u128) -> String {
    format!("nano:{}?amount={}", address, amount_raw)
}

/// Derive the challenge-signing key from the operator's `ROOT_KEY`, kept
/// apart from the macaroon key by a fixed label.
pub fn derive_challenge_key(root_key: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    // HMAC accepts a key of any length, so `new_from_slice` cannot fail; an
    // all-zero key on that unreachable path would only make every challenge
    // fail to verify, never pass.
    if let Ok(mut mac) = HmacSha256::new_from_slice(root_key) {
        Mac::update(&mut mac, b"ngx-l402 nano challenge key v1");
        out.copy_from_slice(&mac.finalize().into_bytes());
    }
    out
}

/// The per-challenge tag, in `1..=NANO_TAG_SPACE`, read from the nonce. Never
/// 0, so a send of the bare price never pays a challenge.
fn tag_from_nonce(nonce: &[u8; 16]) -> u128 {
    let mut v: u64 = 0;
    for b in &nonce[..8] {
        v = (v << 8) | u64::from(*b);
    }
    u128::from(v % NANO_TAG_SPACE) + 1
}

fn challenge_mac(
    key: &[u8; 32],
    issued_at: u64,
    nonce: &[u8; 16],
    price_raw: u128,
    binding: &[String],
) -> Option<HmacSha256> {
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    // Length-prefix every field so no two inputs encode the same bytes.
    let mut field = |bytes: &[u8]| {
        Mac::update(&mut mac, &(bytes.len() as u64).to_be_bytes());
        Mac::update(&mut mac, bytes);
    };
    field(b"ngx-l402 nano challenge v1");
    field(issued_at.to_string().as_bytes());
    field(nonce);
    field(price_raw.to_string().as_bytes());
    for part in binding {
        field(part.as_bytes());
    }
    Some(mac)
}

/// A challenge as issued in a 402, or as recovered from a valid id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NanoChallenge {
    /// Opaque id the client echoes back: `<issued_at>.<nonce hex>.<mac hex>`.
    pub id: String,
    /// Exact amount to send: the route price plus the tag.
    pub amount_raw: u128,
    pub issued_at: u64,
    /// Seconds since the epoch after which the id is refused.
    pub expires_at: u64,
    /// Replay key that marks this challenge as spent.
    pub replay_key: String,
}

fn challenge_replay_key(nonce: &[u8; 16]) -> String {
    format!("nano_challenge:{}", hex::encode(nonce))
}

/// Issue a challenge for a request with `binding` (the same strings the
/// macaroon caveats carry) priced at `price_raw`. `nonce` must be fresh random
/// bytes. `None` only if the amount would overflow.
pub fn issue_challenge(
    key: &[u8; 32],
    nonce: &[u8; 16],
    binding: &[String],
    price_raw: u128,
    now: u64,
    ttl_secs: u64,
) -> Option<NanoChallenge> {
    let amount_raw = price_raw.checked_add(tag_from_nonce(nonce))?;
    let mac = challenge_mac(key, now, nonce, price_raw, binding)?;
    let id = format!(
        "{}.{}.{}",
        now,
        hex::encode(nonce),
        hex::encode(mac.finalize().into_bytes())
    );
    Some(NanoChallenge {
        id,
        amount_raw,
        issued_at: now,
        expires_at: now.saturating_add(ttl_secs),
        replay_key: challenge_replay_key(nonce),
    })
}

/// Why a challenge id was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChallengeReject {
    Malformed,
    /// Not issued by this gateway, or for another request, binding or price.
    BadSignature,
    Expired,
}

impl std::fmt::Display for ChallengeReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChallengeReject::Malformed => "malformed challenge id",
            ChallengeReject::BadSignature => "challenge id not valid for this request",
            ChallengeReject::Expired => "challenge expired",
        })
    }
}

/// Check a challenge id presented on a request with `binding`, priced now at
/// `price_raw`, and recover what it asks for. The signature is compared in
/// constant time.
pub fn check_challenge(
    key: &[u8; 32],
    id: &str,
    binding: &[String],
    price_raw: u128,
    now: u64,
    ttl_secs: u64,
) -> Result<NanoChallenge, ChallengeReject> {
    let mut parts = id.split('.');
    let (Some(ts), Some(nonce_hex), Some(mac_hex), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ChallengeReject::Malformed);
    };
    if ts.is_empty() || ts.len() > 20 || !ts.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ChallengeReject::Malformed);
    }
    let issued_at = ts.parse::<u64>().map_err(|_| ChallengeReject::Malformed)?;
    let mut nonce = [0u8; 16];
    if nonce_hex.len() != 32 || hex::decode_to_slice(nonce_hex, &mut nonce).is_err() {
        return Err(ChallengeReject::Malformed);
    }
    let tag = hex::decode(mac_hex).map_err(|_| ChallengeReject::Malformed)?;
    if tag.len() != 32 {
        return Err(ChallengeReject::Malformed);
    }
    let mac = challenge_mac(key, issued_at, &nonce, price_raw, binding)
        .ok_or(ChallengeReject::BadSignature)?;
    mac.verify_slice(&tag)
        .map_err(|_| ChallengeReject::BadSignature)?;
    let expires_at = issued_at.saturating_add(ttl_secs);
    if now > expires_at || issued_at > now.saturating_add(CHALLENGE_CLOCK_SKEW_SECS) {
        return Err(ChallengeReject::Expired);
    }
    let amount_raw = price_raw
        .checked_add(tag_from_nonce(&nonce))
        .ok_or(ChallengeReject::Malformed)?;
    Ok(NanoChallenge {
        id: id.to_string(),
        amount_raw,
        issued_at,
        expires_at,
        replay_key: challenge_replay_key(&nonce),
    })
}

/// Split `Authorization: Nano <challenge id>:<block hash>` (scheme already
/// removed) into the id and the upper-cased hash.
pub fn parse_nano_credential(value: &str) -> Option<(&str, String)> {
    let (id, hash) = value.trim().rsplit_once(':')?;
    if id.is_empty() {
        return None;
    }
    Some((id, parse_block_hash(hash)?))
}

/// Validate a 64-hex block hash and return it upper-cased (the canonical form
/// node RPCs print), so `abc…` and `ABC…` claim the same replay key.
pub fn parse_block_hash(value: &str) -> Option<String> {
    let v = value.trim();
    if v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(v.to_ascii_uppercase())
    } else {
        None
    }
}

/// Replay key for a verified block. One send block buys one request, and a
/// block hash never expires, so neither may its marker.
pub fn nano_replay_key(block_hash_upper: &str) -> String {
    format!("nano:{}", block_hash_upper)
}

/// JSON body for the node's `block_info` RPC.
pub fn block_info_request(block_hash_upper: &str) -> String {
    serde_json::json!({
        "action": "block_info",
        "json_block": "true",
        "hash": block_hash_upper,
    })
    .to_string()
}

/// Why a `block_info` reply did not prove payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NanoReject {
    /// The node answered `{"error": ...}` (e.g. block not found).
    NodeError(String),
    /// The reply is not the shape `block_info` returns.
    Malformed(&'static str),
    Unconfirmed,
    NotSend,
    WrongDestination,
    /// The amount is not the one the challenge asked for. An amount that
    /// carries no outstanding tag pays no challenge, so it is refused even
    /// when it is larger than the price.
    AmountMismatch {
        paid_raw: u128,
        expected_raw: u128,
    },
}

impl std::fmt::Display for NanoReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NanoReject::NodeError(e) => write!(f, "node error: {}", e),
            NanoReject::Malformed(what) => write!(f, "malformed block_info reply: {}", what),
            NanoReject::Unconfirmed => f.write_str("block is not confirmed"),
            NanoReject::NotSend => f.write_str("block is not a send"),
            NanoReject::WrongDestination => f.write_str("block does not pay this address"),
            NanoReject::AmountMismatch {
                paid_raw,
                expected_raw,
            } => write!(
                f,
                "paid {} raw, challenge asks for exactly {} raw",
                paid_raw, expected_raw
            ),
        }
    }
}

/// Decide whether a `block_info` reply proves a payment of exactly
/// `expected_raw` (a challenge amount, see [`check_challenge`]) to
/// `operator_key`. Returns the amount paid in raw.
///
/// Every check must pass; anything unexpected is a rejection, never a pass.
pub fn judge_block_info(
    reply: &str,
    operator_key: &[u8; 32],
    expected_raw: u128,
) -> Result<u128, NanoReject> {
    let v: Value = serde_json::from_str(reply).map_err(|_| NanoReject::Malformed("not JSON"))?;
    if let Some(err) = v.get("error") {
        return Err(NanoReject::NodeError(
            err.as_str().unwrap_or("unknown").to_string(),
        ));
    }
    if v.get("confirmed").and_then(Value::as_str) != Some("true") {
        return Err(NanoReject::Unconfirmed);
    }
    if v.get("subtype").and_then(Value::as_str) != Some("send") {
        return Err(NanoReject::NotSend);
    }
    let destination = v
        .get("contents")
        .and_then(|c| c.get("link_as_account"))
        .and_then(Value::as_str)
        .ok_or(NanoReject::Malformed("missing contents.link_as_account"))?;
    // Compare keys, not strings: `nano_` and `xrb_` spell the same account.
    match parse_nano_address(destination) {
        Ok(key) if &key == operator_key => {}
        _ => return Err(NanoReject::WrongDestination),
    }
    let amount = v
        .get("amount")
        .and_then(Value::as_str)
        .ok_or(NanoReject::Malformed("missing amount"))?;
    if amount.is_empty() || !amount.bytes().all(|b| b.is_ascii_digit()) {
        return Err(NanoReject::Malformed("amount is not a decimal string"));
    }
    let paid_raw = amount
        .parse::<u128>()
        .map_err(|_| NanoReject::Malformed("amount out of range"))?;
    if paid_raw != expected_raw {
        return Err(NanoReject::AmountMismatch {
            paid_raw,
            expected_raw,
        });
    }
    Ok(paid_raw)
}

/// Operator configuration, read once from the environment at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NanoConfig {
    pub address: String,
    pub public_key: [u8; 32],
    pub raw_per_sat: u128,
    pub rpc_url: String,
    pub rpc_timeout_ms: u64,
    pub challenge_ttl_secs: u64,
}

/// Default bound on one `block_info` round-trip.
pub const DEFAULT_NANO_RPC_TIMEOUT_MS: u64 = 5_000;

impl NanoConfig {
    /// The raw price for a route, or `None` when it cannot be expressed.
    pub fn price_raw(&self, amount_msat: i64) -> Option<u128> {
        msat_to_raw(amount_msat, self.raw_per_sat)
    }

    /// Build from the `NANO_*` values. `Ok(None)` when `NANO_ADDRESS` is unset
    /// or empty: Nano is opt-in and absent by default. `Err` when it is set but
    /// the rest is unusable, so the operator hears why no offer appears.
    pub fn from_values(
        address: Option<&str>,
        raw_per_sat: Option<&str>,
        rpc_url: Option<&str>,
        rpc_timeout_ms: Option<&str>,
        challenge_ttl_secs: Option<&str>,
    ) -> Result<Option<NanoConfig>, String> {
        let address = match address.map(str::trim) {
            None | Some("") => return Ok(None),
            Some(a) => a.to_string(),
        };
        let public_key =
            parse_nano_address(&address).map_err(|e| format!("NANO_ADDRESS invalid: {}", e))?;
        let raw_per_sat = raw_per_sat
            .and_then(parse_raw_per_sat)
            .ok_or("NANO_RAW_PER_SAT must be set to a positive integer")?;
        let rpc_url = rpc_url
            .map(str::trim)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .ok_or("NANO_RPC_URL must be set to an http(s) URL")?
            .to_string();
        let rpc_timeout_ms = match rpc_timeout_ms.map(str::trim) {
            None | Some("") => DEFAULT_NANO_RPC_TIMEOUT_MS,
            Some(t) => t
                .parse::<u64>()
                .ok()
                .filter(|t| (1..=60_000).contains(t))
                .ok_or("NANO_RPC_TIMEOUT_MS must be between 1 and 60000")?,
        };
        let challenge_ttl_secs = match challenge_ttl_secs.map(str::trim) {
            None | Some("") => DEFAULT_NANO_CHALLENGE_TTL_SECS,
            Some(t) => t
                .parse::<u64>()
                .ok()
                .filter(|t| (30..=86_400).contains(t))
                .ok_or("NANO_CHALLENGE_TTL_SECS must be between 30 and 86400")?,
        };
        Ok(Some(NanoConfig {
            address,
            public_key,
            raw_per_sat,
            rpc_url,
            rpc_timeout_ms,
            challenge_ttl_secs,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Genesis account: a published address with a known public key.
    const GENESIS: &str = "nano_3t6k35gi95xu6tergt6p69ck76ogmitsa8mnijtpxm9fkcm736xtoncuohr3";
    const GENESIS_KEY: &str = "E89208DD038FBB269987689621D52292AE9C35941A7484756ECCED92A65093BA";
    // The all-zero key ("burn" address).
    const BURN: &str = "nano_1111111111111111111111111111111111111111111111111111hifc8npp";

    fn genesis_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        k.copy_from_slice(&hex::decode(GENESIS_KEY).unwrap());
        k
    }

    fn other_address() -> String {
        encode_nano_address(&[7u8; 32])
    }

    fn reply(confirmed: &str, subtype: &str, dest: &str, amount: &str) -> String {
        serde_json::json!({
            "block_account": other_address(),
            "amount": amount,
            "confirmed": confirmed,
            "subtype": subtype,
            "contents": {
                "type": "state",
                "link_as_account": dest,
            },
        })
        .to_string()
    }

    #[test]
    fn known_addresses_decode_to_their_keys() {
        assert_eq!(parse_nano_address(GENESIS).unwrap(), genesis_key());
        assert_eq!(parse_nano_address(BURN).unwrap(), [0u8; 32]);
    }

    #[test]
    fn xrb_prefix_is_the_same_account() {
        let xrb = GENESIS.replacen("nano_", "xrb_", 1);
        assert_eq!(parse_nano_address(&xrb).unwrap(), genesis_key());
    }

    #[test]
    fn encode_round_trips() {
        assert_eq!(encode_nano_address(&genesis_key()), GENESIS);
        assert_eq!(encode_nano_address(&[0u8; 32]), BURN);
        let k = [0xA5u8; 32];
        assert_eq!(parse_nano_address(&encode_nano_address(&k)).unwrap(), k);
    }

    #[test]
    fn bad_checksum_is_rejected() {
        // Change one checksum character.
        let mut bad = GENESIS.to_string();
        bad.pop();
        bad.push('4');
        assert_eq!(parse_nano_address(&bad), Err(InvalidNanoAddress::Checksum));
        // Change one key character: the checksum no longer matches.
        let bad_key = GENESIS.replacen("3t6k", "3t6m", 1);
        assert_eq!(
            parse_nano_address(&bad_key),
            Err(InvalidNanoAddress::Checksum)
        );
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        assert_eq!(
            parse_nano_address("ban_1111"),
            Err(InvalidNanoAddress::Prefix)
        );
        assert_eq!(
            parse_nano_address("nano_1111"),
            Err(InvalidNanoAddress::Length)
        );
        let with_zero = GENESIS.replacen("3t6k", "0t6k", 1);
        assert_eq!(
            parse_nano_address(&with_zero),
            Err(InvalidNanoAddress::Character)
        );
        // The first character carries the 4 pad bits; only '1' or '3' leave them clear.
        let padded = GENESIS.replacen("nano_3", "nano_z", 1);
        assert_eq!(
            parse_nano_address(&padded),
            Err(InvalidNanoAddress::Padding)
        );
    }

    #[test]
    fn rate_parsing() {
        assert_eq!(parse_raw_per_sat("1000"), Some(1000));
        assert_eq!(parse_raw_per_sat(" 42 "), Some(42));
        assert_eq!(parse_raw_per_sat("0"), None);
        assert_eq!(parse_raw_per_sat("-5"), None);
        assert_eq!(parse_raw_per_sat("1e3"), None);
        assert_eq!(parse_raw_per_sat(""), None);
    }

    #[test]
    fn msat_to_raw_rounds_up() {
        // 1 sat = 1_000 msat at 10^24 raw/sat.
        let rate = 1_000_000_000_000_000_000_000_000u128;
        assert_eq!(msat_to_raw(1_000, rate), Some(rate));
        assert_eq!(msat_to_raw(10_000, 3), Some(30));
        // 1 msat at 3 raw/sat is 0.003 raw: charged 1, never 0.
        assert_eq!(msat_to_raw(1, 3), Some(1));
        assert_eq!(msat_to_raw(0, 3), None);
        assert_eq!(msat_to_raw(-1, 3), None);
        assert_eq!(msat_to_raw(i64::MAX, u128::MAX), None);
    }

    #[test]
    fn challenge_format() {
        assert_eq!(
            nano_challenge(GENESIS, 30),
            format!("nano:{}?amount=30", GENESIS)
        );
    }

    #[test]
    fn block_hash_validation() {
        let h = "a".repeat(64);
        assert_eq!(parse_block_hash(&h), Some("A".repeat(64)));
        assert_eq!(parse_block_hash(&"g".repeat(64)), None);
        assert_eq!(parse_block_hash(&"a".repeat(63)), None);
        assert_eq!(parse_block_hash(&"a".repeat(65)), None);
        assert_eq!(parse_block_hash(""), None);
        assert_eq!(
            nano_replay_key(&"A".repeat(64)),
            format!("nano:{}", "A".repeat(64))
        );
    }

    #[test]
    fn replay_key_is_case_insensitive_in_the_hash() {
        let lower = parse_block_hash(&"ab".repeat(32)).unwrap();
        let upper = parse_block_hash(&"AB".repeat(32)).unwrap();
        assert_eq!(nano_replay_key(&lower), nano_replay_key(&upper));
    }

    #[test]
    fn request_body_is_block_info() {
        let body: Value = serde_json::from_str(&block_info_request(&"A".repeat(64))).unwrap();
        assert_eq!(body["action"], "block_info");
        assert_eq!(body["json_block"], "true");
        assert_eq!(body["hash"], "A".repeat(64));
    }

    #[test]
    fn confirmed_send_to_operator_for_the_exact_amount_is_accepted() {
        let r = reply("true", "send", GENESIS, "30");
        assert_eq!(judge_block_info(&r, &genesis_key(), 30), Ok(30));
        // xrb_ spelling of the operator's account still matches.
        let xrb = GENESIS.replacen("nano_", "xrb_", 1);
        let r = reply("true", "send", &xrb, "30");
        assert_eq!(judge_block_info(&r, &genesis_key(), 30), Ok(30));
    }

    #[test]
    fn unconfirmed_is_rejected() {
        let r = reply("false", "send", GENESIS, "30");
        assert_eq!(
            judge_block_info(&r, &genesis_key(), 30),
            Err(NanoReject::Unconfirmed)
        );
        // A boolean, or a missing field, is not the string the RPC returns.
        let mut v: Value = serde_json::from_str(&reply("true", "send", GENESIS, "30")).unwrap();
        v["confirmed"] = Value::Bool(true);
        assert_eq!(
            judge_block_info(&v.to_string(), &genesis_key(), 30),
            Err(NanoReject::Unconfirmed)
        );
        v.as_object_mut().unwrap().remove("confirmed");
        assert_eq!(
            judge_block_info(&v.to_string(), &genesis_key(), 30),
            Err(NanoReject::Unconfirmed)
        );
    }

    #[test]
    fn receive_and_other_subtypes_are_rejected() {
        for subtype in ["receive", "open", "change", "epoch", ""] {
            let r = reply("true", subtype, GENESIS, "30");
            assert_eq!(
                judge_block_info(&r, &genesis_key(), 30),
                Err(NanoReject::NotSend),
                "subtype {subtype:?}"
            );
        }
    }

    #[test]
    fn wrong_destination_is_rejected() {
        let r = reply("true", "send", &other_address(), "30");
        assert_eq!(
            judge_block_info(&r, &genesis_key(), 30),
            Err(NanoReject::WrongDestination)
        );
        // A destination that does not even parse is also not ours.
        let r = reply("true", "send", "nano_garbage", "30");
        assert_eq!(
            judge_block_info(&r, &genesis_key(), 30),
            Err(NanoReject::WrongDestination)
        );
    }

    #[test]
    fn any_amount_but_the_challenge_amount_is_rejected() {
        for (paid, paid_raw) in [("29", 29u128), ("31", 31), ("3000", 3000)] {
            let r = reply("true", "send", GENESIS, paid);
            assert_eq!(
                judge_block_info(&r, &genesis_key(), 30),
                Err(NanoReject::AmountMismatch {
                    paid_raw,
                    expected_raw: 30
                }),
                "paid {paid}"
            );
        }
    }

    // ── Challenge binding ────────────────────────────────────────────────

    const KEY: [u8; 32] = [0x42; 32];
    const NOW: u64 = 1_800_000_000;
    const TTL: u64 = DEFAULT_NANO_CHALLENGE_TTL_SECS;
    const PRICE: u128 = 1_000_000;

    fn binding(path: &str) -> Vec<String> {
        vec![
            format!("RequestPath = {path}"),
            "RequestMethod = GET".to_string(),
        ]
    }

    fn issue(nonce: u8) -> NanoChallenge {
        issue_challenge(&KEY, &[nonce; 16], &binding("/paid"), PRICE, NOW, TTL).unwrap()
    }

    #[test]
    fn challenge_amount_is_price_plus_a_nonzero_tag() {
        for nonce in [0u8, 1, 0x7f, 0xff] {
            let c = issue(nonce);
            let tag = c.amount_raw - PRICE;
            assert!(tag >= 1 && tag <= u128::from(NANO_TAG_SPACE), "tag {tag}");
        }
        // Distinct nonces give distinct amounts, so each send names one challenge.
        assert_ne!(issue(1).amount_raw, issue(2).amount_raw);
        assert_ne!(issue(1).id, issue(2).id);
        assert_ne!(issue(1).replay_key, issue(2).replay_key);
        assert!(issue_challenge(&KEY, &[9; 16], &binding("/p"), u128::MAX, NOW, TTL).is_none());
    }

    #[test]
    fn issued_challenge_checks_back_to_the_same_terms() {
        let c = issue(3);
        let back = check_challenge(&KEY, &c.id, &binding("/paid"), PRICE, NOW + 10, TTL).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.expires_at, NOW + TTL);
    }

    #[test]
    fn expired_challenge_is_refused() {
        let c = issue(4);
        let at_expiry = check_challenge(&KEY, &c.id, &binding("/paid"), PRICE, NOW + TTL, TTL);
        assert!(at_expiry.is_ok());
        assert_eq!(
            check_challenge(&KEY, &c.id, &binding("/paid"), PRICE, NOW + TTL + 1, TTL),
            Err(ChallengeReject::Expired)
        );
        // An id dated in the future is not honoured either.
        let future =
            issue_challenge(&KEY, &[5; 16], &binding("/paid"), PRICE, NOW + 3600, TTL).unwrap();
        assert_eq!(
            check_challenge(&KEY, &future.id, &binding("/paid"), PRICE, NOW, TTL),
            Err(ChallengeReject::Expired)
        );
    }

    #[test]
    fn challenge_is_bound_to_request_price_and_key() {
        let c = issue(6);
        // Another path or method.
        assert_eq!(
            check_challenge(&KEY, &c.id, &binding("/other"), PRICE, NOW, TTL),
            Err(ChallengeReject::BadSignature)
        );
        let post = vec![
            "RequestPath = /paid".to_string(),
            "RequestMethod = POST".to_string(),
        ];
        assert_eq!(
            check_challenge(&KEY, &c.id, &post, PRICE, NOW, TTL),
            Err(ChallengeReject::BadSignature)
        );
        // A route whose price changed since the 402.
        assert_eq!(
            check_challenge(&KEY, &c.id, &binding("/paid"), PRICE + 1, NOW, TTL),
            Err(ChallengeReject::BadSignature)
        );
        // Another gateway's key.
        assert_eq!(
            check_challenge(&[0x43; 32], &c.id, &binding("/paid"), PRICE, NOW, TTL),
            Err(ChallengeReject::BadSignature)
        );
    }

    #[test]
    fn forged_or_altered_challenge_ids_are_refused() {
        let c = issue(7);
        let parts: Vec<&str> = c.id.split('.').collect();
        // Swap in another nonce (another tag, so another amount): signature breaks.
        let other_nonce = format!("{}.{}.{}", parts[0], "08".repeat(16), parts[2]);
        assert_eq!(
            check_challenge(&KEY, &other_nonce, &binding("/paid"), PRICE, NOW, TTL),
            Err(ChallengeReject::BadSignature)
        );
        // Re-date it to dodge expiry: signature breaks.
        let redated = format!("{}.{}.{}", NOW + 1, parts[1], parts[2]);
        assert_eq!(
            check_challenge(&KEY, &redated, &binding("/paid"), PRICE, NOW, TTL),
            Err(ChallengeReject::BadSignature)
        );
        for bad in [
            "",
            "garbage",
            "1.2",
            "1.2.3.4",
            &format!("x.{}.{}", parts[1], parts[2]),
            &format!("{}.{}.{}", parts[0], "zz".repeat(16), parts[2]),
            &format!("{}.{}.{}", parts[0], parts[1], "ab"),
            &format!("{}.{}.{}", "9".repeat(21), parts[1], parts[2]),
        ] {
            assert_eq!(
                check_challenge(&KEY, bad, &binding("/paid"), PRICE, NOW, TTL),
                Err(ChallengeReject::Malformed),
                "id {bad:?}"
            );
        }
    }

    /// The front-run this binding exists to stop: an observer sees a confirmed
    /// send to the operator on the ledger and races the payer with its hash.
    /// The observer's own challenge asks for a different amount, so the
    /// payer's block does not pay it; and it cannot present the payer's
    /// challenge, whose id it never saw.
    #[test]
    fn observed_send_does_not_pay_an_observers_challenge() {
        let payer = issue(10);
        let observer = issue(11);
        let payers_block = reply("true", "send", GENESIS, &payer.amount_raw.to_string());
        let observers_terms =
            check_challenge(&KEY, &observer.id, &binding("/paid"), PRICE, NOW + 1, TTL).unwrap();
        assert_eq!(
            judge_block_info(&payers_block, &genesis_key(), observers_terms.amount_raw),
            Err(NanoReject::AmountMismatch {
                paid_raw: payer.amount_raw,
                expected_raw: observer.amount_raw
            })
        );
        // A send of the bare price (no tag) pays no challenge at all.
        let untagged = reply("true", "send", GENESIS, &PRICE.to_string());
        assert!(matches!(
            judge_block_info(&untagged, &genesis_key(), payer.amount_raw),
            Err(NanoReject::AmountMismatch { .. })
        ));
        // The payer's own id and block go through.
        let payers_terms =
            check_challenge(&KEY, &payer.id, &binding("/paid"), PRICE, NOW + 1, TTL).unwrap();
        assert_eq!(
            judge_block_info(&payers_block, &genesis_key(), payers_terms.amount_raw),
            Ok(payer.amount_raw)
        );
    }

    #[test]
    fn credential_parsing() {
        let c = issue(12);
        let hash = "ab".repeat(32);
        let cred = format!("{}:{}", c.id, hash);
        let (id, h) = parse_nano_credential(&cred).unwrap();
        assert_eq!(id, c.id);
        assert_eq!(h, "AB".repeat(32));
        // A bare hash (the old form) carries no challenge and is refused.
        assert_eq!(parse_nano_credential(&hash), None);
        assert_eq!(parse_nano_credential(&format!(":{hash}")), None);
        assert_eq!(parse_nano_credential(&format!("{}:{}", c.id, "zz")), None);
    }

    #[test]
    fn challenge_key_is_derived_and_separated() {
        let a = derive_challenge_key(b"0123456789abcdef0123456789abcdef");
        let b = derive_challenge_key(b"0123456789abcdef0123456789abcdeX");
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
        assert_eq!(a, derive_challenge_key(b"0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn bad_amounts_are_rejected() {
        for amount in [
            "",
            "-1",
            "1.5",
            "0x10",
            "1e30",
            "999999999999999999999999999999999999999999",
        ] {
            let r = reply("true", "send", GENESIS, amount);
            assert!(
                matches!(
                    judge_block_info(&r, &genesis_key(), 1),
                    Err(NanoReject::Malformed(_))
                ),
                "amount {amount:?}"
            );
        }
        // A numeric (not string) amount is not what the RPC returns.
        let mut v: Value = serde_json::from_str(&reply("true", "send", GENESIS, "30")).unwrap();
        v["amount"] = serde_json::json!(30);
        assert!(matches!(
            judge_block_info(&v.to_string(), &genesis_key(), 30),
            Err(NanoReject::Malformed(_))
        ));
    }

    #[test]
    fn node_errors_and_garbage_are_rejected() {
        assert_eq!(
            judge_block_info(r#"{"error":"Block not found"}"#, &genesis_key(), 1),
            Err(NanoReject::NodeError("Block not found".to_string()))
        );
        assert!(matches!(
            judge_block_info("<html>502</html>", &genesis_key(), 1),
            Err(NanoReject::Malformed(_))
        ));
        assert!(matches!(
            judge_block_info("", &genesis_key(), 1),
            Err(NanoReject::Malformed(_))
        ));
        // json_block=false shape: contents is a string, not an object.
        let v = serde_json::json!({
            "amount": "30", "confirmed": "true", "subtype": "send",
            "contents": "{\"link_as_account\":\"x\"}",
        });
        assert!(matches!(
            judge_block_info(&v.to_string(), &genesis_key(), 30),
            Err(NanoReject::Malformed(_))
        ));
    }

    #[test]
    fn unset_address_means_no_offer() {
        assert_eq!(
            NanoConfig::from_values(None, None, None, None, None),
            Ok(None)
        );
        assert_eq!(
            NanoConfig::from_values(Some("  "), Some("1000"), Some("http://n"), None, None),
            Ok(None)
        );
    }

    #[test]
    fn config_requires_valid_address_rate_and_rpc() {
        let url = Some("http://127.0.0.1:7076");
        let ok = NanoConfig::from_values(Some(GENESIS), Some("1000"), url, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(ok.public_key, genesis_key());
        assert_eq!(ok.raw_per_sat, 1000);
        assert_eq!(ok.rpc_timeout_ms, DEFAULT_NANO_RPC_TIMEOUT_MS);
        assert_eq!(ok.challenge_ttl_secs, DEFAULT_NANO_CHALLENGE_TTL_SECS);
        assert_eq!(ok.price_raw(10_000), Some(10_000));

        let mut bad = GENESIS.to_string();
        bad.pop();
        bad.push('4');
        assert!(NanoConfig::from_values(Some(&bad), Some("1000"), url, None, None).is_err());
        assert!(NanoConfig::from_values(Some(GENESIS), None, url, None, None).is_err());
        assert!(NanoConfig::from_values(Some(GENESIS), Some("0"), url, None, None).is_err());
        assert!(NanoConfig::from_values(Some(GENESIS), Some("1000"), None, None, None).is_err());
        assert!(
            NanoConfig::from_values(Some(GENESIS), Some("1000"), Some("ftp://x"), None, None)
                .is_err()
        );
        assert!(
            NanoConfig::from_values(Some(GENESIS), Some("1000"), url, Some("0"), None).is_err()
        );
        assert_eq!(
            NanoConfig::from_values(Some(GENESIS), Some("1000"), url, Some("250"), None)
                .unwrap()
                .unwrap()
                .rpc_timeout_ms,
            250
        );
        assert!(
            NanoConfig::from_values(Some(GENESIS), Some("1000"), url, None, Some("5")).is_err()
        );
        assert_eq!(
            NanoConfig::from_values(Some(GENESIS), Some("1000"), url, None, Some("120"))
                .unwrap()
                .unwrap()
                .challenge_ttl_secs,
            120
        );
    }
}
