//! Parses `git describe --long`; shared with `build.rs` via `#[path]`.

/// `[v]MAJOR.MINOR.PATCH-N-gSHA` → `MAJOR.MINOR.<PATCH+N>`.
pub fn parse_describe(s: &str) -> Option<String> {
    let mut parts = s.rsplitn(3, '-');
    let _sha = parts.next()?;
    let count: u64 = parts.next()?.parse().ok()?;
    let tag = parts.next()?.trim_start_matches('v');
    let mut t = tag.splitn(3, '.');
    let major: u64 = t.next()?.parse().ok()?;
    let minor: u64 = t.next()?.parse().ok()?;
    let patch: u64 = t.next()?.parse().ok()?;
    Some(format!("{major}.{minor}.{}", patch + count))
}

#[cfg(test)]
mod tests {
    use super::parse_describe;

    #[test]
    fn at_tag_has_zero_distance() {
        assert_eq!(parse_describe("0.1.0-0-gabcdef1").as_deref(), Some("0.1.0"));
    }

    #[test]
    fn adds_commit_distance_to_patch() {
        assert_eq!(parse_describe("0.1.0-5-gabcdef1").as_deref(), Some("0.1.5"));
    }

    #[test]
    fn accepts_v_prefix() {
        assert_eq!(
            parse_describe("v0.2.3-4-gabcdef1").as_deref(),
            Some("0.2.7")
        );
    }

    #[test]
    fn preserves_nonzero_patch_from_tag() {
        assert_eq!(
            parse_describe("1.4.7-3-gabcdef1").as_deref(),
            Some("1.4.10")
        );
    }

    #[test]
    fn rejects_non_numeric_tag() {
        assert!(parse_describe("release-5-gabcdef1").is_none());
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse_describe("").is_none());
        assert!(parse_describe("just-a-string").is_none());
    }
}
