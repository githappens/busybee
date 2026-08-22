//! One row per lease bzbd is tracking, running rows first.
//!
//! The columns are `busybee status`' columns (`docs/design/bzbd.md`
//! §Observability) and are formatted by the same helpers, so the table and the
//! one-shot command cannot drift apart.

use bzb_core::protocol::LeaseView;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Row, Table, Widget};

use crate::status::{cores, elapsed, printable};

pub struct LeaseTable<'a> {
    pub leases: &'a [LeaseView],
}

/// Enough for what the columns hold: `running`, `12m34s`, a tool basename, a
/// class name, `holding 12`. The label takes what is left of the width and is
/// clipped to it.
const WIDTHS: [Constraint; 7] = [
    Constraint::Length(5),
    Constraint::Length(7),
    Constraint::Length(7),
    Constraint::Length(12),
    Constraint::Length(9),
    Constraint::Length(11),
    Constraint::Min(0),
];

/// `WIDTHS` with the three columns whose content the monitor does not bound
/// widened to what is actually on screen: bzbd's lease counter climbs for as
/// long as the daemon lives, a lease runs for as long as its command does, and
/// the tool is the basename of whatever the caller wrapped. A clipped id is one
/// the operator would pass to `busybee cancel` wrong, a clipped duration is a
/// different duration rather than a shorter one, and a clipped tool no longer
/// names what is holding the pool.
fn widths(leases: &[&LeaseView]) -> [Constraint; 7] {
    let mut widths = WIDTHS;
    widths[0] = fit(leases, 5, id);
    widths[2] = fit(leases, 7, |lease| elapsed(lease.elapsed_ms));
    widths[3] = fit(leases, 12, |lease| printable(&lease.tool));
    widths
}

fn fit(leases: &[&LeaseView], least: u16, cell: impl Fn(&LeaseView) -> String) -> Constraint {
    let widest = leases
        .iter()
        .map(|lease| cell(lease).chars().count() as u16)
        .max()
        .unwrap_or(0);
    Constraint::Length(widest.max(least))
}

fn id(lease: &LeaseView) -> String {
    format!("#{}", lease.id)
}

impl Widget for LeaseTable<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let (running, queued): (Vec<_>, Vec<_>) = self
            .leases
            .iter()
            .partition(|lease| lease.state == "running");
        let ordered: Vec<&LeaseView> = running.into_iter().chain(queued).collect();

        // The queue is not bounded by the pool size, so it can hold more leases
        // than the panel has rows. The table would drop the ones past the last
        // row without a word; the last row instead says how many it stands for.
        let overflows = ordered.len() > area.height as usize;
        let shown = if overflows {
            area.height as usize - 1
        } else {
            ordered.len()
        };
        let rows: Vec<Row> = ordered[..shown].iter().copied().map(row).collect();
        Table::new(rows, widths(&ordered[..shown])).render(
            Rect {
                height: shown as u16,
                ..area
            },
            buf,
        );
        if overflows {
            buf.set_stringn(
                area.x,
                area.y + area.height - 1,
                format!("… {} more", ordered.len() - shown),
                area.width as usize,
                Style::default().fg(Color::DarkGray),
            );
        }
    }
}

