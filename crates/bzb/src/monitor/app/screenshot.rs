//! Regenerates the SVG behind `docs/images/monitor.png`.
//!
//! ```sh
//! cargo test -p bzb --lib -- --ignored screenshot
//! resvg --width 1033 --height 523 build/monitor.svg docs/images/monitor.png
//! ```

use bzb_core::protocol::{LeaseView, StatusReply};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Terminal;

use super::draw;
use crate::monitor::widgets::pool_gauge::PoolView;

/// The grid the screenshot is taken on.
const COLS: u16 = 102;
const ROWS: u16 = 23;

/// The PNG the README embeds, in pixels.
const CANVAS_W: f64 = 1033.0;
const CANVAS_H: f64 = 523.0;

/// One character cell, sized so the grid fills the frame.
const CELL_W: f64 = 9.7;
const CELL_H: f64 = 20.125;

/// Where the grid starts inside the window frame.
const ORIGIN_X: f64 = 22.0;
const ORIGIN_Y: f64 = 36.0;

const FONT_SIZE: f64 = 16.0;
const BACKGROUND: &str = "#1a1b26";
const FOREGROUND: &str = "#c0caf5";
const FRAME: &str = "#414868";

fn lease(id: u64, label: &str, tool: &str, class: &str, cores: u32, elapsed_ms: u64) -> LeaseView {
    LeaseView {
        id,
        label: label.into(),
        tool: tool.into(),
        class: class.into(),
        cores,
        state: "running".into(),
        elapsed_ms,
        ahead: None,
        pueue_task_id: Some(id as usize),
    }
}

#[test]
#[ignore = "writes build/monitor.svg, the source of docs/images/monitor.png"]
fn screenshot() {
    let mut queued = lease(44, "workspace tests", "cargo", "jobserver", 0, 9_000);
    queued.state = "queued".into();
    queued.ahead = Some(2);
    let reply = StatusReply {
        pool_size: 24,
        free: 8,
        held: 12,
        leases: vec![
            lease(41, "ui build", "xcodebuild", "static", 12, 132_000),
            lease(42, "renderer", "make", "jobserver", 4, 48_000),
            queued,
        ],
    };
    let usages: Vec<u8> = vec![
        94, 91, 88, 90, 12, 9, 74, 71, 68, 22, 17, 14, 9, 7, 6, 5, 4, 4, 3, 3, 2, 1, 1, 0,
    ];

    let mut terminal = Terminal::new(TestBackend::new(COLS, ROWS)).expect("terminal");
    draw(
        &mut terminal,
        &usages,
        &PoolView::Known {
            reply: &reply,
            stale: None,
        },
    )
    .expect("draw");

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../build/monitor.svg");
    std::fs::write(path, svg(terminal.backend().buffer())).expect("write the svg");
}

/// The buffer as one SVG: a window frame, then a `<text>` run per stretch of
/// same-coloured cells.
fn svg(buffer: &Buffer) -> String {
    let mut out = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{CANVAS_W}" height="{CANVAS_H}" viewBox="0 0 {CANVAS_W} {CANVAS_H}">
<rect width="{CANVAS_W}" height="{CANVAS_H}" fill="{BACKGROUND}"/>
<rect x="8" y="8" width="{}" height="{}" rx="10" fill="none" stroke="{FRAME}" stroke-width="2"/>
<rect x="26" y="4" width="180" height="18" fill="{BACKGROUND}"/>
<text x="32" y="21" font-family="Menlo, monospace" font-size="{FONT_SIZE}" font-weight="bold" fill="{FOREGROUND}">busybee monitor</text>
"#,
        CANVAS_W - 16.0,
        CANVAS_H - 16.0,
    );

    for y in 0..ROWS {
        let mut run = String::new();
        let mut run_start = 0u16;
        let mut run_colour = Color::Reset;
        for x in 0..COLS {
            let cell = buffer.get(x, y);
            if cell.fg != run_colour || run.is_empty() {
                out.push_str(&text_run(&run, run_start, y, run_colour));
                run.clear();
                run_start = x;
                run_colour = cell.fg;
            }
            run.push_str(cell.symbol());
        }
        out.push_str(&text_run(&run, run_start, y, run_colour));
    }
    out.push_str("</svg>\n");
    out
}

fn text_run(run: &str, col: u16, row: u16, colour: Color) -> String {
    if run.trim().is_empty() {
        return String::new();
    }
    format!(
        r#"<text x="{x:.2}" y="{y:.2}" font-family="Menlo, monospace" font-size="{FONT_SIZE}" fill="{fill}" textLength="{len:.2}" lengthAdjust="spacingAndGlyphs" xml:space="preserve">{text}</text>
"#,
        x = ORIGIN_X + col as f64 * CELL_W,
        y = ORIGIN_Y + row as f64 * CELL_H + FONT_SIZE * 0.8,
        fill = hex(colour),
        len = run.chars().count() as f64 * CELL_W,
        text = escape(run),
    )
}

/// The palette the screenshot is taken in.
fn hex(colour: Color) -> &'static str {
    match colour {
        Color::Green => "#9ece6a",
        Color::Yellow => "#e0af68",
        Color::Red => "#f7768e",
        Color::Cyan => "#7dcfff",
        Color::DarkGray => "#565f89",
        _ => FOREGROUND,
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
