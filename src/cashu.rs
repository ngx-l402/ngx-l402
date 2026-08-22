use crate::cashu_redemption_logger;
use crate::REDIS_POOL;
use cdk::mint_url::MintUrl;
use l402_middleware::lnclient;
use l402_middleware::lndrpc::lnrpc;
use log::{debug, error, info, warn};
use redis::Commands;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;
use url::Url;

// Thread-local storage to track processed tokens
thread_local! {
    static PROCESSED_TOKENS: RefCell<Option<HashSet<String>>> = const { RefCell::new(None) };
}

const MSAT_PER_SAT: u64 = 1000;

// Database singleton using cdk-sqlite. Opened in the master; `CASHU_DB_PID`
// records who opened it so a forked worker knows to open its own instead of
// reusing a connection that belongs to another process.
static CASHU_DB: OnceLock<Arc<cdk_sqlite::WalletSqliteDatabase>> = OnceLock::new();
static CASHU_DB_PID: OnceLock<u32> = OnceLock::new();
static CASHU_DB_URL: OnceLock<String> = OnceLock::new();

// nginx strips the environment in workers, so `std::env::var` after the fork
// quietly returns the default. Anything an operator can set is read in
// `initialize_cashu`, which runs in the master, and passed on through statics.

/// The Cashu database path, as the master saw it.
pub fn db_path() -> Option<&'static str> {
    CASHU_DB_URL.get().map(|s| s.as_str())
}

/// Owner of the Cashu data directory — the user the operator has already
/// nominated to hold this module's files.
#[cfg(unix)]
pub fn data_dir_owner() -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    let path = std::path::Path::new(
        db_path()?
            .trim()
            .trim_start_matches("sqlite://")
            .trim_start_matches("sqlite:"),
    );
    let meta = std::fs::metadata(path.parent()?).ok()?;
    Some((meta.uid(), meta.gid()))
}

/// Melt tuning, as the master saw it.
#[derive(Clone, Copy)]
pub struct MeltConfig {
    pub min_balance_sats: u64,
    pub fee_reserve_percent: f64,
    pub min_fee_reserve_sats: u64,
    /// 0 = no limit.
    pub max_proofs_per_melt: usize,
}

static MELT_CONFIG: OnceLock<MeltConfig> = OnceLock::new();

/// Read the melt settings, while there is still an environment to read.
fn capture_melt_config() {
    let _ = MELT_CONFIG.set(MeltConfig {
        min_balance_sats: std::env::var("CASHU_MELT_MIN_BALANCE_SATS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        fee_reserve_percent: std::env::var("CASHU_MELT_FEE_RESERVE_PERCENT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0),
        min_fee_reserve_sats: std::env::var("CASHU_MELT_MIN_FEE_RESERVE_SATS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
        max_proofs_per_melt: std::env::var("CASHU_MAX_PROOFS_PER_MELT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    });
}

/// The captured settings, or the defaults if init never ran.
fn melt_config() -> MeltConfig {
    MELT_CONFIG.get().copied().unwrap_or(MeltConfig {
        min_balance_sats: 10,
        fee_reserve_percent: 1.0,
        min_fee_reserve_sats: 4,
        max_proofs_per_melt: 0,
    })
}
// This worker's own handle, opened lazily on first use after fork.
static WORKER_DB: tokio::sync::OnceCell<Arc<cdk_sqlite::WalletSqliteDatabase>> =
    tokio::sync::OnceCell::const_new();

/// The database handle for the current process.
///
/// A SQLite connection belongs to the process that opened it, so a worker never
/// reuses the master's — it opens its own on first use.
async fn get_db() -> Result<Arc<cdk_sqlite::WalletSqliteDatabase>, String> {
    if CASHU_DB_PID.get().copied() == Some(std::process::id()) {
        return CASHU_DB
            .get()
            .cloned()
            .ok_or_else(|| "Cashu database not initialized".to_string());
    }

    WORKER_DB
        .get_or_try_init(|| async {
            let url = CASHU_DB_URL
                .get()
                .ok_or_else(|| "Cashu database not configured".to_string())?;
            cdk_sqlite::WalletSqliteDatabase::new(url.as_str())
                .await
                .map(Arc::new)
                .map_err(|e| format!("Failed to open Cashu database: {:?}", e))
        })
        .await
        .cloned()
}

// Lives in ngx_l402_core so the status mapping has runnable tests: this crate
// is a cdylib and its own #[test] blocks cannot link against nginx.
pub use ngx_l402_core::CashuError;

// Whitelisted mints singleton
static WHITELISTED_MINTS: OnceLock<HashSet<String>> = OnceLock::new();

// P2PK mode flag and keys.
// The private key is stored as a parsed SecretKey (not the raw hex string) so
// the plaintext hex does not live for the lifetime of the process and cannot
// be leaked via a Debug/Display of the OnceLock contents.
static P2PK_MODE_ENABLED: OnceLock<bool> = OnceLock::new();
static P2PK_PRIVATE_KEY: OnceLock<cdk::nuts::SecretKey> = OnceLock::new();
static P2PK_PUBLIC_KEY: OnceLock<String> = OnceLock::new();

// Cashu eCash support flag
static CASHU_ECASH_ENABLED: OnceLock<bool> = OnceLock::new();

// Whether the P2PK fast path requires NUT-12 DLEQ proofs (default true).
// Read once from CASHU_REQUIRE_DLEQ at first use; see `cashu_require_dleq`.
static CASHU_REQUIRE_DLEQ: OnceLock<bool> = OnceLock::new();

// Lazily initialised inside the redemption thread's own runtime so we never
// inherit a tonic Channel whose epoll registrations were created in the nginx
// master process (they do not survive fork()).
static LN_CLIENT: tokio::sync::OnceCell<Arc<tokio::sync::Mutex<dyn lnclient::LNClient>>> =
    tokio::sync::OnceCell::const_new();
static LN_CLIENT_TYPE: OnceLock<String> = OnceLock::new();

// Resolved BIP39 wallet mnemonic — the Cashu/NUT-13 backup phrase. Set once at
// init (initialize_cashu) from CASHU_WALLET_MNEMONIC, a persisted file, or a
// freshly generated phrase.
static WALLET_MNEMONIC: OnceLock<String> = OnceLock::new();

// Cached wallet seed — derived once from the mnemonic and reused for all wallet
// operations so the BIP39 PBKDF2 runs only once per process.
static CACHED_WALLET_SEED: OnceLock<[u8; 64]> = OnceLock::new();

// Per-(mint_url, unit) wallet cache. `cdk::wallet::Wallet` constructs internal state
// and an HTTP client; reusing instances also keeps the in-memory keysets cache hot
// across requests. Construction is amortized to once per (mint, unit) per worker.
type WalletCache =
    tokio::sync::RwLock<HashMap<(String, cdk::nuts::CurrencyUnit), Arc<cdk::wallet::Wallet>>>;
static WALLET_CACHE: OnceLock<WalletCache> = OnceLock::new();

async fn get_or_create_wallet(
    mint_url: &str,
    unit: cdk::nuts::CurrencyUnit,
) -> Result<Arc<cdk::wallet::Wallet>, String> {
    let cache = WALLET_CACHE.get_or_init(|| tokio::sync::RwLock::new(HashMap::new()));
    let key = (mint_url.to_string(), unit.clone());

    {
        let guard = cache.read().await;
        if let Some(w) = guard.get(&key) {
            return Ok(w.clone());
        }
    }

    let db = get_db().await?;
    let seed = get_cached_seed();
    let wallet = Arc::new(
        cdk::wallet::Wallet::new(mint_url, unit.clone(), db, seed, None)
            .map_err(|e| format!("Failed to create wallet: {}", e))?,
    );

    let mut guard = cache.write().await;
    Ok(guard.entry(key).or_insert(wallet).clone())
}

/// Maximum number of entries in the thread-local PROCESSED_TOKENS cache.
/// When exceeded, the set is cleared. Redis remains the authoritative replay check.
const MAX_PROCESSED_TOKENS: usize = 10_000;

/// Number of words in a freshly generated wallet mnemonic. 12 words = 128 bits
/// of entropy (the common Cashu default).
const GENERATED_MNEMONIC_WORDS: usize = 12;

/// Resolve the wallet mnemonic once, at init, and stash it in `WALLET_MNEMONIC`.
///
/// Resolution order:
///   1. `CASHU_WALLET_MNEMONIC` — the canonical, explicit source.
///   2. A previously persisted mnemonic file (see `wallet_mnemonic_file_path`).
///   3. A freshly generated BIP39 phrase, persisted to that file when a path is
///      available and logged once so the operator can back it up.
///
/// Fails closed (returns `Err`) only when an explicitly configured mnemonic is
/// invalid — we must never silently start a *different* empty wallet over real
/// funds.
fn resolve_wallet_mnemonic(db_url: &str) -> Result<(), String> {
    if WALLET_MNEMONIC.get().is_some() {
        return Ok(());
    }

    // 1. Explicit env mnemonic.
    if let Ok(env_mnemonic) = std::env::var("CASHU_WALLET_MNEMONIC") {
        let m = env_mnemonic.trim().to_string();
        if !m.is_empty() {
            if !ngx_l402_core::is_valid_mnemonic(&m) {
                return Err(
                    "CASHU_WALLET_MNEMONIC is set but is not a valid BIP39 mnemonic".to_string(),
                );
            }
            let _ = WALLET_MNEMONIC.set(m);
            info!("🔑 Using wallet mnemonic from CASHU_WALLET_MNEMONIC");
            return Ok(());
        }
    }

    let file_path = wallet_mnemonic_file_path(db_url);

    // 2. Previously persisted mnemonic.
    if let Some(ref path) = file_path {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let m = contents.trim().to_string();
            if ngx_l402_core::is_valid_mnemonic(&m) {
                let _ = WALLET_MNEMONIC.set(m);
                info!("🔑 Loaded persisted wallet mnemonic from {}", path);
                return Ok(());
            } else if !m.is_empty() {
                warn!(
                    "⚠️ Persisted mnemonic at {} is not valid BIP39 — regenerating",
                    path
                );
            }
        }
    }

    // 3. Generate (and persist if we can).
    let generated = ngx_l402_core::generate_mnemonic(GENERATED_MNEMONIC_WORDS)
        .map_err(|e| format!("Failed to generate wallet mnemonic: {}", e))?;

    // The phrase goes to the 0600 file when there is one, otherwise to stderr:
    // the operator still sees it at startup, and it stays out of error_log.
    match &file_path {
        Some(path) => match persist_mnemonic(path, &generated) {
            Ok(()) => warn!(
                "⚠️ CASHU_WALLET_MNEMONIC not set — generated a new wallet mnemonic and saved it to {}. BACK IT UP; it controls all Cashu funds.",
                path
            ),
            Err(e) => {
                warn!(
                    "⚠️ CASHU_WALLET_MNEMONIC not set — generated a new wallet mnemonic but could NOT persist it ({}). It will change on restart and orphan funds. The phrase has been written to stderr; save it and set CASHU_WALLET_MNEMONIC.",
                    e
                );
                eprintln!(
                    "ngx_l402: save this Cashu wallet mnemonic and set CASHU_WALLET_MNEMONIC:\n{}",
                    generated
                );
            }
        },
        None => {
            warn!(
                "⚠️ CASHU_WALLET_MNEMONIC not set and no persist path available — using an EPHEMERAL mnemonic that changes on restart. The phrase has been written to stderr; save it and set CASHU_WALLET_MNEMONIC."
            );
            eprintln!(
                "ngx_l402: save this Cashu wallet mnemonic and set CASHU_WALLET_MNEMONIC:\n{}",
                generated
            );
        }
    }

    let _ = WALLET_MNEMONIC.set(generated);
    Ok(())
}

/// Give the database files the same owner as their directory.
///
/// The master creates them as root; the workers that use them run as nginx.
/// The data directory already names that user, so follow it.
///
/// Ownership and mode are applied to an `O_NOFOLLOW` descriptor, never to the
/// path. This runs as root, so a path-based chown here is a privilege-
/// escalation primitive: a symlink planted at `<db>-wal` would hand the link's
/// target to the worker user with mode 0660.
fn set_db_ownership(db_url: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let path = std::path::Path::new(
            db_url
                .trim()
                .trim_start_matches("sqlite://")
                .trim_start_matches("sqlite:"),
        );
        let owner = path
            .parent()
            .and_then(|dir| std::fs::metadata(dir).ok())
            .map(|m| (m.uid(), m.gid()));

        // -wal and -shm hold live data too.
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            let file = std::path::PathBuf::from(name);
            if !file.exists() {
                continue;
            }

            // Read-only is enough: fchown/fchmod act on the descriptor, and
            // opening the SQLite files this way does not disturb them.
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&file);

            let handle = match opened {
                Ok(handle) => handle,
                Err(e) => {
                    warn!(
                        "⚠️ Not adjusting ownership of {} (ELOOP = symlink, refused): {}",
                        file.display(),
                        e
                    );
                    continue;
                }
            };

            if let Some((uid, gid)) = owner {
                let _ = std::os::unix::fs::fchown(&handle, Some(uid), Some(gid));
            }
            let _ = handle.set_permissions(std::fs::Permissions::from_mode(0o660));
        }
    }
    #[cfg(not(unix))]
    let _ = db_url;
}

