//! `l402_mcp`: deciding from the Streamable HTTP request headers whether an MCP
//! request is charged, and for which tool.
//!
//! The 2026-07-28 transport mirrors the JSON-RPC `method` into `Mcp-Method` and
//! `params.name` / `params.uri` into `Mcp-Name`, and requires the server to
//! reject a request whose headers disagree with its body. The module never reads
//! the body, so the headers are trusted only when `MCP-Protocol-Version` names a
//! revision that requires that check. Anything else fails closed: a request we
//! cannot tie to a tool must not reach the upstream for free, and must not be
//! sold at a price that may belong to a cheaper tool.

use base64::Engine;

/// The first protocol revision whose servers must validate the mirrored
/// headers against the body.
pub const MCP_MIN_HEADER_VERSION: &str = "2026-07-28";

/// JSON-RPC code the spec assigns to header validation failures.
pub const MCP_HEADER_MISMATCH: i64 = -32020;

/// Implementation-defined JSON-RPC code (`-32000`..`-32019`) carried by the 402
/// body. MCP has no standard payment error yet, and the community proposal's
/// `-32042` sits in the range the spec reserves for itself.
pub const MCP_PAYMENT_REQUIRED: i64 = -32002;

/// Methods whose `Mcp-Name` header is required by the transport.
const NAMED_METHODS: [&str; 3] = ["tools/call", "resources/read", "prompts/get"];

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
/// charged methods (`tools/call` unless the operator opts others in).
pub fn classify(headers: &McpHeaders, charged: &[String]) -> McpDecision {
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
    if NAMED_METHODS.contains(&method) && name.is_none() {
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
        None => reject(&format!("{} request without Mcp-Name header", method)),
    }
}

/// Decode an `Mcp-Name` value: the `=?base64?…?=` sentinel form is standard
/// base64 of UTF-8; anything else must already be plain header text.
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
            out.push_str(&format!("%{:02X}", b));
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

/// Revisions are `YYYY-MM-DD`, so a well-formed one orders as a string.
fn version_requires_validation(version: &str) -> bool {
    let b = version.as_bytes();
    let well_formed = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    well_formed && version >= MCP_MIN_HEADER_VERSION
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
            classify(&h, &tools_only()),
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
            classify(&h, &tools_only()),
            McpDecision::Charged { .. }
        ));
    }

    #[test]
    fn discovery_passes_free() {
        let h = headers(&["2026-07-28"], &["tools/list"], &[]);
        assert_eq!(classify(&h, &tools_only()), McpDecision::Free);
    }

    #[test]
    fn resources_read_free_unless_opted_in() {
        let h = headers(&["2026-07-28"], &["resources/read"], &["file:///a"]);
        assert_eq!(classify(&h, &tools_only()), McpDecision::Free);
        let charged = vec!["tools/call".to_string(), "resources/read".to_string()];
        assert_eq!(
            classify(&h, &charged),
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
                matches!(classify(&h, &tools_only()), McpDecision::Reject(_)),
                "{h:?}"
            );
        }
    }

    #[test]
    fn older_client_discovery_is_rejected_too() {
        // Without validated headers we cannot tell discovery from a tool call.
        let h = headers(&["2025-11-25"], &["tools/list"], &[]);
        assert!(matches!(
            classify(&h, &tools_only()),
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
                matches!(classify(&h, &tools_only()), McpDecision::Reject(_)),
                "{h:?}"
            );
        }
    }

    #[test]
    fn named_method_without_name_rejected_even_when_free() {
        let h = headers(&["2026-07-28"], &["prompts/get"], &[]);
        assert!(matches!(
            classify(&h, &tools_only()),
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
            classify(&h, &tools_only()),
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
}
