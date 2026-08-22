//! The token pool as one horizontal bar plus a legend.
//!
//! `docs/design/bzbd.md` §Observability: the monitor shows the same data
//! `busybee status` prints. The in-use figure is `pool − free − held`, an
//! estimate the daemon never schedules on, so it is written `~N` here too.

use std::time::Duration;

use bzb_core::protocol::StatusReply;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// What the monitor knows about the pool.
pub enum PoolView<'a> {
    /// The daemon's most recent reply. `stale` is how old that reply is once a
    /// later poll has failed to replace it, and `None` while it is current.
    Known {
        reply: &'a StatusReply,
        stale: Option<Duration>,
    },
    /// A poll is out and none has come back yet, so nothing is known about the
    /// pool — including whether there is one.
    Pending,
    /// Nothing is listening on bzbd's socket: no daemon, so no pool.
    Absent,
    /// A daemon is there and is not answering, and none has answered yet — so
    /// there is no last-known pool to show instead.
    Unreachable(&'a str),
}

pub struct PoolGauge<'a> {
    pub view: &'a PoolView<'a>,
}

/// A token a static lease drained: it is out of the pool until that lease ends.
const HELD: char = '█';
/// A token the jobserver tasks are estimated to be holding right now.
const IN_USE: char = '▓';
/// A token in the fifo, there for the taking.
const FREE: char = '░';

impl Widget for PoolGauge<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
        let (reply, stale) = match self.view {
            PoolView::Known { reply, stale } => (*reply, *stale),
            // None of these are degraded renderings of a pool: say which one it
            // is and draw no bar, because there is no pool to draw.
            PoolView::Pending => {
                Paragraph::new("pool unknown (waiting for bzbd)").render(rows[0], buf);
                return;
            }
            PoolView::Absent => {
                Paragraph::new("pool idle (daemon not running)").render(rows[0], buf);
                return;
            }
            PoolView::Unreachable(reason) => {
                Paragraph::new(Line::styled(
                    format!("pool unknown: {reason}"),
                    Style::default().fg(Color::Red),
                ))
                .render(rows[0], buf);
                return;
            }
        };

        let (held, in_use, free) = segments(reply, rows[0].width as u64);
        Paragraph::new(Line::from(vec![
            bar(HELD, held, Color::Cyan),
            bar(IN_USE, in_use, Color::Yellow),
            bar(FREE, free, Color::DarkGray),
        ]))
        .render(rows[0], buf);
        Paragraph::new(legend(reply, stale)).render(rows[1], buf);
    }
}

fn bar(cell: char, count: u64, colour: Color) -> Span<'static> {
    Span::styled(
        cell.to_string().repeat(count as usize),
        Style::default().fg(colour),
    )
}

/// Tokens neither free nor held by a static lease, so approximately what the
/// jobserver tasks are using. Clamped at 0 for the same reason `busybee status`
/// clamps it: the pool and the fifo are sampled separately, so a sum over the
/// boundary is drift, not a negative count.
fn approx_in_use(reply: &StatusReply) -> u32 {
    reply
        .pool_size
        .saturating_sub(reply.free)
        .saturating_sub(reply.held)
}

/// How many cells of a `width`-wide bar each segment gets: held, in use, free.
///
/// One cell is one token while the bar fits, and the pool is scaled into the
/// width when it does not — a bar cut off at the edge would hide exactly the
/// free tokens the operator is looking for.
fn segments(reply: &StatusReply, width: u64) -> (u64, u64, u64) {
    let pool = reply.pool_size as u64;
    if pool == 0 || width == 0 {
        return (0, 0, 0);
    }
    let cells = pool.min(width);
    // Segments are cut at scaled cumulative boundaries, rounded down, so a
    // segment can never round itself into the next one's cells.
    let boundary = |tokens: u64| (tokens * cells / pool).min(cells);
    let held = reply.held as u64;
    let in_use = approx_in_use(reply) as u64;
    let held_end = boundary(held);
    let in_use_end = boundary(held + in_use);
    let mut bar = [held_end, in_use_end - held_end, cells - in_use_end];

    // Rounding down still scales a small segment away, and each of the three
    // is the one an operator might be reading the bar for: held capacity a
    // static lease took, work in flight, tokens there for the taking. Give
    // every segment that holds tokens a cell, borrowed from the widest one,
    // as long as the bar is wide enough to spare it.
    for (i, tokens) in [held, in_use, reply.free as u64].into_iter().enumerate() {
        if tokens == 0 || bar[i] > 0 {
            continue;
        }
        let widest = (0..3).max_by_key(|&j| bar[j]).expect("three segments");
        if bar[widest] > 1 {
            bar[widest] -= 1;
            bar[i] = 1;
        }
    }
    (bar[0], bar[1], bar[2])
}

