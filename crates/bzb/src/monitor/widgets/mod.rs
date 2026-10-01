pub mod compact_gauge;
pub mod lease_table;
pub mod pool_gauge;

#[cfg(test)]
pub mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    /// Renders `widget` on a test terminal and returns its rows as text.
    pub fn render(width: u16, height: u16, widget: impl FnOnce(Rect, &mut Buffer)) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| widget(frame.size(), frame.buffer_mut()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.get(x, y).symbol())
                    .collect::<String>()
            })
            .collect()
    }
}
