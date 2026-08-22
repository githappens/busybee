use std::time::Duration;

use bzb_core::status::{QueueSnapshot, TaskView};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

pub struct StatusPanel<'a> {
    pub snapshot: &'a QueueSnapshot,
    pub elapsed: Duration,
}

/// Marquee cadence. 125 ms/cell ≈ 8 cells per second.
const MARQUEE_STEP_MS: u128 = 125;

/// Separator inserted between primary-string repeats during marquee.
const MARQUEE_SEPARATOR: &str = "   ·   ";

impl<'a> Widget for StatusPanel<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let (left, primary, right) = line_parts(self.snapshot);
        let line = compose_line(
            &left,
            primary.as_deref(),
            &right,
            area.width as usize,
            self.elapsed,
        );
        write_line(buf, area, 0, &line, Style::default());
    }
}

/// Build the three line segments: the fixed left prefix (`Running: ` / `Idle`),
/// the variable primary identifier (`None` when idle), and the fixed right
/// suffix (`  ·  In Queue: N`, or empty when idle+empty).
fn line_parts(snapshot: &QueueSnapshot) -> (String, Option<String>, String) {
    let remaining = snapshot.queued.len();
    match snapshot.running.as_ref() {
        Some(task) => {
            let primary = primary_for(task);
            let right = format!("  ·  In Queue: {remaining}");
            ("Running: ".to_string(), Some(primary), right)
        }
        None => {
            let left = "Idle".to_string();
            let right = if remaining == 0 {
                String::new()
            } else {
                format!("  ·  In Queue: {remaining}")
            };
            (left, None, right)
        }
    }
}

fn primary_for(task: &TaskView) -> String {
    if let Some(label) = task.label.as_deref().filter(|s| !s.is_empty()) {
        return label.to_string();
    }
    match task.path.file_name().and_then(|n| n.to_str()) {
        Some(folder) if !folder.is_empty() => format!("{folder}: {}", task.command),
        _ => task.command.clone(),
    }
}

