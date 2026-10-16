//! Named tenants. `l402_tenant <id> lnurl=<address>;` defines one at `http`
//! level, and `l402_tenant_ref <id>;` points a location at it, so an address is
//! written once instead of once per route.

use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    pub id: String,
    pub lnurl_addr: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TenantError {
    InvalidId(String),
    NotKeyValue(String),
    UnknownKey(String),
    RepeatedKey(String),
    EmptyValue(String),
    MissingLnurl,
    Duplicate(String),
    Unknown { id: String, defined: Vec<String> },
}

// Each message reads after nginx's `"l402_tenant" directive` prefix.
impl fmt::Display for TenantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => write!(
                f,
                "invalid tenant name \"{id}\": use letters, digits, '-' and '_'"
            ),
            Self::NotKeyValue(arg) => write!(f, "expected key=value, got \"{arg}\""),
            Self::UnknownKey(key) => write!(f, "unknown key \"{key}\"; expected lnurl"),
            Self::RepeatedKey(key) => write!(f, "\"{key}\" given more than once"),
            Self::EmptyValue(key) => write!(f, "\"{key}\" has an empty value"),
            Self::MissingLnurl => write!(f, "requires lnurl=<address>"),
            Self::Duplicate(id) => write!(f, "tenant \"{id}\" is already defined"),
            Self::Unknown { id, defined } if defined.is_empty() => {
                write!(f, "tenant \"{id}\" is not defined; no l402_tenant exists")
            }
            Self::Unknown { id, defined } => write!(
                f,
                "tenant \"{id}\" is not defined; defined tenants: {}",
                defined.join(", ")
            ),
        }
    }
}

/// Parse the arguments of `l402_tenant`: a name, then `key=value` pairs.
///
/// The name is restricted to a conservative charset because it is meant to
/// become a Redis key component and a macaroon caveat value later. Unknown keys
/// are rejected rather than ignored, so a misspelt `lnrul=` fails at load
/// instead of leaving the tenant without the setting it meant to have.
pub fn parse_tenant(args: &[&str]) -> Result<Tenant, TenantError> {
    let (id, pairs) = args.split_first().ok_or(TenantError::MissingLnurl)?;
    let valid_id = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid_id {
        return Err(TenantError::InvalidId(id.to_string()));
    }

    let mut lnurl_addr = None;
    for arg in pairs {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| TenantError::NotKeyValue(arg.to_string()))?;
        let slot = match key {
            "lnurl" => &mut lnurl_addr,
            _ => return Err(TenantError::UnknownKey(key.to_string())),
        };
        if slot.is_some() {
            return Err(TenantError::RepeatedKey(key.to_string()));
        }
        if value.trim().is_empty() {
            return Err(TenantError::EmptyValue(key.to_string()));
        }
        *slot = Some(value.trim().to_string());
    }

    Ok(Tenant {
        id: id.to_string(),
        lnurl_addr: lnurl_addr.ok_or(TenantError::MissingLnurl)?,
    })
}

/// Every tenant defined by the configuration being parsed.
#[derive(Debug, Default)]
pub struct TenantRegistry {
    tenants: HashMap<String, Tenant>,
}

impl TenantRegistry {
    /// Add a tenant. Redefining a name is an error: with two definitions, which
    /// one pays out would depend on the order of lines in `nginx.conf`.
    pub fn define(&mut self, tenant: Tenant) -> Result<(), TenantError> {
        if self.tenants.contains_key(&tenant.id) {
            return Err(TenantError::Duplicate(tenant.id));
        }
        self.tenants.insert(tenant.id.clone(), tenant);
        Ok(())
    }

    pub fn resolve(&self, id: &str) -> Result<&Tenant, TenantError> {
        self.tenants.get(id).ok_or_else(|| {
            let mut defined: Vec<String> = self.tenants.keys().cloned().collect();
            defined.sort();
            TenantError::Unknown {
                id: id.to_string(),
                defined,
            }
        })
    }

