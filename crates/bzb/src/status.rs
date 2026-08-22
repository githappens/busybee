//! `busybee status [--json]`: one shot of the token pool and the leases bzbd
//! is tracking, for humans and for agents.
//!
//! See `docs/design/bzbd.md` §Observability. The client renders what the
//! daemon reports and computes nothing of its own beyond `approx_in_use`,
//! which is arithmetic on the numbers in the reply.

use std::time::Duration;

use anyhow::{bail, Result};
use bzb_core::{
    daemon::{socket_path, Connection},
    protocol::{LeaseView, Request, Response, StatusReply},
};

/// How long the daemon has to answer, once it has handshaken. The handshake's
/// own deadline ends at the pong, and this command has none of its own — a
/// daemon that wedges after it would hold `status` open for as long as it stays
/// wedged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// Asks the running daemon for a status and prints it.
///
/// Deliberately does not auto-start bzbd the way a lease request does: asking
/// what the pool is doing should not create the pool, and a daemon that failed
/// to *start* is a failure to report, not an idle machine. So a socket nothing
/// is listening on is reported as what it is, and everything else propagates.
pub async fn run(json: bool) -> Result<()> {
    let socket = socket_path()?;
    // Nothing listening is not a degraded path to report around: no daemon
    // means no pool, no leases and nothing being gated, which is what this
    // says. It goes to stderr like every other busybee message, so a `--json`
    // consumer sees an empty stdout rather than an invented reply — an all-zero
    // `StatusReply` is indistinguishable from a real idle pool. A daemon that
    // answered and then failed the handshake is running, so that propagates
    // rather than being reported as an idle machine.
    let Some(mut conn) = Connection::connect_if_listening(&socket).await? else {
        eprintln!("busybee: daemon not running; pool idle");
        return Ok(());
    };

    let exchange = async {
        conn.send(Request::Status).await?;
        conn.recv().await
    };
    let response = match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
        Ok(result) => result?,
        Err(_) => bail!(
            "bzbd took the status request but did not answer within {} seconds",
            REPLY_TIMEOUT.as_secs()
        ),
    };
    let reply = match response {
        Response::Status(reply) => reply,
        Response::Error { message } => bail!("bzbd refused the status request: {message}"),
        other => bail!("expected a status reply from bzbd, got {other:?}"),
    };

    println!(
        "{}",
        if json {
            json_line(&reply)?
        } else {
            render(&reply)
        }
    );
    Ok(())
}

/// The reply verbatim plus `approx_in_use`, as one JSON object.
fn json_line(reply: &StatusReply) -> Result<String> {
    let mut value = serde_json::to_value(reply)?;
    value
        .as_object_mut()
        // `StatusReply` is a struct, so serde always gives us a map here.
        .expect("a StatusReply serialises as a JSON object")
        .insert("approx_in_use".into(), approx_in_use(reply).into());
    Ok(value.to_string())
}

/// Tokens neither free nor held by a static lease, so approximately what the
/// jobserver tasks are using. Clamped at 0: the pool and the fifo are sampled
/// separately, so a sum over the boundary is drift, not a negative count.
fn approx_in_use(reply: &StatusReply) -> u32 {
    reply
        .pool_size
        .saturating_sub(reply.free)
        .saturating_sub(reply.held)
}

fn render(reply: &StatusReply) -> String {
    let pool = format!(
        "pool: {} tokens, {} free, {} held by static leases   \
         (approx. {} in use by jobserver tasks)",
        reply.pool_size,
        reply.free,
        reply.held,
        approx_in_use(reply)
    );
    std::iter::once(pool)
        .chain(reply.leases.iter().map(row))
        .collect::<Vec<_>>()
        .join("\n")
}

fn row(lease: &LeaseView) -> String {
    format!(
        "{:<5}{:<9}{:<7}{:<13}{:<11}{:<14}label: {}",
        format!("#{}", lease.id),
        lease.state,
        elapsed(lease.elapsed_ms),
        lease.tool,
        lease.class,
        cores(lease),
        printable(&lease.label)
    )
}

/// A label is the caller's `--name`, i.e. arbitrary text. One lease is one row,
/// so a newline in it must not split the row and an escape sequence must not
/// rewrite what the terminal has already drawn: control characters are shown as
/// their escape (`\n`, `\u{1b}`) instead of being sent through. `--json` still
/// carries the label verbatim — a decoder is not a terminal.
fn printable(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for character in label.chars() {
        if character.is_control() {
            out.extend(character.escape_debug());
        } else {
            out.push(character);
        }
    }
    out
}

/// What the lease is doing with the pool: waiting for it, sharing it, or
/// holding a slice of it.
fn cores(lease: &LeaseView) -> String {
    match lease.ahead {
        Some(ahead) => format!("{ahead} ahead"),
        // A jobserver task takes and returns tokens as its compile jobs come
        // and go, so its count is an estimate the daemon made and is marked as
        // one. A static task holds exactly what it drained.
        None if lease.class == "jobserver" => format!("using ~{}", lease.cores),
        None => format!("holding {}", lease.cores),
    }
}

fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table itself is asserted column for column in
    /// `crates/bzb/tests/status.rs`, against a daemon.
    fn reply() -> StatusReply {
        StatusReply {
            pool_size: 18,
            free: 6,
            held: 9,
            leases: vec![LeaseView {
                id: 41,
                label: "ui build".into(),
                tool: "xcodebuild".into(),
                class: "static".into(),
                cores: 9,
                state: "running".into(),
                elapsed_ms: 132_000,
                ahead: None,
                pueue_task_id: Some(3),
            }],
        }
    }

    /// A label is whatever the caller passed to `--name`, so it can carry a
    /// newline or an escape sequence. One lease is still one row, and the
    /// terminal above the table is still the caller's.
    #[test]
    fn a_control_character_in_a_label_cannot_break_the_table() {
        let mut reply = reply();
        reply.leases[0].label = "ui build\nfailed\u{1b}[2K".into();

        let rendered = render(&reply);

        assert_eq!(rendered.lines().count(), 2, "rendered {rendered:?}");
        assert!(!rendered.contains('\u{1b}'), "rendered {rendered:?}");
        assert!(
            rendered.contains(r"ui build\nfailed\u{1b}[2K"),
            "rendered {rendered:?}"
        );
    }

    /// The pool and the fifo are sampled separately, so the three numbers can
    /// cross. A wrapped subtraction would report four billion tokens in use.
    #[test]
    fn approx_in_use_clamps_at_zero() {
        let drifted = StatusReply {
            pool_size: 8,
            free: 8,
            held: 4,
            leases: vec![],
        };
        assert_eq!(approx_in_use(&drifted), 0);
    }

    /// `--json` is the agent-facing contract: the field names are
    /// `protocol::StatusReply`'s own, so a consumer can decode the line back
    /// into the type the daemon sent.
    #[test]
    fn the_json_line_decodes_back_into_a_status_reply() {
        let sent = reply();
        let line = json_line(&sent).expect("encode");

        let round_tripped: StatusReply = serde_json::from_str(&line).expect("decode");
        assert_eq!(
            serde_json::to_value(&round_tripped).unwrap(),
            serde_json::to_value(&sent).unwrap()
        );

        let object: serde_json::Value = serde_json::from_str(&line).expect("decode");
        assert_eq!(object["approx_in_use"], 3);
        assert!(!line.contains('\n'), "line was {line:?}");
    }
}
