//! Responses captured from the default whitelisted mints. cdk 0.14 could use
//! neither: it rejected Coinos's keyset (the newer NUT-02 v2 id derivation) and
//! failed to parse Minibits's info (its on-chain payment method). Every token
//! from them got a 500. When a mint upgrades again, refresh these fixtures so
//! the break shows up here rather than in production.

use cdk::nuts::{KeysResponse, MintInfo};

fn assert_usable(info: &str, keys: &str) {
    serde_json::from_str::<MintInfo>(info).expect("mint info parses");
    let keys: KeysResponse = serde_json::from_str(keys).expect("keys response parses");
    assert!(!keys.keysets.is_empty());
    for keyset in keys.keysets {
        keyset.verify_id().expect("keyset id matches its keys");
    }
}

#[test]
fn coinos_nutshell_0_21() {
    assert_usable(
        include_str!("fixtures/coinos-info.json"),
        include_str!("fixtures/coinos-keys.json"),
    );
}

#[test]
fn minibits_cdk_mintd_0_17() {
    assert_usable(
        include_str!("fixtures/minibits-info.json"),
        include_str!("fixtures/minibits-keys.json"),
    );
}
