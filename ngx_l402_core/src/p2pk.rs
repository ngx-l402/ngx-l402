//! P2PK (NUT-11 / NUT-12) key handling.
//!
//! In P2PK mode the module locks Cashu proofs to a public key and later signs
//! with the corresponding private key to melt them. If the public key we publish
//! (in NUT-18 payment requests) and the private key we sign with ever disagree,
//! the locked proofs become permanently unspendable. To make that impossible we
//! derive *both* from a single parse here, and pin the derivation with well-known
//! secp256k1 golden vectors.
//!
//! The P2PK fast path stores incoming proofs without a mint swap, so
//! [`verify_permanent_p2pk_lock`] checks that only we can ever spend them.

use cdk::nuts::{Kind, Nut10Secret, Proof, SecretKey};
use std::fmt;

/// The configured P2PK private key was not a valid secp256k1 secret key (bad
/// hex, wrong length, zero, or out of range). Returned instead of proceeding
/// with a key that cannot lock/unlock proofs.
#[derive(Debug, Clone)]
pub struct InvalidP2pkKey(String);

impl fmt::Display for InvalidP2pkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid P2PK private key: {}", self.0)
    }
}

impl std::error::Error for InvalidP2pkKey {}

/// Parse a hex-encoded secp256k1 private key for Cashu P2PK and return both the
/// parsed key (kept for signing during melt) and its compressed public-key hex
/// (published in NUT-18 payment requests). Both come from the SAME parse so the
/// signing key and the locking public key can never disagree. Surrounding
/// whitespace is ignored.
pub fn parse_p2pk_secret_key(secret_hex: &str) -> Result<(SecretKey, String), InvalidP2pkKey> {
    let secret_key =
        SecretKey::from_hex(secret_hex.trim()).map_err(|e| InvalidP2pkKey(format!("{}", e)))?;
    let public_key_hex = secret_key.public_key().to_string();
    Ok((secret_key, public_key_hex))
}