fn row(lease: &LeaseView) -> Row<'static> {
    let style = if lease.state == "running" {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Row::new(vec![
        id(lease),
        lease.state.clone(),
        elapsed(lease.elapsed_ms),
        printable(&lease.tool),
        lease.class.clone(),
        cores(lease),
        printable(&lease.label),
    ])
    .style(style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::widgets::tests::render;

    fn lease(id: u64, state: &str, class: &str) -> LeaseView {
        LeaseView {
            id,
            label: format!("lease {id}"),
            tool: "cargo".into(),
            class: class.into(),
            cores: 9,
            state: state.into(),
            elapsed_ms: 132_000,
            ahead: (state == "queued").then_some(2),
            pueue_task_id: (state == "running").then_some(3),
        }
    }

    fn three() -> Vec<LeaseView> {
        let mut queued = lease(3, "queued", "jobserver");
        queued.label = "waiting build".into();
        let mut jobserver = lease(2, "running", "jobserver");
        jobserver.tool = "make".into();
        let mut xcode = lease(1, "running", "static");
        xcode.tool = "xcodebuild".into();
        // The daemon sends the queued lease last; the widget must not depend on
        // that, so hand it over first.
        vec![queued, jobserver, xcode]
    }

    fn draw(leases: &[LeaseView], width: u16, height: u16) -> Vec<String> {
        render(width, height, |area, buf| {
            LeaseTable { leases }.render(area, buf)
        })
    }

    #[test]
    fn a_static_a_jobserver_and_a_queued_lease_each_get_a_row() {
        let lines = draw(&three(), 90, 3);

        assert!(lines[0].contains("#2"), "rows were {lines:?}");
        assert!(lines[0].contains("make"), "rows were {lines:?}");
        assert!(lines[0].contains("using ~9"), "rows were {lines:?}");
        assert!(lines[1].contains("#1"), "rows were {lines:?}");
        assert!(lines[1].contains("xcodebuild"), "rows were {lines:?}");
        assert!(lines[1].contains("holding 9"), "rows were {lines:?}");
        assert!(lines[2].contains("#3"), "rows were {lines:?}");
        assert!(lines[2].contains("2 ahead"), "rows were {lines:?}");
        assert!(lines[2].contains("waiting build"), "rows were {lines:?}");
    }

    #[test]
    fn running_leases_are_listed_before_queued_ones() {
        let lines = draw(&three(), 90, 3);
        let states: Vec<&str> = lines
            .iter()
            .map(|line| {
                if line.contains("running") {
                    "running"
                } else {
                    "queued"
                }
            })
            .collect();
        assert_eq!(states, ["running", "running", "queued"]);
    }

    #[test]
    fn every_row_carries_the_elapsed_time() {
        let lines = draw(&three(), 90, 3);
        assert!(
            lines.iter().all(|line| line.contains("2m12s")),
            "rows were {lines:?}"
        );
    }

    #[test]
    fn a_narrow_terminal_truncates_the_label_without_panicking() {
        let lines = draw(&three(), 30, 3);

        assert!(lines.iter().all(|line| line.chars().count() == 30));
        assert!(!lines[0].contains("waiting build"), "rows were {lines:?}");
        assert!(lines[0].contains("#2"), "rows were {lines:?}");
    }

    /// A label is the caller's `--name` and a tool is the basename of whatever
    /// they wrapped, so either can carry an escape sequence. Cells are one row
    /// of a redrawn TUI; a raw escape in one would rewrite the rest of it.
    #[test]
    fn a_control_character_in_a_label_is_shown_as_its_escape() {
        let mut leases = vec![lease(1, "running", "static")];
        leases[0].label = "build\u{1b}[2K".into();

        let lines = draw(&leases, 90, 1);

        assert!(!lines[0].contains('\u{1b}'), "row was {:?}", lines[0]);
        assert!(
            lines[0].contains(r"build\u{1b}[2K"),
            "row was {:?}",
            lines[0]
        );
    }

    /// The id column is what `busybee cancel <id>` takes, and bzbd's counter
    /// keeps going up for as long as the daemon lives: a clipped `#10000` is
    /// an id the operator would type wrong.
    #[test]
    fn a_lease_id_wider_than_the_column_widens_it() {
        let leases = vec![lease(10_000, "running", "static")];
        let lines = draw(&leases, 90, 1);
        assert!(lines[0].starts_with("#10000"), "row was {:?}", lines[0]);
        assert!(lines[0].contains("xcodebuild") || lines[0].contains("cargo"));
    }

    /// The tool is the basename of whatever the caller wrapped, so it is as
    /// long as they made it, and an explicit `--name` means the label does not
    /// carry it either: clipped, the row no longer says what is running.
    #[test]
    fn a_tool_wider_than_the_column_widens_it() {
        let mut leases = vec![lease(1, "running", "static")];
        leases[0].tool = "custom-build-runner".into();

        let lines = draw(&leases, 90, 1);

        assert!(
            lines[0].contains("custom-build-runner"),
            "row was {:?}",
            lines[0]
        );
        assert!(lines[0].contains("lease 1"), "row was {:?}", lines[0]);
    }

    /// A lease can run for hours, and the elapsed time is the column the
    /// observability contract names: `1000m00s` clipped to `1000m0` is a
    /// different duration, not a shorter one.
    #[test]
    fn an_elapsed_time_wider_than_the_column_widens_it() {
        let mut leases = vec![lease(1, "running", "static")];
        leases[0].elapsed_ms = 60_000_000;

        let lines = draw(&leases, 90, 1);

        assert!(lines[0].contains("1000m00s"), "row was {:?}", lines[0]);
    }

    /// The queue is not bounded by the pool, so it can be longer than the panel
    /// is tall. Rows that do not fit are dropped by the table itself; saying how
    /// many keeps the count on screen honest.
    #[test]
    fn leases_that_do_not_fit_are_counted_in_a_final_row() {
        let leases: Vec<LeaseView> = (1..=20).map(|id| lease(id, "queued", "static")).collect();

        let lines = draw(&leases, 90, 4);

        assert!(lines[0].contains("#1 "), "rows were {lines:?}");
        assert!(lines[2].contains("#3 "), "rows were {lines:?}");
        assert!(lines[3].contains("17 more"), "rows were {lines:?}");
    }

    /// Every lease fits, so there is nothing to say.
    #[test]
    fn leases_that_all_fit_get_no_overflow_row() {
        let lines = draw(&three(), 90, 5);
        assert!(
            !lines.iter().any(|line| line.contains("more")),
            "rows were {lines:?}"
        );
    }

    #[test]
    fn no_leases_draws_nothing() {
        let lines = draw(&[], 40, 2);
        assert!(
            lines.iter().all(|line| line.trim().is_empty()),
            "rows were {lines:?}"
        );
    }
}
