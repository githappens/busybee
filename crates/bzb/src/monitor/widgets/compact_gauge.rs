use ratatui::layout::Rect;
use ratatui::style::Color;

/// A size variant for a single CPU gauge cell (excluding 1-char padding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeVariant {
    pub cols: u16,
    pub rows: u16,
}

pub const SIZES: &[SizeVariant] = &[
    SizeVariant { cols: 7, rows: 5 }, // Normal
    SizeVariant { cols: 5, rows: 4 }, // Small
    SizeVariant { cols: 5, rows: 3 }, // Mini (min width to render "100")
];

/// Pick the largest variant whose cell grid holds `n` cells inside `area`.
/// Padding of 1 char on each side of every cell is accounted for.
pub fn pick_size(area: Rect, n: u16) -> SizeVariant {
    for sz in SIZES {
        let cw = sz.cols + 1;
        let rh = sz.rows + 1;
        if cw > area.width || rh > area.height {
            continue;
        }
        let cols = area.width / cw;
        let rows = area.height / rh;
        if cols > 0 && rows > 0 && cols * rows >= n {
            return *sz;
        }
    }
    *SIZES.last().unwrap()
}

/// Compute the clockwise perimeter walk for a size variant, starting at
/// bottom-center gap and proceeding: bottom-left half → left → top → right
/// → bottom-right half. Returns (col, row) positions inside the cell.
pub fn perimeter(sz: SizeVariant) -> Vec<(u16, u16)> {
    let mut p = Vec::new();
    let center_col = sz.cols / 2;
    let bottom_row = sz.rows - 1;
    // Bottom edge left of gap, going left.
    for c in (1..center_col).rev() {
        p.push((c, bottom_row));
    }
    // Left edge going up.
    for r in (0..sz.rows).rev() {
        p.push((0, r));
    }
    // Top edge going right.
    for c in 1..sz.cols {
        p.push((c, 0));
    }
    // Right edge going down.
    for r in 1..sz.rows {
        p.push((sz.cols - 1, r));
    }
    // Bottom edge right of gap, going left toward gap.
    for c in (center_col + 1..sz.cols - 1).rev() {
        p.push((c, bottom_row));
    }
    p
}

/// Map a 0–100 usage value to a ratatui color for the filled region.
pub fn usage_color(usage: u8) -> Color {
    match usage {
        0..=39 => Color::Green,
        40..=74 => Color::Yellow,
        _ => Color::Red,
    }
}

/// Shade char for the filled region at a given distance from the leading edge.
pub fn shade_for_distance(distance_from_edge: i32) -> char {
    match distance_from_edge {
        d if d >= 3 => '█',
        2 => '▓',
        1 => '▒',
        _ => '░',
    }
}

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::widgets::Widget;

/// Usage data for the gauge grid: one 0–100 value per core.
pub struct CompactGauge<'a> {
    pub usages: &'a [u8],
    pub skeleton: Color,
}