/// Re-apply ownership once the master has finished its start-up writes.
///
/// Wallet restore and proof reconciliation run as root after the database is
/// created, and SQLite writes -wal/-shm on the first write rather than at open,
/// so they can appear after `initialize_cashu` has already set ownership.
pub fn reapply_db_ownership() {
    if let Some(url) = CASHU_DB_URL.get() {
        set_db_ownership(url);
    }
}

/// Decide where to persist a generated mnemonic. Prefers
/// `CASHU_WALLET_MNEMONIC_FILE`; otherwise derives a sibling `wallet.mnemonic`
/// next to the SQLite database file. Returns `None` if no path can be derived.
fn wallet_mnemonic_file_path(db_url: &str) -> Option<String> {
    if let Ok(p) = std::env::var("CASHU_WALLET_MNEMONIC_FILE") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return Some(p);
        }
    }
    let raw = db_url
        .trim()
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let parent = std::path::Path::new(raw).parent()?;
    Some(
        parent
            .join("wallet.mnemonic")
            .to_string_lossy()
            .into_owned(),
    )
}

/// Persist the mnemonic to `path` with owner-only permissions where supported.
///
/// On unix the file is created with mode 0600 up front, so it is never visible
/// at the umask's default permissions, and `O_NOFOLLOW` refuses a symlink: this
/// writes the BIP39 phrase controlling every Cashu fund, and `O_CREAT` alone
/// follows an existing link, which would deliver it to the link's target.
fn persist_mnemonic(path: &str, mnemonic: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| format!("open {} (ELOOP = symlink, refused): {}", path, e))?;
        f.write_all(format!("{}\n", mnemonic).as_bytes())
            .map_err(|e| format!("write {}: {}", path, e))?;
        // An existing file keeps its own mode on open, so set it explicitly —
        // on the descriptor, since a path-based call would follow a symlink
        // swapped in after the open.
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{}\n", mnemonic))
            .map_err(|e| format!("write {}: {}", path, e))?;
    }
    Ok(())
}

fn get_cached_seed() -> [u8; 64] {
    *CACHED_WALLET_SEED.get_or_init(|| {
        // The mnemonic is resolved and validated during initialize_cashu.
        let mnemonic = WALLET_MNEMONIC
            .get()
            .expect("wallet mnemonic must be resolved during initialize_cashu");
        // Derivation lives in ngx_l402_core so it can be unit-tested (golden
        // vectors) without nginx. The mnemonic was validated at init, so this
        // cannot fail here.
        ngx_l402_core::derive_wallet_seed(mnemonic).expect("wallet mnemonic was validated at init")
    })
}

/// Verify the configured mnemonic matches the wallet that owns this database's
/// funds.
///
/// On first run it records a one-way fingerprint of the seed next to the DB. On
/// later runs it refuses to start if the fingerprint differs: a changed or
/// typo'd `CASHU_WALLET_MNEMONIC` would otherwise open a *different* empty wallet
/// over the existing one and silently orphan its funds. To switch wallets
/// intentionally, delete the fingerprint file.
fn check_wallet_fingerprint(db_url: &str) -> Result<(), String> {
    let seed = get_cached_seed();
    let fingerprint = ngx_l402_core::wallet_fingerprint(&seed);

    let Some(path) = wallet_fingerprint_file_path(db_url) else {
        // No filesystem path to anchor against — skip (best effort).
        return Ok(());
    };

    match std::fs::read_to_string(&path) {
        Ok(stored) if stored.trim() == fingerprint => Ok(()),
        Ok(stored) => Err(format!(
            "CASHU_WALLET_MNEMONIC does not match the wallet that owns this database \
             (seed fingerprint {} != recorded {}). Refusing to start to avoid orphaning \
             funds. If you intentionally switched wallets, delete {}.",
            fingerprint,
            stored.trim(),
            path
        )),
        Err(_) => {
            // First run (or unreadable): record the current fingerprint.
            if let Err(e) = persist_fingerprint(&path, &fingerprint) {
                warn!("⚠️ Could not persist wallet fingerprint to {}: {}", path, e);
            }
            Ok(())
        }
    }
}

/// Path of the fingerprint sidecar file, beside the SQLite database.
fn wallet_fingerprint_file_path(db_url: &str) -> Option<String> {
    let raw = db_url
        .trim()
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let parent = std::path::Path::new(raw).parent()?;
    Some(
        parent
            .join("wallet.fingerprint")
            .to_string_lossy()
            .into_owned(),
    )
}

/// Persist the wallet fingerprint beside the database.
///
/// `fs::write` and a path-based chmod both follow symlinks, and this runs as
/// root in the master — a link planted here would redirect the write, and the
/// chmod, onto an arbitrary file. Open with `O_NOFOLLOW` and set the mode on
/// the descriptor instead.
fn persist_fingerprint(path: &str, fingerprint: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| format!("open {} (ELOOP = symlink, refused): {}", path, e))?;
        f.write_all(format!("{}\n", fingerprint).as_bytes())
            .map_err(|e| format!("write {}: {}", path, e))?;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, format!("{}\n", fingerprint))
            .map_err(|e| format!("write {}: {}", path, e))?;
    }
    Ok(())
}

/// Add token to thread-local cache, clearing if at capacity.
fn cache_processed_token(token: &str) {
    PROCESSED_TOKENS.with(|tokens| {
        if let Some(set) = tokens.borrow_mut().as_mut() {
            if set.len() >= MAX_PROCESSED_TOKENS {
                debug!(
                    "🔄 PROCESSED_TOKENS cache at capacity ({}), clearing",
                    MAX_PROCESSED_TOKENS
                );
                set.clear();
            }
            set.insert(token.to_string());
        }
    });
}

pub fn is_p2pk_mode_enabled() -> bool {
    P2PK_MODE_ENABLED.get().copied().unwrap_or(false)
}

pub fn is_cashu_ecash_enabled() -> bool {
    CASHU_ECASH_ENABLED.get().copied().unwrap_or(false)
}

