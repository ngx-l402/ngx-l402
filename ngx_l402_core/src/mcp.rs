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
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt::Write;

/// The first protocol revision whose servers must validate the mirrored
/// headers against the body.
pub const MCP_MIN_HEADER_VERSION: &str = "2026-07-28";

/// JSON-RPC code the spec assigns to header validation failures.
pub const MCP_HEADER_MISMATCH: i64 = -32020;

/// JSON-RPC code the spec assigns to a protocol version the server doesn't
/// know or support. Its `data` names the supported versions so a client that
/// also speaks one of them can retry.
pub const MCP_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// JSON-RPC's own codes, for a request body the paywall can't classify
/// (`l402_mcp_legacy`): not JSON, not one request object, or a charged call
/// that doesn't name what it calls.
pub const JSONRPC_PARSE_ERROR: i64 = -32700;
pub const JSONRPC_INVALID_REQUEST: i64 = -32600;
pub const JSONRPC_INVALID_PARAMS: i64 = -32602;

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

/// What one `l402_mcp_tool <name> <price>;` line sets for its tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpToolPrice {
    /// `free`: calls pass without payment, like an uncharged method.
    Free,
    /// A positive price in millisatoshi.
    Msat(i64),
}

impl McpToolPrice {
    /// Parse the directive's price argument: `free` or a positive msat
    /// amount. `0` is refused rather than read as free: elsewhere it means
    /// "unset", and a stray `0` must not give a tool away.
    pub fn parse(s: &str) -> Option<Self> {
        if s == "free" {
            return Some(McpToolPrice::Free);
        }
        s.parse::<i64>()
            .ok()
            .filter(|p| *p > 0)
            .map(McpToolPrice::Msat)
    }
}

/// What the paywall does with one MCP request.
#[derive(Debug, PartialEq, Eq)]
pub enum McpDecision {
    /// The method is not charged at this location, or the tool is listed
    /// `free`; pass it through.
    Free,
    /// Charge for the named tool, prompt or resource (already decoded).
    Charged { method: String, name: String },
    /// Reject with `400` and a `HeaderMismatch` body; the reason is for the
    /// message and the log.
    Reject(String),
    /// Reject with `400` and an `UnsupportedProtocolVersion` body: the client's
    /// `MCP-Protocol-Version` is older than [`MCP_MIN_HEADER_VERSION`]
    /// (`requested` holds it) or isn't a revision date at all (`None`, so the
    /// raw value is never echoed).
    UnsupportedVersion { requested: Option<String> },
    /// Reject with `400` and the given JSON-RPC code: the request was to be
    /// classified from its body (`l402_mcp_legacy`) and the body can't be read
    /// the one way every JSON parser would read it.
    BadBody { code: i64, reason: String },
}

impl McpDecision {
    /// Why the request is refused, for the log; `None` unless it is.
    pub fn reject_reason(&self) -> Option<String> {
        match self {
            McpDecision::Reject(reason) | McpDecision::BadBody { reason, .. } => {
                Some(reason.clone())
            }
            McpDecision::UnsupportedVersion { requested: Some(v) } => Some(format!(
                "MCP-Protocol-Version {} predates {}; mirrored headers are not validated",
                v, MCP_MIN_HEADER_VERSION
            )),
            McpDecision::UnsupportedVersion { requested: None } => {
                Some("MCP-Protocol-Version header is not a protocol revision date".to_string())
            }
            McpDecision::Free | McpDecision::Charged { .. } => None,
        }
    }