/// Accept only a P2PK lock with no locktime.
///
/// Proofs are stored and redeemed later, so the lock must still be ours then.
/// Under NUT-11, once a locktime passes the proof can be spent by its refund
/// keys, or by anyone if it has none. A future locktime can pass before
/// redemption too, so any locktime is rejected. An HTLC is rejected because it
/// needs a preimage we don't have.
///
/// cdk's `Wallet::verify_token_p2pk` still checks the locking key, signature
/// count and refund keys, but it only checks the locktime when refund keys are
/// present.
pub fn verify_permanent_p2pk_lock(proof: &Proof) -> Result<(), String> {
    let secret = Nut10Secret::try_from(&proof.secret)
        .map_err(|e| format!("Proof secret is not a NUT-10 spending condition: {}", e))?;

    if secret.kind() != Kind::P2PK {
        return Err(format!(
            "Proof is locked with {:?}, only P2PK is accepted",
            secret.kind()
        ));
    }

    let has_locktime = secret
        .secret_data()
        .tags()
        .into_iter()
        .flatten()
        .any(|tag| {
            tag.first()
                .is_some_and(|key| key.eq_ignore_ascii_case("locktime"))
        });
    if has_locktime {
        return Err("Proof's P2PK lock has a locktime, which is not accepted".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::nuts::SecretData;

    // Well-known secp256k1 vectors: a private key `k` maps to the compressed
    // public key of `k*G`. k = 1 is the generator point G; k = 2 is 2G. These are
    // stable across every correct secp256k1 implementation, so they pin both the
    // curve math and the compressed-hex encoding cdk uses for NUT-11.
    const PRIV_ONE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    const PUB_ONE: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    const PRIV_TWO: &str = "0000000000000000000000000000000000000000000000000000000000000002";
    const PUB_TWO: &str = "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";

    #[test]
    fn derives_known_public_keys() {
        let (_, pub1) = parse_p2pk_secret_key(PRIV_ONE).expect("valid key");
        assert_eq!(pub1, PUB_ONE);
        let (_, pub2) = parse_p2pk_secret_key(PRIV_TWO).expect("valid key");
        assert_eq!(pub2, PUB_TWO);
    }

    #[test]
    fn public_key_is_33_byte_compressed_hex() {
        let (_, pubkey) = parse_p2pk_secret_key(PRIV_ONE).unwrap();
        assert_eq!(pubkey.len(), 66); // 33 bytes, hex-encoded
        assert!(pubkey.starts_with("02") || pubkey.starts_with("03"));
    }

    /// The returned signing key and the returned public key must always be
    /// consistent — this is the property that prevents unspendable locked proofs.
    #[test]
    fn returned_key_matches_returned_pubkey() {
        let (sk, pubkey) = parse_p2pk_secret_key(PRIV_ONE).unwrap();
        assert_eq!(sk.public_key().to_string(), pubkey);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        let (_, a) = parse_p2pk_secret_key(PRIV_ONE).unwrap();
        let (_, b) = parse_p2pk_secret_key(&format!("  {}\n", PRIV_ONE)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn invalid_keys_are_rejected() {
        assert!(parse_p2pk_secret_key("").is_err());
        assert!(parse_p2pk_secret_key("xyz").is_err());
        assert!(parse_p2pk_secret_key("01").is_err()); // too short
                                                       // Zero scalar is not a valid secp256k1 secret key.
        assert!(parse_p2pk_secret_key(
            "0000000000000000000000000000000000000000000000000000000000000000"
        )
        .is_err());
        // A value >= the curve order n is invalid.
        assert!(parse_p2pk_secret_key(
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        )
        .is_err());
    }

    // A proof with a plain secret, from cdk's nut12 test vectors. Tests replace
    // the secret with the lock they need.
    const UNLOCKED_PROOF_JSON: &str = r#"{"amount": 1,"id": "00882760bfa2eb41","secret": "daf4dd00a2b68a0858a80450f52c8a7d2ccf87d375e43e216e0c571f089f63e9","C": "024369d2d22a80ecf78f3937da9d5f30c1b9f74f0c32684d583cca0fa6a61cdcfc"}"#;

    // 2023-11-14.
    const PAST_LOCKTIME: u64 = 1_700_000_000;

    /// A proof locked as `kind` to `data` with `tags`. The secret is built
    /// directly because `Conditions::new` refuses a past locktime.
    fn locked_proof(kind: Kind, data: &str, tags: &[&[&str]]) -> Proof {
        let tags: Option<Vec<Vec<String>>> = (!tags.is_empty()).then(|| {
            tags.iter()
                .map(|tag| tag.iter().map(|s| s.to_string()).collect())
                .collect()
        });
        let mut proof: Proof = serde_json::from_str(UNLOCKED_PROOF_JSON).unwrap();
        proof.secret = Nut10Secret::new(kind, SecretData::new(data, tags))
            .try_into()
            .unwrap();
        proof
    }

    #[test]
    fn accepts_plain_p2pk_lock() {
        assert_eq!(
            verify_permanent_p2pk_lock(&locked_proof(Kind::P2PK, PUB_ONE, &[])),
            Ok(())
        );
        // Tags other than locktime don't weaken the lock.
        let proof = locked_proof(Kind::P2PK, PUB_ONE, &[&["sigflag", "SIG_INPUTS"]]);
        assert_eq!(verify_permanent_p2pk_lock(&proof), Ok(()));
    }

    /// A past locktime with no refund keys: the mint lets anyone spend this
    /// proof without a signature.
    #[test]
    fn rejects_expired_locktime_without_refund_keys() {
        let locktime = PAST_LOCKTIME.to_string();
        let proof = locked_proof(Kind::P2PK, PUB_ONE, &[&["locktime", &locktime]]);
        assert!(
            proof.verify_p2pk().is_ok(),
            "the mint's own check treats this proof as anyone-can-spend"
        );
        let err = verify_permanent_p2pk_lock(&proof).expect_err("expired lock accepted");
        assert!(err.contains("locktime"), "unexpected error: {}", err);
    }

    /// A future locktime can pass before the proof is redeemed.
    #[test]
    fn rejects_future_locktime() {
        let locktime = (cdk::util::unix_time() + 60).to_string();
        let proof = locked_proof(Kind::P2PK, PUB_ONE, &[&["locktime", &locktime]]);
        assert!(verify_permanent_p2pk_lock(&proof).is_err());
    }

    #[test]
    fn rejects_locktime_with_refund_keys() {
        let locktime = PAST_LOCKTIME.to_string();
        let proof = locked_proof(
            Kind::P2PK,
            PUB_ONE,
            &[&["locktime", &locktime], &["refund", PUB_TWO]],
        );
        assert!(verify_permanent_p2pk_lock(&proof).is_err());
    }

    #[test]
    fn rejects_locktime_tag_in_any_case() {
        let locktime = PAST_LOCKTIME.to_string();
        let proof = locked_proof(Kind::P2PK, PUB_ONE, &[&["LockTime", &locktime]]);
        assert!(verify_permanent_p2pk_lock(&proof).is_err());
    }

    /// An HTLC naming our key passes cdk's pubkey check, but needs a preimage
    /// we don't have.
    #[test]
    fn rejects_htlc_naming_our_key() {
        let hash = "ec4916dd28fc4c10d78e287ca5d9cc51ee1ae73cbfde08c6b37324cbfaac8bc5";
        let proof = locked_proof(Kind::HTLC, hash, &[&["pubkeys", PUB_ONE]]);
        let err = verify_permanent_p2pk_lock(&proof).expect_err("HTLC accepted");
        assert!(err.contains("only P2PK"), "unexpected error: {}", err);
    }

    #[test]
    fn rejects_unlocked_proof() {
        let proof: Proof = serde_json::from_str(UNLOCKED_PROOF_JSON).unwrap();
        assert!(verify_permanent_p2pk_lock(&proof).is_err());
    }
}