    pub fn clear(&mut self) {
        self.tenants.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> Tenant {
        Tenant {
            id: "alice".into(),
            lnurl_addr: "alice@getalby.com".into(),
        }
    }

    #[test]
    fn parses_name_and_lnurl() {
        assert_eq!(
            parse_tenant(&["alice", "lnurl=alice@getalby.com"]),
            Ok(alice())
        );
    }

    #[test]
    fn value_may_contain_equals_signs() {
        let t = parse_tenant(&["t", "lnurl=https://x.example/lnurlp?u=a"]).unwrap();
        assert_eq!(t.lnurl_addr, "https://x.example/lnurlp?u=a");
    }

    #[test]
    fn requires_an_lnurl() {
        assert_eq!(parse_tenant(&["alice"]), Err(TenantError::MissingLnurl));
        assert_eq!(parse_tenant(&[]), Err(TenantError::MissingLnurl));
    }

    #[test]
    fn rejects_names_outside_the_charset() {
        for id in ["", "al ice", "alice:1", "ålice", "a/b"] {
            assert_eq!(
                parse_tenant(&[id, "lnurl=a@b.c"]),
                Err(TenantError::InvalidId(id.to_string())),
                "{id:?}"
            );
        }
        assert!(parse_tenant(&["Tenant_2-b", "lnurl=a@b.c"]).is_ok());
    }

    #[test]
    fn rejects_a_misspelt_key_instead_of_ignoring_it() {
        assert_eq!(
            parse_tenant(&["alice", "lnrul=alice@getalby.com"]),
            Err(TenantError::UnknownKey("lnrul".into()))
        );
    }

    #[test]
    fn rejects_bare_words_repeats_and_empty_values() {
        assert_eq!(
            parse_tenant(&["alice", "alice@getalby.com"]),
            Err(TenantError::NotKeyValue("alice@getalby.com".into()))
        );
        assert_eq!(
            parse_tenant(&["alice", "lnurl=a@b.c", "lnurl=d@e.f"]),
            Err(TenantError::RepeatedKey("lnurl".into()))
        );
        assert_eq!(
            parse_tenant(&["alice", "lnurl= "]),
            Err(TenantError::EmptyValue("lnurl".into()))
        );
    }

    #[test]
    fn resolves_a_defined_tenant() {
        let mut reg = TenantRegistry::default();
        reg.define(alice()).unwrap();
        assert_eq!(reg.resolve("alice"), Ok(&alice()));
    }

    #[test]
    fn refuses_a_second_definition_of_the_same_name() {
        let mut reg = TenantRegistry::default();
        reg.define(alice()).unwrap();
        let again = Tenant {
            lnurl_addr: "mallory@example.org".into(),
            ..alice()
        };
        assert_eq!(
            reg.define(again),
            Err(TenantError::Duplicate("alice".into()))
        );
        assert_eq!(
            reg.resolve("alice").unwrap().lnurl_addr,
            "alice@getalby.com"
        );
    }

    #[test]
    fn two_tenants_may_share_an_address() {
        let mut reg = TenantRegistry::default();
        reg.define(alice()).unwrap();
        let shop = Tenant {
            id: "alice-shop".into(),
            ..alice()
        };
        assert!(reg.define(shop).is_ok());
    }

    #[test]
    fn unknown_name_lists_what_is_defined() {
        let mut reg = TenantRegistry::default();
        reg.define(Tenant {
            id: "bob".into(),
            lnurl_addr: "bob@example.org".into(),
        })
        .unwrap();
        reg.define(alice()).unwrap();
        let err = reg.resolve("alcie").unwrap_err();
        assert_eq!(
            err.to_string(),
            "tenant \"alcie\" is not defined; defined tenants: alice, bob"
        );
    }

    #[test]
    fn unknown_name_with_an_empty_registry_says_so() {
        let err = TenantRegistry::default().resolve("alice").unwrap_err();
        assert_eq!(
            err.to_string(),
            "tenant \"alice\" is not defined; no l402_tenant exists"
        );
    }

    #[test]
    fn clear_forgets_every_tenant() {
        let mut reg = TenantRegistry::default();
        reg.define(alice()).unwrap();
        reg.clear();
        assert!(reg.resolve("alice").is_err());
        assert!(reg.define(alice()).is_ok());
    }
}
