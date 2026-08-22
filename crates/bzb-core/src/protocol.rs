//! Wire protocol between busybee clients and `bzbd`.
//!
//! Newline-delimited JSON over a unix socket: one UTF-8 message per line, no
//! embedded newlines. The client opens with `{"hello": <protocol_version>}`
//! and the daemon answers [`Response::Pong`], or [`Response::Error`] when it
//! does not speak that version. Every line after the handshake is a
//! [`Request`] from the client and a [`Response`] from the daemon.

use std::{collections::BTreeMap, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

use crate::classify::Class;

/// Bumped whenever a change to the types below is not backwards compatible.
/// 2 added [`LeaseView::tool`], which a version-1 daemon does not send: a
/// client left talking to one after an in-place upgrade hears about the
/// mismatch at the handshake instead of failing to decode its replies.
pub const PROTOCOL_VERSION: u32 = 2;

/// The longest line either end will read. Anyone who can open the socket could
/// otherwise stream a newline-free message until the long-lived daemon runs out
/// of memory; real messages are orders of magnitude smaller than this. It binds
/// responses too, so a stale or wedged daemon cannot do the same to a client.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// What one bounded read off a connection produced.
#[derive(Debug)]
pub enum Line {
    Text(String),
    /// The peer closed the connection between messages.
    Closed,
    /// The bytes are not a message and the connection cannot be read past
    /// them, so the reason is all the peer gets: no newline within
    /// [`MAX_LINE_BYTES`], a close part-way through a message, or bytes that
    /// are not UTF-8.
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
        // The newline is the frame delimiter: complete-looking JSON that never
        // got one is a message the peer stopped writing, not one it finished.
        return Ok(Line::Malformed(format!(
            "the connection closed {} bytes into a message with no newline",
            line.len()
        )));
    }
    // Not an `io::Error`: the read succeeded and the frame ended where it
    // should. What arrived simply is not a message, which is a protocol error
    // the peer is owed an answer about — the same answer as any other broken
    // frame, since the decoder cannot be handed these bytes either.
    match String::from_utf8(line) {
        Ok(text) => Ok(Line::Text(text)),
        Err(e) => Ok(Line::Malformed(format!(
            "a line that is not valid utf-8 is not a message: {e}"
        ))),
    }
}

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
    /// Streamed on a `Submit` connection for the lifetime of the lease.
    Event(LeaseEvent),
    Error {
        message: String,
    },
}

/// The longest message [`Response::error`] will carry. A decoder quotes the
/// input it choked on, so an error over a line that fit within
/// [`MAX_LINE_BYTES`] can already be longer than one, and JSON-encoding the
/// message escapes it again. Cutting the message this far below the limit
/// leaves room for either expansion; nothing said past a kilobyte of
/// explanation helps the peer.
const MAX_ERROR_MESSAGE_BYTES: usize = 1024;

/// Marks a message [`Response::error`] cut short. Its bytes come out of
/// [`MAX_ERROR_MESSAGE_BYTES`], not on top of it.
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
    /// Basename of the tool [`crate::classify`] recognised, which is what
    /// decides the class once the injection work lands. Until it does,
    /// admission gives every lease `none` (see `docs/design/bzbd.md`
    /// §Observability), so this field reports what was recognised and `class`
    /// does not follow from it yet. Separate from `label`, which is the
    /// caller's `--name` when there is one.
    pub tool: String,
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

    /// The newline is the frame delimiter, not decoration: a peer that closes
    /// after syntactically complete JSON never finished the message. Handing
    /// it to the decoder anyway would let a truncated write pass for a whole
    /// one whenever the truncation happened to land on a `}`.
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

    /// The cap is on the message the peer receives, so the ellipsis has to
    /// come out of the budget rather than be added on top of it.
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

    /// `tool` was added to [`LeaseView`] after protocol version 1, and a
    /// version-1 daemon — one still running from before an in-place upgrade —
    /// sends status replies without it. Making the field optional would leave
    /// `busybee status` printing a blank tool column against such a daemon
    /// instead of naming the mismatch, so the field is required and
    /// [`PROTOCOL_VERSION`] moved with it: the handshake refuses the pairing
    /// before any reply is decoded.
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
