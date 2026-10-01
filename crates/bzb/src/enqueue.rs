//! Blocking mode: take a lease from bzbd, stream the task's output, mirror its
//! exit code. The connection is the lease (bzbd.md §Lease model).

use std::{
    collections::BTreeMap,
    os::unix::process::CommandExt,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use bzb_core::client::Client;
use bzb_core::{
    classify::{classify, default_table, Class, Overrides},
    client,
    daemon::connect_or_spawn_bzbd,
    log::fetch_log_chunk,
    nest::{self, LEASE_ENV},
    protocol::{LeaseEvent, LeaseRequest, Request, Response},
    wait::QueueLines,
};
use tokio::{
    io::{AsyncWriteExt, Stdout},
    signal::unix::{signal, SignalKind},
    sync::mpsc,
    time::interval,
};

/// Log sweep interval, and the tick the queue heartbeat counts in.
const POLL: Duration = Duration::from_secs(1);

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
        // The daemon runs the task, so it needs the caller's environment.
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
    if let Some(id) = live_parent_lease().await? {
        // Queueing under a live parent lease would deadlock (bzbd.md §Nesting).
        eprintln!("{}", nest::passthrough_line(id));
        exec_command(&cmd)?;
    }
    let request = lease_request(cmd, name, class, cores, false)?;
    // Only for the running line; the admitted class comes back from bzbd.
    let tool = classify(
        &request.argv,
        &Overrides { class, cores: None },
        &default_table(),
    )
    .tool;

    let mut conn = connect_or_spawn_bzbd().await?;
    conn.send(Request::Submit(request)).await?;
    // `read_line` is not cancel-safe in `select!`, so the socket is read on
    // its own task; aborting that task is also how Ctrl-C closes the lease.
    let (events, mut incoming) = mpsc::unbounded_channel();
    let mut reader = tokio::spawn(async move {
        loop {
            let event = conn.events().next().await;
            let last =
                !matches!(event, Ok(Some(ref e)) if !matches!(e, LeaseEvent::Finished { .. }));
            if events.send(event).is_err() || last {
                return;
            }
        }
    });

    let mut queue = QueueLines::new();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
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
                        // Connect, never spawn: a pueued of our own would
                        // have an empty queue.
                        pueue: client::connect().await.context(
                            "cannot read the task's log: bzbd started it on a pueued this \
                             client cannot reach (check PUEUE_CONFIG_PATH)",
                        )?,
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
            _ = sigint.recv() => break,
            _ = sigterm.recv() => break,
        }
    }

    // Closing the socket is what tells bzbd to drop the lease and kill the task.
    eprintln!("busybee: cancelling…");
    reader.abort();
    let _ = (&mut reader).await;
    eprintln!("{}", exit_line(CANCELLED, started_at.map(|t| t.elapsed())));
    std::process::exit(CANCELLED);
}

/// SIGINT's conventional exit code.
const CANCELLED: i32 = 130;

struct Task {
    pueue: Client,
    id: usize,
    log_offset: u64,
}

impl Task {
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

/// See bzbd.md §Client output contract.
fn running_line(tool: &str, class: &str, cores: u32, pool_size: u32, peers: usize) -> String {
    if class == Class::Jobserver.as_str() {
        format!(
            "running — {tool}, {class}, sharing {pool_size}-token pool with {}",
            others(peers)
        )
    } else if class == Class::None.as_str() {
        format!("running — {tool}, {class}, exclusive ({pool_size} cores)")
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

/// The parent lease this process runs under, if bzbd still holds it: a stale
/// marker must not disable gating.
async fn live_parent_lease() -> Result<Option<u64>> {
    let Ok(marker) = std::env::var(LEASE_ENV) else {
        return Ok(None);
    };
    let mut conn = connect_or_spawn_bzbd().await?;
    conn.send(Request::Status).await?;
    match conn.recv().await? {
        Response::Status(status) => Ok(nest::passthrough_parent(Some(&marker), &status.leases)),
        Response::Error { message } => {
            bail!("cannot read status to check nesting: {message}")
        }
        other => bail!("expected a status reply while checking nesting, got {other:?}"),
    }
}

/// Returns only if the exec itself failed.
fn exec_command(cmd: &[String]) -> Result<()> {
    anyhow::ensure!(!cmd.is_empty(), "no command given");
    let err = std::process::Command::new(&cmd[0]).args(&cmd[1..]).exec();
    Err(err).with_context(|| format!("cannot exec {}", cmd[0]))
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

    #[test]
    fn a_task_running_alone_reports_no_peers() {
        assert_eq!(
            running_line("xcodebuild", "static", 8, 8, 0),
            "running — xcodebuild, static, holding 8/8 cores (0 other tasks active)"
        );
    }

    #[test]
    fn an_exclusive_lease_reports_the_whole_machine() {
        assert_eq!(
            running_line("make", "none", 1, 18, 0),
            "running — make, none, exclusive (18 cores)"
        );
    }
}
