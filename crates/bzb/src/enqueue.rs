//! Blocking mode: take a lease from bzbd, stream the task's output, mirror
//! its exit code.
//!
//! The connection is the lease (`docs/design/bzbd.md` §Lease model), so it is
//! held open for the command's whole life and closing it is how Ctrl-C
//! cancels. The task's own output is not on that connection: bzbd hands back
//! the pueue task id, and the log is read straight from pueued, which is the
//! one thing the client still talks to directly — read-only.
//!
//! Everything busybee says goes to stderr; stdout carries the task's output
//! and nothing else.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use bzb_core::{
    classify::{classify, default_table, Class, Overrides},
    client,
    daemon::connect_or_spawn_bzbd,
    log::fetch_log_chunk,
    protocol::{LeaseEvent, LeaseRequest, Request},
    wait::QueueLines,
};
use pueue_lib::Client;
use tokio::{
    io::{AsyncWriteExt, Stdout},
    sync::mpsc,
    time::interval,
};

use crate::signals::{self, SignalEvent};

/// How often the task's log is swept while it runs, and the tick the queue
/// heartbeat counts in.
const POLL: Duration = Duration::from_secs(1);

/// What the client asks bzbd for. `detached` decides whether the lease
/// outlives this process.
pub fn lease_request(
    cmd: Vec<String>,
    name: Option<String>,
    class: Option<Class>,
    cores: Option<u32>,
    detached: bool,
) -> Result<LeaseRequest> {
    Ok(LeaseRequest {
        argv: cmd,
        cwd: std::env::current_dir().context("cannot read the working directory")?,
        // The daemon runs the task, so it needs the environment the caller
        // would have run it in. Colour variables are added on that side.
        env: std::env::vars().collect::<BTreeMap<_, _>>(),
        label: name,
        class_override: class,
        cores_wanted: cores,
        detached,
    })
}

pub async fn run(
    cmd: Vec<String>,
    name: Option<String>,
    class: Option<Class>,
    cores: Option<u32>,
) -> Result<()> {
    let request = lease_request(cmd, name, class, cores, false)?;
    // Only for the running line: the class the task is admitted under is the
    // daemon's to decide and comes back in the event.
    let tool = classify(
        &request.argv,
        &Overrides { class, cores: None },
        &default_table(),
    )
    .tool;

    let mut conn = connect_or_spawn_bzbd().await?;
    conn.send(Request::Submit(request)).await?;
    // The socket is read on its own task: `read_line` buffers, so cancelling
    // it in a `select!` would lose half a message and break the framing. A
    // channel receive is cancel-safe; this one also gives Ctrl-C a way to
    // close the connection, by dropping the task that owns it.
    let (events, mut incoming) = mpsc::unbounded_channel();
    let mut reader = tokio::spawn(async move {
        loop {
            let event = conn.events().next().await;
            let last = !matches!(event, Ok(Some(ref event)) if !finished(event));
            if events.send(event).is_err() || last {
                return;
            }
        }
    });

    let mut queue = QueueLines::new();
    let mut signals = signals::install();
    let mut ticker = interval(POLL);
    let mut stdout = tokio::io::stdout();
    let mut task: Option<Task> = None;
    let mut started_at: Option<Instant> = None;
    let mut tick: u64 = 0;

    loop {
        tokio::select! {
            event = incoming.recv() => match event {
                Some(Ok(Some(LeaseEvent::Queued { ahead, .. }))) => {
                    if let Some(line) = queue.queued(ahead) {
                        eprintln!("busybee: {line}");
                    }
                }
                Some(Ok(Some(LeaseEvent::Notice { text }))) => eprintln!("busybee: note: {text}"),
                Some(Ok(Some(LeaseEvent::Admitted {
                    pueue_task_id, class, cores, pool_size, peers, ..
                }))) => {
                    eprintln!("busybee: {}", running_line(&tool, &class, cores, pool_size, peers));
                    started_at = Some(Instant::now());
                    task = Some(Task {
                        // pueued is already up: bzbd needed it to start the
                        // task we were just told about.
                        pueue: client::connect_or_spawn().await?,
                        id: pueue_task_id,
                        log_offset: 0,
                    });
                }
                Some(Ok(Some(LeaseEvent::Finished { exit_code, .. }))) => {
                    if let Some(task) = task.as_mut() {
                        task.sweep(&mut stdout).await?;
                    }
                    eprintln!("{}", exit_line(exit_code, started_at.map(|t| t.elapsed())));
                    std::process::exit(exit_code);
                }
                Some(Ok(None)) | None => bail!("bzbd stopped streaming the lease before it finished"),
                Some(Err(err)) => return Err(err.into()),
            },
            _ = ticker.tick() => {
                tick += 1;
                match task.as_mut() {
                    Some(task) => task.sweep(&mut stdout).await?,
                    None => if let Some(line) = queue.tick(tick) {
                        eprintln!("busybee: {line}");
                    },
                }
            }
            signal = signals.recv() => match signal {
                // Connection = lease: dropping the reader closes the socket,
                // which is what tells bzbd to drop the lease and kill the task.
                // Whatever the task wrote since the last sweep stays in
                // `pueue log`; waiting for it here is what the second Ctrl-C
                // would be for.
                Some(SignalEvent::SoftCancel) => {
                    eprintln!("busybee: cancelling…");
                    reader.abort();
                    let _ = (&mut reader).await;
                    eprintln!("{}", exit_line(CANCELLED, started_at.map(|t| t.elapsed())));
                    std::process::exit(CANCELLED);
                }
                Some(SignalEvent::HardKill) => std::process::exit(CANCELLED),
                None => {}
            },
        }
    }
}

