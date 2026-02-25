use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
    Frame,
};

use crate::app::{Mode, UiApp};

pub fn draw_preview(frame: &mut Frame, app: &UiApp, area: Rect) {
    let title = app
        .active_preview_name()
        .map(|name| format!(" {name} "))
        .unwrap_or_else(|| " Preview ".to_string());

    if app.mode == Mode::Compose {
        // Compose mode: split preview area into conversation + compose input
        let compose_line_count = app.compose.lines.len() as u16;
        let compose_height = (compose_line_count + 3).min(area.height / 3).max(4); // +2 border +1 hint

        let compose_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(compose_height)])
            .split(area);

        let conv_area = compose_chunks[0];
        let input_area = compose_chunks[1];

        // Draw conversation preview (scrolled to bottom)
        let border_style = Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD);
        let conv_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Thick)
            .title(title)
            .border_style(border_style);

        let conv_inner_height = conv_area.height.saturating_sub(2);
        let total_lines = app.preview.line_count;
        let max_scroll_offset = total_lines.saturating_sub(conv_inner_height);
        let capped_offset = app.preview.scroll_offset.min(max_scroll_offset);
        let scroll_y = max_scroll_offset.saturating_sub(capped_offset);

        let conv_preview = if let Some(ref text) = app.preview.text {
            Paragraph::new(text.clone())
                .block(conv_block)
                .scroll((scroll_y, 0))
        } else {
            Paragraph::new(app.preview.content.as_str())
                .block(conv_block)
                .scroll((scroll_y, 0))
        };
        frame.render_widget(conv_preview, conv_area);

        // Draw compose input area
        draw_compose_input(frame, app, input_area);
    } else {
        // Browse mode: normal preview
        let border_style = Style::default().fg(Color::Cyan);
        let inner_height = area.height.saturating_sub(2);
        let total_lines = app.preview.line_count;
        let max_scroll_offset = total_lines.saturating_sub(inner_height);
        let capped_offset = app.preview.scroll_offset.min(max_scroll_offset);
        let scroll_y = max_scroll_offset.saturating_sub(capped_offset);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style);

        let preview = if let Some(ref text) = app.preview.text {
            Paragraph::new(text.clone())
                .block(block)
                .scroll((scroll_y, 0))
        } else {
            Paragraph::new(app.preview.content.as_str())
                .block(block)
                .scroll((scroll_y, 0))
        };

        frame.render_widget(preview, area);
    }
}

fn draw_compose_input(frame: &mut Frame, app: &UiApp, area: Rect) {
    let compose_style = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(" Compose ")
        .border_style(compose_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Reserve last row for hint
    let text_height = inner.height.saturating_sub(1);
    let text_area = Rect::new(inner.x, inner.y, inner.width, text_height);
    let hint_area = Rect::new(inner.x, inner.y + text_height, inner.width, 1);

    // Render compose text
    let compose_lines: Vec<Line> = app
        .compose
        .lines
        .iter()
        .map(|l| Line::from(l.as_str().to_string()))
        .collect();
    let paragraph = Paragraph::new(compose_lines);
    frame.render_widget(paragraph, text_area);

    // Render hint
    let hint = Line::from(Span::styled(
        "Enter: send | Shift+Enter: newline | Esc: cancel",
        Style::default().add_modifier(Modifier::DIM),
    ));
    frame.render_widget(Paragraph::new(hint), hint_area);

    // Set cursor position
    let cursor_x = inner.x + app.compose.cursor_col as u16;
    let cursor_y = inner.y + app.compose.cursor_row as u16;
    if cursor_x < inner.x + inner.width && cursor_y < inner.y + text_height {
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
}