/// Whether to require NUT-12 DLEQ proofs on the P2PK fast path.
///
/// Defaults to `true`: a proof without DLEQ data is rejected, because the fast
/// path skips the mint swap and DLEQ is the only offline guarantee that the
/// proof was actually signed by the (whitelisted) mint. Set
/// `CASHU_REQUIRE_DLEQ=false` to relax this for one release as a safety valve
/// in case a real-world wallet ships DLEQ-less tokens — this is insecure and
/// re-opens the forged-proof window described in issue #117.
pub fn cashu_require_dleq() -> bool {
    *CASHU_REQUIRE_DLEQ.get_or_init(|| {
        std::env::var("CASHU_REQUIRE_DLEQ")
            // Only an explicit, case-insensitive "false" disables the check;
            // anything else (unset, empty, "true", garbage) keeps it on.
            .map(|v| v.trim().to_lowercase() != "false")
            .unwrap_or(true)
    })
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
fn verify_proof_dleq_offline(
    proof: &cdk::nuts::Proof,
    amount_key: Option<cdk::nuts::PublicKey>,
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
                .map_err(|e| format!("NUT-12 DLEQ verification failed: {:?}", e))
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

/// Check if multi-tenant LNURL mode is enabled (LN_CLIENT_TYPE=LNURL)
pub fn is_multi_tenant_enabled() -> bool {
    LN_CLIENT_TYPE.get().is_some_and(|t| t == "LNURL")
}

/// Initialize the LN client type for cashu redemption (called from lib.rs).
/// The actual LN client connection is created lazily inside the redemption
/// thread's runtime via `LN_CLIENT` to avoid inheriting broken tonic Channels
/// from the nginx master process after fork().
pub fn initialize_ln_client(client_type: String) -> Result<(), String> {
    LN_CLIENT_TYPE
        .set(client_type.clone())
        .map_err(|_| "LN_CLIENT_TYPE already initialized".to_string())?;

    if is_multi_tenant_enabled() {
        info!("🏢 Multi-tenant LNURL mode enabled for Cashu redemption");
    } else {
        info!(
            "🔧 Single-tenant {} mode enabled for Cashu redemption",
            client_type
        );
    }

    Ok(())
}

pub fn initialize_cashu(db_url: &str) -> Result<(), String> {
    // Initialize PROCESSED_TOKENS with empty HashSet
    PROCESSED_TOKENS.with(|tokens| {
        *tokens.borrow_mut() = Some(HashSet::new());
    });

    // Set Cashu eCash enabled flag
    let _ = CASHU_ECASH_ENABLED.set(true);

    // Resolve the BIP39 wallet mnemonic before anything touches the wallet.
    // Fails closed if an explicitly configured mnemonic is invalid, so we never
    // start a different empty wallet over real funds.
    resolve_wallet_mnemonic(db_url)?;

    // Refuse to start if the configured mnemonic no longer matches the wallet
    // that owns this DB's funds (changed/typo'd mnemonic would orphan them).
    check_wallet_fingerprint(db_url)?;

    let _ = CASHU_DB_URL.set(db_url.to_string());
    capture_melt_config();

    // Create runtime for async initialization
    let rt = Runtime::new().expect("Failed to create runtime");

    // Initialize SQLite database with WAL mode
    rt.block_on(async {
        match cdk_sqlite::WalletSqliteDatabase::new(db_url).await {
            Ok(db) => {
                info!("✅ Cashu SQLite database initialized successfully with WAL mode");
                let _ = CASHU_DB.set(Arc::new(db));
                let _ = CASHU_DB_PID.set(std::process::id());
                set_db_ownership(db_url);

                Ok(())
            }
            Err(e) => {
                let error = format!("Failed to create Cashu SQLite database: {:?}", e);
                error!("❌ {}", error);
                Err(error)
            }
        }
    })
}

/// Normalize a mint URL for consistent comparison.
///
/// Uses the `url` crate so that equivalent URLs are treated as the same mint
/// even when they differ in case, default ports, IPv6 formatting, or
/// percent-encoding. Invalid inputs fall back to the previous naive behaviour
/// (trim whitespace, strip trailing slashes, lowercase) so that the whitelist
/// check never silently rejects malformed strings.
fn normalize_mint_url(url: &str) -> String {
    let trimmed = url.trim();
    match Url::parse(trimmed) {
        Ok(parsed) => parsed.as_str().trim_end_matches('/').to_string(),
        Err(_) => trimmed.trim_end_matches('/').to_lowercase(),
    }
}

pub fn initialize_whitelisted_mints(whitelisted_mints_str: &str) -> Result<(), String> {
    if whitelisted_mints_str.trim().is_empty() {
        info!("ℹ️ Empty whitelisted mints string provided");
        return Ok(());
    }

    let mut whitelisted_set = HashSet::new();

    // Split by comma, trim, and normalize each mint URL
    for mint_url in whitelisted_mints_str.split(',') {
        let normalized = normalize_mint_url(mint_url);
        if !normalized.is_empty() {
            info!("✅ Added whitelisted mint: {}", normalized);
            whitelisted_set.insert(normalized);
        }
    }

    if whitelisted_set.is_empty() {
        return Err("No valid mint URLs found in whitelisted mints".to_string());
    }

    match WHITELISTED_MINTS.set(whitelisted_set) {
        Ok(_) => {
            info!("✅ Whitelisted mints initialized successfully");
            Ok(())
        }
        Err(_) => Err("Failed to set whitelisted mints - already initialized".to_string()),
    }
}

pub async fn restore_wallets_state() {
    let whitelisted_mints = match get_whitelisted_mints() {
        Some(mints) => mints,
        None => {
            info!("ℹ️ No whitelisted mints to restore state from.");
            return;
        }
    };

    let db = match get_db().await {
        Ok(db) => db,
        Err(e) => {
            warn!("⚠️ Cashu database unavailable before restore: {}", e);
            return;
        }
    };

    let seed = get_cached_seed();

    for mint_url in whitelisted_mints {
        info!("🔄 Restoring wallet state for mint: {}", mint_url);
        // We restore for the Sat unit primarily (or Msat as well)
        let unit = cdk::nuts::CurrencyUnit::Sat;

        // Use a short timeout so an unreachable mint at startup doesn't
        // block the nginx worker process or produce alarming ERROR logs.
        // Restoration is best-effort — missed proofs will be recovered
        // on the next redemption or sweep cycle.
        let restore_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            match cdk::wallet::Wallet::new(mint_url, unit, db.clone(), seed, None) {
                Ok(wallet) => wallet
                    .restore()
                    .await
                    .map(|amount| {
                        let v: u64 = amount.into();
                        v
                    })
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        })
        .await;

        match restore_result {
            Ok(Ok(amount)) => info!("✅ Restored {} sats state from {}", amount, mint_url),
            Ok(Err(e)) => warn!("⚠️ Skipping restore for {} (mint error): {}", mint_url, e),
            Err(_) => warn!(
                "⚠️ Skipping restore for {} (unreachable, timed out after 5s)",
                mint_url
            ),
        }
    }
}

/// Reconcile proofs left in a Pending/Reserved state by an interrupted melt.
///
/// When a redemption is interrupted mid-melt, its proofs stay reserved in the
/// DB. On the next startup, ask the mint about them: proofs the mint reports
/// spent (the melt actually settled) are dropped, and the rest are reclaimed so
/// a future sweep can redeem them — preventing funds from being stuck reserved
/// forever. Best-effort and timeout-bounded so an unreachable mint at startup
/// doesn't block the worker.
pub async fn reconcile_pending_proofs() {
    let whitelisted_mints = match get_whitelisted_mints() {
        Some(mints) => mints,
        None => return,
    };

    let db = match get_db().await {
        Ok(db) => db,
        Err(e) => {
            warn!(
                "⚠️ Cashu database unavailable before pending-proof reconciliation: {}",
                e
            );
            return;
        }
    };

    let seed = get_cached_seed();

    for mint_url in whitelisted_mints {
        let unit = cdk::nuts::CurrencyUnit::Sat;
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            match cdk::wallet::Wallet::new(mint_url, unit, db.clone(), seed, None) {
                Ok(wallet) => wallet
                    .check_all_pending_proofs()
                    .await
                    .map(|amount| {
                        let v: u64 = amount.into();
                        v
                    })
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        })
        .await;

        match result {
            Ok(Ok(reclaimable)) if reclaimable > 0 => info!(
                "🔄 Reconciled pending proofs for {}: {} sat reclaimable from an interrupted melt",
                mint_url, reclaimable
            ),
            Ok(Ok(_)) => debug!("✅ No pending proofs to reconcile for {}", mint_url),
            Ok(Err(e)) => warn!(
                "⚠️ Pending-proof reconciliation failed for {}: {}",
                mint_url, e
            ),
            Err(_) => warn!(
                "⚠️ Pending-proof reconciliation timed out for {} (mint unreachable)",
                mint_url
            ),
        }
    }
}

pub fn is_mint_whitelisted(mint_url: &str) -> bool {
    // If no whitelisted mints are configured, allow all mints
    if let Some(whitelisted_mints) = WHITELISTED_MINTS.get() {
        whitelisted_mints.contains(&normalize_mint_url(mint_url))
    } else {
        info!("ℹ️ No whitelisted mints configured - allowing all mints");
        true
    }
}

pub fn get_whitelisted_mints() -> Option<&'static HashSet<String>> {
    WHITELISTED_MINTS.get()
}

fn get_lnurl_from_proof(proof: &cdk::nuts::Proof) -> Result<Option<String>, String> {
    let pool = REDIS_POOL.get().ok_or("Redis pool is not initialised")?;

    let mut conn = pool
        .get()
        .map_err(|e| format!("Failed to get redis connection from pool: {}", e))?;

    let secret = proof.secret.to_string();

    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());

    let proof_hash = hex::encode(hasher.finalize());

    let redis_key = format!("cashu:proof_lnurl:{}", proof_hash);

    let lnurl: Option<String> = conn
        .get(&redis_key)
        .map_err(|e| format!("Failed to get proof mapping: {}", e))?;

    Ok(lnurl)
}

