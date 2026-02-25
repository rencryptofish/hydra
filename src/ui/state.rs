use std::collections::VecDeque;

use ratatui::text::Text;

const MAX_HISTORY: usize = 50;

/// State for the compose input area in Compose mode.
pub struct ComposeState {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
    /// Remembered column for vertical movement (ghost column).
    pub(crate) desired_col: usize,
    /// Ring buffer of previously sent prompts (newest last).
    pub(crate) history: VecDeque<String>,
    /// Current position in history navigation (None = editing new text).
    pub(crate) history_index: Option<usize>,
    /// Stashed in-progress draft when navigating history.
    pub(crate) history_draft: Option<String>,
}

impl ComposeState {
    pub(crate) fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            desired_col: 0,
            history: VecDeque::new(),
            history_index: None,
            history_draft: None,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.desired_col = 0;
        self.history_index = None;
        self.history_draft = None;
    }

    /// Record a sent message in the history ring buffer.
    pub(crate) fn push_history(&mut self, text: String) {
        // Don't store duplicates of the most recent entry.
        if self.history.back().map(|s| s.as_str()) == Some(text.as_str()) {
            return;
        }
        self.history.push_back(text);
        if self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    /// Navigate to the previous (older) history entry.
    /// Returns true if the buffer changed.
    pub(crate) fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let new_idx = match self.history_index {
            None => {
                // Stash current draft before entering history.
                self.history_draft = Some(self.text());
                self.history.len() - 1
            }
            Some(0) => return false, // already at oldest
            Some(idx) => idx - 1,
        };
        self.history_index = Some(new_idx);
        self.load_text(&self.history[new_idx].clone());
        true
    }

    /// Navigate to the next (newer) history entry, or back to the draft.
    /// Returns true if the buffer changed.
    pub(crate) fn history_next(&mut self) -> bool {
        let Some(idx) = self.history_index else {
            return false; // not in history mode
        };
        if idx + 1 < self.history.len() {
            self.history_index = Some(idx + 1);
            self.load_text(&self.history[idx + 1].clone());
        } else {
            // Return to the stashed draft.
            self.history_index = None;
            let draft = self.history_draft.take().unwrap_or_default();
            self.load_text(&draft);
        }
        true
    }

    /// Replace the buffer contents with the given text, placing cursor at end.
    fn load_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(String::from).collect()
        };
        // If text ends with newline, the last line is empty.
        if text.ends_with('\n') {
            self.lines.push(String::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_row].chars().count();
        self.desired_col = self.cursor_col;
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

    pub(crate) fn move_word_left(&mut self) {
        let line = &self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        let mut idx = self.cursor_col;

        // Skip spaces backward
        while idx > 0 && chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        // Skip non-spaces backward
        while idx > 0 && !chars[idx - 1].is_whitespace() {
            idx -= 1;
        }

        self.cursor_col = idx;
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn move_word_right(&mut self) {
        let line = &self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        let mut idx = self.cursor_col;
        let len = chars.len();

        // Skip non-spaces forward
        while idx < len && !chars[idx].is_whitespace() {
            idx += 1;
        }
        // Skip spaces forward
        while idx < len && chars[idx].is_whitespace() {
            idx += 1;
        }

        self.cursor_col = idx;
        self.desired_col = self.cursor_col;
    }

    pub(crate) fn clear_line(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        let end_idx = char_to_byte_index(line, self.cursor_col);
        line.replace_range(..end_idx, "");
        self.cursor_col = 0;
        self.desired_col = 0;
    }

    pub(crate) fn delete_word_left(&mut self) {
        let end_col = self.cursor_col;
        self.move_word_left();
        let start_col = self.cursor_col;
        if start_col < end_col {
            let line = &mut self.lines[self.cursor_row];
            let byte_start = char_to_byte_index(line, start_col);
            let byte_end = char_to_byte_index(line, end_col);
            line.replace_range(byte_start..byte_end, "");
        }
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

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn compose_state_fuzzing(
            ops in prop::collection::vec(
                prop_oneof![
                    any::<char>().prop_map(|c| c.to_string()),
                    Just("\n".to_string()),
                    Just("backspace".to_string()),
                    Just("delete".to_string()),
                    Just("left".to_string()),
                    Just("right".to_string()),
                    Just("up".to_string()),
                    Just("down".to_string()),
                    Just("home".to_string()),
                    Just("end".to_string()),
                    Just("word_left".to_string()),
                    Just("word_right".to_string()),
                    Just("clear_line".to_string()),
                    Just("delete_word_left".to_string()),
                ],
                0..100
            )
        ) {
            let mut c = ComposeState::new();
            for op in ops {
                match op.as_str() {
                    "\n" => c.insert_newline(),
                    "backspace" => c.backspace(),
                    "delete" => c.delete_forward(),
                    "left" => c.move_left(),
                    "right" => c.move_right(),
                    "up" => c.move_up(),
                    "down" => c.move_down(),
                    "home" => c.move_home(),
                    "end" => c.move_end(),
                    "word_left" => c.move_word_left(),
                    "word_right" => c.move_word_right(),
                    "clear_line" => c.clear_line(),
                    "delete_word_left" => c.delete_word_left(),
                    s => {
                        let ch = s.chars().next().unwrap();
                        if !ch.is_control() {
                            c.insert_char(ch);
                        }
                    }
                }
                assert!(c.cursor_row < c.lines.len(), "cursor_row out of bounds");
                assert!(c.cursor_col <= c.lines[c.cursor_row].chars().count(), "cursor_col out of bounds");
            }
        }
    }
}
