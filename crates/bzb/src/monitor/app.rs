//! `busybee monitor`: per-core CPU gauges plus the bzbd pool. A viewer only:
//! it polls status and never starts the daemon.

use std::time::{Duration, Instant};

use anyhow::Result;
use bzb_core::protocol::{Request, Response, StatusReply};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders};
use ratatui::Terminal;
use tokio::{select, time};

use super::cpu::{self, usage_percent, CoreSample};
use super::widgets::compact_gauge::CompactGauge;
use super::widgets::lease_table::LeaseTable;
use super::widgets::pool_gauge::{PoolGauge, PoolView};

/// Matches the poll interval so polls never stack up behind a wedged daemon.
const REPLY_TIMEOUT: Duration = Duration::from_secs(1);

pub async fn run() -> Result<()> {
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

    let mut pool = Pool::default();

    // Polls run off the interactive loop so a slow daemon cannot freeze
    // redraws or the `q` key. One poll is in flight at a time.
    let (polls_tx, mut polls) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let mut status_tick = time::interval(Duration::from_millis(1000));
        status_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            status_tick.tick().await;
            if polls_tx.send(poll().await).await.is_err() {
                break;
            }
        }
    });

    let mut cpu_tick = time::interval(Duration::from_millis(500));
    cpu_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
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
            Some(polled) = polls.recv() => {
                pool.record(polled, Instant::now());
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

enum Poll {
    Reply(StatusReply),
    /// Nothing listening, no leases left behind.
    Absent,
    /// Nothing listening, but the lease record is non-empty or unreadable.
    Crashed(String),
    /// A daemon is there and did not answer.
    Failed(String),
}

async fn poll() -> Poll {
    match crate::status::ask_running(Request::Status, REPLY_TIMEOUT).await {
        Ok(Some(Response::Status(reply))) => Poll::Reply(reply),
        Ok(Some(other)) => {
            Poll::Failed(format!("expected a status reply from bzbd, got {other:?}"))
        }
        Ok(None) => absent(crate::status::recorded_leases()),
        Err(e) => Poll::Failed(format!("{e:#}")),
    }
}

/// Same rule as `busybee status`: leases left by a dead daemon are not idle.
fn absent(recorded: anyhow::Result<usize>) -> Poll {
    match recorded {
        Ok(0) => Poll::Absent,
        Ok(n) => Poll::Crashed(format!(
            "bzbd is not running, but {n} lease(s) it held are still recorded; \
             the tasks pueued started for them may still be on the machine"
        )),
        Err(e) => Poll::Crashed(format!("bzbd is not running, and {e:#}")),
    }
}

#[derive(Default)]
struct Pool {
    last_good: Option<(StatusReply, Instant)>,
    /// Why the most recent poll produced no reply.
    failure: Option<String>,
    /// False until the first poll comes back: "no daemon" is a finding.
    answered: bool,
}

impl Pool {
    fn record(&mut self, poll: Poll, now: Instant) {
        self.answered = true;
        match poll {
            Poll::Reply(reply) => {
                self.last_good = Some((reply, now));
                self.failure = None;
            }
            Poll::Absent => {
                self.last_good = None;
                self.failure = None;
            }
            // Its tasks may outlive it, so the last reply no longer describes
            // the machine.
            Poll::Crashed(reason) => {
                self.last_good = None;
                self.failure = Some(reason);
            }
            // Loses one sample, not the view: the last one is marked stale.
            Poll::Failed(reason) => self.failure = Some(reason),
        }
    }

    fn view(&self, now: Instant) -> PoolView<'_> {
        if !self.answered {
            return PoolView::Pending;
        }
        match (&self.last_good, &self.failure) {
            (Some((reply, at)), failure) => PoolView::Known {
                reply,
                stale: failure.as_ref().map(|_| now.saturating_duration_since(*at)),
            },
            (None, Some(reason)) => PoolView::Unreachable(reason),
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
            PoolView::Pending | PoolView::Absent | PoolView::Unreachable(_) => &[],
        };
        // The queue is unbounded, so the lease table is capped to leave the CPU
        // gauges their borders and one row of cells.
        const CPU_MIN: u16 = 8;
        const POOL: u16 = 4;
        let leases_height =
            (leases.len() as u16 + 2).min(frame.size().height.saturating_sub(CPU_MIN + POOL));
        let chunks = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(POOL),
            Constraint::Length(leases_height),
        ])
        .split(frame.size());

        let cpu_block = Block::default().borders(Borders::ALL).title("CPU");
        let inner_cpu = cpu_block.inner(chunks[0]);
        frame.render_widget(cpu_block, chunks[0]);
        frame.render_widget(CompactGauge { usages }, inner_cpu);

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
    use bzb_core::protocol::LeaseView;
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
            "(stale 3s) · 18 tokens · 9 held · ~3 in use · 6 free"
        );
    }

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

    #[test]
    fn leases_left_behind_by_a_dead_daemon_are_not_an_idle_pool() {
        assert!(matches!(absent(Ok(0)), Poll::Absent));

        let Poll::Crashed(reason) = absent(Ok(2)) else {
            panic!("a record of two leases is not an idle pool");
        };
        assert!(reason.contains('2'), "reason was {reason:?}");
    }

    #[test]
    fn an_unreadable_lease_record_is_reported_rather_than_ignored() {
        let Poll::Crashed(reason) = absent(Err(anyhow::anyhow!("leases.json is not JSON"))) else {
            panic!("an unreadable record is not an idle pool");
        };
        assert!(reason.contains("not JSON"), "reason was {reason:?}");
    }

    #[test]
    fn a_crashed_daemon_replaces_the_last_view_with_the_reason() {
        let start = Instant::now();
        let mut pool = Pool::default();
        pool.record(Poll::Reply(reply()), start);
        pool.record(
            Poll::Crashed("bzbd is not running, but 2 leases are recorded".into()),
            start + Duration::from_secs(1),
        );

        assert!(matches!(
            pool.view(start + Duration::from_secs(1)),
            PoolView::Unreachable(reason) if reason.contains("2 leases")
        ));
    }

    #[test]
    fn a_pool_polled_but_not_yet_answered_is_not_reported_as_idle() {
        let pool = Pool::default();

        assert!(matches!(pool.view(Instant::now()), PoolView::Pending));
    }

    #[test]
    fn a_queue_longer_than_the_terminal_leaves_the_cpu_gauges_on_screen() {
        let leases: Vec<LeaseView> = (1..=20)
            .map(|id| LeaseView {
                id,
                label: format!("lease {id}"),
                tool: "cargo".into(),
                class: "static".into(),
                cores: 1,
                state: "queued".into(),
                elapsed_ms: 1_000,
                ahead: Some(id as usize),
                pueue_task_id: None,
            })
            .collect();
        let reply = StatusReply { leases, ..reply() };

        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(102, 23)).expect("test terminal");
        draw(
            &mut terminal,
            &[42; 8],
            &PoolView::Known {
                reply: &reply,
                stale: None,
            },
        )
        .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let screen: Vec<String> = (0..23)
            .map(|y| (0..102).map(|x| buffer.get(x, y).symbol()).collect())
            .collect();
        assert!(
            screen.iter().any(|line| line.contains("42")),
            "no CPU gauge was drawn: {screen:?}"
        );
        assert!(
            screen.iter().any(|line| line.contains("more")),
            "no overflow row was drawn: {screen:?}"
        );
    }

    #[test]
    fn a_failure_before_any_reply_is_reported_as_unreachable() {
        let now = Instant::now();
        let mut pool = Pool::default();
        pool.record(Poll::Failed("connection reset".into()), now);

        assert!(matches!(pool.view(now), PoolView::Unreachable(reason)
            if reason == "connection reset"));
    }
}
