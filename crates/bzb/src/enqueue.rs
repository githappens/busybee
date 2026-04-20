use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bzb_core::{
    client,
    enqueue as core_enqueue,
    exit_code::task_result_to_exit_code,
    group,
    log::fetch_log_chunk,
    wait::{WaitEvent, WaitState},
};
use pueue_lib::Client;
use pueue_lib::message::{KillRequest, Request, Response, Signal, TaskSelection};
use pueue_lib::state::State;
use pueue_lib::task::TaskStatus;
use tokio::{io::AsyncWriteExt, time::sleep};

use crate::detach::shell_escape_join;
use crate::signals::{self, SignalEvent};

pub async fn run(cmd: Vec<String>, name: Option<String>) -> Result<()> {
    let mut client = client::connect_or_spawn().await?;
    group::ensure_busybee_group(&mut client).await?;

    let cmd_string = shell_escape_join(&cmd);
    let display_label = name.clone().unwrap_or_else(|| cmd_string.clone());
    let spec = core_enqueue::TaskSpec::from_current_env(cmd_string, name)?;
    let task_id = core_enqueue::enqueue(&mut client, spec).await?;

    let mut wait_state = WaitState::new(task_id, display_label);
    let mut signals = signals::install();
    let mut log_offset: u64 = 0;
    let mut started = false;
    let mut started_at: Option<Instant> = None;
    let mut stdout = tokio::io::stdout();
    let mut tick: u64 = 0;

    loop {
        tokio::select! {
            sig = signals.recv() => match sig {
                Some(SignalEvent::SoftCancel) => {
                    soft_cancel(&mut client, task_id).await;
                }
                Some(SignalEvent::HardKill) => {
                    hard_kill(&mut client, task_id).await;
                    std::process::exit(130);
                }
                None => {}
            },
            _ = sleep(Duration::from_millis(1000)) => {
                tick += 1;
                let state = fetch_state(&mut client).await?;
                let events = wait_state.observe(tick, state.tasks.values());
                let mut finished = false;
                for e in events {
                    match e {
                        WaitEvent::Line(l) => eprintln!("busybee: {l}"),
                        WaitEvent::Started => {
                            started = true;
                            started_at = Some(Instant::now());
                        }
                        WaitEvent::Finished { .. } => { finished = true; }
                    }
                }
                if started {
                    let (bytes, new_off) = fetch_log_chunk(&mut client, task_id, log_offset).await?;
                    if !bytes.is_empty() {
                        stdout.write_all(&bytes).await.context("write stdout")?;
                        stdout.flush().await.ok();
                    }
                    log_offset = new_off;
                }
                if finished {
                    // Final sweep for any buffered bytes.
                    let (bytes, _) = fetch_log_chunk(&mut client, task_id, log_offset).await?;
                    if !bytes.is_empty() {
                        stdout.write_all(&bytes).await.ok();
                        stdout.flush().await.ok();
                    }
                    let code = final_exit_code(&state, task_id);
                    eprintln!("{}", exit_line(code, started_at.map(|t| t.elapsed())));
                    std::process::exit(code);
                }
            }
        }
    }
}

fn exit_line(code: i32, elapsed: Option<Duration>) -> String {
    match elapsed {
        Some(d) => format!("busybee: command exited {code} (elapsed {})", format_elapsed(d)),
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
    use super::{exit_line, format_elapsed};
    use std::time::Duration;

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
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 23 * 60 + 5)), "1h23m");
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
}

async fn fetch_state(client: &mut Client) -> Result<State> {
    client.send_request(Request::Status).await
        .map_err(|e| anyhow::anyhow!("status request: {e}"))?;
    match client.receive_response().await
        .map_err(|e| anyhow::anyhow!("status response: {e}"))? {
        Response::Status(state) => Ok(*state),
        other => anyhow::bail!("unexpected response to Status: {other:?}"),
    }
}

fn final_exit_code(state: &State, task_id: usize) -> i32 {
    let Some(task) = state.tasks.get(&task_id) else { return 1 };
    match &task.status {
        TaskStatus::Done { result, .. } => task_result_to_exit_code(result),
        _ => 1,
    }
}

async fn soft_cancel(client: &mut Client, task_id: usize) {
    let _ = client
        .send_request(Request::Kill(KillRequest {
            tasks: TaskSelection::TaskIds(vec![task_id]),
            signal: Some(Signal::SigTerm),
        }))
        .await;
    let _ = client.receive_response().await;
    eprintln!("busybee: cancelling task {task_id}…");
}

async fn hard_kill(client: &mut Client, task_id: usize) {
    let _ = client
        .send_request(Request::Kill(KillRequest {
            tasks: TaskSelection::TaskIds(vec![task_id]),
            signal: Some(Signal::SigKill),
        }))
        .await;
    let _ = client.receive_response().await;
}
