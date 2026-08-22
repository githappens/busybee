//! `busybee monitor`: per-core CPU gauges over the pool bzbd is handing out.
//!
//! A viewer only (`docs/design/bzbd.md` §Observability): it asks the daemon for
//! a status once a second and never starts one, because looking at the pool
//! should not create it.

use std::time::{Duration, Instant};

use anyhow::Result;
use bzb_core::{
    daemon::{socket_path, Connection},
    protocol::{Request, Response, StatusReply},
};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Color;
use ratatui::widgets::{Block, Borders};
use ratatui::Terminal;
use tokio::{select, time};

use super::cpu::{self, usage_percent, CoreSample};
use super::widgets::compact_gauge::CompactGauge;
use super::widgets::lease_table::LeaseTable;
use super::widgets::pool_gauge::{PoolGauge, PoolView};

/// How long the daemon has to answer one poll. Shorter than the poll interval
/// would drop every answer that arrived late; longer would stack polls up
/// behind a wedged daemon.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

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

    // Polled before the first draw so the panel reports the pool it found
    // rather than a pool it has not asked about yet.
    let mut pool = Pool::default();
    pool.record(poll().await, Instant::now());

    let mut cpu_tick = time::interval(Duration::from_millis(500));
    cpu_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut status_tick = time::interval(Duration::from_millis(1000));
    status_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut render_tick = time::interval(Duration::from_millis(250));
    render_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut events = EventStream::new();

    loop {
        select! {
            _ = cpu_tick.tick() => {
                let curr = cpu::sample();
                usages = prev_samples.iter().zip(curr.iter())
                    .map(|(p, c)| usage_percent(*p, *c))
                    .collect();
                prev_samples = curr;
            }
            _ = status_tick.tick() => {
                pool.record(poll().await, Instant::now());
            }
            _ = render_tick.tick() => {
                draw(terminal, &usages, &pool.view(Instant::now()))?;
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

/// What one status poll produced. The IO is here; the state machine below is
/// pure.
enum Poll {
    Reply(StatusReply),
    /// Nothing is listening on the socket.
    Absent,
    /// A daemon is there and did not answer the request.
    Failed(String),
}

async fn poll() -> Poll {
    match ask().await {
        Ok(Some(reply)) => Poll::Reply(reply),
        Ok(None) => Poll::Absent,
        Err(e) => Poll::Failed(format!("{e:#}")),
    }
}

/// `Ok(None)` means nothing is listening — the one outcome that says there is
/// no pool. Everything else is a daemon that is running and not answering, and
/// the monitor says so rather than drawing an idle pool over it.
async fn ask() -> Result<Option<StatusReply>> {
    let socket = socket_path()?;
    let Some(mut conn) = Connection::connect_if_listening(&socket).await? else {
        return Ok(None);
    };
    let exchange = async {
        conn.send(Request::Status).await?;
        conn.recv().await
    };
    match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
        Ok(Ok(Response::Status(reply))) => Ok(Some(reply)),
        Ok(Ok(Response::Error { message })) => {
            anyhow::bail!("bzbd refused the status request: {message}")
        }
        Ok(Ok(other)) => anyhow::bail!("expected a status reply from bzbd, got {other:?}"),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => anyhow::bail!(
            "bzbd did not answer within {} second",
            REPLY_TIMEOUT.as_secs()
        ),
    }
}

/// The last thing the monitor learned about the pool.
#[derive(Default)]
struct Pool {
    /// The last reply and when it arrived.
    last_good: Option<(StatusReply, Instant)>,
    /// Why the most recent poll produced no reply, when it produced none.
    failure: Option<String>,
}

impl Pool {
    fn record(&mut self, poll: Poll, now: Instant) {
        match poll {
            Poll::Reply(reply) => {
                self.last_good = Some((reply, now));
                self.failure = None;
            }
            // No daemon means no pool, so the leases and counts of a daemon
            // that is gone are not the machine's state any more — and nothing
            // listening is an answer, not a failed poll.
            Poll::Absent => {
                self.last_good = None;
                self.failure = None;
            }
            // A failed poll against a live daemon loses one sample, not the
            // view: the last one is kept and marked as old.
            Poll::Failed(reason) => self.failure = Some(reason),
        }
    }

    fn view(&self, now: Instant) -> PoolView<'_> {
        match (&self.last_good, &self.failure) {
            (Some((reply, at)), failure) => PoolView::Known {
                reply,
                stale: failure.as_ref().map(|_| now.saturating_duration_since(*at)),
            },
            (None, Some(reason)) => PoolView::Unreachable(reason),
            // Nothing listening, and nothing polled yet before the first poll
            // the loop makes: either way the monitor knows of no pool.
            (None, None) => PoolView::Absent,
        }
    }
}

fn draw<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    usages: &[u8],
    view: &PoolView,
) -> anyhow::Result<()> {
    terminal.draw(|frame| {
        let leases: &[_] = match view {
            PoolView::Known { reply, .. } => &reply.leases,
            PoolView::Absent | PoolView::Unreachable(_) => &[],
        };
        // The pool panel is the bar, its legend and the two borders; the lease
        // table takes a row per lease and the CPU gauges keep the rest.
        let chunks = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(4),
            Constraint::Length(leases.len() as u16 + 2),
        ])
        .split(frame.size());

        let cpu_block = Block::default().borders(Borders::ALL).title("CPU");
        let inner_cpu = cpu_block.inner(chunks[0]);
        frame.render_widget(cpu_block, chunks[0]);
        frame.render_widget(
            CompactGauge {
                usages,
                skeleton: Color::DarkGray,
            },
            inner_cpu,
        );

        let pool_block = Block::default().borders(Borders::ALL).title("Pool");
        let inner_pool = pool_block.inner(chunks[1]);
        frame.render_widget(pool_block, chunks[1]);
        frame.render_widget(PoolGauge { view }, inner_pool);

        let lease_block = Block::default().borders(Borders::ALL).title("Leases");
        let inner_leases = lease_block.inner(chunks[2]);
        frame.render_widget(lease_block, chunks[2]);
        frame.render_widget(LeaseTable { leases }, inner_leases);
    })?;
    Ok(())
}

