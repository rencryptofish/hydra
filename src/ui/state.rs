use ratatui::text::Text;

/// State for the compose input area in Compose mode.
pub struct ComposeState {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_col: usize,
}

impl ComposeState {
    pub(crate) fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
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
    }

    pub(crate) fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub(crate) fn move_right(&mut self) {
        let line_chars = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_chars {
            self.cursor_col += 1;
        }
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

    /// Reset scroll/cache state when the selected session changes.
    pub(crate) fn reset_on_selection_change(&mut self) {
        self.scroll_offset = 0;
    }
}

fn count_lines_u16(content: &str) -> u16 {
    content.lines().count().min(u16::MAX as usize) as u16
}