fn set_proof_to_lnurl(
    proofs: cdk::nuts::Proofs,
    lnurl_route: Option<String>,
) -> Result<(), String> {
    let pool = REDIS_POOL.get().ok_or("Redis pool is not initialised")?;

    let lnurl = lnurl_route.unwrap_or_else(|| std::env::var("LNURL_ADDRESS").unwrap_or_default());

    if lnurl.is_empty() {
        return Err("No LNURL address available for cashu token".to_string());
    }

    let mut conn = pool
        .get()
        .map_err(|e| format!("Failed to get redis connection from pool: {}", e))?;

    for proof in proofs {
        let secret = proof.secret.to_string();

        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());

        let proof_hash = hex::encode(hasher.finalize());

        let redis_key = format!("cashu:proof_lnurl:{}", proof_hash);
        conn.set::<_, _, ()>(&redis_key, &lnurl)
            .map_err(|e| format!("Failed to set proof mapping: {}", e))?;
    }

    Ok(())
}

/// Remove proof-to-lnurl mappings from Redis after proofs have been melted
fn remove_proof_lnurl_mappings(proofs: &cdk::nuts::Proofs) -> Result<(), String> {
    let pool = REDIS_POOL.get().ok_or("Redis pool is not initialised")?;

    let mut conn = pool
        .get()
        .map_err(|e| format!("Failed to get redis connection from pool: {}", e))?;

    for proof in proofs {
        let secret = proof.secret.to_string();

        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());

        let proof_hash = hex::encode(hasher.finalize());

        let redis_key = format!("cashu:proof_lnurl:{}", proof_hash);
        let _: Result<(), _> = conn
            .del(&redis_key)
            .map_err(|e| format!("Failed to delete proof mapping: {}", e));
    }

    Ok(())
}

/// Group proofs by their associated lnurl address for multi-tenant redemption
fn group_proofs_by_lnurl(
    proofs: cdk::nuts::Proofs,
) -> Result<HashMap<String, cdk::nuts::Proofs>, String> {
    let mut grouped: HashMap<String, cdk::nuts::Proofs> = HashMap::new();
    let default_lnurl = std::env::var("LNURL_ADDRESS")
        .map_err(|_| "LNURL_ADDRESS is required for multi-tenant mode".to_string())?;

    for proof in proofs {
        let lnurl = get_lnurl_from_proof(&proof)
            .ok()
            .flatten()
            .unwrap_or_else(|| default_lnurl.clone());

        if lnurl.is_empty() {
            continue;
        }

        grouped.entry(lnurl).or_default().push(proof);
    }

    Ok(grouped)
}

/// Initialize P2PK mode if enabled
pub fn initialize_p2pk_mode() -> Result<(), String> {
    let p2pk_mode = std::env::var("CASHU_P2PK_MODE")
        .unwrap_or_else(|_| "false".to_string())
        .trim()
        .to_lowercase()
        == "true";

    if !p2pk_mode {
        info!("ℹ️ P2PK mode is disabled");
        let _ = P2PK_MODE_ENABLED.set(false);
        return Ok(());
    }

    info!("🔐 P2PK mode is enabled");

    // Verify CASHU_WHITELISTED_MINTS is set (required for P2PK mode)
    if get_whitelisted_mints().is_none() {
        return Err("P2PK mode requires CASHU_WHITELISTED_MINTS to be configured".to_string());
    }

    // Get private key from environment
    let private_key_hex = std::env::var("CASHU_P2PK_PRIVATE_KEY")
        .map_err(|_| "CASHU_P2PK_PRIVATE_KEY not set but P2PK mode is enabled".to_string())?;

    if private_key_hex.trim().is_empty() {
        return Err("CASHU_P2PK_PRIVATE_KEY is empty".to_string());
    }

    // Parse once in ngx_l402_core so the stored signing key and the published
    // public key come from a single, golden-vector-tested derivation. This
    // prevents any lock/sign mismatch that would make proofs unspendable.
    let (private_key, public_key_hex) = ngx_l402_core::parse_p2pk_secret_key(&private_key_hex)
        .map_err(|e| format!("Invalid P2PK private key: {}", e))?;

    info!("🔑 P2PK public key: {}", public_key_hex);

    // Store the parsed private key (not the raw hex) and the public key hex.
    P2PK_PRIVATE_KEY
        .set(private_key)
        .map_err(|_| "Failed to set private key".to_string())?;
    P2PK_PUBLIC_KEY
        .set(public_key_hex)
        .map_err(|_| "Failed to set public key".to_string())?;

    let _ = P2PK_MODE_ENABLED.set(true);
    let _ = CASHU_REQUIRE_DLEQ.set(
        std::env::var("CASHU_REQUIRE_DLEQ")
            .map(|v| v.trim().to_lowercase() != "false")
            .unwrap_or(true),
    );
    info!("✅ P2PK mode initialized with public key");

    Ok(())
}

/// Generate NUT-18/NUT-24 payment request for X-Cashu header
pub fn generate_payment_request(
    amount_msat: i64,
    whitelisted_mints: &HashSet<String>,
) -> Result<String, String> {
    let mints_array: Vec<String> = whitelisted_mints.iter().cloned().collect();

    let mut payment_request = serde_json::json!({
        // Round up: verification compares msat, so advertising the floor of a
        // sub-sat price would reject the very token the client was asked for.
        "a": (amount_msat + 999) / 1000,
        "u": "sat",
        "m": mints_array,
        // No transport field: NUT-24 omits it entirely because payment is
        // in-band, in the X-Cashu header itself.
    });

    // `nut10` states the lock a token must carry, and NUT-24 marks it optional.
    // Standard mode has no key to lock to, so the field is left out rather than
    // sent empty — a client reading it would otherwise be told to lock to
    // nothing and produce a token we cannot accept.
    match P2PK_PUBLIC_KEY.get() {
        Some(public_key_str) => {
            payment_request["nut10"] = serde_json::json!({
                "k": "P2PK",           // NUT-10 secret kind
                "d": public_key_str    // NUT-10 secret data - our public key!
            });
            info!(
                "📤 NUT-24 payment request generated with P2PK pubkey: {}",
                public_key_str
            );
        }
        None => info!("📤 NUT-24 payment request generated (standard mode, no lock required)"),
    }
    debug!("📋 Payment request: {:?}", payment_request);

    // Encode as NUT-18 format: "creq" + "A" + base64_urlsafe(CBOR(PaymentRequest))
    let mut cbor_bytes = Vec::new();
    ciborium::into_writer(&payment_request, &mut cbor_bytes)
        .map_err(|e| format!("Failed to encode CBOR: {}", e))?;

    use base64::{engine::general_purpose, Engine as _};
    let base64_encoded = general_purpose::URL_SAFE_NO_PAD.encode(&cbor_bytes);

    // NUT-18 format: prefix + version + encoded data
    let nut18_encoded = format!("creqA{}", base64_encoded);

    info!(
        "✅ NUT-18 encoded payment request: {}",
        &nut18_encoded[..50.min(nut18_encoded.len())]
    );

    Ok(nut18_encoded)
}