/// The exit code of a command its caller cancelled, by convention SIGINT's.
const CANCELLED: i32 = 130;

fn finished(event: &LeaseEvent) -> bool {
    matches!(event, LeaseEvent::Finished { .. })
}

/// The running task's log, as far as the client has read it.
struct Task {
    pueue: Client,
    id: usize,
    log_offset: u64,
}

impl Task {
    /// Writes whatever the task has produced since the last sweep.
    async fn sweep(&mut self, stdout: &mut Stdout) -> Result<()> {
        let (bytes, offset) = fetch_log_chunk(&mut self.pueue, self.id, self.log_offset).await?;
        self.log_offset = offset;
        if bytes.is_empty() {
            return Ok(());
        }
        stdout.write_all(&bytes).await.context("write stdout")?;
        stdout.flush().await.context("flush stdout")
    }
}

/// The line the client prints when its lease is admitted
/// (`docs/design/bzbd.md` §Client output contract). A jobserver task holds no
/// tokens of its own — it shares the pool at compile-job granularity — so
/// there is no held count to report for it.
fn running_line(tool: &str, class: &str, cores: u32, pool_size: u32, peers: usize) -> String {
    if class == Class::Jobserver.as_str() {
        format!(
            "running — {tool}, {class}, sharing {pool_size}-token pool with {}",
            others(peers)
        )
    } else {
        format!(
            "running — {tool}, {class}, holding {cores}/{pool_size} cores ({} active)",
            others(peers)
        )
    }
}

fn others(peers: usize) -> String {
    match peers {
        1 => "1 other task".into(),
        n => format!("{n} other tasks"),
    }
}

fn exit_line(code: i32, elapsed: Option<Duration>) -> String {
    match elapsed {
        Some(d) => format!(
            "busybee: command exited {code} (elapsed {})",
            format_elapsed(d)
        ),
        None => format!("busybee: command exited {code}"),
    }
}

fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s / 60) % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_under_a_minute() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "7s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_elapsed_minutes_and_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(2 * 60 + 14)), "2m14s");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1h00m");
        assert_eq!(
            format_elapsed(Duration::from_secs(3600 + 23 * 60 + 5)),
            "1h23m"
        );
    }

    #[test]
    fn exit_line_includes_elapsed_when_started() {
        let line = exit_line(0, Some(Duration::from_secs(134)));
        assert_eq!(line, "busybee: command exited 0 (elapsed 2m14s)");
    }

    #[test]
    fn exit_line_omits_elapsed_when_never_started() {
        assert_eq!(exit_line(130, None), "busybee: command exited 130");
    }

    /// Both shapes are quoted from `docs/design/bzbd.md` §Client output
    /// contract.
    #[test]
    fn the_running_line_matches_the_output_contract() {
        assert_eq!(
            running_line("cmake", "jobserver", 9, 18, 1),
            "running — cmake, jobserver, sharing 18-token pool with 1 other task"
        );
        assert_eq!(
            running_line("xcodebuild", "static", 9, 18, 2),
            "running — xcodebuild, static, holding 9/18 cores (2 other tasks active)"
        );
    }

    /// The one running task has no peers, and the line still has to read like
    /// a sentence.
    #[test]
    fn a_task_running_alone_reports_no_peers() {
        assert_eq!(
            running_line("make", "none", 8, 8, 0),
            "running — make, none, holding 8/8 cores (0 other tasks active)"
        );
    }
}
