use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
};

use pueue_lib::message::{AddRequest, Request, Response};
use pueue_lib::Client;

use crate::env::color_envs;
use crate::errors::BusybeeError;
use crate::group::BUSYBEE_GROUP;

/// Describes a task we want pueued to queue up.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub command: String,
    pub cwd: PathBuf,
    /// Environment variables in `key → value` form. Use a BTreeMap for
    /// deterministic iteration order in tests and debug output; it's
    /// converted to a HashMap when writing the pueue `AddRequest`.
    pub env: BTreeMap<String, String>,
    pub label: Option<String>,
    /// Bypass pueue's dispatcher and start the task on arrival. bzbd sets this:
    /// it has already decided the task may run, and the `busybee` group is held
    /// at `parallel_tasks = 0` so nothing else would ever start it.
    pub start_immediately: bool,
}

impl TaskSpec {
    pub fn from_current_env(command: String, label: Option<String>) -> std::io::Result<Self> {
        Ok(Self {
            command,
            cwd: std::env::current_dir()?,
            env: std::env::vars().collect(),
            label,
            start_immediately: false,
        })
    }
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

/// Build the pueue `AddRequest` we will send. Color env injection happens
/// here (§4.1 step 4 of the design).
pub fn build_add_request(spec: TaskSpec) -> AddRequest {
    let envs_btree = color_envs(spec.env);
    let envs: HashMap<String, String> = envs_btree.into_iter().collect();
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
    let add = build_add_request(spec);
    client.send_request(Request::Add(add)).await.map_err(io)?;
    match client.receive_response().await.map_err(io)? {
        Response::AddedTask(r) => Ok(r.task_id),
        Response::Failure(msg) => Err(BusybeeError::EnqueueRejected(msg)),
        other => Err(BusybeeError::UnexpectedResponse(format!("{other:?}"))),
    }
}

fn io(e: impl std::fmt::Display) -> BusybeeError {
    BusybeeError::Other(format!("pueue-lib io: {e}"))
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

    /// bzbd's admitted tasks must not wait for pueue's dispatcher, which the
    /// `busybee` group's `parallel_tasks = 0` has switched off anyway.
    #[test]
    fn build_add_request_carries_start_immediately() {
        let mut spec = spec("echo hi");
        spec.start_immediately = true;
        assert!(build_add_request(spec).start_immediately);
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