    /// The JSON-RPC body sent with the `400`; `None` unless it is refused.
    pub fn reject_body(&self) -> Option<String> {
        match self {
            McpDecision::UnsupportedVersion { requested } => Some(jsonrpc_error_body(
                MCP_UNSUPPORTED_PROTOCOL_VERSION,
                "Unsupported protocol version",
                Some(&format!(
                    "{{\"supported\":[\"{}\"],\"requested\":\"{}\"}}",
                    MCP_MIN_HEADER_VERSION,
                    // `requested` is required by the schema; a value that
                    // isn't a real date is sent as "" rather than echoed.
                    requested.as_deref().unwrap_or("")
                )),
            )),
            McpDecision::Reject(_) => self.reject_reason().map(|reason| {
                jsonrpc_error_body(
                    MCP_HEADER_MISMATCH,
                    &format!("Header mismatch: {}", reason),
                    None,
                )
            }),
            McpDecision::BadBody { code, reason } => {
                let label = match *code {
                    JSONRPC_PARSE_ERROR => "Parse error",
                    JSONRPC_INVALID_PARAMS => "Invalid params",
                    _ => "Invalid request",
                };
                Some(jsonrpc_error_body(
                    *code,
                    &format!("{}: {}", label, reason),
                    None,
                ))
            }
            McpDecision::Free | McpDecision::Charged { .. } => None,
        }
    }
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
/// of [`MCP_CHARGEABLE_METHODS`]. `tools` is the location's `l402_mcp_tool`
/// lines; a name listed `free` passes under every charged method, after the
/// same header checks as a charged call. `upstream_validates` is the operator's
/// declaration that the upstream rejects a request whose headers disagree with
/// its body; without it no header can be trusted, discovery included.
///
/// Refusing an undeclared upstream here is a backstop: it answers every
/// client with a `400` for something only the operator can fix, so the config
/// must refuse `l402_mcp` without the declaration at `nginx -t`.
pub fn classify(
    headers: &McpHeaders,
    charged: &[String],
    tools: &[(String, McpToolPrice)],
    upstream_validates: bool,
) -> McpDecision {
    if !upstream_validates {
        return reject("the upstream is not declared to validate MCP headers against the body");
    }
    let version = match single("MCP-Protocol-Version", &headers.protocol_version) {
        Ok(Some(v)) => v,
        Ok(None) => return reject("missing MCP-Protocol-Version header"),
        Err(e) => return McpDecision::Reject(e),
    };
    // Checked before the value appears in any message: only a real date is
    // echoed back. Revisions are `YYYY-MM-DD`, so a real one orders as a string.
    if !is_revision_date(version) {
        return McpDecision::UnsupportedVersion { requested: None };
    }
    if version < MCP_MIN_HEADER_VERSION {
        return McpDecision::UnsupportedVersion {
            requested: Some(version.to_string()),
        };
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
        Some(name) => charge_or_free(method, name, tools),
        // A charged method must carry a name to price and bind the token to.
        // Only reached when `charged` holds a method outside
        // `MCP_CHARGEABLE_METHODS`, which the config should refuse.
        None => reject(&format!("{} request without Mcp-Name header", method)),
    }
}

/// A call to a charged method, by name: free when the name is listed `free`,
/// charged otherwise. Decided here rather than as a price of zero: a free call
/// then needs no Redis lookup, no token and no payment backend.
fn charge_or_free(method: &str, name: String, tools: &[(String, McpToolPrice)]) -> McpDecision {
    if tools
        .iter()
        .any(|(n, p)| *n == name && *p == McpToolPrice::Free)
    {
        return McpDecision::Free;
    }
    McpDecision::Charged {
        method: method.to_string(),
        name,
    }
}

/// Where a request's method and tool are read from on a location that also
/// serves older clients (`l402_mcp_legacy on`).
#[derive(Debug, PartialEq, Eq)]
pub enum McpSource {
    /// 2026-07-28 or later: the mirrored headers, as [`classify`] reads them.
    Headers,
    /// An older revision, or no `MCP-Protocol-Version` at all (the first
    /// `initialize` of a session carries none): the JSON-RPC body, as
    /// [`classify_body`] reads it.
    Body,
}

/// Pick the source for one request. Only the version header decides, the way
/// an upstream that serves both eras routes on it; a duplicate, or a value
/// that isn't a revision date, is refused as [`classify`] refuses it.
pub fn source(headers: &McpHeaders) -> Result<McpSource, McpDecision> {
    match single("MCP-Protocol-Version", &headers.protocol_version) {
        Err(e) => Err(McpDecision::Reject(e)),
        Ok(None) => Ok(McpSource::Body),
        Ok(Some(v)) if !is_revision_date(v) => {
            Err(McpDecision::UnsupportedVersion { requested: None })
        }
        Ok(Some(v)) if v < MCP_MIN_HEADER_VERSION => Ok(McpSource::Body),
        Ok(Some(_)) => Ok(McpSource::Headers),
    }
}

/// The `params` key that names what a chargeable method calls.
fn name_key(method: &str) -> Option<&'static str> {
    match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    }
}

