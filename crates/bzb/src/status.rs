//! `busybee status [--json]`: one shot of the pool and leases (bzbd.md
//! §Observability).

use std::{io::ErrorKind, time::Duration};

use anyhow::{bail, Context, Result};
use bzb_core::{
    daemon::{leases_path, socket_path, Connection},
    protocol::{LeaseView, Request, Response, StatusReply},
};

/// Bounds the exchange after the handshake, whose own deadline ends at the pong.
const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

/// Sends `req` to a running bzbd and returns its reply, or `None` if nothing is
/// listening. Never starts the daemon; `Response::Error` becomes an error.
pub(crate) async fn ask_running(req: Request, timeout: Duration) -> Result<Option<Response>> {
    let socket = socket_path()?;
    let Some(mut conn) = Connection::connect_if_listening(&socket).await? else {
        return Ok(None);
    };
    let exchange = async {
        conn.send(req).await?;
        conn.recv().await
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(Ok(Response::Error { message })) => bail!("bzbd refused the request: {message}"),
        Ok(result) => Ok(Some(result?)),
        Err(_) => bail!(
            "bzbd took the request but did not answer within {}s",
            timeout.as_secs()
        ),
    }
}

/// Deliberately no auto-start: asking about the pool should not create it.
pub async fn run(json: bool) -> Result<()> {
    let reply = match ask_running(Request::Status, REPLY_TIMEOUT).await? {
        None => return report_absent_daemon(),
        Some(Response::Status(reply)) => reply,
        Some(other) => bail!("expected a status reply from bzbd, got {other:?}"),
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

/// Nothing listening is an idle pool, unless a dead daemon left leases in
/// `leases.json`: their tasks may still run under pueued (bzbd.md §Failure and
/// recovery), so that is an error, not "idle".
fn report_absent_daemon() -> Result<()> {
    let recorded = recorded_leases()?;
    if recorded > 0 {
        bail!(
            "bzbd is not running, but {} still records {recorded} lease(s) it held: the tasks \
             pueued started for them may still be on the machine, so the pool is not idle",
            leases_path()?.display(),
        );
    }
    eprintln!("busybee: daemon not running; pool idle");
    Ok(())
}

/// Leases recorded in `leases.json`. Unreadable is an error, not zero: it is
/// the only evidence of what a dead daemon left running.
pub(crate) fn recorded_leases() -> Result<usize> {
    let path = leases_path()?;
    let recorded: Vec<serde_json::Value> = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("cannot read the leases bzbd left in {}", path.display()))?,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => bail!(
            "cannot open the leases bzbd left in {}: {e}",
            path.display()
        ),
    };
    Ok(recorded.len())
}

fn json_line(reply: &StatusReply) -> Result<String> {
    let mut value = serde_json::to_value(reply)?;
    value
        .as_object_mut()
        .expect("a StatusReply serialises as a JSON object")
        .insert("approx_in_use".into(), approx_in_use(reply).into());
    Ok(value.to_string())
}

/// Tokens neither free nor held, i.e. roughly what jobserver tasks use. Clamped
/// at 0 because the pool and the fifo are sampled separately.
pub(crate) fn approx_in_use(reply: &StatusReply) -> u32 {
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
        printable(&lease.tool),
        lease.class,
        cores(lease),
        printable(&lease.label)
    )
}

/// Escapes control characters in caller-supplied text (labels, tool names) so
/// they cannot split a row or drive the terminal.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            out.extend(character.escape_debug());
        } else {
            out.push(character);
        }
    }
    out
}

pub(crate) fn cores(lease: &LeaseView) -> String {
    match lease.ahead {
        Some(ahead) => format!("{ahead} ahead"),
        // A `none` lease owns the machine but drains no tokens.
        None if lease.class == "none" => "exclusive".to_string(),
        // Without per-process attribution a jobserver lease has no count of
        // its own (spec §Observability).
        None if lease.class == "jobserver" => match lease.cores {
            Some(n) => format!("using ~{n}"),
            None => "sharing".to_string(),
        },
        // A held count is never missing; if bzbd omits it, say so, not 0.
        None => match lease.cores {
            Some(n) => format!("holding {n}"),
            None => "holding ?".to_string(),
        },
    }
}

pub(crate) fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                cores: Some(9),
                state: "running".into(),
                elapsed_ms: 132_000,
                ahead: None,
                pueue_task_id: Some(3),
            }],
        }
    }

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

    #[test]
    fn a_control_character_in_a_tool_name_cannot_break_the_table() {
        let mut reply = reply();
        reply.leases[0].tool = "xcode\nbuild\u{1b}[2K".into();

        let rendered = render(&reply);

        assert_eq!(rendered.lines().count(), 2, "rendered {rendered:?}");
        assert!(!rendered.contains('\u{1b}'), "rendered {rendered:?}");
        assert!(
            rendered.contains(r"xcode\nbuild\u{1b}[2K"),
            "rendered {rendered:?}"
        );
    }

    #[test]
    fn an_exclusive_lease_is_reported_as_exclusive_not_as_holding_zero() {
        let mut reply = reply();
        reply.free = 18;
        reply.held = 0;
        reply.leases[0].class = "none".into();
        reply.leases[0].cores = Some(0);

        let rendered = render(&reply);

        assert!(rendered.contains("exclusive"), "rendered {rendered:?}");
        assert!(!rendered.contains("holding"), "rendered {rendered:?}");
    }

    #[test]
    fn a_running_jobserver_lease_without_attribution_shows_sharing() {
        let mut lease = reply().leases.remove(0);
        lease.class = "jobserver".into();
        lease.cores = None;
        assert_eq!(cores(&lease), "sharing");
    }

    #[test]
    fn a_jobserver_lease_with_attributed_cores_shows_using_n() {
        let mut lease = reply().leases.remove(0);
        lease.class = "jobserver".into();
        lease.cores = Some(5);
        assert_eq!(cores(&lease), "using ~5");
    }

    #[test]
    fn a_running_static_lease_shows_holding_n() {
        assert_eq!(cores(&reply().leases[0]), "holding 9");
    }

    #[test]
    fn a_static_lease_without_a_count_is_not_reported_as_holding_zero() {
        let mut lease = reply().leases.remove(0);
        lease.cores = None;
        assert_eq!(cores(&lease), "holding ?");
    }

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
