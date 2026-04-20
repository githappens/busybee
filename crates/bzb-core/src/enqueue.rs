use std::{collections::{BTreeMap, HashMap}, path::PathBuf};

use pueue_lib::Client;
use pueue_lib::message::{AddRequest, Request, Response};

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
}

impl TaskSpec {
    pub fn from_current_env(command: String, label: Option<String>) -> std::io::Result<Self> {
        Ok(Self {
            command,
            cwd: std::env::current_dir()?,
            env: std::env::vars().collect(),
            label,
        })
    }
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

    #[test]
    fn build_add_request_sets_group_and_envs() {
        let spec = TaskSpec {
            command: "echo hi".into(),
            cwd: PathBuf::from("/tmp"),
            env: BTreeMap::from([("NO_COLOR".into(), "1".into())]),
            label: Some("test".into()),
        };
        let msg = build_add_request(spec);
        assert_eq!(msg.group, BUSYBEE_GROUP);
        assert_eq!(msg.command, "echo hi");
        assert_eq!(msg.label.as_deref(), Some("test"));
        assert!(!msg.envs.contains_key("NO_COLOR"));
        assert_eq!(msg.envs.get("FORCE_COLOR").map(String::as_str), Some("1"));
    }
}
