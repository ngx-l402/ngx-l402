//! Redis keys mapping a Cashu proof to the tenant it was paid to.

use sha2::{Digest, Sha256};

/// Keys per `DEL`, so one sweep of a large backlog doesn't block Redis.
pub const MAPPING_DELETE_BATCH: usize = 500;

/// The Redis key holding the tenant address for the proof with this secret.
pub fn proof_mapping_key(secret: &str) -> String {
    format!(
        "cashu:proof_lnurl:{}",
        hex::encode(Sha256::digest(secret.as_bytes()))
    )
}

/// Delete the mapping keys of `proofs`, given as `(id, secret)`, in batches.
///
/// Returns the ids whose keys are confirmed deleted, and the error that stopped
/// the sweep, if any. A failed batch may or may not have been applied, so its
/// ids are left out; deleting a missing key is a no-op, so retrying is safe.
pub fn delete_proof_mappings<Id, E>(
    proofs: Vec<(Id, String)>,
    mut delete: impl FnMut(&[String]) -> Result<(), E>,
) -> (Vec<Id>, Option<E>) {
    let mut deleted = Vec::with_capacity(proofs.len());
    let mut proofs = proofs.into_iter().peekable();

    while proofs.peek().is_some() {
        let (ids, keys): (Vec<Id>, Vec<String>) = proofs
            .by_ref()
            .take(MAPPING_DELETE_BATCH)
            .map(|(id, secret)| (id, proof_mapping_key(&secret)))
            .unzip();
        if let Err(e) = delete(&keys) {
            return (deleted, Some(e));
        }
        deleted.extend(ids);
    }

    (deleted, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn key_is_the_sha256_of_the_secret() {
        assert_eq!(
            proof_mapping_key("abc"),
            "cashu:proof_lnurl:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn failed_delete_is_retried_on_the_next_sweep() {
        let mut redis: HashMap<String, &str> = HashMap::new();
        redis.insert(proof_mapping_key("spent"), "tenant@a");
        redis.insert(proof_mapping_key("unspent"), "tenant@b");
        let mut fail_next = true;
        let mut del = |keys: &[String]| {
            if std::mem::take(&mut fail_next) {
                return Err("connection reset");
            }
            for key in keys {
                redis.remove(key);
            }
            Ok(())
        };

        let (deleted, err) = delete_proof_mappings(vec![(1, "spent".to_string())], &mut del);
        assert!(deleted.is_empty());
        assert_eq!(err, Some("connection reset"));

        let (deleted, err) = delete_proof_mappings(vec![(1, "spent".to_string())], &mut del);
        assert_eq!(deleted, vec![1]);
        assert_eq!(err, None);

        assert_eq!(redis.len(), 1);
        assert!(redis.contains_key(&proof_mapping_key("unspent")));
    }

    #[test]
    fn deletes_in_batches_and_stops_at_the_first_failure() {
        let proofs: Vec<_> = (0..MAPPING_DELETE_BATCH * 3)
            .map(|i| (i, i.to_string()))
            .collect();
        let mut batches = Vec::new();
        let (deleted, err) = delete_proof_mappings(proofs, |keys: &[String]| {
            batches.push(keys.len());
            if batches.len() == 2 {
                Err(())
            } else {
                Ok(())
            }
        });

        assert_eq!(batches, vec![MAPPING_DELETE_BATCH, MAPPING_DELETE_BATCH]);
        assert_eq!(deleted, (0..MAPPING_DELETE_BATCH).collect::<Vec<_>>());
        assert_eq!(err, Some(()));
    }

    #[test]
    fn nothing_to_delete_makes_no_call() {
        let (deleted, err) = delete_proof_mappings(
            Vec::<(u8, String)>::new(),
            |_: &[String]| -> Result<(), ()> { panic!("no keys, no DEL") },
        );
        assert!(deleted.is_empty());
        assert!(err.is_none());
    }
}
