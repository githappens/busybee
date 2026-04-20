use std::time::{Duration, Instant};

use anyhow::Result;
use bzb_core::{client, group, status::QueueSnapshot};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent};
use futures::StreamExt;
use pueue_lib::Client;
use pueue_lib::message::{Request, Response};
use ratatui::Terminal;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Color;
use ratatui::widgets::{Block, Borders};
use tokio::{select, time};

use super::cpu::{self, CoreSample, usage_percent};
use super::widgets::compact_gauge::CompactGauge;
use super::widgets::status_panel::StatusPanel;

pub async fn run() -> Result<()> {
    // TUI setup.
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal).await;

    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;
    result
}

async fn run_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    let mut prev_samples: Vec<CoreSample> = cpu::sample();
    let mut usages: Vec<u8> = vec![0; prev_samples.len()];
    let mut snapshot = QueueSnapshot {
        running: None,
        queued: vec![],
    };

    let mut stream: Option<Client> = client::connect_or_spawn().await.ok();
    if let Some(ref mut s) = stream {
        let _ = group::ensure_busybee_group(s).await;
    }

    let mut cpu_tick = time::interval(Duration::from_millis(500));
    cpu_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut queue_tick = time::interval(Duration::from_millis(1000));
    queue_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut anim_tick = time::interval(Duration::from_millis(100));
    anim_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let start = Instant::now();
    let mut events = EventStream::new();

    loop {
        select! {
            _ = cpu_tick.tick() => {
                let curr = cpu::sample();
                usages = prev_samples.iter().zip(curr.iter())
                    .map(|(p, c)| usage_percent(*p, *c))
                    .collect();
                prev_samples = curr;
                draw(terminal, &usages, &snapshot, start.elapsed())?;
            }
            _ = queue_tick.tick() => {
                if stream.is_none() {
                    stream = client::connect_or_spawn().await.ok();
                    if let Some(ref mut s) = stream {
                        let _ = group::ensure_busybee_group(s).await;
                    }
                }
                if let Some(ref mut s) = stream {
                    if let Ok(snap) = fetch_snapshot(s).await {
                        snapshot = snap;
                    } else {
                        stream = None;
                    }
                }
                draw(terminal, &usages, &snapshot, start.elapsed())?;
            }
            _ = anim_tick.tick() => {
                draw(terminal, &usages, &snapshot, start.elapsed())?;
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(CtEvent::Key(KeyEvent { code: KeyCode::Char('q'), .. }))) => break,
                    Some(Ok(CtEvent::Key(KeyEvent { code: KeyCode::Char('c'), modifiers, .. })))
                        if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => break,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn fetch_snapshot(client: &mut Client) -> anyhow::Result<QueueSnapshot> {
    client.send_request(Request::Status).await
        .map_err(|e| anyhow::anyhow!("status request: {e}"))?;
    let resp = client.receive_response().await
        .map_err(|e| anyhow::anyhow!("status response: {e}"))?;
    match resp {
        Response::Status(state) => Ok(QueueSnapshot::from_tasks(state.tasks.values(), "busybee")),
        other => anyhow::bail!("unexpected: {other:?}"),
    }
}

fn draw<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    usages: &[u8],
    snapshot: &QueueSnapshot,
    elapsed: Duration,
) -> anyhow::Result<()> {
    terminal.draw(|frame| {
        let area = frame.size();
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);
        let top = chunks[0];
        let bottom = chunks[1];

        let top_block = Block::default().borders(Borders::ALL).title("CPU");
        let inner_top = top_block.inner(top);
        frame.render_widget(top_block, top);
        frame.render_widget(
            CompactGauge {
                usages,
                skeleton: Color::DarkGray,
            },
            inner_top,
        );

        let bottom_block = Block::default().borders(Borders::ALL).title("Queue");
        let inner_bottom = bottom_block.inner(bottom);
        frame.render_widget(bottom_block, bottom);
        frame.render_widget(
            StatusPanel {
                snapshot,
                elapsed,
            },
            inner_bottom,
        );
    })?;
    Ok(())
}
