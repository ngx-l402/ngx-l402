//! nginx's 2026-11-18 security fixes landed in 1.30.7 (stable) and 1.31.9
//! (mainline); 1.28 and 1.29 are end of life and never got them.

/// The boot warning for an nginx `nginx_version` number (`1_030_007` is
/// 1.30.7) that predates those fixes, or `None` once it has them.
pub fn nginx_security_warning(version: u32) -> Option<String> {
    if version >= 1_031_009 || (1_030_007..1_031_000).contains(&version) {
        return None;
    }
    Some(format!(
        "nginx {}.{}.{} predates the 2026-11-18 security fixes, including HTTP/2 \
         memory corruption (CVE-2026-123007, CVE-2026-123633); upgrade to 1.30.7+ \
         or 1.31.9+ unless your build backports them",
        version / 1_000_000,
        version / 1000 % 1000,
        version % 1000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_only_before_each_branchs_fix() {
        for v in [1_028_000, 1_029_008, 1_030_006, 1_031_000, 1_031_008] {
            assert!(nginx_security_warning(v).is_some(), "{v}");
        }
        for v in [1_030_007, 1_030_008, 1_031_009, 1_032_000] {
            assert!(nginx_security_warning(v).is_none(), "{v}");
        }
    }

    #[test]
    fn names_the_running_version() {
        let w = nginx_security_warning(1_028_000).unwrap();
        assert!(w.starts_with("nginx 1.28.0 predates"), "{w}");
    }
}
