//! One row per lease, running first, formatted by `busybee status`' helpers.

use bzb_core::protocol::LeaseView;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Row, Table, Widget};

use crate::status::{cores, elapsed, printable};

pub struct LeaseTable<'a> {
    pub leases: &'a [LeaseView],
}

/// Minimum widths; the label takes the rest.
const WIDTHS: [Constraint; 7] = [
    Constraint::Length(5),
    Constraint::Length(7),
    Constraint::Length(7),
    Constraint::Length(12),
    Constraint::Length(9),
    Constraint::Length(11),
    Constraint::Min(0),
];

/// Widens the unbounded columns (id, elapsed, tool, cores) to their content:
/// clipping any of them changes its meaning.
fn widths(leases: &[&LeaseView]) -> [Constraint; 7] {
    let mut widths = WIDTHS;
    widths[0] = fit(leases, 5, id);
    widths[2] = fit(leases, 7, |lease| elapsed(lease.elapsed_ms));
    widths[3] = fit(leases, 12, |lease| printable(&lease.tool));
    widths[5] = fit(leases, 11, cores);
    widths
}

/// Measured in terminal cells (`Span::width`), since a character can fill two.
fn fit(leases: &[&LeaseView], least: u16, cell: impl Fn(&LeaseView) -> String) -> Constraint {
    let widest = leases
        .iter()
        .map(|lease| Span::raw(cell(lease)).width() as u16)
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

        // The table would silently drop rows past the panel; count them instead.
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
        // Queued first, so the test does not depend on the daemon's order.
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
    fn a_lease_id_wider_than_the_column_widens_it() {
        let leases = vec![lease(10_000, "running", "static")];
        let lines = draw(&leases, 90, 1);
        assert!(lines[0].starts_with("#10000"), "row was {:?}", lines[0]);
        assert!(lines[0].contains("xcodebuild") || lines[0].contains("cargo"));
    }

    #[test]
    fn a_tool_of_wide_characters_is_measured_in_cells() {
        let mut leases = vec![lease(1, "running", "static")];
        leases[0].tool = "ビルドランナー".into();

        let lines = draw(&leases, 90, 1);

        // The second cell of a wide character reads back blank.
        assert!(
            lines[0].contains("ビ ル ド ラ ン ナ ー"),
            "row was {:?}",
            lines[0]
        );
        assert!(lines[0].contains("static"), "row was {:?}", lines[0]);
    }

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

    #[test]
    fn an_elapsed_time_wider_than_the_column_widens_it() {
        let mut leases = vec![lease(1, "running", "static")];
        leases[0].elapsed_ms = 60_000_000;

        let lines = draw(&leases, 90, 1);

        assert!(lines[0].contains("1000m00s"), "row was {:?}", lines[0]);
    }

    #[test]
    fn a_token_count_wider_than_the_column_widens_it() {
        let mut leases = vec![lease(1, "running", "static")];
        leases[0].cores = 4096;

        let lines = draw(&leases, 90, 1);

        assert!(lines[0].contains("holding 4096"), "row was {:?}", lines[0]);
        assert!(lines[0].contains("lease 1"), "row was {:?}", lines[0]);
    }

    #[test]
    fn leases_that_do_not_fit_are_counted_in_a_final_row() {
        let leases: Vec<LeaseView> = (1..=20).map(|id| lease(id, "queued", "static")).collect();

        let lines = draw(&leases, 90, 4);

        assert!(lines[0].contains("#1 "), "rows were {lines:?}");
        assert!(lines[2].contains("#3 "), "rows were {lines:?}");
        assert!(lines[3].contains("17 more"), "rows were {lines:?}");
    }

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
