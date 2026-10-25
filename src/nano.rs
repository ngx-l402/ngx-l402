//! Nano payment verification against a node's RPC.
//!
//! Opt-in: nothing here runs unless `NANO_ADDRESS` is set (with a valid
//! checksum), `NANO_RAW_PER_SAT` and `NANO_RPC_URL`, and `ROOT_KEY` is set to
//! sign challenges. The module only *checks* a payment the client already
//! made; it holds no Nano keys and sends nothing. The deciding logic is pure
//! and tested in `ngx_l402_core::nano`; this file is the challenge issue, the
//! one bounded RPC call and the replay claim around it.

use log::{error, info, warn};
use ngx_l402_core::{NanoConfig, NanoReject};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Read in the master by `init_module`; workers see the copy nginx forks.
/// `None` (never set) means Nano is off and nothing about the 402 changes.
static NANO_CONFIG: OnceLock<NanoConfig> = OnceLock::new();

/// Challenge-signing key, derived from `ROOT_KEY` in the master next to
/// `NANO_CONFIG`. Set only when the config is.
static NANO_CHALLENGE_KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// Built lazily in each worker, never before fork.
static NANO_HTTP: OnceLock<reqwest::Client> = OnceLock::new();

/// Largest `block_info` reply read. A real one is well under 2 KiB.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Read `NANO_*` from the environment. Call once, in the master, before fork.
pub fn init_from_env() {
    let get = |k: &str| std::env::var(k).ok();
    let address = get("NANO_ADDRESS");
    let rate = get("NANO_RAW_PER_SAT");
    let rpc = get("NANO_RPC_URL");
    let timeout = get("NANO_RPC_TIMEOUT_MS");
    let ttl = get("NANO_CHALLENGE_TTL_SECS");
    match NanoConfig::from_values(
        address.as_deref(),
        rate.as_deref(),
        rpc.as_deref(),
        timeout.as_deref(),
        ttl.as_deref(),
    ) {
        Ok(Some(cfg)) => match crate::require_root_key() {
            Ok(root_key) => {
                info!("Nano payments enabled for {}", cfg.address);
                let _ = NANO_CHALLENGE_KEY.set(ngx_l402_core::derive_challenge_key(&root_key));
                let _ = NANO_CONFIG.set(cfg);
            }
            Err(e) => error!("❌ Nano payments disabled: challenges need ROOT_KEY: {}", e),
        },
        Ok(None) => {}
        Err(e) => error!("❌ Nano payments disabled: {}", e),
    }
}

pub fn config() -> Option<&'static NanoConfig> {
    NANO_CONFIG.get()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `X-Nano` and `X-Nano-Challenge` values for a request with `binding`
/// priced at `amount_msat`, when Nano is on. The amount carries a fresh tag,
/// so every 402 asks for its own amount.
pub fn challenge(amount_msat: i64, binding: &[String]) -> Option<(String, String)> {
    let cfg = config()?;
    let key = NANO_CHALLENGE_KEY.get()?;
    let price_raw = cfg.price_raw(amount_msat)?;
    let mut nonce = [0u8; 16];
    if let Err(e) = openssl::rand::rand_bytes(&mut nonce) {
        // No randomness, no challenge: the 402 still carries the other rails.
        error!(
            "❌ No randomness for a Nano challenge; not offering Nano: {}",
            e
        );
        return None;
    }
    let c = ngx_l402_core::issue_challenge(
        key,
        &nonce,
        binding,
        price_raw,
        now_secs(),
        cfg.challenge_ttl_secs,
    )?;
    Some((
        ngx_l402_core::nano_challenge(&cfg.address, c.amount_raw),
        c.id,
    ))
}

