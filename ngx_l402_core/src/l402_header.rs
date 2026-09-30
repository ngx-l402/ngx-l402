//! Parsing of the L402 `WWW-Authenticate` header value, and of the scheme a
//! request's `Authorization` header carries.

/// Whether an `Authorization` value carries an L402 credential. A request
/// authenticating for something else, such as a Blossom client's BUD-02
/// `Nostr <event>` upload auth, carries no L402 credential rather than a broken
/// one, and so belongs on the challenge path, not the rejection path. The
/// scheme is case-insensitive (RFC 9110).
pub fn is_l402_scheme(auth: &str) -> bool {
    auth.trim_start()
        .get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("L402 "))
}

/// Extract the raw macaroon (base64) and invoice (bolt11) strings from a
/// `WWW-Authenticate` header value of the form:
///   `L402 macaroon="<b64>", invoice="<bolt11>"`
pub fn parse_l402_header_value(header: &str) -> Option<(String, String)> {
    let mac = header
        .split("macaroon=\"")
        .nth(1)?
        .split('"')
        .next()?
        .to_string();
    let inv = header
        .split("invoice=\"")
        .nth(1)?
        .split('"')
        .next()?
        .to_string();
    Some((mac, inv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_header() {
        let (mac, inv) =
            parse_l402_header_value(r#"L402 macaroon="AbCd123==", invoice="lnbc100n1pabc""#)
                .expect("well-formed header");
        assert_eq!(mac, "AbCd123==");
        assert_eq!(inv, "lnbc100n1pabc");
    }

    #[test]
    fn missing_macaroon_returns_none() {
        assert!(parse_l402_header_value(r#"L402 invoice="lnbc100n1pabc""#).is_none());
    }

    #[test]
    fn missing_invoice_returns_none() {
        assert!(parse_l402_header_value(r#"L402 macaroon="abc""#).is_none());
    }

    #[test]
    fn empty_header_returns_none() {
        assert!(parse_l402_header_value("").is_none());
    }

    #[test]
    fn an_l402_credential_is_recognized_whatever_its_case() {
        for auth in ["L402 abc", "l402 abc", "L402 mac:preimage", " L402 abc"] {
            assert!(is_l402_scheme(auth), "{auth}");
        }
    }

    /// A client authenticating for something else is not presenting a failed
    /// L402 credential, so it must reach the challenge rather than a 401.
    #[test]
    fn another_scheme_is_not_an_l402_credential() {
        for auth in [
            "Nostr eyJpZCI6IjEyMyJ9",
            "Bearer abc",
            "Basic dXNlcjpwYXNz",
            "Cashu cashuBo2Ft",
            "L402",
            "",
            "é402 abc",
        ] {
            assert!(!is_l402_scheme(auth), "{auth}");
        }
    }

    /// The first quoted value after each key is taken; extra fields don't break it.
    #[test]
    fn ignores_trailing_fields() {
        let (mac, inv) =
            parse_l402_header_value(r#"L402 macaroon="m1", invoice="i1", extra="x""#).unwrap();
        assert_eq!(mac, "m1");
        assert_eq!(inv, "i1");
    }
}