/// Decide a request from its JSON-RPC body, for a client older than the
/// mirrored headers. `charged` and `tools` are as in [`classify`].
///
/// The upstream parses the same bytes with its own JSON parser, and the two
/// must agree on the method and the name, or a call could be priced as one
/// tool and run as another. So the body is refused, never guessed at, wherever
/// parsers are known to differ:
///
/// - a key that appears twice in the request object or in `params` (first or
///   last wins, by parser);
/// - `method`, `params`, `name` or `uri` in another letter case, or any
///   non-ASCII key next to them (some parsers match keys case-insensitively,
///   Unicode case folding included);
/// - a batch array (allowed only by 2025-03-26, and one body would then hold
///   several calls);
/// - anything that isn't UTF-8 JSON with nothing after it.
///
/// A body without a `method` is a response to the server and passes free.
///
/// Returned with the decision is the request's `id` as JSON text (a string or
/// a number; `None` for a notification), so a charged call can be answered
/// with a JSON-RPC error response instead of a bare HTTP status.
pub fn classify_body(
    body: &[u8],
    charged: &[String],
    tools: &[(String, McpToolPrice)],
) -> (McpDecision, Option<String>) {
    let mut de = serde_json::Deserializer::from_slice(body);
    let mut scan = match de.deserialize_any(TopVisitor).and_then(|top| {
        de.end()?;
        Ok(top)
    }) {
        Ok(Top::Request(scan)) => scan,
        other => return (classify_unscanned(other), None),
    };
    let id = scan.id.take();
    (classify_scan(scan, charged, tools), id)
}

/// The decision for a body that isn't one request object.
fn classify_unscanned(top: Result<Top, serde_json::Error>) -> McpDecision {
    match top {
        // Not reached: `classify_body` keeps a request for `classify_scan`.
        Ok(Top::Request(_)) => bad_body(JSONRPC_INVALID_REQUEST, "the body was not classified"),
        Ok(Top::Batch) => bad_body(JSONRPC_INVALID_REQUEST, "batch requests are not supported"),
        Ok(Top::Other) => bad_body(
            JSONRPC_INVALID_REQUEST,
            "the body is not a JSON-RPC request object",
        ),
        // The parser's message can quote the body; the client gets none of it.
        Err(_) => bad_body(JSONRPC_PARSE_ERROR, "the body is not valid JSON"),
    }
}

fn classify_scan(
    scan: RequestScan,
    charged: &[String],
    tools: &[(String, McpToolPrice)],
) -> McpDecision {
    if let Some(problem) = scan.problem {
        return bad_body(JSONRPC_INVALID_REQUEST, problem);
    }
    let method = match scan.method {
        Field::Missing => return McpDecision::Free,
        Field::Other => return bad_body(JSONRPC_INVALID_REQUEST, "method is not a string"),
        Field::Str(m) => m,
    };
    if !charged.contains(&method) {
        return McpDecision::Free;
    }
    // As in `classify`: a charged method must name what it calls.
    let Some(key) = name_key(&method) else {
        return bad_body(JSONRPC_INVALID_REQUEST, "this method cannot be charged");
    };
    let Some(Params::Object(params)) = scan.params else {
        return bad_body(JSONRPC_INVALID_PARAMS, "params is not an object");
    };
    if let Some(problem) = params.problem {
        return bad_body(JSONRPC_INVALID_REQUEST, problem);
    }
    let name = match if key == "uri" {
        params.uri
    } else {
        params.name
    } {
        Field::Str(name) => name,
        Field::Missing | Field::Other => {
            return bad_body(
                JSONRPC_INVALID_PARAMS,
                if key == "uri" {
                    "params.uri is not a string"
                } else {
                    "params.name is not a string"
                },
            )
        }
    };
    // The same bounds a decoded `Mcp-Name` has: the name reaches log lines and
    // the token's caveat.
    if name.is_empty() || name.chars().any(char::is_control) {
        return bad_body(
            JSONRPC_INVALID_PARAMS,
            "the name is empty or contains control characters",
        );
    }
    charge_or_free(&method, name, tools)
}

fn bad_body(code: i64, reason: &str) -> McpDecision {
    McpDecision::BadBody {
        code,
        reason: reason.to_string(),
    }
}

/// A value [`classify_body`] reads: absent, a string, or something else.
#[derive(Default)]
enum Field {
    #[default]
    Missing,
    Str(String),
    Other,
}

impl Field {
    fn from_value(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::String(s) => Field::Str(s),
            _ => Field::Other,
        }
    }
}

/// What the keys of one JSON object say, for the keys [`classify_body`] reads.
/// `problem` is the first reason parsers could disagree about them.
#[derive(Default)]
struct RequestScan {
    /// The request's `id` as JSON text, when it is a string or a number.
    id: Option<String>,
    method: Field,
    params: Option<Params>,
    problem: Option<&'static str>,
}

#[derive(Default)]
struct ParamsScan {
    name: Field,
    uri: Field,
    problem: Option<&'static str>,
}

enum Params {
    Object(ParamsScan),
    Other,
}

enum Top {
    Request(RequestScan),
    Batch,
    Other,
}

/// Note one key of an object: `Some` when it makes the object ambiguous.
/// `reserved` are the lowercase keys read from this object.
fn key_problem(key: &str, seen: &mut HashSet<String>, reserved: &[&str]) -> Option<&'static str> {
    if !key.is_ascii() {
        return Some("a key is not ASCII");
    }
    let lower = key.to_ascii_lowercase();
    if reserved.contains(&lower.as_str()) && key != lower {
        return Some("a key differs from a JSON-RPC key only in letter case");
    }
    if !seen.insert(lower) {
        return Some("a key appears twice");
    }
    None
}

