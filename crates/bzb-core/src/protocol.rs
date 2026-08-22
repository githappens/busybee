//! Wire protocol between busybee clients and `bzbd`.
//!
//! Newline-delimited JSON over a unix socket: one UTF-8 message per line, no
//! embedded newlines. The client opens with `{"hello": <protocol_version>}`
//! and the daemon answers [`Response::Pong`], or [`Response::Error`] when it
//! does not speak that version. Every line after the handshake is a
//! [`Request`] from the client and a [`Response`] from the daemon.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::classify::Class;

/// Bumped whenever a change to the types below is not backwards compatible.
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest line the daemon will read. Anyone who can open the socket could
/// otherwise stream a newline-free message until the long-lived daemon runs out
/// of memory; real messages are orders of magnitude smaller than this.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// The client's first line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub hello: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Status,
    Submit(LeaseRequest),
    Cancel { lease: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub label: Option<String>,
    pub class_override: Option<Class>,
    pub cores_wanted: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Pong {
        version: String,
        pid: u32,
    },
    Status(StatusReply),
    /// Streamed on a `Submit` connection for the lifetime of the lease.
    Event(LeaseEvent),
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReply {
    pub pool_size: u32,
    pub free: u32,
    pub held: u32,
    pub leases: Vec<LeaseView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseView {
    pub id: u64,
    pub label: String,
    pub class: String,
    pub cores: u32,
    pub state: String,
    pub elapsed_ms: u64,
    pub ahead: Option<usize>,
    pub pueue_task_id: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LeaseEvent {
    Queued {
        id: u64,
        ahead: usize,
    },
    Notice {
        text: String,
    },
    Admitted {
        id: u64,
        pueue_task_id: usize,
        class: String,
        cores: u32,
        pool_size: u32,
        peers: usize,
    },
    Finished {
        id: u64,
        exit_code: i32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_as_one_json_line() {
        let line = serde_json::to_string(&Request::Cancel { lease: 7 }).unwrap();
        assert!(!line.contains('\n'), "line was {line:?}");
        assert!(matches!(
            serde_json::from_str::<Request>(&line).unwrap(),
            Request::Cancel { lease: 7 }
        ));
    }

    /// The class vocabulary is closed (`docs/design/bzbd.md` §Lease model
    /// types the field as `Option<Class>`), so a typo is a wire error the
    /// client hears about now rather than a string the scheduler has to
    /// re-validate at admission.
    #[test]
    fn a_class_override_carries_only_a_known_class() {
        let request = LeaseRequest {
            argv: vec!["make".into()],
            cwd: PathBuf::from("/somewhere"),
            env: BTreeMap::new(),
            label: None,
            class_override: Some(Class::Static),
            cores_wanted: Some(4),
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(
            line.contains(r#""class_override":"static""#),
            "line was {line:?}"
        );

        assert!(
            serde_json::from_str::<Class>(r#""statik""#).is_err(),
            "an unknown class must not decode"
        );
    }

    #[test]
    fn an_event_round_trips_inside_a_response() {
        let line = serde_json::to_string(&Response::Event(LeaseEvent::Queued { id: 1, ahead: 2 }))
            .unwrap();
        match serde_json::from_str::<Response>(&line).unwrap() {
            Response::Event(LeaseEvent::Queued { id, ahead }) => {
                assert_eq!((id, ahead), (1, 2));
            }
            other => panic!("expected a queued event, got {other:?}"),
        }
    }
}
