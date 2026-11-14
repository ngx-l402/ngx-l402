//! `l402_mcp`: deciding from the Streamable HTTP request headers whether an MCP
//! request is charged, and for which tool.
//!
//! The 2026-07-28 transport mirrors the JSON-RPC `method` into `Mcp-Method` and
//! `params.name` / `params.uri` into `Mcp-Name`, and requires the server to
//! reject a request whose headers disagree with its body. The module never reads
//! the body, so the headers are only as good as the upstream's check, and only
//! the operator can vouch for that: the client picks `MCP-Protocol-Version`,
//! and an older or lax upstream skips the check whatever the client claims. So
//! the headers are trusted only when the operator declares that the upstream
//! validates them, and the client's version must also name a revision that
//! requires the check. Anything else fails closed: a request we cannot tie to a
//! tool must not reach the upstream for free, and must not be sold at a price
//! that may belong to a cheaper tool.

use base64::Engine;
use std::fmt::Write;

/// The first protocol revision whose servers must validate the mirrored
/// headers against the body.
pub const MCP_MIN_HEADER_VERSION: &str = "2026-07-28";

/// JSON-RPC code the spec assigns to header validation failures.
pub const MCP_HEADER_MISMATCH: i64 = -32020;

/// JSON-RPC code carried by the 402 body. MCP defines no payment error, and
/// 2026-07-28 keeps `-32000`..`-32099` off limits: `-32020` up is the spec's
/// own, no new codes go below it (`-32002` still means "resource not found" to
/// older clients), and new codes belong outside `-32768`..`-32000`. 402 is
/// outside that range and matches the HTTP status.
pub const MCP_PAYMENT_REQUIRED: i64 = 402;

/// Methods whose `Mcp-Name` header is required by the transport, and so the
/// only ones that can be charged: the price and the token are bound to a name.
pub const MCP_CHARGEABLE_METHODS: [&str; 3] = ["tools/call", "resources/read", "prompts/get"];

const B64_PREFIX: &str = "=?base64?";
const B64_SUFFIX: &str = "?=";

/// What the paywall does with one MCP request.
#[derive(Debug, PartialEq, Eq)]
pub enum McpDecision {
    /// The method is not charged at this location; pass it through.
    Free,
    /// Charge for the named tool, prompt or resource (already decoded).
    Charged { method: String, name: String },
    /// Reject with `400` and a `HeaderMismatch` body; the reason is for the
    /// message and the log.
    Reject(String),
}

/// The values of every occurrence of the three headers the decision reads.
/// Occurrences rather than a single value, so a duplicate can be refused
/// instead of silently picking one.
#[derive(Debug, Default)]
pub struct McpHeaders<'a> {
    pub method: Vec<&'a str>,
    pub name: Vec<&'a str>,
    pub protocol_version: Vec<&'a str>,
}

/// Decide whether a request is charged. `charged` is the location's list of
/// charged methods (`tools/call` unless the operator opts others in), each one
/// of [`MCP_CHARGEABLE_METHODS`]. `upstream_validates` is the operator's
/// declaration that the upstream rejects a request whose headers disagree with
/// its body; without it no header can be trusted, discovery included.
pub fn classify(headers: &McpHeaders, charged: &[String], upstream_validates: bool) -> McpDecision {
    if !upstream_validates {
        return reject("the upstream is not declared to validate MCP headers against the body");
    }
    let version = match single("MCP-Protocol-Version", &headers.protocol_version) {
        Ok(Some(v)) => v,
        Ok(None) => return reject("missing MCP-Protocol-Version header"),
        Err(e) => return McpDecision::Reject(e),
    };
    if !version_requires_validation(version) {
        return reject(&format!(
            "MCP-Protocol-Version {} predates {}; mirrored headers are not validated",
            version, MCP_MIN_HEADER_VERSION
        ));
    }

    let method = match single("Mcp-Method", &headers.method) {
        Ok(Some(m)) => m,
        Ok(None) => return reject("missing Mcp-Method header"),
        Err(e) => return McpDecision::Reject(e),
    };
    if !is_plain_header_text(method) || method.is_empty() {
        return reject("Mcp-Method header contains invalid characters");
    }

    let raw_name = match single("Mcp-Name", &headers.name) {
        Ok(n) => n,
        Err(e) => return McpDecision::Reject(e),
    };
    let name = match raw_name {
        Some(raw) => match decode_mcp_name(raw) {
            Ok(n) => Some(n),
            Err(e) => return McpDecision::Reject(e),
        },
        None => None,
    };
    if MCP_CHARGEABLE_METHODS.contains(&method) && name.is_none() {
        return reject(&format!("{} request without Mcp-Name header", method));
    }

    if !charged.iter().any(|c| c == method) {
        return McpDecision::Free;
    }
    match name {
        Some(name) => McpDecision::Charged {
            method: method.to_string(),
            name,
        },
        // A charged method must carry a name to price and bind the token to.
        // Only reached when `charged` holds a method outside
        // `MCP_CHARGEABLE_METHODS`, which the config should refuse.
        None => reject(&format!("{} request without Mcp-Name header", method)),
    }
}

