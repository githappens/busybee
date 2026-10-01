//! Wire protocol between busybee clients and `bzbd`: newline-delimited JSON
//! over a unix socket, opened by a [`Hello`] handshake (spec §Protocol).

use std::{collections::BTreeMap, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use crate::classify::Class;

/// Bumped on every incompatible change to the types below; the handshake
/// matches it exactly. 2: `LeaseView::tool`, 3: `detached`, 4: `ConfigReload`.
pub const PROTOCOL_VERSION: u32 = 4;

/// The longest line either end will read, so a newline-free stream cannot
/// exhaust the daemon's (or a client's) memory.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// What one bounded read off a connection produced.
#[derive(Debug)]
pub enum Line {
    Text(String),
    /// The peer closed the connection between messages.
    Closed,
    /// Over [`MAX_LINE_BYTES`], closed mid-message, or not UTF-8. The
    /// connection cannot be read past it.
    Malformed(String),
}

/// Reads one line, refusing to buffer more than [`MAX_LINE_BYTES`] of it.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Line> {
    let mut line = Vec::new();
    reader
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if line.last() == Some(&b'\n') {
        line.pop();
    } else if line.len() > MAX_LINE_BYTES {
        return Ok(Line::Malformed(format!(
            "a line longer than {MAX_LINE_BYTES} bytes is not a message"
        )));
    } else if line.is_empty() {
        return Ok(Line::Closed);
    } else {
        // Complete-looking JSON without its newline is a truncated write.
        return Ok(Line::Malformed(format!(
            "the connection closed {} bytes into a message with no newline",
            line.len()
        )));
    }
    match String::from_utf8(line) {
        Ok(text) => Ok(Line::Text(text)),
        Err(e) => Ok(Line::Malformed(format!(
            "a line that is not valid utf-8 is not a message: {e}"
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub hello: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Status,
    Submit(LeaseRequest),
    Cancel {
        lease: u64,
    },
    /// Re-read the config file. The daemon answers
    /// [`Response::ConfigReloaded`] with what it now runs on, or
    /// [`Response::Error`] with the reason it kept what it had.
    ConfigReload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub label: Option<String>,
    pub class_override: Option<Class>,
    pub cores_wanted: Option<u32>,
    /// `--detach`: the lease outlives the connection that asked for it, so
    /// hanging up does not cancel it. Only [`Request::Cancel`] does.
    pub detached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Pong {
        version: String,
        pid: u32,
    },
    Status(StatusReply),
    /// The request was carried out. A cancel that changed nothing is an
    /// [`Response::Error`] instead: the caller named a lease that is not there.
    Ack,
    /// The configuration the daemon runs on after a [`Request::ConfigReload`].
    ConfigReloaded {
        pool_size: u32,
        max_concurrent: u32,
        drain_deadline_ms: u64,
    },
    /// Streamed on a `Submit` connection for the lifetime of the lease.
    Event(LeaseEvent),
    Error {
        message: String,
    },
}

/// Far below [`MAX_LINE_BYTES`]: a decoder error quotes the offending line,
/// and JSON-encoding escapes it again.
const MAX_ERROR_MESSAGE_BYTES: usize = 1024;

const ELLIPSIS: char = '…';

impl Response {
    /// An [`Response::Error`] whose encoded line fits [`MAX_LINE_BYTES`]
    /// whatever the message quotes.
    pub fn error(message: impl std::fmt::Display) -> Self {
        let mut message = message.to_string();
        if message.len() > MAX_ERROR_MESSAGE_BYTES {
            let mut end = MAX_ERROR_MESSAGE_BYTES - ELLIPSIS.len_utf8();
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push(ELLIPSIS);
        }
        Response::Error { message }
    }
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
    /// The tool that decided the class; `label` is the caller's `--name`.
    pub tool: String,
    pub class: String,
    pub cores: u32,
    /// `queued`, `running`, or `orphaned` (adopted from a dead daemon, no
    /// client; spec §Failure and recovery).
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

    #[tokio::test]
    async fn a_message_that_ends_without_a_newline_is_a_framing_error() {
        let mut reader = tokio::io::BufReader::new(&br#"{"hello":1}"#[..]);

        let Line::Malformed(reason) = read_line(&mut reader).await.unwrap() else {
            panic!("expected an unterminated message to be refused");
        };
        assert!(reason.contains("newline"), "reason was {reason:?}");
    }

    #[test]
    fn a_request_round_trips_as_one_json_line() {
        let line = serde_json::to_string(&Request::Cancel { lease: 7 }).unwrap();
        assert!(!line.contains('\n'), "line was {line:?}");
        assert!(matches!(
            serde_json::from_str::<Request>(&line).unwrap(),
            Request::Cancel { lease: 7 }
        ));
    }

    #[test]
    fn a_class_override_carries_only_a_known_class() {
        let request = LeaseRequest {
            argv: vec!["make".into()],
            cwd: PathBuf::from("/somewhere"),
            env: BTreeMap::new(),
            label: None,
            class_override: Some(Class::Static),
            cores_wanted: Some(4),
            detached: false,
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
    fn a_truncated_error_message_stays_within_the_cap() {
        let Response::Error { message } = Response::error("x".repeat(5000)) else {
            panic!("expected an error response");
        };
        assert!(
            message.len() <= MAX_ERROR_MESSAGE_BYTES,
            "message was {} bytes",
            message.len()
        );
        assert!(message.ends_with('…'), "message was {message:?}");
    }

    /// `tool` is required rather than optional, so the version bump (not a
    /// blank column) is what a v1 daemon runs into.
    #[test]
    fn a_lease_view_from_protocol_version_1_does_not_decode() {
        let version_1 = r#"{"id":41,"label":"ui build","class":"static","cores":9,
                            "state":"running","elapsed_ms":132000,"ahead":null,
                            "pueue_task_id":3}"#;

        let error = serde_json::from_str::<LeaseView>(version_1)
            .expect_err("a reply without a tool must not decode")
            .to_string();

        assert!(error.contains("tool"), "error was {error:?}");
        assert_ne!(
            PROTOCOL_VERSION, 1,
            "a required field the previous version never sent needs a version bump"
        );
    }

    #[test]
    fn a_config_reload_does_not_decode_against_protocol_version_3() {
        /// [`Request`] as version 3 spelled it.
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        enum Version3 {
            Ping,
            Status,
            Submit(LeaseRequest),
            Cancel { lease: u64 },
        }

        let line = serde_json::to_string(&Request::ConfigReload).expect("encode");

        let error = serde_json::from_str::<Version3>(&line)
            .expect_err("a version-3 daemon must not decode a config reload")
            .to_string();

        assert!(error.contains("ConfigReload"), "error was {error:?}");
        assert_ne!(
            PROTOCOL_VERSION, 3,
            "a request the previous version cannot decode needs a version bump"
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
