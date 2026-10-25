//! `ngx_l402_core` — pure, dependency-light logic extracted from the main
//! `ngx_l402` nginx module so it can be unit-tested with plain `cargo test`.
//!
//! The main crate is a `cdylib` that links nginx via `nginx-sys`; a test binary
//! built from it can't resolve nginx's runtime-provided symbols, so it can't
//! host runnable unit tests. The custody-critical pure pieces live here
//! instead, where `cargo test -p ngx_l402_core`
//! runs in seconds with no nginx and no Docker. A silent change to any of these
//! can strand user funds, so each is pinned by tests in its own module.

mod cashu_error;
mod dleq;
mod escaping;
mod fast_exit;
mod fee;
mod info;
mod l402_header;
mod nano;
mod p2pk;
mod payment_page;
mod proof_mapping;
mod rate_limit;
mod redact;
mod replay_cache;
mod tenant;
mod wallet_seed;

pub use cashu_error::CashuError;
pub use dleq::{parse_require_dleq, verify_proof_dleq_offline, DleqCheck};
pub use escaping::{escape_json, html_escape};
pub use fast_exit::install_fast_exit;
pub use fee::{fee_reserve_msat, melt_reserve_msat, sat_to_msat, MSAT_PER_SAT};
pub use info::{render as render_info, GlobalInfo, LocationInfo};
pub use l402_header::{is_l402_scheme, parse_l402_header_value};
pub use nano::{
    block_info_request, check_challenge, derive_challenge_key, encode_nano_address,
    issue_challenge, judge_block_info, msat_to_raw, nano_challenge, nano_replay_key,
    parse_block_hash, parse_nano_address, parse_nano_credential, parse_raw_per_sat,
    ChallengeReject, InvalidNanoAddress, NanoChallenge, NanoConfig, NanoReject,
    DEFAULT_NANO_CHALLENGE_TTL_SECS, DEFAULT_NANO_RPC_TIMEOUT_MS, NANO_AUTH_SCHEME,
    NANO_CHALLENGE_HEADER, NANO_CHALLENGE_ID_HEADER, NANO_TAG_SPACE,
};
pub use p2pk::{parse_p2pk_secret_key, InvalidP2pkKey};
pub use payment_page::render_payment_page;
pub use proof_mapping::proof_mapping_key;
pub use rate_limit::{invoice_rate_limit_key, parse_rate_limit};
pub use redact::redact_redis_url;
pub use replay_cache::{ReplayCache, DEFAULT_REPLAY_CACHE_CAP};
pub use tenant::{parse_tenant, Tenant, TenantError, TenantRegistry};
pub use wallet_seed::{
    derive_wallet_seed, generate_mnemonic, is_valid_mnemonic, wallet_fingerprint, InvalidMnemonic,
    WALLET_SEED_LEN,
};