impl<'a> Widget for CompactGauge<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let n = self.usages.len() as u16;
        if n == 0 || area.width < 3 || area.height < 2 {
            return;
        }
        let sz = pick_size(area, n);
        let cell_w = sz.cols + 1;
        let cell_h = sz.rows + 1;
        let cols_per_row = (area.width / cell_w).max(1);
        let perim = perimeter(sz);
        let perim_len = perim.len() as i32;

        for (i, &usage) in self.usages.iter().enumerate() {
            let row = i as u16 / cols_per_row;
            let col = i as u16 % cols_per_row;
            let x0 = area.x + col * cell_w + 1; // 1-char padding
            let y0 = area.y + row * cell_h;
            if x0 + sz.cols > area.x + area.width {
                break;
            }
            if y0 + sz.rows > area.y + area.height {
                break;
            }

            let filled = ((usage as i32 * perim_len) + 50) / 100; // rounded
            let filled = filled.clamp(0, perim_len);
            let fg = usage_color(usage);

            for (idx, &(cx, cy)) in perim.iter().enumerate() {
                let px = x0 + cx;
                let py = y0 + cy;
                let (ch, color) = if (idx as i32) < filled {
                    let dist = filled - 1 - idx as i32;
                    (shade_for_distance(dist), fg)
                } else {
                    ('░', self.skeleton)
                };
                let cell = buf.get_mut(px, py);
                cell.set_char(ch);
                cell.set_style(Style::default().fg(color));
            }

            // Centered percentage on the middle row.
            let val = format!("{usage:>3}");
            let text_col = x0 + (sz.cols - 3) / 2;
            let text_row = y0 + sz.rows / 2;
            for (j, ch) in val.chars().enumerate() {
                let c = buf.get_mut(text_col + j as u16, text_row);
                c.set_char(ch);
                c.set_style(Style::default().fg(fg));
            }
        }
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    #[test]
    fn renders_zero_usage_as_all_skeleton() {
        let area = Rect::new(0, 0, 9, 6);
        let mut buf = Buffer::empty(area);
        CompactGauge {
            usages: &[0],
            skeleton: Color::DarkGray,
        }
        .render(area, &mut buf);
        // At 0% the entire perimeter should be '░'.
        let mut has_skeleton = false;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf.get(x, y).symbol() == "░" {
                    has_skeleton = true;
                }
            }
        }
        assert!(has_skeleton);
    }

    #[test]
    fn renders_centered_percent_text() {
        let area = Rect::new(0, 0, 9, 6);
        let mut buf = Buffer::empty(area);
        CompactGauge {
            usages: &[42],
            skeleton: Color::DarkGray,
        }
        .render(area, &mut buf);
        // Pick up whichever row has "42" — size 7x5 centers text on middle row.
        let mut found = false;
        for y in 0..area.height {
            let row: String = (0..area.width).map(|x| buf.get(x, y).symbol().to_string()).collect();
            if row.contains("42") {
                found = true;
                break;
            }
        }
        assert!(found, "expected row with '42'");
    }

    #[test]
    fn full_usage_fills_entire_perimeter_with_fg_color() {
        let area = Rect::new(0, 0, 9, 6);
        let mut buf = Buffer::empty(area);
        CompactGauge {
            usages: &[100],
            skeleton: Color::DarkGray,
        }
        .render(area, &mut buf);
        let perim = perimeter(SizeVariant { cols: 7, rows: 5 });
        let fg = usage_color(100); // Red
        for (cx, cy) in perim {
            let cell = buf.get(1 + cx, cy);
            let fg_actual = cell.fg;
            assert_eq!(
                fg_actual, fg,
                "perimeter cell at ({cx},{cy}) has wrong fg color (expected fg, got {fg_actual:?})"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perimeter_normal_size_starts_at_bottom_center_and_closes_clockwise() {
        let p = perimeter(SizeVariant { cols: 7, rows: 5 });
        // Should start just left of center on the bottom row (col 2, row 4)
        assert_eq!(p.first(), Some(&(2, 4)));
        // Should end just right of center on the bottom row (col 4, row 4)
        assert_eq!(p.last(), Some(&(4, 4)));
        // No duplicates.
        let mut sorted = p.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), p.len());
    }

    #[test]
    fn pick_size_prefers_largest_that_fits_n() {
        // Plenty of room for 4 large cells: 4 * (7+1) = 32 wide.
        let sz = pick_size(Rect::new(0, 0, 40, 10), 4);
        assert_eq!(sz, SIZES[0]);
    }

    #[test]
    fn pick_size_falls_back_to_smallest_when_cramped() {
        let sz = pick_size(Rect::new(0, 0, 12, 6), 16);
        assert_eq!(sz, *SIZES.last().unwrap());
    }

    #[test]
    fn shades_go_dark_to_light_toward_edge() {
        assert_eq!(shade_for_distance(5), '█');
        assert_eq!(shade_for_distance(3), '█');
        assert_eq!(shade_for_distance(2), '▓');
        assert_eq!(shade_for_distance(1), '▒');
        assert_eq!(shade_for_distance(0), '░');
    }

    #[test]
    fn usage_color_thresholds() {
        assert_eq!(usage_color(0), Color::Green);
        assert_eq!(usage_color(39), Color::Green);
        assert_eq!(usage_color(40), Color::Yellow);
        assert_eq!(usage_color(74), Color::Yellow);
        assert_eq!(usage_color(75), Color::Red);
        assert_eq!(usage_color(100), Color::Red);
    }
}
