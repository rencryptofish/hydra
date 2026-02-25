use ratatui::text::Text;

/// State for the compose input area in Compose mode.
pub struct ComposeState {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
    /// Remembered column for vertical movement (ghost column).
    pub(crate) desired_col: usize,
}

impl ComposeState {
    pub(crate) fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            desired_col: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.desired_col = 0;
    }

    /// Get the full text content of the compose buffer.
    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        line.insert(byte_idx, ch);
        self.cursor_col += 1;
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn insert_newline(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        let byte_idx = char_to_byte_index(line, self.cursor_col);
        let tail = line.split_off(byte_idx);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.desired_col = 0;
        self.lines.insert(self.cursor_row, tail);
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte_index(line, self.cursor_col - 1);
            let end_idx = char_to_byte_index(line, self.cursor_col);
            line.replace_range(byte_idx..end_idx, "");
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current_line);
        }
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn delete_forward(&mut self) {
        let line_chars = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_chars {
            let line = &mut self.lines[self.cursor_row];
            let byte_idx = char_to_byte_index(line, self.cursor_col);
            let end_idx = char_to_byte_index(line, self.cursor_col + 1);
            line.replace_range(byte_idx..end_idx, "");
        } else if self.cursor_row + 1 < self.lines.len() {
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn move_right(&mut self) {
        let line_chars = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_chars {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            let line_chars = self.lines[self.cursor_row].chars().count();
            self.cursor_col = self.desired_col.min(line_chars);
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            let line_chars = self.lines[self.cursor_row].chars().count();
            self.cursor_col = self.desired_col.min(line_chars);
        }
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor_col = 0;
        self.desired_col = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].chars().count();
        self.desired_col = self.cursor_col;
    }

    /// Insert text from a paste event. Handles embedded newlines.
    pub(crate) fn insert_text(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\n' => self.insert_newline(),
                '\r' => {
                    // \r\n → single newline; bare \r → newline
                    if chars.peek() != Some(&'\n') {
                        self.insert_newline();
                    }
                }
                _ => self.insert_char(ch),
            }
        }
        self.desired_col = self.cursor_col;
    }
}

/// Convert a character index to a byte index in a string.
fn char_to_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// Preview pane state: content, scroll position, and caching metadata.
#[derive(Debug)]
pub struct PreviewState {
    pub content: String,
    /// ANSI-parsed preview content for colored rendering.
    pub text: Option<Text<'static>>,
    /// Cached preview line count to avoid O(n) line scans every frame.
    pub line_count: u16,
    pub scroll_offset: u16,
}

impl PreviewState {
    pub(crate) fn new() -> Self {
        Self {
            content: String::new(),
            text: None,
            line_count: 0,
            scroll_offset: 0,
        }
    }

    pub fn set_text(&mut self, content: String) {
        self.line_count = count_lines_u16(&content);
        self.text = ansi_to_tui::IntoText::into_text(&content).ok();
        self.content = content;
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(3);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
    }

    pub fn scroll_page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(15);
    }

    pub fn scroll_page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(15);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = u16::MAX;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    /// Reset scroll/cache state when the selected session changes.
    pub(crate) fn reset_on_selection_change(&mut self) {
        self.scroll_offset = 0;
    }
}