/// Decode an `Mcp-Name` value: the `=?base64?…?=` sentinel form is padded,
/// standard-alphabet base64 of UTF-8, the form the spec's examples use;
/// anything else must already be plain header text. Other base64 spellings are
/// refused rather than guessed at, so this reading of a name cannot differ
/// from the upstream's.
pub fn decode_mcp_name(raw: &str) -> Result<String, String> {
    if let Some(inner) = raw
        .strip_prefix(B64_PREFIX)
        .and_then(|s| s.strip_suffix(B64_SUFFIX))
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(inner)
            .map_err(|_| "Mcp-Name header is not valid base64".to_string())?;
        let name = String::from_utf8(bytes)
            .map_err(|_| "Mcp-Name header does not decode to UTF-8".to_string())?;
        if name.is_empty() {
            return Err("Mcp-Name header is empty".to_string());
        }
        // The name reaches log lines and the token's caveat, and no tool,
        // prompt or resource URI needs a control character.
        if name.chars().any(char::is_control) {
            return Err("Mcp-Name header decodes to control characters".to_string());
        }
        return Ok(name);
    }
    if raw.is_empty() {
        return Err("Mcp-Name header is empty".to_string());
    }
    if !is_plain_header_text(raw) {
        return Err("Mcp-Name header contains invalid characters".to_string());
    }
    Ok(raw.to_string())
}

/// Redis key for a tool's dynamic price. The name is percent-encoded including
/// `:` and `%`, so the last `:` always separates path from name and a path that
/// contains `:` cannot produce another route's key.
pub fn mcp_tool_price_key(path: &str, name: &str) -> String {
    let mut out = String::with_capacity(9 + path.len() + 1 + name.len());
    out.push_str("mcp-tool:");
    out.push_str(path);
    out.push(':');
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// JSON-RPC error body without `id`, the shape the spec uses for its own
/// transport-level rejections. `data` is inserted verbatim and must already be
/// a JSON value.
pub fn jsonrpc_error_body(code: i64, message: &str, data: Option<&str>) -> String {
    let message = crate::escape_json(message);
    match data {
        Some(d) => format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":{code},\"message\":\"{message}\",\"data\":{d}}}}}"
        ),
        None => format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{{\"code\":{code},\"message\":\"{message}\"}}}}"
        ),
    }
}

fn reject(reason: &str) -> McpDecision {
    McpDecision::Reject(reason.to_string())
}

fn single<'a>(header: &str, values: &[&'a str]) -> Result<Option<&'a str>, String> {
    match values {
        [] => Ok(None),
        [v] => Ok(Some(v.trim_matches(|c| c == ' ' || c == '\t'))),
        _ => Err(format!("duplicate {} header", header)),
    }
}

/// Revisions are `YYYY-MM-DD` dates, so a real one orders as a string.
fn version_requires_validation(version: &str) -> bool {
    is_revision_date(version) && version >= MCP_MIN_HEADER_VERSION
}

/// A `YYYY-MM-DD` that names a day on the calendar, so `9999-99-99` is not
/// taken for a revision after every real one.
fn is_revision_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |digits: &[u8]| {
        digits.iter().try_fold(0u32, |n, c| {
            c.is_ascii_digit().then(|| n * 10 + u32::from(c - b'0'))
        })
    };
    let (Some(year), Some(month), Some(day)) = (num(&b[..4]), num(&b[5..7]), num(&b[8..])) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