fn legend(reply: &StatusReply, stale: Option<Duration>) -> String {
    // The counts are the last ones bzbd sent, and a poll has failed since: say
    // how old they are rather than let them read as current. The marker leads
    // because the counts are variable-length and the line is clipped from the
    // right, so a trailing marker is the first thing a narrow terminal drops.
    let mut legend = match stale {
        Some(age) => format!("(stale {}s) · ", age.as_secs()),
        None => String::new(),
    };
    legend.push_str(&format!(
        "{} tokens · {} held · ~{} in use · {} free",
        reply.pool_size,
        reply.held,
        approx_in_use(reply),
        reply.free
    ));
    legend
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::widgets::tests::render;

    fn idle() -> StatusReply {
        StatusReply {
            pool_size: 18,
            free: 18,
            held: 0,
            leases: vec![],
        }
    }

    fn busy() -> StatusReply {
        StatusReply {
            pool_size: 18,
            free: 6,
            held: 9,
            leases: vec![],
        }
    }

    fn draw(view: &PoolView, width: u16) -> Vec<String> {
        render(width, 2, |area, buf| PoolGauge { view }.render(area, buf))
    }

    #[test]
    fn an_idle_pool_is_all_free_cells() {
        let reply = idle();
        let lines = draw(
            &PoolView::Known {
                reply: &reply,
                stale: None,
            },
            60,
        );

        assert_eq!(lines[0].trim_end(), FREE.to_string().repeat(18));
        assert_eq!(
            lines[1].trim_end(),
            "18 tokens · 0 held · ~0 in use · 18 free"
        );
    }

    #[test]
    fn held_in_use_and_free_share_the_bar() {
        let reply = busy();
        let lines = draw(
            &PoolView::Known {
                reply: &reply,
                stale: None,
            },
            60,
        );

        let expected = format!(
            "{}{}{}",
            HELD.to_string().repeat(9),
            IN_USE.to_string().repeat(3),
            FREE.to_string().repeat(6)
        );
        assert_eq!(lines[0].trim_end(), expected);
        assert_eq!(
            lines[1].trim_end(),
            "18 tokens · 9 held · ~3 in use · 6 free"
        );
    }

    /// The estimate is an estimate wherever it appears.
    #[test]
    fn the_in_use_figure_is_marked_approximate() {
        let reply = busy();
        let lines = draw(
            &PoolView::Known {
                reply: &reply,
                stale: None,
            },
            60,
        );
        assert!(lines[1].contains("~3 in use"), "legend was {:?}", lines[1]);
    }

    /// A bar wider than the terminal would be cut off at the right edge, which
    /// is where the free tokens are.
    #[test]
    fn a_pool_wider_than_the_terminal_is_scaled_into_it() {
        let reply = busy();
        let lines = draw(
            &PoolView::Known {
                reply: &reply,
                stale: None,
            },
            9,
        );

        let bar = lines[0].trim_end();
        assert_eq!(bar.chars().count(), 9, "bar was {bar:?}");
        assert!(bar.contains(FREE), "bar was {bar:?}");
        assert!(bar.contains(HELD), "bar was {bar:?}");
    }

    #[test]
    fn a_stale_view_says_how_old_it_is() {
        let reply = busy();
        let lines = draw(
            &PoolView::Known {
                reply: &reply,
                stale: Some(Duration::from_secs(3)),
            },
            60,
        );
        assert!(
            lines[1].trim_end().starts_with("(stale 3s)"),
            "legend was {:?}",
            lines[1]
        );
    }

    /// The counts are variable-length, so a marker after them is the first
    /// thing a narrow terminal clips — and clipping it turns a degraded poll
    /// into numbers that read as current.
    #[test]
    fn a_narrow_terminal_clips_the_counts_before_the_stale_marker() {
        let reply = busy();
        let lines = draw(
            &PoolView::Known {
                reply: &reply,
                stale: Some(Duration::from_secs(3)),
            },
            12,
        );
        assert!(lines[1].contains("stale 3s"), "legend was {:?}", lines[1]);
    }

    /// Before the first poll comes back the monitor has not asked anyone
    /// anything yet, and "idle" would be a claim about a pool it has not looked
    /// at.
    #[test]
    fn a_pool_that_has_not_answered_yet_is_not_reported_as_idle() {
        let lines = draw(&PoolView::Pending, 60);
        assert_eq!(lines[0].trim_end(), "pool unknown (waiting for bzbd)");
        assert_eq!(lines[1].trim_end(), "");
    }

    #[test]
    fn no_daemon_is_reported_as_an_idle_pool_with_the_reason() {
        let lines = draw(&PoolView::Absent, 60);
        assert_eq!(lines[0].trim_end(), "pool idle (daemon not running)");
        assert_eq!(lines[1].trim_end(), "");
    }

    /// A daemon that is there and silent is not an idle pool, and saying so
    /// would be the silent fallback the design rules out.
    #[test]
    fn a_daemon_that_does_not_answer_is_not_reported_as_idle() {
        let lines = draw(&PoolView::Unreachable("connection reset"), 60);
        assert!(
            lines[0].starts_with("pool unknown:"),
            "line was {:?}",
            lines[0]
        );
        assert!(
            lines[0].contains("connection reset"),
            "line was {:?}",
            lines[0]
        );
    }

    #[test]
    fn a_very_narrow_terminal_renders_without_panicking() {
        let reply = busy();
        for width in 1..=6 {
            draw(
                &PoolView::Known {
                    reply: &reply,
                    stale: None,
                },
                width,
            );
            draw(&PoolView::Absent, width);
            draw(&PoolView::Pending, width);
        }
    }

    /// Scaling the pool into a narrower bar must not round the free tokens
    /// away: a bar that says the machine is full when six tokens are there for
    /// the taking is the one reading the operator acts on.
    #[test]
    fn scaling_never_rounds_a_free_segment_to_nothing() {
        let reply = StatusReply {
            pool_size: 18,
            free: 2,
            held: 8,
            leases: vec![],
        };
        let (held, in_use, free) = segments(&reply, 10);
        assert_eq!(held + in_use + free, 10, "bar was {held}/{in_use}/{free}");
        assert!(free > 0, "bar was {held}/{in_use}/{free}");
    }

    /// The same holds for a single held token: a static lease keeping capacity
    /// out of the pool is the reason the machine feels slow, and a bar that
    /// scales it away leaves nothing on screen that says so.
    #[test]
    fn scaling_never_rounds_a_held_segment_to_nothing() {
        let reply = StatusReply {
            pool_size: 18,
            free: 1,
            held: 1,
            leases: vec![],
        };
        let (held, in_use, free) = segments(&reply, 10);
        assert_eq!(held + in_use + free, 10, "bar was {held}/{in_use}/{free}");
        assert!(held > 0, "bar was {held}/{in_use}/{free}");
        assert!(free > 0, "bar was {held}/{in_use}/{free}");
    }

    /// bzbd reports the fifo and the leases from separate samples, so the two
    /// can cross; a wrapped subtraction would ask for four billion cells.
    #[test]
    fn drifted_counts_do_not_overflow_the_bar() {
        let drifted = StatusReply {
            pool_size: 8,
            free: 8,
            held: 4,
            leases: vec![],
        };
        assert_eq!(segments(&drifted, 80), (4, 0, 4));
    }
}