/// Compose a full-width line. If there is no primary (idle), just `left + right`
/// padded to `width`. Otherwise fit `primary` into the middle, marquee-scrolling
/// when it doesn't fit.
fn compose_line(
    left: &str,
    primary: Option<&str>,
    right: &str,
    width: usize,
    elapsed: Duration,
) -> String {
    let left_len = left.chars().count();
    let right_len = right.chars().count();

    let Some(primary) = primary else {
        let content_len = left_len + right_len;
        let pad = width.saturating_sub(content_len);
        return format!("{left}{}{right}", " ".repeat(pad));
    };

    let budget = width.saturating_sub(left_len).saturating_sub(right_len);
    let primary_len = primary.chars().count();

    // Degenerate — not enough room to marquee. Fall back to …-truncation.
    if budget <= 3 {
        let truncated = truncate(primary, budget);
        let pad = budget.saturating_sub(truncated.chars().count());
        return format!("{left}{truncated}{}{right}", " ".repeat(pad));
    }

    // Primary fits in full — static.
    if primary_len <= budget {
        let pad = budget - primary_len;
        return format!("{left}{primary}{}{right}", " ".repeat(pad));
    }

    // Primary overflows — marquee the middle region.
    let offset = (elapsed.as_millis() / MARQUEE_STEP_MS) as usize;
    let window = marquee_window(primary, budget, offset);
    format!("{left}{window}{right}")
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.into()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub(super) fn marquee_window(primary: &str, width: usize, offset: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cycle_len = primary.chars().count() + MARQUEE_SEPARATOR.chars().count();
    if cycle_len == 0 {
        return String::new();
    }
    let start = offset % cycle_len;
    primary
        .chars()
        .chain(MARQUEE_SEPARATOR.chars())
        .cycle()
        .skip(start)
        .take(width)
        .collect()
}

fn write_line(buf: &mut Buffer, area: Rect, row: u16, text: &str, style: Style) {
    let y = area.y + row;
    for (i, ch) in text.chars().enumerate() {
        let x = area.x + i as u16;
        if x >= area.x + area.width {
            break;
        }
        let cell = buf.get_mut(x, y);
        cell.set_char(ch);
        cell.set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzb_core::status::{QueueSnapshot, TaskView};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::path::PathBuf;
    use std::time::Duration;

    fn buf_lines(buf: &Buffer, area: Rect) -> Vec<String> {
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.get(area.x + x, area.y + y).symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn render(snap: &QueueSnapshot, width: u16, elapsed: Duration) -> String {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        StatusPanel {
            snapshot: snap,
            elapsed,
        }
        .render(area, &mut buf);
        buf_lines(&buf, area).remove(0)
    }

    fn empty_snap() -> QueueSnapshot {
        QueueSnapshot {
            running: None,
            queued: vec![],
        }
    }

    fn running_snap(label: Option<&str>, command: &str, path: &str) -> QueueSnapshot {
        QueueSnapshot {
            running: Some(TaskView {
                id: 1,
                label: label.map(|s| s.into()),
                command: command.into(),
                path: PathBuf::from(path),
            }),
            queued: vec![
                TaskView {
                    id: 2,
                    label: None,
                    command: "next".into(),
                    path: PathBuf::from("/"),
                },
                TaskView {
                    id: 3,
                    label: None,
                    command: "next2".into(),
                    path: PathBuf::from("/"),
                },
            ],
        }
    }

    #[test]
    fn idle_empty_queue_shows_only_idle() {
        let line = render(&empty_snap(), 40, Duration::ZERO);
        assert!(line.starts_with("Idle"));
        assert!(!line.contains("In Queue"));
    }

    #[test]
    fn idle_with_queue_shows_count() {
        let snap = QueueSnapshot {
            running: None,
            queued: (0..3)
                .map(|i| TaskView {
                    id: i,
                    label: None,
                    command: "x".into(),
                    path: PathBuf::from("/"),
                })
                .collect(),
        };
        let line = render(&snap, 40, Duration::ZERO);
        assert!(line.starts_with("Idle"));
        assert!(line.contains("In Queue: 3"));
    }

    #[test]
    fn running_with_label_uses_label_as_primary() {
        let snap = running_snap(Some("my build"), "cmake --build", "/Users/bob/work/busybee");
        let line = render(&snap, 60, Duration::ZERO);
        assert!(line.starts_with("Running: my build"));
        assert!(!line.contains("cmake --build"));
        assert!(line.contains("In Queue: 2"));
    }

    #[test]
    fn running_without_label_uses_folder_and_command() {
        let snap = running_snap(None, "cmake --build", "/Users/bob/work/busybee");
        let line = render(&snap, 60, Duration::ZERO);
        assert!(line.starts_with("Running: busybee: cmake --build"));
        assert!(line.contains("In Queue: 2"));
    }

    #[test]
    fn empty_label_falls_back_to_folder_and_command() {
        let snap = running_snap(Some(""), "cmake --build", "/Users/bob/work/busybee");
        let line = render(&snap, 60, Duration::ZERO);
        assert!(line.contains("busybee: cmake --build"));
    }

    #[test]
    fn primary_fits_no_marquee_motion() {
        // At width 60 with short command, primary fits — line should be stable across time.
        let snap = running_snap(None, "ls", "/work/busybee");
        let a = render(&snap, 60, Duration::ZERO);
        let b = render(&snap, 60, Duration::from_millis(500));
        assert_eq!(a, b);
    }

    #[test]
    fn primary_overflow_marquees_over_time() {
        let long_cmd = "cargo test --workspace --all-features -- --nocapture";
        let snap = running_snap(None, long_cmd, "/work/busybee");
        // Narrow enough that the primary cannot fit but budget > 3.
        let width = 40;
        let a = render(&snap, width, Duration::ZERO);
        // One marquee step is 125 ms — advance two steps to be safe against off-by-one.
        let b = render(&snap, width, Duration::from_millis(250));
        assert_ne!(a, b, "line should have shifted after two marquee steps");
        assert!(a.contains("In Queue: 2"), "right-fixed stays visible");
        assert!(b.contains("In Queue: 2"));
    }

    #[test]
    fn narrow_width_falls_back_to_ellipsis() {
        let long_cmd = "cargo test --workspace";
        let snap = running_snap(None, long_cmd, "/work/busybee");
        // Tight width: "Running: " (9) + "  ·  In Queue: 2" (16) = 25.
        // Width 28 leaves budget 3 → ellipsis fallback.
        let line = render(&snap, 28, Duration::ZERO);
        assert!(line.contains("…"));
        assert!(line.contains("In Queue: 2"));
    }

    #[test]
    fn path_without_file_name_falls_back_to_command_only() {
        let snap = running_snap(None, "ls", "/");
        let line = render(&snap, 40, Duration::ZERO);
        // Root path has no file_name — primary is just the command.
        assert!(line.starts_with("Running: ls"));
        // No <folder>: ls pattern — check the remainder after "Running: " has no folder prefix.
        let remainder = &line["Running: ".len()..];
        assert!(
            !remainder.contains(": ls"),
            "no folder prefix when path is root"
        );
    }

    #[test]
    fn marquee_at_offset_zero_starts_with_primary() {
        let out = marquee_window("abcdef", 4, 0);
        assert_eq!(out, "abcd");
    }

    #[test]
    fn marquee_shifts_one_per_unit_offset() {
        let out = marquee_window("abcdef", 4, 1);
        assert_eq!(out, "bcde");
    }

    #[test]
    fn marquee_wraps_through_separator_and_back_to_start() {
        // primary is 6 chars, separator is "   ·   " (7 chars), cycle is 13.
        let full = marquee_window("abcdef", 13, 0);
        assert_eq!(full, "abcdef   ·   ");
        // One cycle later we are back to offset 0.
        assert_eq!(marquee_window("abcdef", 4, 13), "abcd");
        // Offset just past the primary starts inside the separator.
        assert_eq!(marquee_window("abcdef", 4, 6), "   ·");
    }

    #[test]
    fn marquee_handles_multibyte_chars() {
        // Middle dot is one codepoint but multi-byte — must slice by chars.
        let out = marquee_window("α·β·γ", 3, 0);
        assert_eq!(out, "α·β");
    }

    #[test]
    fn truncate_returns_empty_at_zero_max() {
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("", 0), "");
    }
}