/// Visible ASCII and space, with no leading or trailing space: values outside
/// that set must arrive in the base64 form.
fn is_plain_header_text(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b)) && s.trim() == s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_only() -> Vec<String> {
        vec!["tools/call".to_string()]
    }

    fn headers<'a>(version: &[&'a str], method: &[&'a str], name: &[&'a str]) -> McpHeaders<'a> {
        McpHeaders {
            method: method.to_vec(),
            name: name.to_vec(),
            protocol_version: version.to_vec(),
        }
    }

    #[test]
    fn tools_call_is_charged_for_its_tool() {
        let h = headers(&["2026-07-28"], &["tools/call"], &["search"]);
        assert_eq!(
            classify(&h, &tools_only(), true),
            McpDecision::Charged {
                method: "tools/call".into(),
                name: "search".into()
            }
        );
    }

    #[test]
    fn later_revision_is_trusted() {
        let h = headers(&["2027-01-01"], &["tools/call"], &["search"]);
        assert!(matches!(
            classify(&h, &tools_only(), true),
            McpDecision::Charged { .. }
        ));
    }

    #[test]
    fn discovery_passes_free() {
        let h = headers(&["2026-07-28"], &["tools/list"], &[]);
        assert_eq!(classify(&h, &tools_only(), true), McpDecision::Free);
    }

    #[test]
    fn resources_read_free_unless_opted_in() {
        let h = headers(&["2026-07-28"], &["resources/read"], &["file:///a"]);
        assert_eq!(classify(&h, &tools_only(), true), McpDecision::Free);
        let charged = vec!["tools/call".to_string(), "resources/read".to_string()];
        assert_eq!(
            classify(&h, &charged, true),
            McpDecision::Charged {
                method: "resources/read".into(),
                name: "file:///a".into()
            }
        );
    }

    #[test]
    fn missing_headers_fail_closed() {
        for h in [
            headers(&[], &["tools/call"], &["search"]),
            headers(&["2025-11-25"], &["tools/call"], &["search"]),
            headers(&["2026-07-28"], &[], &["search"]),
            headers(&["2026-07-28"], &["tools/call"], &[]),
            headers(&["latest"], &["tools/call"], &["search"]),
            headers(&["2026-7-28"], &["tools/call"], &["search"]),
        ] {
            assert!(
                matches!(classify(&h, &tools_only(), true), McpDecision::Reject(_)),
                "{h:?}"
            );
        }
    }

    #[test]
    fn older_client_discovery_is_rejected_too() {
        // Without validated headers we cannot tell discovery from a tool call.
        let h = headers(&["2025-11-25"], &["tools/list"], &[]);
        assert!(matches!(
            classify(&h, &tools_only(), true),
            McpDecision::Reject(_)
        ));
    }

    #[test]
    fn duplicates_are_rejected() {
        for h in [
            headers(&["2026-07-28"], &["tools/list", "tools/call"], &["search"]),
            headers(&["2026-07-28"], &["tools/call"], &["cheap", "pricey"]),
            headers(&["2026-07-28", "2026-07-28"], &["tools/call"], &["search"]),
        ] {
            assert!(
                matches!(classify(&h, &tools_only(), true), McpDecision::Reject(_)),
                "{h:?}"
            );
        }
    }

    #[test]
    fn named_method_without_name_rejected_even_when_free() {
        let h = headers(&["2026-07-28"], &["prompts/get"], &[]);
        assert!(matches!(
            classify(&h, &tools_only(), true),
            McpDecision::Reject(_)
        ));
    }

    #[test]
    fn encoded_name_is_decoded_before_pricing() {
        // "summarize" — an encoded expensive tool must not miss its price line.
        let h = headers(
            &["2026-07-28"],
            &["tools/call"],
            &["=?base64?c3VtbWFyaXpl?="],
        );
        assert_eq!(
            classify(&h, &tools_only(), true),
            McpDecision::Charged {
                method: "tools/call".into(),
                name: "summarize".into()
            }
        );
    }

    #[test]
    fn decode_spec_examples() {
        assert_eq!(
            decode_mcp_name("=?base64?SGVsbG8sIOS4lueVjA==?=").unwrap(),
            "Hello, 世界"
        );
        assert_eq!(
            decode_mcp_name("=?base64?IHBhZGRlZCA=?=").unwrap(),
            " padded "
        );
        assert_eq!(
            decode_mcp_name("=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?=").unwrap(),
            "=?base64?literal?="
        );
        assert_eq!(decode_mcp_name("get_weather").unwrap(), "get_weather");
    }

    #[test]
    fn decode_rejects_malformed() {
        assert!(decode_mcp_name("=?base64?not base64!?=").is_err());
        assert!(decode_mcp_name("=?base64?/w==?=").is_err()); // 0xFF, not UTF-8
        assert!(decode_mcp_name("=?base64??=").is_err());
        assert!(decode_mcp_name("").is_err());
        assert!(decode_mcp_name("caf\u{e9}").is_err());
        assert!(decode_mcp_name("a\tb").is_err());
        // Sentinel markers are case-sensitive: this is a plain value.
        assert_eq!(
            decode_mcp_name("=?BASE64?eA==?=").unwrap(),
            "=?BASE64?eA==?="
        );
    }

    #[test]
    fn price_key_escapes_name() {
        assert_eq!(mcp_tool_price_key("/mcp", "search"), "mcp-tool:/mcp:search");
        assert_eq!(
            mcp_tool_price_key("/mcp", "a:b %"),
            "mcp-tool:/mcp:a%3Ab%20%25"
        );
        // Path "/a:b" + "c" must not collide with path "/a" + "b:c".
        assert_ne!(
            mcp_tool_price_key("/a:b", "c"),
            mcp_tool_price_key("/a", "b:c")
        );
    }

    #[test]
    fn error_body_has_null_id_and_escapes() {
        assert_eq!(
            jsonrpc_error_body(MCP_HEADER_MISMATCH, "bad \"x\"", None),
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32020,"message":"bad \"x\""}}"#
        );
        let body = jsonrpc_error_body(MCP_PAYMENT_REQUIRED, "pay", Some(r#"{"a":1}"#));
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["data"]["a"], 1);
        assert!(v["id"].is_null());
    }

    #[test]
    fn undeclared_upstream_trusts_no_header() {
        // A client can claim any version; only the operator can say the
        // upstream checks headers against the body.
        for h in [
            headers(&["2026-07-28"], &["tools/call"], &["search"]),
            headers(&["2026-07-28"], &["tools/list"], &[]),
        ] {
            assert!(
                matches!(classify(&h, &tools_only(), false), McpDecision::Reject(_)),
                "{h:?}"
            );
        }
    }

    #[test]
    fn version_must_be_a_real_date() {
        for v in [
            "9999-99-99",
            "2026-13-01",
            "2026-00-10",
            "2026-09-31",
            "2027-02-29",
            "2026-08-00",
        ] {
            let h = headers(&[v], &["tools/call"], &["search"]);
            assert!(
                matches!(classify(&h, &tools_only(), true), McpDecision::Reject(_)),
                "{v}"
            );
        }
        for v in ["2026-07-28", "2028-02-29", "2026-12-31"] {
            let h = headers(&[v], &["tools/call"], &["search"]);
            assert!(
                matches!(
                    classify(&h, &tools_only(), true),
                    McpDecision::Charged { .. }
                ),
                "{v}"
            );
        }
    }

    #[test]
    fn charged_method_without_name_never_reaches_payment() {
        // Why the config refuses to charge anything outside
        // MCP_CHARGEABLE_METHODS: such a request could only ever be a 400.
        assert!(!MCP_CHARGEABLE_METHODS.contains(&"completion/complete"));
        let h = headers(&["2026-07-28"], &["completion/complete"], &[]);
        let charged = vec!["completion/complete".to_string()];
        assert!(matches!(
            classify(&h, &charged, true),
            McpDecision::Reject(_)
        ));
    }

    #[test]
    fn decode_rejects_control_characters() {
        for encoded in [
            "=?base64?YQ0KYgA=?=",         // "a\r\nb\0"
            "=?base64?bGluZTEKbGluZTI=?=", // "line1\nline2", the spec's own example
            "=?base64?YQli?=",             // "a\tb"
            "=?base64?fw==?=",             // DEL
            "=?base64?woU=?=",             // U+0085, a C1 control
        ] {
            assert!(decode_mcp_name(encoded).is_err(), "{encoded}");
        }
        assert_eq!(decode_mcp_name("=?base64?Y2Fmw6k=?=").unwrap(), "caf\u{e9}");
    }

    #[test]
    fn decode_accepts_only_padded_standard_base64() {
        assert_eq!(decode_mcp_name("=?base64?eA==?=").unwrap(), "x");
        // Unpadded and URL-safe spellings are refused, not guessed at.
        assert!(decode_mcp_name("=?base64?eA?=").is_err());
        assert!(decode_mcp_name("=?base64?Pz8-?=").is_err());
        assert!(decode_mcp_name("=?base64?Pz8_?=").is_err());
        assert_eq!(decode_mcp_name("=?base64?Pz8+?=").unwrap(), "??>");
    }

    #[test]
    fn payment_code_is_outside_the_reserved_range() {
        assert!(!(-32768..=-32000).contains(&MCP_PAYMENT_REQUIRED));
    }
}
