use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use pueue_lib::message::{AddRequest, Request, Response};
use pueue_lib::Client;

use crate::client::request;
use crate::errors::BusybeeError;
use crate::group::BUSYBEE_GROUP;

#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub command: String,
    pub cwd: PathBuf,
    /// BTreeMap for deterministic order in tests and debug output.
    pub env: BTreeMap<String, String>,
    pub label: Option<String>,
    /// Bypass pueue's dispatcher: bzbd has already admitted the task, and the
    /// `busybee` group's `parallel_tasks = 0` would never start it.
    pub start_immediately: bool,
}

/// Join argv-style command parts into a single shell-safe string for pueue's
/// `sh -c` runner.
pub fn shell_escape_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Best-effort POSIX shell quoting for a single argv element.
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        return s.into();
    }
    let escaped = s.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

/// `base` with colour forced on and `NO_COLOR` removed.
fn color_envs(mut base: BTreeMap<String, String>) -> BTreeMap<String, String> {
    base.remove("NO_COLOR");
    base.insert("CLICOLOR_FORCE".into(), "1".into());
    base.insert("FORCE_COLOR".into(), "1".into());
    base.insert("CARGO_TERM_COLOR".into(), "always".into());
    base
}

fn build_add_request(spec: TaskSpec) -> AddRequest {
    let envs: HashMap<String, String> = color_envs(spec.env).into_iter().collect();
    AddRequest {
        command: spec.command,
        path: spec.cwd,
        envs,
        group: BUSYBEE_GROUP.into(),
        label: spec.label,
        start_immediately: spec.start_immediately,
        ..Default::default()
    }
}

/// Send an `AddRequest` and return the assigned task id.
pub async fn enqueue(client: &mut Client, spec: TaskSpec) -> Result<usize, BusybeeError> {
    match request(client, Request::Add(build_add_request(spec))).await? {
        Response::AddedTask(r) => Ok(r.task_id),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str) -> TaskSpec {
        TaskSpec {
            command: command.into(),
            cwd: PathBuf::from("/tmp"),
            env: BTreeMap::from([("NO_COLOR".into(), "1".into())]),
            label: Some("test".into()),
            start_immediately: false,
        }
    }

    fn m(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn build_add_request_sets_group_and_envs() {
        let msg = build_add_request(spec("echo hi"));
        assert_eq!(msg.group, BUSYBEE_GROUP);
        assert_eq!(msg.command, "echo hi");
        assert_eq!(msg.label.as_deref(), Some("test"));
        assert!(!msg.envs.contains_key("NO_COLOR"));
        assert_eq!(msg.envs.get("FORCE_COLOR").map(String::as_str), Some("1"));
        assert!(!msg.start_immediately);
    }

    #[test]
    fn build_add_request_carries_start_immediately() {
        let mut spec = spec("echo hi");
        spec.start_immediately = true;
        assert!(build_add_request(spec).start_immediately);
    }

    #[test]
    fn color_envs_injects_color_force_vars() {
        let out = color_envs(BTreeMap::new());
        assert_eq!(out.get("CLICOLOR_FORCE").map(String::as_str), Some("1"));
        assert_eq!(out.get("FORCE_COLOR").map(String::as_str), Some("1"));
        assert_eq!(
            out.get("CARGO_TERM_COLOR").map(String::as_str),
            Some("always")
        );
    }

    #[test]
    fn color_envs_removes_no_color_if_present() {
        let out = color_envs(m(&[("NO_COLOR", "1")]));
        assert!(!out.contains_key("NO_COLOR"));
    }

    #[test]
    fn color_envs_preserves_other_envs() {
        let out = color_envs(m(&[("PATH", "/usr/bin"), ("HOME", "/Users/x")]));
        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(out.get("HOME").map(String::as_str), Some("/Users/x"));
    }

    #[test]
    fn color_envs_overrides_existing_color_vars() {
        let out = color_envs(m(&[("FORCE_COLOR", "0")]));
        assert_eq!(out.get("FORCE_COLOR").map(String::as_str), Some("1"));
    }

    #[test]
    fn join_simple_words_is_passthrough() {
        assert_eq!(shell_escape_join(&["echo".into(), "hi".into()]), "echo hi");
    }

    #[test]
    fn join_quotes_args_with_spaces() {
        assert_eq!(
            shell_escape_join(&["echo".into(), "hello world".into()]),
            "echo 'hello world'"
        );
    }

    #[test]
    fn join_escapes_single_quotes() {
        assert_eq!(
            shell_escape_join(&["echo".into(), "it's".into()]),
            r#"echo 'it'\''s'"#
        );
    }
}