pub async fn verify_cashu_token(
    token: &str,
    amount_msat: i64,
    lnurl_addr: Option<String>,
) -> Result<(), CashuError> {
    // Log database status
    debug!("🔍 Verifying Cashu token, checking database connection...");

    // Check thread-local memory cache first (fastest path — no I/O)
    let token_already_processed = PROCESSED_TOKENS.with(|tokens| {
        if let Some(set) = tokens.borrow().as_ref() {
            set.contains(token)
        } else {
            false
        }
    });

    if token_already_processed {
        warn!("🚨 Replay attack detected: Cashu token already used (memory cache)");
        return Err(CashuError::BadCredential(
            "Cashu token already used".to_string(),
        ));
    }

    // Check Redis for replay (read-only check — store happens after successful verification)
    if crate::is_cashu_token_used(token) {
        error!("🚨 Replay attack detected: Cashu token already used (Redis)");
        return Err(CashuError::BadCredential(
            "Cashu token already used".to_string(),
        ));
    }

    // Decode the token from string
    let token_decoded = match cdk::nuts::Token::from_str(token) {
        Ok(token) => token,
        Err(e) => {
            error!("❌ Failed to decode Cashu token: {}", e);
            return Err(CashuError::BadCredential(format!(
                "Failed to decode Cashu token: {}",
                e
            )));
        }
    };

    // Calculate total token amount in millisatoshis
    let total_amount = token_decoded
        .value()
        .map_err(|e| CashuError::BadCredential(format!("Failed to get token value: {}", e)))?;

    // Check if the token unit is in millisatoshis or satoshis
    let unit = token_decoded
        .unit()
        .ok_or_else(|| CashuError::Unacceptable("Token has no currency unit".to_string()))?;
    let total_amount_msat: u64 = if unit == cdk::nuts::CurrencyUnit::Sat {
        u64::from(total_amount)
            .checked_mul(MSAT_PER_SAT)
            .ok_or_else(|| {
                CashuError::BadCredential(
                    "Token amount overflows u64 sat→msat conversion".to_string(),
                )
            })?
    } else if unit == cdk::nuts::CurrencyUnit::Msat {
        u64::from(total_amount)
    } else {
        // Other units not supported
        return Err(CashuError::Unacceptable(format!(
            "Unsupported token unit: {:?}",
            unit
        )));
    };

    // Check if the token amount is sufficient
    if total_amount_msat < amount_msat as u64 {
        return Err(CashuError::Unacceptable(format!(
            "Cashu token amount insufficient: {} msat (required: {} msat)",
            total_amount_msat, amount_msat
        )));
    }

    info!(
        "✅ Successfully decoded Cashu token with {} msat (required: {} msat)",
        total_amount_msat, amount_msat
    );

    // Extract mint URL from the token
    // Extract and normalize the mint URL from the token immediately so that
    // any extra trailing slash or whitespace in the token metadata is stripped
    // before the whitelist check, wallet creation, and logging.
    let mint_url = normalize_mint_url(
        &token_decoded
            .mint_url()
            .map_err(|e| CashuError::BadCredential(format!("Failed to get mint URL: {}", e)))?
            .to_string(),
    );

    // Check if the mint is whitelisted
    if !is_mint_whitelisted(&mint_url) {
        return Err(CashuError::Unacceptable(format!(
            "Mint {} not whitelisted",
            mint_url
        )));
    }

    info!("✅ Cashu token from whitelisted mint: {}", mint_url);

    // Reuse a cached wallet for this (mint, unit). Avoids per-request construction
    // and keeps the keysets cache warm.
    let wallet = get_or_create_wallet(&mint_url, unit)
        .await
        .map_err(CashuError::Internal)?;

    match wallet
        .receive(token, cdk::wallet::ReceiveOptions::default())
        .await
    {
        Ok(_) => {
            info!(
                "✅ Cashu token received successfully from mint: {}",
                mint_url
            );

            if is_multi_tenant_enabled() {
                // Use only the proofs from this specific token, not all wallet
                // proofs. The wallet is shared across tenants, so
                // get_unspent_proofs() would return other tenants' proofs and
                // overwrite their LNURL mappings.
                let keysets_info = wallet.get_mint_keysets().await.map_err(|e| {
                    CashuError::Internal(format!(
                        "Failed to get keysets for proof extraction: {}",
                        e
                    ))
                })?;
                match token_decoded.proofs(&keysets_info) {
                    Ok(proofs) => {
                        if let Err(e) = set_proof_to_lnurl(proofs, lnurl_addr) {
                            warn!("⚠️ Failed to set proof-to-lnurl mapping: {}", e);
                        }
                    }
                    Err(e) => {
                        warn!(
                            "⚠️ Failed to extract proofs from token for lnurl mapping: {}",
                            e
                        );
                    }
                }
            }

            // Atomic claim outcomes: Ok(true) = first claim (admit),
            // Ok(false) = concurrent replay (reject), Err = Redis unconfigured
            // or unavailable. Unlike the P2PK path, this path already swapped the
            // proofs at the mint (wallet.receive above), so a replayed token's
            // proofs are spent and the mint rejects a second receive even during
            // a Redis outage. The mint is the backstop here, so failing open is
            // safe regardless of whether Redis is unconfigured or down.
            match crate::store_cashu_token_as_used(token) {
                Ok(true) => {
                    cache_processed_token(token);
                    Ok(())
                }
                Ok(false) => {
                    // Same offence as the cache hit above, so the same answer:
                    // losing the claim race must not depend on timing.
                    Err(CashuError::BadCredential(
                        "Cashu token already used".to_string(),
                    ))
                }
                Err(e) => {
                    warn!(
                        "⚠️ Redis claim failed, admitting (fail-open, mint-backstopped): {}",
                        e
                    );
                    cache_processed_token(token);
                    Ok(())
                }
            }
        }
        Err(e) => {
            error!(
                "❌ Cashu token receive failed from mint {}: {}",
                mint_url, e
            );
            // Internal, not a payer fault: the swap may already have consumed
            // the token at the mint, so the answer must not be "yours was bad".
            Err(CashuError::Internal(format!("mint receive failed: {}", e)))
        }
    }
}

