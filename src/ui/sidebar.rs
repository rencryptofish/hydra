use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

use crate::app::UiApp;
use crate::session::{format_duration, SessionStatus};
use crate::ui::diff::{build_diff_tree_lines, draw_diff_tree};
use crate::ui::stats::draw_stats;
use crate::ui::truncate_chars;

fn status_color(status: &SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::Green,
        SessionStatus::Running => Color::Red,
        SessionStatus::Exited => Color::Yellow,
    }
}

pub fn draw_sidebar(frame: &mut Frame, app: &UiApp, area: Rect) {
    // Show stats when there is any machine-wide agent usage.
    let has_stats = app.snapshot.global_stats.has_usage();

    let stats_height = if has_stats { 5 } else { 0 }; // 3 lines + top/bottom border

    // Update diff tree cache if inputs changed (diff_files or sidebar width).
    // Avoids recomputing sort + format on every frame (~4+ FPS) when data
    // only changes every ~5 seconds.
    let width = area.width.saturating_sub(2) as usize;
    {
        let mut cache = app.diff_tree_cache.borrow_mut();
        if cache.0 != app.snapshot.diff_files || cache.1 != width {
            cache.2 = build_diff_tree_lines(&app.snapshot.diff_files, width);
            cache.0 = app.snapshot.diff_files.clone();
            cache.1 = width;
        }
    }
    let cache = app.diff_tree_cache.borrow();
    let tree_lines = &cache.2;

    let max_tree_rows: u16 = 8;
    let tree_height = if tree_lines.is_empty() {
        0
    } else {
        (tree_lines.len() as u16 + 2).min(max_tree_rows + 2) // +2 for top/bottom border
    };

    let sidebar_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(stats_height),
            Constraint::Length(tree_height),
        ])
        .split(area);

    let list_area = sidebar_chunks[0];
    let stats_area = sidebar_chunks[1];
    let tree_area = sidebar_chunks[2];

    // Build session list with status group headers.
    // Sessions are already sorted by status group then name in app.rs.
    // We insert a header ListItem when the status group changes.
    // `selected_visual_row` maps app.selected (session index) to the
    // visual row in the list (accounting for header items).
    let inner_width = list_area.width.saturating_sub(2) as usize; // inside border
    let subtle = Style::default();
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_visual_row: usize = 0;
    let mut current_group: Option<u8> = None;

    for (i, session) in app.snapshot.sessions.iter().enumerate() {
        let group = session.status.sort_order();
        if current_group != Some(group) {
            current_group = Some(group);
            // Build header: "── ● Running ──────"
            let label = format!(" {} ", session.status);
            let dot_color = status_color(&session.status);
            let dashes_left = "── ";
            let dashes_right_len = inner_width.saturating_sub(dashes_left.len() + 2 + label.len()); // 2 for "● "
            let dashes_right: String = "─".repeat(dashes_right_len);
            let header_spans = vec![
                Span::styled(dashes_left, subtle),
                Span::styled("● ", Style::default().fg(dot_color)),
                Span::styled(label, Style::default()),
                Span::styled(dashes_right, subtle),
            ];
            items.push(ListItem::new(Line::from(header_spans)));
        }

        if i == app.selected {
            selected_visual_row = items.len();
        }

        let marker = if i == app.selected { ">> " } else { "   " };
        let name_style = if i == app.selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(marker, name_style),
            Span::styled("● ", Style::default().fg(status_color(&session.status))),
            Span::styled(
                format!("{} [{}]", session.name, session.agent_type),
                name_style,
            ),
        ];
        if let Some(elapsed) = session.task_elapsed {
            spans.push(Span::styled(
                format!(" {}", format_duration(elapsed)),
                Style::default(),
            ));
        }
        if let Some(stats) = app.snapshot.session_stats.get(&session.tmux_name) {
            if stats.active_subagents > 0 {
                spans.push(Span::styled(
                    format!(" [{}T]", stats.active_subagents),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }
        let mut lines = vec![Line::from(spans)];
        if let Some(msg) = app.snapshot.last_messages.get(&session.tmux_name) {
            let max_chars = 50;
            let display = if msg.chars().count() > max_chars {
                let truncated = truncate_chars(msg, max_chars);
                format!("     {truncated}...")
            } else {
                format!("     {msg}")
            };
            lines.push(Line::from(Span::styled(display, Style::default())));
        }
        items.push(ListItem::new(lines));
    }

    let session_count = app.snapshot.sessions.len();
    let title = format!(" Sessions ({session_count}) ");
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(Style::default()) // selection handled manually via ">>"
        .highlight_symbol("");

    // Use stateful rendering to scroll the list to the selected visual row.
    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(Some(selected_visual_row));
    frame.render_stateful_widget(list, list_area, &mut list_state);

    // Draw aggregate stats across all sessions
    if has_stats {
        draw_stats(frame, app, stats_area);
    }

    // Draw diff tree
    if !tree_lines.is_empty() {
        draw_diff_tree(frame, tree_lines, tree_area);
    }
}

#[cfg(test)]
mod tests {
    use crate::session::SessionStatus;
    use ratatui::style::Color;

    #[test]
    fn status_color_maps_correctly() {
        assert_eq!(super::status_color(&SessionStatus::Idle), Color::Green);
        assert_eq!(super::status_color(&SessionStatus::Running), Color::Red);
        assert_eq!(super::status_color(&SessionStatus::Exited), Color::Yellow);
    }
}