#[cfg(test)]
mod screenshot;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::widgets::tests::render;
    use ratatui::widgets::Widget;

    fn reply() -> StatusReply {
        StatusReply {
            pool_size: 18,
            free: 6,
            held: 9,
            leases: vec![],
        }
    }

    fn legend(view: &PoolView) -> String {
        render(60, 2, |area, buf| PoolGauge { view }.render(area, buf))
            .remove(1)
            .trim_end()
            .to_string()
    }

    /// A poll that fails against a live daemon loses one sample. Clearing the
    /// view for it would blank the panel every time a status request is late.
    #[test]
    fn a_failed_poll_keeps_the_last_view_and_marks_it_stale() {
        let start = Instant::now();
        let mut pool = Pool::default();
        pool.record(Poll::Reply(reply()), start);
        pool.record(
            Poll::Failed("connection reset".into()),
            start + Duration::from_secs(3),
        );

        let view = pool.view(start + Duration::from_secs(3));
        assert!(matches!(
            view,
            PoolView::Known {
                stale: Some(age),
                ..
            } if age == Duration::from_secs(3)
        ));
        assert_eq!(
            legend(&view),
            "18 tokens · 9 held · ~3 in use · 6 free · (stale 3s)"
        );
    }

    /// The next answer replaces the stale one.
    #[test]
    fn a_later_reply_clears_the_stale_marker() {
        let start = Instant::now();
        let mut pool = Pool::default();
        pool.record(Poll::Reply(reply()), start);
        pool.record(Poll::Failed("connection reset".into()), start);
        pool.record(Poll::Reply(reply()), start + Duration::from_secs(1));

        let view = pool.view(start + Duration::from_secs(1));
        assert!(matches!(view, PoolView::Known { stale: None, .. }));
        assert!(!legend(&view).contains("stale"));
    }

    /// A daemon that went away takes its pool with it: the leases it reported
    /// are not on the machine any more, so showing them as stale would be
    /// showing load that is over.
    #[test]
    fn a_daemon_that_went_away_clears_the_view() {
        let start = Instant::now();
        let mut pool = Pool::default();
        pool.record(Poll::Reply(reply()), start);
        pool.record(Poll::Absent, start + Duration::from_secs(1));

        assert!(matches!(
            pool.view(start + Duration::from_secs(1)),
            PoolView::Absent
        ));
    }

    /// With no reply to fall back on there is nothing to mark stale, and a
    /// daemon that is not answering is not an idle pool.
    #[test]
    fn a_failure_before_any_reply_is_reported_as_unreachable() {
        let now = Instant::now();
        let mut pool = Pool::default();
        pool.record(Poll::Failed("connection reset".into()), now);

        assert!(matches!(pool.view(now), PoolView::Unreachable(reason)
            if reason == "connection reset"));
    }
}