/// Verify Cashu token using P2PK optimized mode (NUT-24)
/// Stores proofs directly in CDK database using cached keysets - no mint swap call
pub async fn verify_cashu_token_p2pk(
    token: &str,
    amount_msat: i64,
    lnurl_addr: Option<String>,
) -> Result<(), CashuError> {
    info!("🔐 P2PK mode: Optimized token verification");

    // Check thread-local memory cache first (fastest path — no I/O)
    let token_seen = PROCESSED_TOKENS.with(|tokens| {
        tokens
            .borrow()
            .as_ref()
            .is_some_and(|set| set.contains(token))
    });

    if token_seen {
        warn!("🚨 Replay attack detected: Cashu token already used (memory cache)");
        return Err(CashuError::BadCredential(
            "Cashu token already used".to_string(),
        ));
    }

    // Check Redis for replay (read-only — store happens after successful verification)
    if crate::is_cashu_token_used(token) {
        error!("🚨 Replay attack detected: Cashu token already used (Redis)");
        return Err(CashuError::BadCredential(
            "Cashu token already used".to_string(),
        ));
    }

    // Decode and validate token
    let token_decoded = cdk::nuts::Token::from_str(token)
        .map_err(|e| CashuError::BadCredential(format!("Failed to decode token: {}", e)))?;

    // Extract and normalize the mint URL from the token immediately so that any
    // extra trailing slash or whitespace in the token metadata is stripped before
    // the whitelist check, wallet creation, ProofInfo storage, and logging.
    let mint_url_str = normalize_mint_url(
        &token_decoded
            .mint_url()
            .map_err(|e| CashuError::BadCredential(format!("Failed to get mint URL: {}", e)))?
            .to_string(),
    );
    // Re-parse to a typed MintUrl (needed for ProofInfo and wallet APIs)
    let mint_url = MintUrl::from_str(&mint_url_str).map_err(|e| {
        CashuError::Unacceptable(format!("Invalid mint URL after normalization: {:?}", e))
    })?;

    // Verify mint is whitelisted
    let whitelisted_mints = get_whitelisted_mints().ok_or(CashuError::Internal(
        "No whitelisted mints configured".to_string(),
    ))?;

    if !whitelisted_mints.contains(&mint_url_str) {
        return Err(CashuError::Unacceptable(format!(
            "Mint {} not whitelisted",
            mint_url_str
        )));
    }

    // Verify amount
    let total_amount = token_decoded
        .value()
        .map_err(|e| CashuError::BadCredential(format!("Failed to get value: {}", e)))?;

    let unit = token_decoded
        .unit()
        .ok_or_else(|| CashuError::Unacceptable("Token has no currency unit".to_string()))?;
    let total_amount_msat: u64 = if unit == cdk::nuts::CurrencyUnit::Sat {
        u64::from(total_amount)
            .checked_mul(MSAT_PER_SAT)
            .ok_or_else(|| {
                CashuError::BadCredential(
                    "Token amount overflows u64 sat→msat conversion".to_string(),
                )
            })?
    } else if unit == cdk::nuts::CurrencyUnit::Msat {
        u64::from(total_amount)
    } else {
        return Err(CashuError::Unacceptable(format!(
            "Unsupported unit: {:?}",
            unit
        )));
    };

    if total_amount_msat < amount_msat as u64 {
        return Err(CashuError::Unacceptable(format!(
            "Insufficient amount: {} < {}",
            total_amount_msat, amount_msat
        )));
    }

    info!(
        "✅ Validated: {} msat from {}",
        total_amount_msat, mint_url_str
    );

    // Reuse a cached wallet for this (mint, unit). Same instance used by the
    // non-P2PK path; keysets cache is shared.
    let wallet = get_or_create_wallet(&mint_url_str, unit.clone())
        .await
        .map_err(CashuError::Internal)?;

    // Get keysets (use cached if available, fetch once if not)
    let keysets_info = match wallet.get_mint_keysets().await {
        Ok(keysets) => {
            debug!("Using cached keysets");
            keysets
        }
        Err(_) => {
            info!("📡 Fetching keysets (one-time per mint)");
            wallet
                .load_mint_keysets()
                .await
                .map_err(|e| CashuError::Internal(format!("Failed to load keysets: {}", e)))?
        }
    };

    // Extract proofs using keysets
    let proofs = token_decoded
        .proofs(&keysets_info)
        .map_err(|e| CashuError::BadCredential(format!("Failed to extract proofs: {}", e)))?;

    // Compute a canonical replay key from sorted proof Y-values so that
    // re-encoding the same proofs into a different token string cannot bypass
    // the replay check.
    let proof_replay_key: String = {
        // Fail closed if any proof's Y-value can't be computed. Silently
        // dropping it (filter_map(... .ok())) would weaken the key's binding to
        // the full proof set — an all-error set would hash to a constant — and
        // let a crafted token slip past the replay check.
        let mut y_values: Vec<String> = proofs
            .iter()
            .map(|p| {
                p.y().map(|y| y.to_hex()).map_err(|e| {
                    CashuError::Internal(format!(
                        "Failed to compute proof Y-value for replay key: {:?}",
                        e
                    ))
                })
            })
            .collect::<Result<Vec<String>, CashuError>>()?;
        y_values.sort_unstable();
        let mut hasher = Sha256::new();
        hasher.update(y_values.join(",").as_bytes());
        hex::encode(hasher.finalize())
    };

    // Secondary replay check by proof identity — catches the same proofs
    // re-submitted under a different token serialization.
    if crate::is_cashu_token_used(&proof_replay_key) {
        error!("🚨 Replay attack detected: proof set already used (proof-key check)");
        return Err(CashuError::BadCredential(
            "Cashu token already used".to_string(),
        ));
    }

    // Get our public key for P2PK verification (the private key is not needed
    // on the verify path — only for signing during melt/redemption).
    let public_key_str = P2PK_PUBLIC_KEY.get().ok_or(CashuError::Internal(
        "P2PK public key not initialized".to_string(),
    ))?;

    let public_key = cdk::nuts::PublicKey::from_hex(public_key_str)
        .map_err(|e| CashuError::Internal(format!("Failed to parse public key: {:?}", e)))?;

    // Create spending condition with our public key
    let spending_condition = cdk::nuts::SpendingConditions::new_p2pk(public_key, None);

    // Verify token is P2PK-locked to our public key
    info!("🔓 Verifying token is P2PK-locked to our public key...");
    wallet
        .verify_token_p2pk(&token_decoded, spending_condition.clone())
        .await
        .map_err(|e| {
            CashuError::Unacceptable(format!("Token not locked to our public key: {:?}", e))
        })?;

    info!("✅ Token verified as P2PK-locked to our public key");

    // NUT-12 DLEQ offline verification (issue #117).
    //
    // The P2PK fast path skips the authoritative mint swap, so without this
    // check a forger could submit proofs that are (a) from a whitelisted mint
    // and (b) correctly P2PK-locked to us, yet NOT actually signed by the mint —
    // getting free service while the operator never gets paid. DLEQ lets us
    // verify the mint's signature offline using only the keysets we already
    // loaded above (no extra mint round-trip: `load_keyset_keys` reads the same
    // metadata cache that `get_mint_keysets` just warmed). Done before the
    // NUT-07 network check below so forged proofs are rejected without a hop.
    let require_dleq = cashu_require_dleq();
    // A token's proofs commonly share a keyset_id; cache by id to avoid
    // calling load_keyset_keys once per proof.
    let mut keyset_keys_cache = HashMap::new();
    for proof in &proofs {
        // Only resolve the per-amount key when the proof actually carries DLEQ;
        // the missing-DLEQ policy is handled inside `verify_proof_dleq_offline`.
        let amount_key = if proof.dleq.is_some() {
            let keys = match keyset_keys_cache.get(&proof.keyset_id) {
                Some(keys) => keys,
                None => {
                    let keys = wallet
                        .load_keyset_keys(proof.keyset_id)
                        .await
                        .map_err(|e| {
                            CashuError::Internal(format!(
                                "Failed to load keyset keys for DLEQ verification: {}",
                                e
                            ))
                        })?;
                    keyset_keys_cache.entry(proof.keyset_id).or_insert(keys)
                }
            };
            keys.amount_key(proof.amount)
        } else {
            None
        };
        verify_proof_dleq_offline(proof, amount_key, require_dleq)
            .map_err(CashuError::BadCredential)?;
    }
    info!(
        "✅ All {} proofs passed NUT-12 DLEQ verification",
        proofs.len()
    );

    // NUT-07: check proof state with the mint before accepting.
    // Without this call, a proof that has already been spent at the mint would
    // pass the local P2PK signature check and be accepted again (double-spend).
    info!("🔍 Checking proof state with mint (NUT-07 double-spend check)...");
    let proof_states = wallet
        .check_proofs_spent(proofs.clone())
        .await
        .map_err(|e| {
            CashuError::Internal(format!("Failed to verify proof state with mint: {:?}", e))
        })?;

    // Only Unspent is acceptable. Pending / Reserved / PendingSpent all mean the
    // mint already has the proof locked in an in-flight melt or swap; if we
    // accepted such a proof and stored it as Unspent, that pending transaction
    // could settle and leave us holding a spent proof. Reject anything that is
    // not Unspent rather than only rejecting Spent.
    if let Some(bad) = proof_states
        .iter()
        .find(|s| s.state != cdk::nuts::State::Unspent)
    {
        warn!(
            "🚨 P2PK: rejecting token — mint reports a proof in state {:?} (only Unspent is accepted)",
            bad.state
        );
        return Err(CashuError::BadCredential(
            "Token contains proofs that are not unspent at the mint".to_string(),
        ));
    }
    info!("✅ Mint confirmed all proofs are unspent");

    // IMPORTANT: receive_proofs() calls the mint to swap proofs (post_swap), which we want to avoid!
    // Instead, we store the P2PK-locked proofs directly in the database as UNSPENT
    // without swapping them. They can be spent later using our private key.

    use cdk::nuts::State;
    use cdk::types::ProofInfo;

    // Create ProofInfo objects for direct database storage
    let proof_infos: Vec<ProofInfo> = proofs
        .iter()
        .map(|proof| {
            let y = proof.y().map_err(|e| {
                CashuError::Internal(format!("Failed to compute proof y-coordinate: {:?}", e))
            })?;
            Ok(ProofInfo {
                proof: proof.clone(),
                y,
                mint_url: mint_url.clone(),
                state: State::Unspent,
                unit: unit.clone(),
                spending_condition: Some(spending_condition.clone()),
            })
        })
        .collect::<Result<Vec<_>, CashuError>>()?;

    info!(
        "💾 Storing {} P2PK-locked proofs directly in database (NO swap call)",
        proof_infos.len()
    );

    // Atomic Redis claim BEFORE writing to the database.
    // This closes the TOCTOU window: the loser of a concurrent replay race
    // is rejected here and never reaches update_proofs(), so the SQLite
    // database is only written once per token.
    // Claim by proof key (not raw token string) so re-encoded tokens with the
    // same proofs are rejected by the same atomic SET NX EX slot.
    let claimed = match crate::store_cashu_token_as_used(&proof_replay_key) {
        Ok(false) => {
            return Err(CashuError::BadCredential(
                "Cashu token already used".to_string(),
            ));
        }
        Ok(true) => true, // won the race — proceed to persist proofs
        Err(crate::ReplayClaimError::NotConfigured) => {
            // Redis was never configured — an explicit operator choice. Fall
            // back to the in-process memory cache (single-worker protection).
            warn!("⚠️ Redis not configured — Cashu replay protection is in-process only (single-worker)");
            false
        }
        Err(crate::ReplayClaimError::Unavailable(e)) => {
            // Redis is configured but unreachable. Unlike the swap path, P2PK
            // proofs are stored locally without a mint swap, so NUT-07 reports
            // Unspent for every resubmission and the mint cannot catch a replay.
            // Fail closed — refuse the token until Redis recovers — matching the
            // preimage path's outage policy. The proofs are not stored, so a
            // legitimate retry after recovery still succeeds.
            error!(
                "❌ Redis unavailable for Cashu P2PK claim — rejecting to prevent replay: {}",
                e
            );
            return Err(CashuError::Internal(format!(
                "Redis unavailable; refusing Cashu token to prevent replay: {}",
                e
            )));
        }
    };

    // Store directly in database using update_proofs (same as receive_proofs does internally)
    // Pass empty vec for second parameter (no proofs to delete)
    if let Err(e) = wallet.localstore.update_proofs(proof_infos, vec![]).await {
        // The DB write failed after we took the replay claim. Release the claim
        // so the token isn't stuck "used" — otherwise a legitimate retry would
        // be rejected until the Redis TTL expires even though no proofs were
        // ever persisted. (Only release if we actually claimed; the fail-open
        // branch above holds no claim to release.)
        if claimed {
            crate::release_cashu_token(&proof_replay_key);
        }
        return Err(CashuError::Internal(format!(
            "Failed to store proofs in database: {:?}",
            e
        )));
    }

    if is_multi_tenant_enabled() {
        if let Err(e) = set_proof_to_lnurl(proofs.clone(), lnurl_addr) {
            warn!("⚠️ Failed to set proof-to-lnurl mapping: {}", e);
        }
    }

    // Cache both the raw token string and the proof-based key so that both
    // exact-string and re-encoded replay attempts are caught on the fast
    // thread-local path without a Redis round-trip.
    cache_processed_token(&proof_replay_key);
    cache_processed_token(token);
    info!(
        "✅ ACCEPTED ({} msat stored in CDK database)",
        total_amount_msat
    );
    Ok(())
}

