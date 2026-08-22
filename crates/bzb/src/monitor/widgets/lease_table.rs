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

impl Widget for LeaseTable<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (running, queued): (Vec<_>, Vec<_>) = self
            .leases
            .iter()
            .partition(|lease| lease.state == "running");
        let rows: Vec<Row> = running.into_iter().chain(queued).map(row).collect();
        Table::new(rows, WIDTHS).render(area, buf);
    }
}

fn row(lease: &LeaseView) -> Row<'static> {
    let style = if lease.state == "running" {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Row::new(vec![
        format!("#{}", lease.id),
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

    #[test]
    fn no_leases_draws_nothing() {
        let lines = draw(&[], 40, 2);
        assert!(
            lines.iter().all(|line| line.trim().is_empty()),
            "rows were {lines:?}"
        );
    }
}
