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
            // Both of these are reports, not degraded renderings of a pool: say
            // which one it is and draw no bar, because there is no pool to draw.
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
    let scale = |n: u32| (n as u64 * cells).div_ceil(pool).min(cells);
    let held = scale(reply.held);
    let in_use = scale(approx_in_use(reply)).min(cells - held);
    (held, in_use, cells - held - in_use)
}

fn legend(reply: &StatusReply, stale: Option<Duration>) -> String {
    let mut legend = format!(
        "{} tokens · {} held · ~{} in use · {} free",
        reply.pool_size,
        reply.held,
        approx_in_use(reply),
        reply.free
    );
    // The numbers above are the last ones bzbd sent, and a poll has failed
    // since: say how old they are rather than let them read as current.
    if let Some(age) = stale {
        legend.push_str(&format!(" · (stale {}s)", age.as_secs()));
    }
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
            lines[1].trim_end().ends_with("(stale 3s)"),
            "legend was {:?}",
            lines[1]
        );
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
        }
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
