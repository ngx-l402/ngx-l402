//! Runtime introspection document for the `l402_info_endpoint` handler:
//! what the module loaded (backend, Redis/Cashu) and the post-merge
//! per-location knobs — the operator-facing counterpart to the capability
//! manifest.
//!
//! Pure rendering only: the caller in `lib.rs` owns the nginx-FFI reads.
//! Nothing in this module may ever carry a secret — no macaroon key
//! paths, no NWC URIs, no Redis URLs, no wallet state. Add new fields
//! against that bar.

use serde_json::{json, Value};

/// Server-wide runtime state, gathered by the caller from the master-process
/// environment snapshot and startup-time statics.
#[derive(Clone, Debug)]
pub struct GlobalInfo {
    /// Active Lightning backend label (`LN_CLIENT_TYPE`: `LND`, `LNURL`,
    /// `NWC`, `CLN`, `BOLT12`, `ECLAIR`).
    pub backend: String,
    /// `REDIS_URL` was set at startup. Deliberately not a liveness check —
    /// pinging Redis from the handler would block the nginx event loop.
    pub redis_configured: bool,
    /// Cashu ecash support enabled (`CASHU_ECASH_SUPPORT`).
    pub cashu_enabled: bool,
}

/// Post-merge snapshot of one l402-protected location's operator knobs.
#[derive(Clone, Debug)]
pub struct LocationInfo {
    pub path: String,
    pub dry_run: bool,
    pub indefinite_access: bool,
    pub auto_detect_payment: bool,
    pub default_amount_msat: i64,
    /// `0` means no expiry, matching `l402_macaroon_timeout` semantics.
    pub macaroon_timeout_secs: i64,
    /// Route is hidden from the public manifest; surfaced here so the
    /// operator endpoint shows every protected route with its visibility.
    pub manifest_hidden: bool,
}

/// Render the introspection document as a pretty-printed JSON string.
pub fn render(global: &GlobalInfo, locations: &[LocationInfo]) -> String {
    let doc = json!({
        "version": "1",
        "backend": global.backend,
        "redis_configured": global.redis_configured,
        "cashu_enabled": global.cashu_enabled,
        "locations": locations.iter().map(location_block).collect::<Vec<_>>(),
    });

    // Infallible for `serde_json::Value`; the input is JSON we built.
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

fn location_block(l: &LocationInfo) -> Value {
    let mut block = json!({
        "path": l.path,
        "dry_run": l.dry_run,
        "indefinite_access": l.indefinite_access,
        "auto_detect_payment": l.auto_detect_payment,
        "default_amount_msat": l.default_amount_msat,
        "macaroon_timeout_secs": l.macaroon_timeout_secs,
    });
    if l.manifest_hidden {
        block["manifest_hidden"] = json!(true);
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global() -> GlobalInfo {
        GlobalInfo {
            backend: "LND".to_string(),
            redis_configured: true,
            cashu_enabled: false,
        }
    }

    fn location(path: &str) -> LocationInfo {
        LocationInfo {
            path: path.to_string(),
            dry_run: false,
            indefinite_access: false,
            auto_detect_payment: false,
            default_amount_msat: 10000,
            macaroon_timeout_secs: 0,
            manifest_hidden: false,
        }
    }

    #[test]
    fn renders_global_fields() {
        let doc: Value = serde_json::from_str(&render(&global(), &[])).unwrap();
        assert_eq!(doc["version"], "1");
        assert_eq!(doc["backend"], "LND");
        assert_eq!(doc["redis_configured"], true);
        assert_eq!(doc["cashu_enabled"], false);
        assert_eq!(doc["locations"], json!([]));
    }

    #[test]
    fn renders_per_location_fields() {
        let mut shadow = location("/shadow");
        shadow.dry_run = true;
        let mut indefinite = location("/protected-indefinite");
        indefinite.indefinite_access = true;
        indefinite.macaroon_timeout_secs = 60;

        let doc: Value = serde_json::from_str(&render(&global(), &[shadow, indefinite])).unwrap();
        let locations = doc["locations"].as_array().unwrap();
        assert_eq!(locations.len(), 2);

        assert_eq!(locations[0]["path"], "/shadow");
        assert_eq!(locations[0]["dry_run"], true);
        assert_eq!(locations[0]["indefinite_access"], false);
        assert_eq!(locations[0]["default_amount_msat"], 10000);

        assert_eq!(locations[1]["indefinite_access"], true);
        assert_eq!(locations[1]["macaroon_timeout_secs"], 60);
    }

    #[test]
    fn manifest_hidden_only_present_when_set() {
        let mut hidden = location("/internal");
        hidden.manifest_hidden = true;
        let visible = location("/protected");

        let doc: Value = serde_json::from_str(&render(&global(), &[hidden, visible])).unwrap();
        let locations = doc["locations"].as_array().unwrap();
        assert_eq!(locations[0]["manifest_hidden"], true);
        assert!(locations[1].get("manifest_hidden").is_none());
    }

    #[test]
    fn renders_unknown_backend_label() {
        // If `init_module` never ran (unit-test-style usage) the label is
        // unset and `backend_label()` falls back to "unknown"; the document
        // must still parse.
        let mut g = global();
        g.backend = "unknown".to_string();
        let doc: Value = serde_json::from_str(&render(&g, &[])).unwrap();
        assert_eq!(doc["backend"], "unknown");
    }
}
