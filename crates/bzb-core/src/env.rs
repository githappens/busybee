use std::collections::BTreeMap;

/// Return `base` augmented with busybee's color-forcing env vars and with
/// `NO_COLOR` removed. `base` is consumed; the returned map is suitable for
/// passing to `pueue_lib`'s `AddMessage.envs`.
pub fn color_envs(mut base: BTreeMap<String, String>) -> BTreeMap<String, String> {
    base.remove("NO_COLOR");
    base.insert("CLICOLOR_FORCE".into(), "1".into());
    base.insert("FORCE_COLOR".into(), "1".into());
    base.insert("CARGO_TERM_COLOR".into(), "always".into());
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    #[test]
    fn injects_color_force_vars() {
        let out = color_envs(BTreeMap::new());
        assert_eq!(out.get("CLICOLOR_FORCE").map(String::as_str), Some("1"));
        assert_eq!(out.get("FORCE_COLOR").map(String::as_str), Some("1"));
        assert_eq!(out.get("CARGO_TERM_COLOR").map(String::as_str), Some("always"));
    }

    #[test]
    fn removes_no_color_if_present() {
        let out = color_envs(m(&[("NO_COLOR", "1")]));
        assert!(!out.contains_key("NO_COLOR"));
    }

    #[test]
    fn preserves_other_envs() {
        let out = color_envs(m(&[("PATH", "/usr/bin"), ("HOME", "/Users/x")]));
        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(out.get("HOME").map(String::as_str), Some("/Users/x"));
    }

    #[test]
    fn overrides_existing_color_vars() {
        let out = color_envs(m(&[("FORCE_COLOR", "0")]));
        assert_eq!(out.get("FORCE_COLOR").map(String::as_str), Some("1"));
    }
}