struct TopVisitor;

impl<'de> Visitor<'de> for TopVisitor {
    type Value = Top;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Top, A::Error> {
        let mut scan = RequestScan::default();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let problem = key_problem(&key, &mut seen, &["method", "params"]);
            scan.problem = scan.problem.or(problem);
            match key.as_str() {
                "method" => scan.method = Field::from_value(map.next_value()?),
                "params" => scan.params = Some(map.next_value_seed(ParamsSeed)?),
                "id" => {
                    scan.id = match map.next_value::<serde_json::Value>()? {
                        v @ (serde_json::Value::String(_) | serde_json::Value::Number(_)) => {
                            serde_json::to_string(&v).ok()
                        }
                        _ => None,
                    }
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Top::Request(scan))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Top, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Top::Batch)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Top, E> {
        Ok(Top::Other)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Top, E> {
        Ok(Top::Other)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Top, E> {
        Ok(Top::Other)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Top, E> {
        Ok(Top::Other)
    }
    fn visit_str<E>(self, _: &str) -> Result<Top, E> {
        Ok(Top::Other)
    }
    fn visit_unit<E>(self) -> Result<Top, E> {
        Ok(Top::Other)
    }
}

struct ParamsSeed;

impl<'de> serde::de::DeserializeSeed<'de> for ParamsSeed {
    type Value = Params;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Params, D::Error> {
        d.deserialize_any(ParamsVisitor)
    }
}

struct ParamsVisitor;

impl<'de> Visitor<'de> for ParamsVisitor {
    type Value = Params;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Params, A::Error> {
        let mut scan = ParamsScan::default();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            let problem = key_problem(&key, &mut seen, &["name", "uri"]);
            scan.problem = scan.problem.or(problem);
            match key.as_str() {
                "name" => scan.name = Field::from_value(map.next_value()?),
                "uri" => scan.uri = Field::from_value(map.next_value()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(Params::Object(scan))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Params, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Params::Other)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Params, E> {
        Ok(Params::Other)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Params, E> {
        Ok(Params::Other)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Params, E> {
        Ok(Params::Other)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Params, E> {
        Ok(Params::Other)
    }
    fn visit_str<E>(self, _: &str) -> Result<Params, E> {
        Ok(Params::Other)
    }
    fn visit_unit<E>(self) -> Result<Params, E> {
        Ok(Params::Other)
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
        // prompt or resource URI needs a control character. `is_control` is
        // `Cc` only: U+2028/2029, bidi overrides and zero-width characters
        // still pass, so every sink must escape the name (`{:?}` and
        // `escape_json` do).
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

/// JSON-RPC error body with no `id` member: the schema makes an error
/// response's `id` optional and `string | number`, never `null`, and the
/// transport answers these rejections with "a JSON-RPC error response that has
/// no `id`". `data` is inserted verbatim and must already be a JSON value.
pub fn jsonrpc_error_body(code: i64, message: &str, data: Option<&str>) -> String {
    let message = crate::escape_json(message);
    match data {
        Some(d) => format!(
            "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":{code},\"message\":\"{message}\",\"data\":{d}}}}}"
        ),
        None => format!(
            "{{\"jsonrpc\":\"2.0\",\"error\":{{\"code\":{code},\"message\":\"{message}\"}}}}"
        ),
    }
}

/// A JSON-RPC error *response*: [`jsonrpc_error_body`] with the `id` of the
/// request it answers. `id` is JSON text, as [`classify_body`] returns it.
pub fn jsonrpc_error_response(id: &str, code: i64, message: &str, data: Option<&str>) -> String {
    let message = crate::escape_json(message);
    let data = data.map(|d| format!(",\"data\":{d}")).unwrap_or_default();
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":\"{message}\"{data}}}}}"
    )
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
            classify(&h, &tools_only(), &[], true),
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
            classify(&h, &tools_only(), &[], true),
            McpDecision::Charged { .. }
        ));
    }

    fn tool(name: &str, price: McpToolPrice) -> (String, McpToolPrice) {
        (name.to_string(), price)
    }

    #[test]
    fn free_tool_passes_and_others_stay_charged() {
        let tools = [
            tool("ping", McpToolPrice::Free),
            tool("summarize", McpToolPrice::Msat(5000)),
        ];
        let call = |name: &'static str| {
            classify(
                &headers(&["2026-07-28"], &["tools/call"], &[name]),
                &tools_only(),
                &tools,
                true,
            )
        };
        assert_eq!(call("ping"), McpDecision::Free);
        // The encoded spelling of the same name is free too.
        assert_eq!(call("=?base64?cGluZw==?="), McpDecision::Free);
        // A priced tool, an unlisted one and a near miss are all charged.
        for name in ["summarize", "search", "Ping", "pings"] {
            assert!(
                matches!(call(name), McpDecision::Charged { .. }),
                "{name:?}"
            );
        }
    }

    #[test]
    fn free_tool_still_needs_trustworthy_headers() {
        let tools = [tool("ping", McpToolPrice::Free)];
        let h = headers(&["2025-11-25"], &["tools/call"], &["ping"]);
        assert!(matches!(
            classify(&h, &tools_only(), &tools, true),
            McpDecision::UnsupportedVersion { .. }
        ));
        let h = headers(&["2026-07-28"], &["tools/call"], &["ping", "search"]);
        assert!(matches!(
            classify(&h, &tools_only(), &tools, true),
            McpDecision::Reject(_)
        ));
        let h = headers(&["2026-07-28"], &["tools/call"], &["ping"]);
        assert!(matches!(
            classify(&h, &tools_only(), &tools, false),
            McpDecision::Reject(_)
        ));
    }

    #[test]
    fn tool_price_parses_free_or_a_positive_amount() {
        assert_eq!(McpToolPrice::parse("free"), Some(McpToolPrice::Free));
        assert_eq!(McpToolPrice::parse("5000"), Some(McpToolPrice::Msat(5000)));
        for bad in ["0", "-1", "", "Free", "FREE", "free ", "1.5", "1000msat"] {
            assert_eq!(McpToolPrice::parse(bad), None, "{bad:?}");
        }
    }

    fn body(json: &str) -> McpDecision {
        let tools = [
            tool("ping", McpToolPrice::Free),
            tool("summarize", McpToolPrice::Msat(5000)),
        ];
        classify_body(json.as_bytes(), &tools_only(), &tools).0
    }

    fn bad_code(d: &McpDecision) -> Option<i64> {
        match d {
            McpDecision::BadBody { code, .. } => Some(*code),
            _ => None,
        }
    }

    #[test]
    fn source_follows_the_version_header() {
        let src = |v: &[&str]| source(&headers(v, &[], &[]));
        // The first `initialize` of an older session carries no version.
        assert_eq!(src(&[]), Ok(McpSource::Body));
        for v in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            assert_eq!(src(&[v]), Ok(McpSource::Body), "{v}");
        }
        assert_eq!(src(&["2026-07-28"]), Ok(McpSource::Headers));
        assert_eq!(src(&["2027-01-01"]), Ok(McpSource::Headers));
        // Refused as on a header-only location.
        assert!(matches!(
            src(&["2025-11-25", "2026-07-28"]),
            Err(McpDecision::Reject(_))
        ));
        assert_eq!(
            src(&["latest"]),
            Err(McpDecision::UnsupportedVersion { requested: None })
        );
    }

    #[test]
    fn body_call_is_charged_for_its_tool() {
        assert_eq!(
            body(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search","arguments":{"q":"x"}}}"#
            ),
            McpDecision::Charged {
                method: "tools/call".into(),
                name: "search".into()
            }
        );
        // Key order and escapes don't change what is read.
        assert_eq!(
            body(
                r#"{"params":{"arguments":{},"name":"summ\u0061rize"},"method":"tools\/call","id":"a"}"#
            ),
            McpDecision::Charged {
                method: "tools/call".into(),
                name: "summarize".into()
            }
        );
    }

    #[test]
    fn body_request_id_comes_back_as_json_text() {
        let id = |json: &str| classify_body(json.as_bytes(), &tools_only(), &[]).1;
        assert_eq!(
            id(r#"{"id":7,"method":"tools/call","params":{"name":"a"}}"#).as_deref(),
            Some("7")
        );
        assert_eq!(
            id(r#"{"id":"a\"b","method":"tools/call","params":{"name":"a"}}"#).as_deref(),
            Some(r#""a\"b""#)
        );
        // A notification has none; null, objects and arrays aren't request ids.
        for json in [
            r#"{"method":"tools/call","params":{"name":"a"}}"#,
            r#"{"id":null,"method":"tools/call","params":{"name":"a"}}"#,
            r#"{"id":{"x":1},"method":"tools/call","params":{"name":"a"}}"#,
            r#"{"id":[1],"method":"tools/call","params":{"name":"a"}}"#,
            "[]",
            "{",
        ] {
            assert_eq!(id(json), None, "{json}");
        }
        let body = jsonrpc_error_response(r#""a\"b""#, 402, "Pay \"now\"", Some(r#"{"k":1}"#));
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], "a\"b");
        assert_eq!(v["error"]["code"], 402);
        assert_eq!(v["error"]["message"], "Pay \"now\"");
        assert_eq!(v["error"]["data"]["k"], 1);
    }

    #[test]
    fn body_free_tool_and_uncharged_methods_pass() {
        assert_eq!(
            body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ping"}}"#),
            McpDecision::Free
        );
        for json in [
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":[1,2]}"#,
            // `resources/read` isn't charged here, so its params aren't read.
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"file:///a","URI":"x"}}"#,
            // A response to the server (a sampling or elicitation result).
            r#"{"jsonrpc":"2.0","id":7,"result":{"content":[]}}"#,
        ] {
            assert_eq!(body(json), McpDecision::Free, "{json}");
        }
    }

    #[test]
    fn body_resources_read_is_priced_by_uri_when_charged() {
        let charged = vec!["tools/call".to_string(), "resources/read".to_string()];
        let tools = [tool("file:///free", McpToolPrice::Free)];
        let call = |json: &str| classify_body(json.as_bytes(), &charged, &tools).0;
        assert_eq!(
            call(r#"{"id":1,"method":"resources/read","params":{"uri":"file:///a"}}"#),
            McpDecision::Charged {
                method: "resources/read".into(),
                name: "file:///a".into()
            }
        );
        assert_eq!(
            call(r#"{"id":1,"method":"resources/read","params":{"uri":"file:///free"}}"#),
            McpDecision::Free
        );
        // `name` doesn't name a resource.
        assert_eq!(
            bad_code(&call(
                r#"{"id":1,"method":"resources/read","params":{"name":"file:///free"}}"#
            )),
            Some(JSONRPC_INVALID_PARAMS)
        );
    }

    #[test]
    fn body_parsers_could_read_differently_is_refused() {
        // Each of these names the free tool to one parser and a paid one, or a
        // charged method, to another.
        for json in [
            // Duplicate keys: first or last wins, by parser.
            r#"{"method":"tools/call","params":{"name":"ping","name":"summarize"}}"#,
            r#"{"method":"tools/list","method":"tools/call","params":{"name":"summarize"}}"#,
            r#"{"method":"tools/call","params":{"name":"ping"},"params":{"name":"summarize"}}"#,
            // Case-insensitive key matching.
            r#"{"method":"tools/list","Method":"tools/call","params":{"name":"summarize"}}"#,
            r#"{"method":"tools/call","params":{"name":"ping","Name":"summarize"}}"#,
            r#"{"method":"tools/call","params":{"name":"ping"},"PARAMS":{"name":"summarize"}}"#,
            // Unicode case folding: U+017F folds to `s`, U+212A to `k`.
            "{\"method\":\"tools/call\",\"params\":{\"name\":\"ping\"},\"param\u{17f}\":{\"name\":\"summarize\"}}",
            "{\"method\":\"tools/call\",\"params\":{\"name\":\"ping\",\"n\u{e4}me\":\"x\"}}",
        ] {
            assert_eq!(
                bad_code(&body(json)),
                Some(JSONRPC_INVALID_REQUEST),
                "{json}"
            );
        }
    }

    #[test]
    fn body_that_is_not_one_request_is_refused() {
        for (json, code) in [
            // A batch could hold a free call and a paid one.
            (
                r#"[{"method":"tools/call","params":{"name":"ping"}},{"method":"tools/call","params":{"name":"summarize"}}]"#,
                JSONRPC_INVALID_REQUEST,
            ),
            ("[]", JSONRPC_INVALID_REQUEST),
            ("\"tools/call\"", JSONRPC_INVALID_REQUEST),
            ("null", JSONRPC_INVALID_REQUEST),
            ("7", JSONRPC_INVALID_REQUEST),
            (r#"{"method":7}"#, JSONRPC_INVALID_REQUEST),
            ("", JSONRPC_PARSE_ERROR),
            ("{", JSONRPC_PARSE_ERROR),
            (
                r#"{"method":"tools/list"} {"method":"tools/call"}"#,
                JSONRPC_PARSE_ERROR,
            ),
            (r#"{"method":"tools/list"}x"#, JSONRPC_PARSE_ERROR),
            // A UTF-8 byte-order mark, which some parsers skip.
            ("\u{feff}{\"method\":\"tools/list\"}", JSONRPC_PARSE_ERROR),
        ] {
            assert_eq!(bad_code(&body(json)), Some(code), "{json}");
        }
        // UTF-16, which some parsers detect and decode.
        let utf16: Vec<u8> =
            r#"{"method":"tools/list"}"#.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(
            bad_code(&classify_body(&utf16, &tools_only(), &[]).0),
            Some(JSONRPC_PARSE_ERROR)
        );
    }

    #[test]
    fn body_charged_call_must_name_its_tool() {
        for json in [
            r#"{"method":"tools/call"}"#,
            r#"{"method":"tools/call","params":null}"#,
            r#"{"method":"tools/call","params":["ping"]}"#,
            r#"{"method":"tools/call","params":{}}"#,
            r#"{"method":"tools/call","params":{"name":7}}"#,
            r#"{"method":"tools/call","params":{"name":null}}"#,
            r#"{"method":"tools/call","params":{"name":["ping"]}}"#,
            r#"{"method":"tools/call","params":{"name":""}}"#,
            r#"{"method":"tools/call","params":{"name":"pi\r\nng"}}"#,
            // `uri` doesn't name a tool.
            r#"{"method":"tools/call","params":{"uri":"ping"}}"#,
        ] {
            assert_eq!(
                bad_code(&body(json)),
                Some(JSONRPC_INVALID_PARAMS),
                "{json}"
            );
        }
    }

    #[test]
    fn body_rejection_is_a_jsonrpc_error_that_quotes_nothing() {
        let d =
            body(r#"{"method":"tools/call","params":{"name":"ping","<script>":1,"<script>":2}}"#);
        let text = d.reject_body().unwrap();
        assert!(!text.contains("script"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["error"]["code"], JSONRPC_INVALID_REQUEST);
        assert!(v.get("id").is_none());
        let d = body("{\"method\": <script>");
        assert!(!d.reject_body().unwrap().contains("script"));
        assert!(!d.reject_reason().unwrap().contains("script"));
    }

    #[test]
    fn body_deep_nesting_cannot_exhaust_the_stack() {
        let nest = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        // In a value that isn't read, depth is skipped without recursion.
        let ignored = format!(
            r#"{{"method":"tools/call","params":{{"name":"ping","arguments":{}}}}}"#,
            nest(100_000)
        );
        assert_eq!(body(&ignored), McpDecision::Free);
        // In a value that is read, the parser's depth limit refuses it.
        let read = format!(
            r#"{{"method":"tools/call","params":{{"name":{}}}}}"#,
            nest(100_000)
        );
        assert_eq!(bad_code(&body(&read)), Some(JSONRPC_PARSE_ERROR));
    }

    #[test]
    fn discovery_passes_free() {
        let h = headers(&["2026-07-28"], &["tools/list"], &[]);
        assert_eq!(classify(&h, &tools_only(), &[], true), McpDecision::Free);
    }

    #[test]
    fn resources_read_free_unless_opted_in() {
        let h = headers(&["2026-07-28"], &["resources/read"], &["file:///a"]);
        assert_eq!(classify(&h, &tools_only(), &[], true), McpDecision::Free);
        let charged = vec!["tools/call".to_string(), "resources/read".to_string()];
        assert_eq!(
            classify(&h, &charged, &[], true),
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
            let d = classify(&h, &tools_only(), &[], true);
            assert!(d.reject_body().is_some(), "{h:?}: {d:?}");
        }
    }

    #[test]
    fn older_client_discovery_is_rejected_too() {
        // Without validated headers we cannot tell discovery from a tool call.
        let h = headers(&["2025-11-25"], &["tools/list"], &[]);
        assert_eq!(
            classify(&h, &tools_only(), &[], true),
            McpDecision::UnsupportedVersion {
                requested: Some("2025-11-25".into())
            }
        );
    }

    #[test]
    fn unsupported_version_body_lets_the_client_retry() {
        let older = classify(
            &headers(&["2025-11-25"], &["tools/call"], &["search"]),
            &tools_only(),
            &[],
            true,
        );
        let v: serde_json::Value = serde_json::from_str(&older.reject_body().unwrap()).unwrap();
        assert_eq!(v["error"]["code"], MCP_UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(v["error"]["data"]["supported"][0], MCP_MIN_HEADER_VERSION);
        assert_eq!(v["error"]["data"]["requested"], "2025-11-25");
        assert!(v.get("id").is_none());

        // Not a date: still -32022, but the raw value is not echoed.
        let junk = classify(
            &headers(&["<script>"], &["tools/call"], &["search"]),
            &tools_only(),
            &[],
            true,
        );
        let body = junk.reject_body().unwrap();
        assert!(!body.contains("<script>"), "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], MCP_UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(v["error"]["data"]["requested"], "");

        // A missing header is a header problem, not a version one.
        let missing = classify(
            &headers(&[], &["tools/call"], &["search"]),
            &tools_only(),
            &[],
            true,
        );
        let v: serde_json::Value = serde_json::from_str(&missing.reject_body().unwrap()).unwrap();
        assert_eq!(v["error"]["code"], MCP_HEADER_MISMATCH);
    }

    #[test]
    fn duplicates_are_rejected() {
        for h in [
            headers(&["2026-07-28"], &["tools/list", "tools/call"], &["search"]),
            headers(&["2026-07-28"], &["tools/call"], &["cheap", "pricey"]),
            headers(&["2026-07-28", "2026-07-28"], &["tools/call"], &["search"]),
        ] {
            assert!(
                matches!(
                    classify(&h, &tools_only(), &[], true),
                    McpDecision::Reject(_)
                ),
                "{h:?}"
            );
        }
    }

    #[test]
    fn named_method_without_name_rejected_even_when_free() {
        let h = headers(&["2026-07-28"], &["prompts/get"], &[]);
        assert!(matches!(
            classify(&h, &tools_only(), &[], true),
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
            classify(&h, &tools_only(), &[], true),
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
    fn error_body_has_no_id_and_escapes() {
        assert_eq!(
            jsonrpc_error_body(MCP_HEADER_MISMATCH, "bad \"x\"", None),
            r#"{"jsonrpc":"2.0","error":{"code":-32020,"message":"bad \"x\""}}"#
        );
        for body in [
            jsonrpc_error_body(MCP_PAYMENT_REQUIRED, "pay", Some(r#"{"a":1}"#)),
            jsonrpc_error_body(MCP_HEADER_MISMATCH, "bad", None),
        ] {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            // `v["id"].is_null()` would also pass for `"id": null`.
            assert!(v.get("id").is_none(), "{body}");
        }
        let v: serde_json::Value = serde_json::from_str(&jsonrpc_error_body(
            MCP_PAYMENT_REQUIRED,
            "pay",
            Some(r#"{"a":1}"#),
        ))
        .unwrap();
        assert_eq!(v["error"]["data"]["a"], 1);
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
                matches!(
                    classify(&h, &tools_only(), &[], false),
                    McpDecision::Reject(_)
                ),
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
            assert_eq!(
                classify(&h, &tools_only(), &[], true),
                McpDecision::UnsupportedVersion { requested: None },
                "{v}"
            );
        }
        for v in ["2026-07-28", "2028-02-29", "2026-12-31"] {
            let h = headers(&[v], &["tools/call"], &["search"]);
            assert!(
                matches!(
                    classify(&h, &tools_only(), &[], true),
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
            classify(&h, &charged, &[], true),
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
    fn invalid_version_is_not_echoed() {
        for v in ["latest", "9999-99-99", "2026-07-28\u{7f}x", "<script>"] {
            let h = headers(&[v], &["tools/call"], &["search"]);
            let d = classify(&h, &tools_only(), &[], true);
            let reason = d.reject_reason().unwrap();
            assert!(!reason.contains(v), "{reason}");
            assert!(!reason.contains("predates"), "{reason}");
            assert!(!d.reject_body().unwrap().contains(v), "{v}");
        }
        // A real but older date is safe to name, and is reported as older.
        let d = classify(
            &headers(&["2025-11-25"], &["tools/call"], &["search"]),
            &tools_only(),
            &[],
            true,
        );
        assert!(d.reject_reason().unwrap().contains("2025-11-25 predates"));
    }

    #[test]
    fn header_values_are_trimmed_of_sp_and_ht() {
        let h = headers(&[" \t2026-07-28\t"], &["\ttools/call "], &[" search\t"]);
        assert_eq!(
            classify(&h, &tools_only(), &[], true),
            McpDecision::Charged {
                method: "tools/call".into(),
                name: "search".into()
            }
        );
    }

    #[test]
    fn malformed_name_rejected_on_free_method() {
        // Decoded before the charged check, so a bad name cannot ride a free
        // method through to the upstream.
        for name in ["=?base64?not base64!?=", "=?base64?YQ0KYg==?=", "caf\u{e9}"] {
            let h = headers(&["2026-07-28"], &["tools/list"], &[name]);
            assert!(
                matches!(
                    classify(&h, &tools_only(), &[], true),
                    McpDecision::Reject(_)
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn method_is_case_sensitive() {
        // Header values are case-sensitive, so `Tools/Call` is not `tools/call`.
        let h = headers(&["2026-07-28"], &["Tools/Call"], &["search"]);
        assert_eq!(classify(&h, &tools_only(), &[], true), McpDecision::Free);
    }

    #[test]
    fn payment_code_is_outside_the_reserved_range() {
        assert!(!(-32768..=-32000).contains(&MCP_PAYMENT_REQUIRED));
    }
}