/// Ask the node for the block, bounded in time and size. Any transport
/// failure is an `Err`, which the caller turns into a refusal.
async fn fetch_block_info(cfg: &NanoConfig, hash_upper: &str) -> Result<String, String> {
    let timeout = Duration::from_millis(cfg.rpc_timeout_ms);
    let client = match NANO_HTTP.get() {
        Some(c) => c,
        None => {
            let c = reqwest::Client::builder()
                .timeout(timeout)
                .connect_timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| format!("client: {}", e))?;
            let _ = NANO_HTTP.set(c);
            NANO_HTTP.get().ok_or("client unavailable")?
        }
    };
    let call = async {
        let mut resp = client
            .post(&cfg.rpc_url)
            .header("Content-Type", "application/json")
            .body(ngx_l402_core::block_info_request(hash_upper))
            .send()
            .await
            .map_err(|e| format!("request: {}", e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read: {}", e))? {
            if body.len() + chunk.len() > MAX_REPLY_BYTES {
                return Err("reply too large".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).map_err(|_| "reply is not UTF-8".to_string())
    };
    tokio::time::timeout(timeout, call)
        .await
        .map_err(|_| format!("timed out after {} ms", cfg.rpc_timeout_ms))?
}

/// Verify `Authorization: Nano <challenge id>:<hash>` for a request with
/// `binding` priced at `amount_msat`. Returns the nginx access-phase code:
/// `NGX_DECLINED` admits.
///
/// Fails closed: a malformed credential, a challenge that is not this
/// request's or has expired, a block that does not pay exactly the challenge
/// amount, or a replay of the block or the challenge is 401; an unreachable
/// node or Redis is 503. Nothing admits on error.
pub fn verify(
    credential: &str,
    amount_msat: i64,
    binding: &[String],
    rt: &tokio::runtime::Runtime,
    declined: isize,
) -> isize {
    let (Some(cfg), Some(key)) = (config(), NANO_CHALLENGE_KEY.get()) else {
        // Not offered; the caller only routes here when it is.
        return 401;
    };
    let Some((id, hash)) = ngx_l402_core::parse_nano_credential(credential) else {
        warn!("❌ Nano credential is not <challenge id>:<64-hex block hash>");
        return 401;
    };
    let Some(price_raw) = cfg.price_raw(amount_msat) else {
        error!("❌ Nano price for {} msat cannot be expressed", amount_msat);
        return 500;
    };
    let now = now_secs();
    let terms = match ngx_l402_core::check_challenge(
        key,
        id,
        binding,
        price_raw,
        now,
        cfg.challenge_ttl_secs,
    ) {
        Ok(t) => t,
        Err(e) => {
            warn!("❌ Nano payment rejected: {}", e);
            return 401;
        }
    };
    let block_key = ngx_l402_core::nano_replay_key(&hash);
    if crate::is_replay_key_claimed(&block_key) || crate::is_replay_key_claimed(&terms.replay_key) {
        warn!("🚨 Nano block or challenge already used — replay rejected");
        return 401;
    }

    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(fetch_block_info(cfg, &hash))
    }));
    let reply = match reply {
        Ok(Ok(body)) => body,
        Ok(Err(e)) => {
            error!("❌ Nano RPC failed, refusing: {}", e);
            return 503;
        }
        Err(_) => {
            error!("❌ Panic during Nano RPC; refusing");
            return 500;
        }
    };

    match ngx_l402_core::judge_block_info(&reply, &cfg.public_key, terms.amount_raw) {
        Ok(paid) => {
            // The block is spent forever (a hash never expires); the challenge
            // marker only has to outlive the challenge. Both in one step, so a
            // block cannot pay twice and a challenge cannot be paid twice.
            let challenge_ttl = terms.expires_at.saturating_sub(now).saturating_add(60);
            match crate::claim_nano_payment(&block_key, &terms.replay_key, challenge_ttl) {
                Ok(true) => {
                    info!("✅ Nano payment verified ({} raw)", paid);
                    declined
                }
                Ok(false) => {
                    warn!("🚨 Nano block or challenge already used — replay rejected");
                    401
                }
                Err(e) => {
                    error!("❌ Redis unavailable for Nano claim, refusing: {}", e);
                    503
                }
            }
        }
        Err(NanoReject::NodeError(e)) => {
            warn!("❌ Nano block rejected by node: {}", e);
            401
        }
        Err(e) => {
            warn!("❌ Nano payment rejected: {}", e);
            401
        }
    }
}
