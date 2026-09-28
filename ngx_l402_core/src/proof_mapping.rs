//! Redis keys mapping a Cashu proof to the tenant it was paid to.

use sha2::{Digest, Sha256};

/// The Redis key holding the tenant address for the proof with this secret.
pub fn proof_mapping_key(secret: &str) -> String {
    format!(
        "cashu:proof_lnurl:{}",
        hex::encode(Sha256::digest(secret.as_bytes()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Changing the format orphans every mapping already in Redis, and proofs
    // without one redeem to the default address.
    #[test]
    fn key_is_the_sha256_of_the_secret() {
        assert_eq!(
            proof_mapping_key("abc"),
            "cashu:proof_lnurl:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