fn count_lines_u16(content: &str) -> u16 {
    content.lines().count().min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ComposeState tests ──────────────────────────────────────────

    #[test]
    fn delete_forward_mid_line() {
        let mut c = ComposeState::new();
        c.insert_text("hello");
        c.cursor_col = 2;
        c.delete_forward();
        assert_eq!(c.text(), "helo");
        assert_eq!(c.cursor_col, 2);
    }

    #[test]
    fn delete_forward_joins_next_line() {
        let mut c = ComposeState::new();
        c.insert_text("ab\ncd");
        c.cursor_row = 0;
        c.cursor_col = 2;
        c.delete_forward();
        assert_eq!(c.lines.len(), 1);
        assert_eq!(c.text(), "abcd");
        assert_eq!(c.cursor_col, 2);
    }

    #[test]
    fn delete_forward_at_end_is_noop() {
        let mut c = ComposeState::new();
        c.insert_text("abc");
        c.delete_forward();
        assert_eq!(c.text(), "abc");
    }

    #[test]
    fn move_home_and_end() {
        let mut c = ComposeState::new();
        c.insert_text("hello world");
        assert_eq!(c.cursor_col, 11);
        c.move_home();
        assert_eq!(c.cursor_col, 0);
        assert_eq!(c.desired_col, 0);
        c.move_end();
        assert_eq!(c.cursor_col, 11);
        assert_eq!(c.desired_col, 11);
    }

    #[test]
    fn ghost_column_round_trip() {
        let mut c = ComposeState::new();
        c.insert_text("long line here\nhi\nanother long line");
        // Cursor at end of line 2 (col 17)
        assert_eq!(c.cursor_row, 2);
        assert_eq!(c.cursor_col, 17);
        assert_eq!(c.desired_col, 17);

        // Move up to "hi" (2 chars) — col clamps to 2, desired stays 17
        c.move_up();
        assert_eq!(c.cursor_row, 1);
        assert_eq!(c.cursor_col, 2);
        assert_eq!(c.desired_col, 17);

        // Move up to "long line here" (14 chars) — col = min(17, 14) = 14
        c.move_up();
        assert_eq!(c.cursor_row, 0);
        assert_eq!(c.cursor_col, 14);
        assert_eq!(c.desired_col, 17);

        // Move down back to "hi" — col clamps to 2 again
        c.move_down();
        assert_eq!(c.cursor_row, 1);
        assert_eq!(c.cursor_col, 2);
        assert_eq!(c.desired_col, 17);

        // Move down to "another long line" — col = min(17, 17) = 17
        c.move_down();
        assert_eq!(c.cursor_row, 2);
        assert_eq!(c.cursor_col, 17);
    }

    #[test]
    fn ghost_column_resets_on_horizontal_movement() {
        let mut c = ComposeState::new();
        c.insert_text("long line\nhi");
        // cursor at (1, 2), desired_col = 2
        c.move_left();
        assert_eq!(c.cursor_col, 1);
        assert_eq!(c.desired_col, 1);

        // Now move up — should use desired_col=1, not the old 2
        c.move_up();
        assert_eq!(c.cursor_col, 1);
    }

    #[test]
    fn insert_text_with_crlf() {
        let mut c = ComposeState::new();
        c.insert_text("a\r\nb\rc");
        assert_eq!(c.lines.len(), 3);
        assert_eq!(c.text(), "a\nb\nc");
    }

    #[test]
    fn insert_text_multiline() {
        let mut c = ComposeState::new();
        c.insert_text("hello\nworld");
        assert_eq!(c.lines.len(), 2);
        assert_eq!(c.text(), "hello\nworld");
        assert_eq!(c.cursor_row, 1);
        assert_eq!(c.cursor_col, 5);
    }

    // ── PreviewState scroll tests ───────────────────────────────────

    #[test]
    fn preview_page_up_down() {
        let mut p = PreviewState::new();
        p.scroll_page_up();
        assert_eq!(p.scroll_offset, 15);
        p.scroll_page_up();
        assert_eq!(p.scroll_offset, 30);
        p.scroll_page_down();
        assert_eq!(p.scroll_offset, 15);
        p.scroll_page_down();
        assert_eq!(p.scroll_offset, 0);
        p.scroll_page_down(); // saturates at 0
        assert_eq!(p.scroll_offset, 0);
    }

    #[test]
    fn preview_scroll_to_top_bottom() {
        let mut p = PreviewState::new();
        p.scroll_to_top();
        assert_eq!(p.scroll_offset, u16::MAX);
        p.scroll_to_bottom();
        assert_eq!(p.scroll_offset, 0);
    }
}
