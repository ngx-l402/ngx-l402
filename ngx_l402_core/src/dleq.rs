//! Offline NUT-12 DLEQ verification of Cashu proofs against mint keys, and
//! the `CASHU_REQUIRE_DLEQ` policy parsing. Pure logic over cdk types, so it
//! lives here where `cargo test` can pin it down — the P2PK fast path skips
//! the mint swap and this verification is the only offline guarantee that a
//! proof was actually signed by the (whitelisted) mint (see issue #117).

use cdk::nuts::{Proof, PublicKey};
use log::warn;

/// Parse the `CASHU_REQUIRE_DLEQ` value: only an explicit, case-insensitive
/// "false" disables the check; anything else (unset, empty, "true", garbage)
/// keeps it on.
pub fn parse_require_dleq(value: Option<&str>) -> bool {
    value
        .map(|v| v.trim().to_lowercase() != "false")
        .unwrap_or(true)
}

/// Verify a single proof's NUT-12 DLEQ against the per-amount mint key,
/// applying the require-DLEQ policy.
///
/// `amount_key` is the mint public key for this proof's (keyset, amount),
/// resolved by the caller from the already-cached keysets (no network here).
/// When the proof carries DLEQ data, the caller passes the resolved key and we
/// cryptographically verify the proof was signed by the mint. When DLEQ is
/// absent, the `require_dleq` policy decides admit-vs-reject.
///
/// Returns `Err` on any failure so the fast path can reject the whole token.
pub fn verify_proof_dleq_offline(
    proof: &Proof,
    amount_key: Option<PublicKey>,
    require_dleq: bool,
) -> Result<(), String> {
    match proof.dleq {
        Some(_) => {
            let key = amount_key.ok_or_else(|| {
                format!(
                    "No mint key for amount {} in keyset {} — cannot verify DLEQ",
                    proof.amount, proof.keyset_id
                )
            })?;
            proof
                .verify_dleq(key)
                .map_err(|e| format!("NUT-12 DLEQ verification failed: {}", e))
        }
        None => {
            if require_dleq {
                Err(
                    "Proof is missing NUT-12 DLEQ data; rejecting on P2PK fast path. \
                     Set CASHU_REQUIRE_DLEQ=false to admit DLEQ-less tokens (insecure)."
                        .to_string(),
                )
            } else {
                warn!(
                    "⚠️ Proof missing DLEQ and CASHU_REQUIRE_DLEQ=false — admitting \
                     without offline mint-signature verification (insecure)"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // A proof carrying a valid NUT-12 DLEQ, signed by the mint key `A` below.
    // Lifted from cdk's own nut12 test vectors (cashu-0.14.3 nut12.rs).
    const VALID_PROOF_JSON: &str = r#"{"amount": 1,"id": "00882760bfa2eb41","secret": "daf4dd00a2b68a0858a80450f52c8a7d2ccf87d375e43e216e0c571f089f63e9","C": "024369d2d22a80ecf78f3937da9d5f30c1b9f74f0c32684d583cca0fa6a61cdcfc","dleq": {"e": "b31e58ac6527f34975ffab13e70a48b6d2b0d35abc4b03f0151f09ee1a9763d4","s": "8fbae004c59e754d71df67e392b6ae4e29293113ddc2ec86592a0431d16306d8","r": "a6d13fcd7a18442e6076f5e1e7c887ad5de40a019824bdfa9fe740d302e8d861"}}"#;

    // The mint public key `A` that signed the proof above.
    const MINT_KEY_HEX: &str = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";

    fn mint_key() -> PublicKey {
        PublicKey::from_str(MINT_KEY_HEX).unwrap()
    }

    #[test]
    fn require_dleq_defaults_to_true() {
        assert!(parse_require_dleq(None)); // unset -> required
        assert!(parse_require_dleq(Some("")));
        assert!(parse_require_dleq(Some("true")));
        assert!(parse_require_dleq(Some("garbage")));
        assert!(!parse_require_dleq(Some("false")));
        assert!(!parse_require_dleq(Some("  FALSE  ")));
    }

    #[test]
    fn dleq_offline_accepts_valid_proof() {
        let proof: Proof = serde_json::from_str(VALID_PROOF_JSON).unwrap();
        // Valid DLEQ + correct per-amount key -> admitted.
        assert!(verify_proof_dleq_offline(&proof, Some(mint_key()), true).is_ok());
    }

    #[test]
    fn dleq_offline_rejects_tampered_c() {
        // Simulate a forger: a proof from a whitelisted mint, P2PK-shaped, but
        // whose unblinded signature C does not match the mint's DLEQ. Today
        // (pre-#117) this would be admitted on the fast path; it must be rejected.
        let mut proof: Proof = serde_json::from_str(VALID_PROOF_JSON).unwrap();
        proof.c = PublicKey::from_str(
            "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
        )
        .unwrap();
        let err = verify_proof_dleq_offline(&proof, Some(mint_key()), true)
            .expect_err("tampered C must be rejected");
        assert!(err.contains("DLEQ"), "unexpected error: {}", err);
    }

    #[test]
    fn dleq_offline_rejects_wrong_amount_key() {
        // DLEQ present but we couldn't resolve the per-amount key -> reject,
        // never silently admit.
        let proof: Proof = serde_json::from_str(VALID_PROOF_JSON).unwrap();
        assert!(verify_proof_dleq_offline(&proof, None, true).is_err());
    }

    #[test]
    fn dleq_offline_missing_dleq_respects_policy() {
        let json = r#"{"amount": 1,"id": "00882760bfa2eb41","secret": "daf4dd00a2b68a0858a80450f52c8a7d2ccf87d375e43e216e0c571f089f63e9","C": "024369d2d22a80ecf78f3937da9d5f30c1b9f74f0c32684d583cca0fa6a61cdcfc"}"#;
        let proof: Proof = serde_json::from_str(json).unwrap();
        assert!(proof.dleq.is_none());
        // require_dleq = true -> reject DLEQ-less proof.
        assert!(verify_proof_dleq_offline(&proof, None, true).is_err());
        // require_dleq = false -> admit (safety valve).
        assert!(verify_proof_dleq_offline(&proof, None, false).is_ok());
    }
}