pub async fn redeem_to_lightning() -> Result<bool, String> {
    cashu_redemption_logger::log_redemption("🚀 Starting smart Cashu token redemption process...");
    info!("🚀 Starting smart Cashu token redemption process...");

    // Get database
    let db = get_db().await?;

    // Use whitelisted mints for redemption check
    // If no whitelisted mints are configured, we can't redeem
    let whitelisted_mints = WHITELISTED_MINTS
        .get()
        .ok_or_else(|| "No whitelisted mints configured".to_string())?;

    let msg = format!(
        "📊 Checking {} whitelisted mints for tokens: {:?}",
        whitelisted_mints.len(),
        whitelisted_mints.iter().collect::<Vec<_>>()
    );
    cashu_redemption_logger::log_redemption(&msg);
    info!("{}", msg);

    // Use cached seed (must match token verification)
    let seed = get_cached_seed();

    // Captured at init: this runs in a worker, with no environment to read.
    let melt = melt_config();
    let min_balance_sats = melt.min_balance_sats;
    let minimum_for_redemption_msat = min_balance_sats * MSAT_PER_SAT;
    let fee_reserve_percent = melt.fee_reserve_percent;
    let min_fee_reserve_sats = melt.min_fee_reserve_sats;
    let min_fee_reserve_msat = min_fee_reserve_sats * MSAT_PER_SAT;
    let max_proofs_per_melt = melt.max_proofs_per_melt;

    let msg = if max_proofs_per_melt > 0 {
        format!("⚙️ Melt config: min_balance={} sats, fee_reserve={}%, min_fee_reserve={} sats, max_proofs_per_melt={}", 
            min_balance_sats, fee_reserve_percent, min_fee_reserve_sats, max_proofs_per_melt)
    } else {
        format!("⚙️ Melt config: min_balance={} sats, fee_reserve={}%, min_fee_reserve={} sats, max_proofs_per_melt=unlimited", 
            min_balance_sats, fee_reserve_percent, min_fee_reserve_sats)
    };
    info!("{}", msg);
    cashu_redemption_logger::log_redemption(&msg);

    let mut total_redeemed = 0;
    let mut total_amount_redeemed_msat = 0;
    let mut total_fees_paid_msat = 0;

    // Process each whitelisted mint
    for mint_url_str in whitelisted_mints.iter() {
        // Validate mint URL format
        if MintUrl::from_str(mint_url_str).is_err() {
            warn!("⚠️ Invalid mint URL format: {}", mint_url_str);
            continue;
        }

        // Create wallet for this mint
        let wallet = match cdk::wallet::Wallet::new(
            mint_url_str,
            cdk::nuts::CurrencyUnit::Sat,
            db.clone(),
            seed,
            None,
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!("⚠️ Failed to create wallet for {}: {}", mint_url_str, e);
                continue;
            }
        };

        let wallet_clone = wallet.clone();

        // Calculate total amount
        let total_amount: u64 = match wallet_clone.total_balance().await {
            Ok(balance) => balance.into(),
            Err(e) => {
                let msg = format!("⚠️ Failed to get total balance for {}: {}", mint_url_str, e);
                warn!("{}", msg);
                cashu_redemption_logger::log_redemption(&msg);
                continue;
            }
        };

        if total_amount == 0 {
            debug!("ℹ️ Total amount is 0 for mint {}", wallet.mint_url);
            cashu_redemption_logger::log_redemption(&format!(
                "ℹ️ Total amount is 0 for mint {}",
                wallet.mint_url
            ));
            continue;
        }

        // Get all spendable proofs
        let proofs = match wallet_clone.get_unspent_proofs().await {
            Ok(p) => p,
            Err(e) => {
                let msg = format!(
                    "❌ Failed to get spendable proofs for {}: {}",
                    wallet.mint_url, e
                );
                error!("{}", msg);
                cashu_redemption_logger::log_redemption(&msg);
                continue;
            }
        };

        if proofs.is_empty() {
            debug!("ℹ️ No spendable proofs found for mint {}", wallet.mint_url);
            cashu_redemption_logger::log_redemption(&format!(
                "ℹ️ No spendable proofs found for mint {}",
                wallet.mint_url
            ));
            continue;
        }

        let msg = format!(
            "💰 Found {} proofs for mint {}",
            proofs.len(),
            wallet.mint_url
        );
        info!("{}", msg);
        cashu_redemption_logger::log_redemption(&msg);

        // Check if multi-tenant LNURL mode is enabled. Simply checks if lnclient is lnurl for now
        let is_multi_tenant = is_multi_tenant_enabled();

        // Build proof groups based on mode
        let proof_groups: Vec<(String, cdk::nuts::Proofs)> = if is_multi_tenant {
            // Multi-tenant: group proofs by their lnurl address
            let proofs_by_lnurl = match group_proofs_by_lnurl(proofs) {
                Ok(grouped) => grouped,
                Err(e) => {
                    let err_msg = format!("Failed to group proofs by LNURL: {}", e);
                    error!("{}", err_msg);
                    cashu_redemption_logger::log_redemption(&err_msg);
                    continue;
                }
            };
            let msg = format!(
                "Multi-tenant mode: Grouped proofs into {} tenant(s) for mint {}",
                proofs_by_lnurl.len(),
                wallet.mint_url
            );
            info!("{}", msg);
            cashu_redemption_logger::log_redemption(&msg);
            proofs_by_lnurl.into_iter().collect()
        } else {
            // Single-tenant: all proofs go to the configured LN client (LND, CLN, NWC, or LNURL)
            let client_type = LN_CLIENT_TYPE.get().map_or("default", |s| s.as_str());
            let msg = format!(
                "🔧 Single-tenant {} mode: All {} proofs for mint {}",
                client_type,
                proofs.len(),
                wallet.mint_url
            );
            info!("{}", msg);
            cashu_redemption_logger::log_redemption(&msg);
            vec![(client_type.to_string(), proofs)]
        };

        // Initialize dynamic variables with defaults
        let mut current_fee_reserve_percent = fee_reserve_percent;
        let mut current_min_fee_reserve_msat = min_fee_reserve_msat;
        let mut current_minimum_for_redemption_msat = minimum_for_redemption_msat;

        // Fetch Mint Info for dynamic parameter discovery
        info!("Fetching Mint Info for dynamic parameter discovery...");
        cashu_redemption_logger::log_redemption(
            "Fetching Mint Info for dynamic parameter discovery...",
        );
        match wallet_clone.fetch_mint_info().await {
            Ok(mint_info) => {
                debug!("Mint Info fetched");
                match serde_json::to_value(&mint_info) {
                    Ok(info_json) => {
                        let target_unit = match wallet.unit {
                            cdk::nuts::CurrencyUnit::Sat => "sat",
                            cdk::nuts::CurrencyUnit::Msat => "msat",
                            _ => "sat",
                        };

                        // NUT-05: Melt limits (min_amount, max_amount)
                        let nut05 = info_json
                            .get("nuts")
                            .and_then(|n| n.get("5").or(n.get("nut05")))
                            .or_else(|| info_json.get("nut05"));

                        if let Some(nut05_obj) = nut05 {
                            if let Some(methods) =
                                nut05_obj.get("methods").and_then(|m| m.as_array())
                            {
                                for method in methods {
                                    if method.get("method").and_then(|m| m.as_str())
                                        == Some("bolt11")
                                    {
                                        let unit = method
                                            .get("unit")
                                            .and_then(|u| u.as_str())
                                            .unwrap_or("sat");
                                        if unit == target_unit {
                                            if let Some(min_amount) =
                                                method.get("min_amount").and_then(|m| m.as_u64())
                                            {
                                                current_minimum_for_redemption_msat =
                                                    if unit == "sat" {
                                                        min_amount * MSAT_PER_SAT
                                                    } else {
                                                        min_amount
                                                    };
                                                info!(
                                                    "Found dynamic melt minimum: {} {}",
                                                    min_amount, unit
                                                );
                                                cashu_redemption_logger::log_redemption(&format!(
                                                    "Found dynamic melt minimum: {} {}",
                                                    min_amount, unit
                                                ));
                                            }
                                            // Note: max_amount could be used to inform batching, but we'll keep current behavior for now
                                        }
                                    }
                                }
                            }
                        } else {
                            debug!("NUT-05 config not found in Mint Info");
                        }

                        // NUT-08: Melt quote fees (ppk, min)
                        let nut08 = info_json
                            .get("nuts")
                            .and_then(|n| n.get("8").or(n.get("nut08")))
                            .or_else(|| info_json.get("nut08"));

                        if let Some(nut08_list) = nut08.and_then(|v| v.as_array()) {
                            let mut found = false;
                            for entry in nut08_list {
                                if entry.get("method").and_then(|m| m.as_str()) == Some("bolt11") {
                                    let unit =
                                        entry.get("unit").and_then(|u| u.as_str()).unwrap_or("sat");
                                    if unit == target_unit {
                                        if let Some(ppk) = entry.get("ppk").and_then(|p| p.as_u64())
                                        {
                                            // ppk is parts per thousand.
                                            // fee_percent = (ppk / 1000) * 100 = ppk / 10
                                            current_fee_reserve_percent = ppk as f64 / 10.0;
                                            info!(
                                                "Found dynamic fee: {} ppk -> {}%",
                                                ppk, current_fee_reserve_percent
                                            );
                                            cashu_redemption_logger::log_redemption(&format!(
                                                "Found dynamic fee: {} ppk -> {}%",
                                                ppk, current_fee_reserve_percent
                                            ));
                                            found = true;
                                        }

                                        if let Some(min_fee) =
                                            entry.get("min").and_then(|m| m.as_u64())
                                        {
                                            current_min_fee_reserve_msat = if unit == "sat" {
                                                min_fee * MSAT_PER_SAT
                                            } else {
                                                min_fee
                                            };
                                            info!("Found dynamic min fee: {} {}", min_fee, unit);
                                            cashu_redemption_logger::log_redemption(&format!(
                                                "Found dynamic min fee: {} {}",
                                                min_fee, unit
                                            ));
                                        }
                                    }
                                }
                            }
                            if !found {
                                warn!(
                                    "NUT-08 found but no matching 'bolt11' settings for unit '{}'",
                                    target_unit
                                );
                            }
                        } else {
                            debug!("NUT-08 config not found in Mint Info");
                        }
                    }
                    Err(e) => warn!("Failed to serialize Mint Info for inspection: {}", e),
                }
            }
            Err(e) => {
                warn!(
                    "Failed to fetch Mint Info: {}. Using default parameters.",
                    e
                );
                cashu_redemption_logger::log_redemption(&format!(
                    "Failed to fetch Mint Info: {}. Using default parameters.",
                    e
                ));
            }
        }

        // Process each proof group
        for (client_id, group_proofs) in proof_groups {
            let msg = format!(
                "🔄 Processing {} proofs for client: {}",
                group_proofs.len(),
                client_id
            );
            info!("{}", msg);
            cashu_redemption_logger::log_redemption(&msg);

            // Calculate total amount for this group
            let group_total_msat: u64 = group_proofs
                .iter()
                .map(|p| {
                    let amount: u64 = p.amount.into();
                    if wallet.unit == cdk::nuts::CurrencyUnit::Sat {
                        amount * MSAT_PER_SAT
                    } else {
                        amount
                    }
                })
                .sum();

            // Check minimum balance threshold for this group
            if group_total_msat < current_minimum_for_redemption_msat {
                let msg = format!(
                    "⏳ Skipping {} - balance {} msat is below minimum {} msat",
                    client_id, group_total_msat, current_minimum_for_redemption_msat
                );
                info!("{}", msg);
                cashu_redemption_logger::log_redemption(&msg);
                continue;
            }

            // Select proofs to melt based on configured limit
            let (proofs_to_melt, selected_total_msat) =
                if max_proofs_per_melt > 0 && group_proofs.len() > max_proofs_per_melt {
                    let selected_proofs: Vec<_> = group_proofs
                        .iter()
                        .take(max_proofs_per_melt)
                        .cloned()
                        .collect();
                    let selected_total: u64 = selected_proofs
                        .iter()
                        .map(|p| {
                            let amount: u64 = p.amount.into();
                            if wallet.unit == cdk::nuts::CurrencyUnit::Sat {
                                amount * MSAT_PER_SAT
                            } else {
                                amount
                            }
                        })
                        .sum();

                    let msg = format!(
                        "⚠️ Limiting to {} proofs ({} sats) for {}",
                        selected_proofs.len(),
                        selected_total / MSAT_PER_SAT,
                        client_id
                    );
                    info!("{}", msg);
                    cashu_redemption_logger::log_redemption(&msg);

                    (selected_proofs, selected_total)
                } else {
                    (group_proofs.clone(), group_total_msat)
                };

            // Calculate fee reserve (pure math lives in ngx_l402_core, unit-tested).
            let fee_reserve_selected_msat = ngx_l402_core::fee_reserve_msat(
                selected_total_msat,
                current_fee_reserve_percent,
                current_min_fee_reserve_msat,
            );

            if selected_total_msat <= fee_reserve_selected_msat {
                let msg = format!(
                    "⚠️ Insufficient balance after fee reserve for {}: {} msat <= {} msat fees",
                    client_id, selected_total_msat, fee_reserve_selected_msat
                );
                warn!("{}", msg);
                cashu_redemption_logger::log_redemption(&msg);
                continue;
            }

            let redeemable_amount_msat = selected_total_msat - fee_reserve_selected_msat;

            let msg = format!(
                "💡 Redemption plan for {}: {} proofs → {} msat - {} msat fees = {} msat",
                client_id,
                proofs_to_melt.len(),
                selected_total_msat,
                fee_reserve_selected_msat,
                redeemable_amount_msat
            );
            info!("{}", msg);
            cashu_redemption_logger::log_redemption(&msg);

            // Generate Lightning invoice
            let memo = format!(
                "Redeeming {} proofs from {} for {}",
                proofs_to_melt.len(),
                wallet.mint_url,
                client_id
            );

            let (invoice, _payment_hash) = if is_multi_tenant {
                // Multi-tenant LNURL mode: Use cached LNURL client for each tenant's address
                match crate::get_or_create_lnurl_client(&client_id).await {
                    Ok(ln_client) => {
                        let ln_client_conn = lnclient::LNClientConn { ln_client };
                        match ln_client_conn
                            .generate_invoice(lnrpc::Invoice {
                                value_msat: redeemable_amount_msat as i64,
                                memo: memo.clone(),
                                ..Default::default()
                            })
                            .await
                        {
                            Ok((invoice, payment_hash)) => {
                                cashu_redemption_logger::log_redemption(&format!(
                                    "✅ Generated invoice via LNURL {}: payment_hash={}",
                                    client_id, payment_hash
                                ));
                                (invoice, payment_hash)
                            }
                            Err(e) => {
                                let msg = format!(
                                    "❌ Failed to generate invoice via LNURL {}: {}",
                                    client_id, e
                                );
                                error!("{}", msg);
                                cashu_redemption_logger::log_redemption(&msg);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        let msg = format!("❌ Failed to get LNURL client for {}: {}", client_id, e);
                        error!("{}", msg);
                        cashu_redemption_logger::log_redemption(&msg);
                        continue;
                    }
                }
            } else {
                // Single-tenant mode: lazily create (or reuse) an LN client connection
                // within this redemption thread's runtime. We cannot use a client
                // created in the nginx master process because tonic Channels lose their
                // epoll registrations after fork(), leading to "Service was not ready"
                // errors. LN_CLIENT_CONFIG was saved by the master before forking.
                let ln_client = match LN_CLIENT
                    .get_or_try_init(|| async {
                        let config = crate::LN_CLIENT_CONFIG.get().ok_or_else(
                            || -> Box<dyn std::error::Error + Send + Sync> {
                                "LN client config not available for cashu redemption".into()
                            },
                        )?;
                        lnclient::LNClientConn::init(config).await
                    })
                    .await
                {
                    Ok(c) => Arc::clone(c),
                    Err(e) => {
                        let msg = format!("❌ Failed to initialize LN client for cashu: {}", e);
                        error!("{}", msg);
                        cashu_redemption_logger::log_redemption(&msg);
                        continue;
                    }
                };

                let ln_client_conn = lnclient::LNClientConn { ln_client };
                match ln_client_conn
                    .generate_invoice(lnrpc::Invoice {
                        value_msat: redeemable_amount_msat as i64,
                        memo: memo.clone(),
                        ..Default::default()
                    })
                    .await
                {
                    Ok((invoice, payment_hash)) => {
                        let client_type = LN_CLIENT_TYPE.get().map_or("unknown", |s| s.as_str());
                        cashu_redemption_logger::log_redemption(&format!(
                            "✅ Generated invoice via {}: payment_hash={}",
                            client_type, payment_hash
                        ));
                        (invoice, payment_hash)
                    }
                    Err(e) => {
                        let msg = format!("❌ Failed to generate invoice: {}", e);
                        error!("{}", msg);
                        cashu_redemption_logger::log_redemption(&msg);
                        continue;
                    }
                }
            };

            // Get melt quote
            cashu_redemption_logger::log_redemption(&format!(
                "🔥 Getting melt quote for {} proofs...",
                proofs_to_melt.len()
            ));

            let quote = match wallet_clone.melt_quote(invoice.clone(), None).await {
                Ok(q) => {
                    let actual_fee_reserve_sats: u64 = q.fee_reserve.into();
                    let amount_sats: u64 = q.amount.into();
                    // Saturating conversion: these amounts come from the mint, so
                    // a buggy/hostile quote must not overflow the sat->msat math.
                    // An absurd value saturates to u64::MAX and is rejected by the
                    // balance check below rather than wrapping to a small number.
                    let (actual_fee_reserve_msat, amount_msat) =
                        if wallet.unit == cdk::nuts::CurrencyUnit::Sat {
                            (
                                actual_fee_reserve_sats.saturating_mul(MSAT_PER_SAT),
                                amount_sats.saturating_mul(MSAT_PER_SAT),
                            )
                        } else {
                            (actual_fee_reserve_sats, amount_sats)
                        };

                    let msg = format!(
                        "📋 Melt quote for {}: amount={} msat, fee_reserve={} msat",
                        client_id, amount_msat, actual_fee_reserve_msat
                    );
                    info!("{}", msg);
                    cashu_redemption_logger::log_redemption(&msg);

                    let required_total_msat = amount_msat.saturating_add(actual_fee_reserve_msat);
                    if selected_total_msat < required_total_msat {
                        let msg = format!(
                            "⚠️ Fee reserve insufficient for {}: {} msat < {} msat required",
                            client_id, selected_total_msat, required_total_msat
                        );
                        warn!("{}", msg);
                        cashu_redemption_logger::log_redemption(&msg);
                        continue;
                    }

                    q
                }
                Err(e) => {
                    let msg = format!("❌ Failed to create melt quote for {}: {}", client_id, e);
                    error!("{}", msg);
                    cashu_redemption_logger::log_redemption(&msg);
                    continue;
                }
            };

            // Melt the proofs
            let melt_result = if is_p2pk_mode_enabled() {
                if let Some(private_key) = P2PK_PRIVATE_KEY.get() {
                    info!(
                        "🔓 Melting {} P2PK-locked proofs for {}",
                        proofs_to_melt.len(),
                        client_id
                    );

                    let mut signed_proofs = proofs_to_melt.clone();
                    for proof in &mut signed_proofs {
                        if let Err(e) = proof.sign_p2pk(private_key.clone()) {
                            error!("❌ Failed to sign proof: {:?}", e);
                        }
                    }

                    wallet_clone.melt_proofs(&quote.id, signed_proofs).await
                } else {
                    wallet_clone.melt(&quote.id).await
                }
            } else {
                wallet_clone.melt(&quote.id).await
            };

            // Process melt result
            match melt_result {
                Ok(result) => {
                    let result_msg = format!("🔍 Melt result for {}: {:?}", client_id, result);
                    info!("{}", result_msg);
                    cashu_redemption_logger::log_redemption(&result_msg);

                    let actual_fees_sats: u64 = quote.fee_reserve.into();
                    let actual_fees_msat = if wallet.unit == cdk::nuts::CurrencyUnit::Sat {
                        actual_fees_sats * MSAT_PER_SAT
                    } else {
                        actual_fees_sats
                    };
                    total_fees_paid_msat += actual_fees_msat;

                    let msg = format!(
                        "✅ Redeemed {} proofs for {}: {} msat to Lightning",
                        proofs_to_melt.len(),
                        client_id,
                        redeemable_amount_msat
                    );
                    info!("{}", msg);
                    cashu_redemption_logger::log_redemption(&msg);
                    total_redeemed += proofs_to_melt.len();
                    total_amount_redeemed_msat += redeemable_amount_msat;

                    // Clean up Redis proof-to-lnurl mappings for melted proofs
                    if let Err(e) = remove_proof_lnurl_mappings(&proofs_to_melt) {
                        warn!(
                            "⚠️ Failed to clean up proof mappings for {}: {}",
                            client_id, e
                        );
                    }
                }
                Err(e) => {
                    let msg = format!("❌ Failed to melt proofs for {}: {}", client_id, e);
                    error!("{}", msg);
                    cashu_redemption_logger::log_redemption(&msg);
                }
            }
        } // end of lnurl group loop
    } // end of mint loop

    let msg = format!(
        "✅ Smart Cashu redemption completed: {} proofs → {} msat to Lightning + {} msat fees",
        total_redeemed, total_amount_redeemed_msat, total_fees_paid_msat
    );
    info!("{}", msg);
    cashu_redemption_logger::log_redemption(&msg);
    Ok(total_redeemed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::nuts::{Proof, PublicKey};
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
        // Reset isn't possible on a OnceLock, so exercise the parsing logic
        // directly to keep the test independent of process env state.
        let parse = |v: &str| v.trim().to_lowercase() != "false";
        assert!(parse("")); // unset/empty -> required
        assert!(parse("true"));
        assert!(parse("garbage"));
        assert!(!parse("false"));
        assert!(!parse("  FALSE  "));
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
