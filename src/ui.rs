use std::collections::VecDeque;

use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{Mode, UiApp};
use crate::logs::{format_cost, format_tokens, ConversationEntry};
use crate::session::{format_duration, AgentType, SessionStatus};

pub fn draw(frame: &mut Frame, app: &UiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    let main_area = chunks[0];
    let help_area = chunks[1];

    // Main layout: sidebar | preview
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
        .split(main_area);

    app.sidebar_area.set(panels[0]);
    app.preview_area.set(panels[1]);

    draw_sidebar(frame, app, panels[0]);
    draw_preview(frame, app, panels[1]);
    draw_help_bar(frame, app, help_area);

    // Draw modal overlays
    match app.mode {
        Mode::NewSessionAgent => draw_agent_select(frame, app),
        Mode::ConfirmDelete => draw_confirm_delete(frame, app),
        Mode::Browse | Mode::Compose => {}
    }
}

fn status_color(status: &SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::Green,
        SessionStatus::Running => Color::Red,
        SessionStatus::Exited => Color::Yellow,
    }
}

pub fn draw_sidebar(frame: &mut Frame, app: &UiApp, area: Rect) {
    // Show stats if global stats have any tokens
    let has_stats = app.global_stats.tokens_in + app.global_stats.tokens_out > 0;

    let stats_height = if has_stats { 3 } else { 0 }; // 1 line + top/bottom border

    // Update diff tree cache if inputs changed (diff_files or sidebar width).
    // Avoids recomputing sort + format on every frame (~4+ FPS) when data
    // only changes every ~5 seconds.
    let width = area.width.saturating_sub(2) as usize;
    {
        let mut cache = app.diff_tree_cache.borrow_mut();
        if cache.0 != app.diff_files || cache.1 != width {
            cache.2 = build_diff_tree_lines(&app.diff_files, width);
            cache.0 = app.diff_files.clone();
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

    for (i, session) in app.sessions.iter().enumerate() {
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
        if let Some(stats) = app.session_stats.get(&session.tmux_name) {
            if stats.active_subagents > 0 {
                spans.push(Span::styled(
                    format!(" [{}T]", stats.active_subagents),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }
        let mut lines = vec![Line::from(spans)];
        if let Some(msg) = app.last_messages.get(&session.tmux_name) {
            let max_chars = 50;
            let display = if msg.chars().count() > max_chars {
                let truncated: String = msg.chars().take(max_chars).collect();
                format!("     {truncated}...")
            } else {
                format!("     {msg}")
            };
            lines.push(Line::from(Span::styled(display, Style::default())));
        }
        items.push(ListItem::new(lines));
    }

    let session_count = app.sessions.len();
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

/// Truncate a string to at most `max` characters (Unicode-safe).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Build lines for a compact diff tree grouped by directory.
/// Files sorted by path, grouped under directory headers.
/// Output example:
///   src/
///    app.rs       +45-12
///    ui.rs        +30-5
///   README.md     +3
pub fn build_diff_tree_lines(
    diff_files: &[crate::app::DiffFile],
    width: usize,
) -> Vec<Line<'static>> {
    if diff_files.is_empty() {
        return vec![];
    }

    let green = Style::default().fg(Color::Green);
    let red = Style::default().fg(Color::Red);
    let cyan = Style::default().fg(Color::Cyan);

    let mut sorted: Vec<&crate::app::DiffFile> = diff_files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut lines: Vec<Line> = Vec::new();
    let mut current_dir: Option<&str> = None;
    let inner_w = width.saturating_sub(1); // leave 1 char margin

    for f in &sorted {
        let (dir, basename) = match f.path.rfind('/') {
            Some(i) => (Some(&f.path[..=i]), &f.path[i + 1..]),
            None => (None, f.path.as_str()),
        };

        // Skip entries with empty basenames (e.g. trailing-slash paths)
        if basename.is_empty() {
            continue;
        }

        // Emit directory header if changed
        let dir_str = dir.unwrap_or("");
        let show_dir = match current_dir {
            Some(prev) => prev != dir_str,
            None => dir.is_some(),
        };
        if show_dir {
            if let Some(d) = dir {
                let display: String = if d.chars().count() > inner_w {
                    truncate_chars(d, inner_w)
                } else {
                    d.to_string()
                };
                lines.push(Line::from(Span::styled(
                    format!(" {display}"),
                    Style::default(),
                )));
            }
            current_dir = Some(dir_str);
        }

        // Build diff stat string
        let stat = if f.untracked {
            "new".to_string()
        } else {
            format_compact_diff(f.insertions, f.deletions)
        };
        let indent = if dir.is_some() { "  " } else { " " };

        // Compute available space for filename
        let stat_len = stat.chars().count();
        let prefix_len = indent.len();
        let available = inner_w.saturating_sub(prefix_len + stat_len + 1);

        let basename_chars = basename.chars().count();
        let name: String = if available == 0 {
            String::new()
        } else if basename_chars > available && available > 3 {
            format!("{}…", truncate_chars(basename, available - 1))
        } else if basename_chars > available {
            truncate_chars(basename, available)
        } else {
            basename.to_string()
        };

        let name_chars = name.chars().count();
        let padding = inner_w.saturating_sub(prefix_len + name_chars + stat_len);
        let pad_str: String = " ".repeat(padding);

        let mut spans = vec![Span::styled(
            format!("{indent}{name}{pad_str}"),
            Style::default(),
        )];

        if f.untracked {
            spans.push(Span::styled("new", cyan));
        } else {
            if f.insertions > 0 {
                spans.push(Span::styled(format!("+{}", f.insertions), green));
            }
            if f.deletions > 0 {
                spans.push(Span::styled(format!("-{}", f.deletions), red));
            }
        }

        lines.push(Line::from(spans));
    }

    lines
}

/// Format compact diff: "+45-12", "+45", "-12"
fn format_compact_diff(ins: u32, del: u32) -> String {
    match (ins > 0, del > 0) {
        (true, true) => format!("+{ins}-{del}"),
        (true, false) => format!("+{ins}"),
        (false, true) => format!("-{del}"),
        (false, false) => String::new(),
    }
}

fn draw_diff_tree(frame: &mut Frame, lines: &[Line], area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Changes ")
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Show the tail that fits (most relevant files at bottom)
    let max_rows = inner.height as usize;
    let start = lines.len().saturating_sub(max_rows);
    let visible: Vec<Line> = lines[start..].to_vec();

    let paragraph = Paragraph::new(visible);
    frame.render_widget(paragraph, inner);
}

pub fn draw_stats(frame: &mut Frame, app: &UiApp, area: Rect) {
    // Use machine-wide global stats for cost and tokens
    let total_cost = app.global_stats.cost_usd();
    let total_tokens = app.global_stats.tokens_in + app.global_stats.tokens_out;

    // Edits are hydra-specific (per-session)
    let total_edits: u16 = app.session_stats.values().map(|s| s.edits).sum();

    let val = Style::default();

    // Total diff across all files
    let total_diff: u32 = app
        .diff_files
        .iter()
        .map(|f| f.insertions + f.deletions)
        .sum();

    let mut spans = vec![
        Span::styled(format_cost(total_cost), Style::default().fg(Color::Green)),
        Span::styled(format!(" {}", format_tokens(total_tokens)), val),
        Span::styled(format!(" {}✎", total_edits), val),
    ];

    if total_diff > 0 {
        spans.push(Span::styled(format!(" Δ{total_diff}"), val));
    }

    let line = Line::from(spans);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Stats ")
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(line).block(block);
    frame.render_widget(paragraph, area);
}

pub fn draw_preview(frame: &mut Frame, app: &UiApp, area: Rect) {
    let title = if let Some(session) = app.sessions.get(app.selected) {
        format!(" {} ", session.name)
    } else {
        " Preview ".to_string()
    };

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
        "Enter: send | Esc: cancel",
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

fn push_component_title(lines: &mut Vec<Line<'static>>, title: &str, style: Style) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(title.to_string(), style)));
}

fn push_component_body(lines: &mut Vec<Line<'static>>, text: &str, style: Style) {
    for line in text.lines() {
        lines.push(Line::from(Span::styled(format!("  {line}"), style)));
    }
}

fn push_tool_result_component(
    lines: &mut Vec<Line<'static>>,
    filenames: &[String],
    summary: Option<&str>,
    style: Style,
) {
    push_component_title(lines, "TOOL RESULT", style);
    let preview_count = filenames.len().min(4);
    for file in filenames.iter().take(preview_count) {
        lines.push(Line::from(Span::styled(format!("  - {file}"), style)));
    }
    if filenames.len() > preview_count {
        lines.push(Line::from(Span::styled(
            format!("  ... +{} more", filenames.len() - preview_count),
            style.add_modifier(Modifier::DIM),
        )));
    }
    if let Some(summary) = summary {
        for line in summary.lines().take(3) {
            lines.push(Line::from(Span::styled(format!("  > {line}"), style)));
        }
    }
}

fn push_unparsed_component(
    lines: &mut Vec<Line<'static>>,
    reason: &str,
    raw: &str,
    reason_style: Style,
    raw_style: Style,
) {
    lines.push(Line::from(vec![
        Span::styled(format!("  [{reason}] "), reason_style),
        Span::styled(raw.to_string(), raw_style),
    ]));
}

/// Render conversation entries into styled `Text` for the preview pane.
pub fn render_conversation(entries: &VecDeque<ConversationEntry>) -> ratatui::text::Text<'static> {
    if entries.is_empty() {
        return ratatui::text::Text::from(Line::from(Span::styled(
            "Waiting for agent output...",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    let user_title = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let assistant_title = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);
    let tool_title = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let queue_title = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let progress_title = Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD);
    let system_title = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let snapshot_title = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let body = Style::default();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let warn = Style::default().fg(Color::Magenta);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut unparsed_lines: Vec<Line<'static>> = Vec::new();

    for entry in entries {
        match entry {
            ConversationEntry::UserMessage { text } => {
                push_component_title(&mut lines, "USER", user_title);
                push_component_body(&mut lines, text, body);
            }
            ConversationEntry::AssistantText { text } => {
                push_component_title(&mut lines, "ASSISTANT", assistant_title);
                push_component_body(&mut lines, text, body);
            }
            ConversationEntry::ToolUse { tool_name, details } => {
                push_component_title(&mut lines, "TOOL", tool_title);
                lines.push(Line::from(Span::styled(format!("  {tool_name}"), dim)));
                if let Some(details) = details {
                    lines.push(Line::from(Span::styled(format!("  {details}"), dim)));
                }
            }
            ConversationEntry::ToolResult { filenames, summary } => {
                push_tool_result_component(&mut lines, filenames, summary.as_deref(), dim);
            }
            ConversationEntry::QueueOperation { operation, task_id } => {
                push_component_title(&mut lines, "SUBAGENT", queue_title);
                let text = match task_id {
                    Some(task_id) => format!("  {operation} ({task_id})"),
                    None => format!("  {operation}"),
                };
                lines.push(Line::from(Span::styled(text, dim)));
            }
            ConversationEntry::Progress { kind, detail } => {
                push_component_title(&mut lines, &format!("PROGRESS ({kind})"), progress_title);
                lines.push(Line::from(Span::styled(format!("  {detail}"), dim)));
            }
            ConversationEntry::SystemEvent { subtype, detail } => {
                push_component_title(&mut lines, &format!("SYSTEM ({subtype})"), system_title);
                lines.push(Line::from(Span::styled(format!("  {detail}"), dim)));
            }
            ConversationEntry::FileHistorySnapshot {
                tracked_files,
                files,
                is_update,
            } => {
                push_component_title(&mut lines, "FILE SNAPSHOT", snapshot_title);
                let kind = if *is_update { "update" } else { "new" };
                lines.push(Line::from(Span::styled(
                    format!("  {kind}: {tracked_files} tracked file(s)"),
                    dim,
                )));
                for file in files {
                    lines.push(Line::from(Span::styled(format!("  - {file}"), dim)));
                }
                if *tracked_files > files.len() {
                    lines.push(Line::from(Span::styled(
                        format!("  ... +{} more", tracked_files - files.len()),
                        dim,
                    )));
                }
            }
            ConversationEntry::Unparsed { reason, raw } => {
                push_unparsed_component(&mut unparsed_lines, reason, raw, warn, dim);
            }
        }
    }

    if !unparsed_lines.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "UNPARSED JSONL",
            warn.add_modifier(Modifier::BOLD),
        )));
        lines.extend(unparsed_lines);
    }

    ratatui::text::Text::from(lines)
}

fn draw_help_bar(frame: &mut Frame, app: &UiApp, area: Rect) {
    let help_text = match app.mode {
        Mode::Browse if !app.mouse_captured => "SELECT TEXT TO COPY  |  c: exit copy mode",
        Mode::Browse => "j/k: navigate  Enter: attach  n: new  d: delete  c: copy  q: quit",
        Mode::Compose => "Enter: send  Esc: cancel",
        Mode::NewSessionAgent => "j/k: select agent  Enter: confirm  Esc: cancel",
        Mode::ConfirmDelete => "y: confirm delete  Esc: cancel",
    };

    let status = if let Some(msg) = &app.status_message {
        format!(" {msg} | {help_text}")
    } else {
        format!(" {help_text}")
    };

    let bar = Paragraph::new(Line::from(Span::styled(
        status,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    frame.render_widget(bar, area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

#[cfg(test)]
fn inset_rect(area: Rect, margin: u16) -> Rect {
    let double = margin.saturating_mul(2);
    Rect::new(
        area.x.saturating_add(margin),
        area.y.saturating_add(margin),
        area.width.saturating_sub(double),
        area.height.saturating_sub(double),
    )
}

fn draw_agent_select(frame: &mut Frame, app: &UiApp) {
    let agents = AgentType::all();
    let height = agents.len() as u16 + 2;
    let area = centered_rect(30, height, frame.area());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let marker = if i == app.agent_selection {
                ">> "
            } else {
                "   "
            };
            let label = format!("{marker}{agent}");
            let style = if i == app.agent_selection {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Select Agent ")
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(list, area);
}

fn draw_confirm_delete(frame: &mut Frame, app: &UiApp) {
    let area = centered_rect(40, 5, frame.area());
    frame.render_widget(Clear, area);

    let name = app
        .sessions
        .get(app.selected)
        .map(|s| s.name.as_str())
        .unwrap_or("?");

    let text = format!(" Kill session '{name}'? (y/n)");
    let confirm = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Delete ")
            .border_style(Style::default().fg(Color::Red)),
    );
    frame.render_widget(confirm, area);
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use crate::app::{Mode, UiApp};
    use crate::session::{AgentType, Session, SessionStatus};

    fn make_app() -> UiApp {
        UiApp::new_test()
    }

    fn make_session(name: &str, agent: AgentType) -> Session {
        make_session_with_status(name, agent, SessionStatus::Idle)
    }

    fn make_session_with_status(name: &str, agent: AgentType, status: SessionStatus) -> Session {
        Session {
            name: name.to_string(),
            tmux_name: format!("hydra-testproj-{name}"),
            agent_type: agent,
            status,
            task_elapsed: None,
            _alive: true,
        }
    }

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                output.push_str(cell.symbol());
            }
            let trimmed = output.trim_end();
            output = trimmed.to_string();
            output.push('\n');
        }
        output
    }

    #[test]
    fn browse_mode_with_sessions() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![
            make_session("worker-1", AgentType::Claude),
            make_session("worker-2", AgentType::Codex),
            make_session("research", AgentType::Claude),
        ];
        app.selected = 0;
        app.preview.set_text("some preview content".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_empty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.preview
            .set_text("No sessions. Press 'n' to create one.".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn new_session_agent_modal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 0;

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn confirm_delete_modal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("doomed-session", AgentType::Claude)];
        app.selected = 0;
        app.mode = Mode::ConfirmDelete;

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn compose_mode() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("active-session", AgentType::Claude)];
        app.selected = 0;
        app.mode = Mode::Compose;
        app.preview
            .set_text("$ claude\nHello, how can I help?".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn status_message_displayed() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.status_message = Some("Created session 'worker-1' with Claude".to_string());
        app.preview
            .set_text("No sessions. Press 'n' to create one.".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_all_statuses() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![
            make_session_with_status("idle-one", AgentType::Claude, SessionStatus::Idle),
            make_session_with_status("running-one", AgentType::Codex, SessionStatus::Running),
            make_session_with_status("exited-one", AgentType::Claude, SessionStatus::Exited),
        ];
        app.selected = 1;
        app.preview.set_text("running session output".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_task_elapsed() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        let mut session = make_session("worker-1", AgentType::Claude);
        session.status = SessionStatus::Running;
        session.task_elapsed = Some(std::time::Duration::from_secs(125));
        app.sessions = vec![session];
        app.selected = 0;
        app.preview.set_text("working...".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_last_messages() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![
            make_session("worker-1", AgentType::Claude),
            make_session("worker-2", AgentType::Codex),
        ];
        app.selected = 0;
        app.preview.set_text("preview".to_string());
        app.last_messages.insert(
            "hydra-testproj-worker-1".to_string(),
            "I'll help you with that task.".to_string(),
        );

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_long_last_message_truncated() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("worker-1", AgentType::Claude)];
        app.selected = 0;
        app.preview.set_text("preview".to_string());
        app.last_messages.insert(
            "hydra-testproj-worker-1".to_string(),
            "This is a very long message that should be truncated at fifty characters to fit sidebar".to_string(),
        );

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn preview_scrolling_renders() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("s1", AgentType::Claude)];
        app.selected = 0;
        // Create content taller than the preview area
        app.preview.set_text(
            (0..50)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.preview.scroll_offset = 10;

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn agent_select_second_highlighted() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 1; // Select Codex

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn confirm_delete_no_sessions() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.mode = Mode::ConfirmDelete;
        // No sessions — should show "?"

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    // ── Unit tests for helper functions ───────────────────────────────

    #[test]
    fn status_color_maps_correctly() {
        assert_eq!(
            super::status_color(&SessionStatus::Idle),
            ratatui::style::Color::Green
        );
        assert_eq!(
            super::status_color(&SessionStatus::Running),
            ratatui::style::Color::Red
        );
        assert_eq!(
            super::status_color(&SessionStatus::Exited),
            ratatui::style::Color::Yellow
        );
    }

    #[test]
    fn centered_rect_normal() {
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let result = super::centered_rect(40, 10, area);
        assert_eq!(result.width, 40);
        assert_eq!(result.height, 10);
        assert_eq!(result.x, 20); // (80 - 40) / 2
        assert_eq!(result.y, 7); // (24 - 10) / 2
    }

    #[test]
    fn centered_rect_larger_than_area() {
        let area = ratatui::layout::Rect::new(0, 0, 20, 10);
        let result = super::centered_rect(40, 20, area);
        // Width and height clamped to area
        assert_eq!(result.width, 20);
        assert_eq!(result.height, 10);
    }

    #[test]
    fn centered_rect_with_offset() {
        let area = ratatui::layout::Rect::new(10, 5, 60, 20);
        let result = super::centered_rect(20, 10, area);
        assert_eq!(result.x, 30); // 10 + (60-20)/2
        assert_eq!(result.y, 10); // 5 + (20-10)/2
    }

    #[test]
    fn centered_rect_zero_size_area() {
        let area = ratatui::layout::Rect::new(0, 0, 0, 0);
        let result = super::centered_rect(40, 10, area);
        assert_eq!(result.width, 0);
        assert_eq!(result.height, 0);
    }

    // ── format_compact_diff unit tests ──────────────────────────────

    #[test]
    fn format_compact_diff_both() {
        assert_eq!(super::format_compact_diff(10, 5), "+10-5");
    }

    #[test]
    fn format_compact_diff_insert_only() {
        assert_eq!(super::format_compact_diff(7, 0), "+7");
    }

    #[test]
    fn format_compact_diff_delete_only() {
        assert_eq!(super::format_compact_diff(0, 3), "-3");
    }

    #[test]
    fn format_compact_diff_zero() {
        assert_eq!(super::format_compact_diff(0, 0), "");
    }

    // ── build_diff_tree_lines unit tests ─────────────────────────────

    #[test]
    fn diff_tree_empty() {
        let lines = super::build_diff_tree_lines(&[], 40);
        assert!(lines.is_empty());
    }

    #[test]
    fn diff_tree_root_level_file() {
        let files = vec![crate::app::DiffFile {
            path: "README.md".into(),
            insertions: 3,
            deletions: 0,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_with_directory() {
        let files = vec![
            crate::app::DiffFile {
                path: "src/app.rs".into(),
                insertions: 10,
                deletions: 2,
                untracked: false,
            },
            crate::app::DiffFile {
                path: "src/ui.rs".into(),
                insertions: 5,
                deletions: 0,
                untracked: false,
            },
        ];
        let lines = super::build_diff_tree_lines(&files, 40);
        // 1 directory header + 2 files
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn diff_tree_multiple_directories() {
        let files = vec![
            crate::app::DiffFile {
                path: "src/app.rs".into(),
                insertions: 1,
                deletions: 0,
                untracked: false,
            },
            crate::app::DiffFile {
                path: "tests/cli.rs".into(),
                insertions: 2,
                deletions: 1,
                untracked: false,
            },
        ];
        let lines = super::build_diff_tree_lines(&files, 40);
        // 2 directory headers + 2 files
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn diff_tree_deletion_only_file() {
        let files = vec![crate::app::DiffFile {
            path: "old.rs".into(),
            insertions: 0,
            deletions: 15,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_narrow_width_truncates_name() {
        let files = vec![crate::app::DiffFile {
            path: "src/very_long_filename_that_exceeds.rs".into(),
            insertions: 100,
            deletions: 50,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 10);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_narrow_width_truncates_directory() {
        let files = vec![crate::app::DiffFile {
            path: "very/deeply/nested/directory/structure/file.rs".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // inner_w = 11, dir "very/deeply/nested/directory/structure/" is 40 chars > 11
        let lines = super::build_diff_tree_lines(&files, 12);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_zero_changes_file() {
        let files = vec![crate::app::DiffFile {
            path: "unchanged.rs".into(),
            insertions: 0,
            deletions: 0,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_untracked_file() {
        let files = vec![crate::app::DiffFile {
            path: "new_file.rs".into(),
            insertions: 0,
            deletions: 0,
            untracked: true,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_mixed_tracked_and_untracked() {
        let files = vec![
            crate::app::DiffFile {
                path: "src/app.rs".into(),
                insertions: 10,
                deletions: 2,
                untracked: false,
            },
            crate::app::DiffFile {
                path: "src/new.rs".into(),
                insertions: 0,
                deletions: 0,
                untracked: true,
            },
        ];
        let lines = super::build_diff_tree_lines(&files, 40);
        // 1 directory header + 2 files
        assert_eq!(lines.len(), 3);
    }

    // ── Snapshot with deletion-only diff ─────────────────────────────

    #[test]
    fn browse_mode_with_deletion_only_diff() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("worker-1", AgentType::Claude)];
        app.selected = 0;
        app.preview.set_text("preview content".to_string());

        let stats = crate::logs::SessionStats {
            turns: 5,
            tokens_in: 5000,
            tokens_out: 1000,
            edits: 2,
            ..Default::default()
        };
        app.session_stats
            .insert("hydra-testproj-worker-1".to_string(), stats);

        app.diff_files = vec![crate::app::DiffFile {
            path: "old.rs".into(),
            insertions: 0,
            deletions: 20,
            untracked: false,
        }];

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_stats() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("worker-1", AgentType::Claude)];
        app.selected = 0;
        app.preview.set_text("some preview content".to_string());

        // Populate global stats (machine-wide, drives stats block visibility)
        app.global_stats.tokens_in = 28200;
        app.global_stats.tokens_out = 6500;
        app.global_stats.tokens_cache_read = 500;
        app.global_stats.tokens_cache_write = 100;

        // Per-session stats (edits are still hydra-specific)
        let stats1 = crate::logs::SessionStats {
            turns: 12,
            edits: 5,
            bash_cmds: 3,
            ..Default::default()
        };
        app.session_stats
            .insert("hydra-testproj-worker-1".to_string(), stats1);

        let stats2 = crate::logs::SessionStats {
            turns: 8,
            edits: 3,
            bash_cmds: 2,
            ..Default::default()
        };
        app.session_stats
            .insert("hydra-testproj-worker-2".to_string(), stats2);

        // Per-file git diff stats
        app.diff_files = vec![
            crate::app::DiffFile {
                path: "src/app.rs".into(),
                insertions: 45,
                deletions: 12,
                untracked: false,
            },
            crate::app::DiffFile {
                path: "src/ui.rs".into(),
                insertions: 30,
                deletions: 5,
                untracked: false,
            },
            crate::app::DiffFile {
                path: "README.md".into(),
                insertions: 8,
                deletions: 0,
                untracked: false,
            },
            crate::app::DiffFile {
                path: "src/new_mod.rs".into(),
                insertions: 0,
                deletions: 0,
                untracked: true,
            },
        ];

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    // ── diff tree resilience tests ──────────────────────────────────

    #[test]
    fn diff_tree_utf8_filename() {
        // Non-ASCII filename that would panic with byte slicing
        let files = vec![crate::app::DiffFile {
            path: "src/café.rs".into(),
            insertions: 5,
            deletions: 2,
            untracked: false,
        }];
        // Width narrow enough to force truncation
        let lines = super::build_diff_tree_lines(&files, 10);
        assert!(
            !lines.is_empty(),
            "should produce lines for UTF-8 filenames"
        );
    }

    #[test]
    fn diff_tree_width_zero() {
        let files = vec![crate::app::DiffFile {
            path: "src/app.rs".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // width=0 should not panic
        let lines = super::build_diff_tree_lines(&files, 0);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_width_one() {
        let files = vec![crate::app::DiffFile {
            path: "src/app.rs".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // width=1 should not panic
        let lines = super::build_diff_tree_lines(&files, 1);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_trailing_slash_path() {
        let files = vec![crate::app::DiffFile {
            path: "src/".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // Trailing slash produces empty basename — should be skipped
        let lines = super::build_diff_tree_lines(&files, 40);
        assert!(lines.is_empty(), "trailing-slash path should be skipped");
    }

    #[test]
    fn truncate_chars_ascii() {
        assert_eq!(super::truncate_chars("hello", 3), "hel");
        assert_eq!(super::truncate_chars("hi", 10), "hi");
    }

    #[test]
    fn truncate_chars_unicode() {
        assert_eq!(super::truncate_chars("café", 3), "caf");
        assert_eq!(super::truncate_chars("日本語テスト", 3), "日本語");
    }

    #[test]
    fn browse_mode_copy_mode_help_bar() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("s1", AgentType::Claude)];
        app.preview.set_text("test output".to_string());
        app.mouse_captured = false; // copy mode enabled

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    // ── inset_rect tests ──

    #[test]
    fn inset_rect_basic() {
        let area = Rect::new(10, 20, 100, 50);
        let inset = super::inset_rect(area, 5);
        assert_eq!(inset.x, 15);
        assert_eq!(inset.y, 25);
        assert_eq!(inset.width, 90);
        assert_eq!(inset.height, 40);
    }

    #[test]
    fn inset_rect_zero_margin() {
        let area = Rect::new(0, 0, 80, 24);
        let inset = super::inset_rect(area, 0);
        assert_eq!(inset, area);
    }

    #[test]
    fn inset_rect_margin_exceeds_size() {
        let area = Rect::new(0, 0, 10, 8);
        let inset = super::inset_rect(area, 20);
        // saturating_sub means width and height become 0
        assert_eq!(inset.width, 0);
        assert_eq!(inset.height, 0);
        assert_eq!(inset.x, 20); // saturating_add
        assert_eq!(inset.y, 20);
    }

    // ── render_conversation tests ───────────────────────────────────

    #[test]
    fn conversation_empty() {
        let entries = std::collections::VecDeque::new();
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_basic() {
        use crate::logs::ConversationEntry;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back(ConversationEntry::UserMessage {
            text: "Fix the bug".to_string(),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "I'll fix that for you.".to_string(),
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Edit".to_string(),
            details: Some("id=t1 | file=src/main.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/main.rs".to_string()],
            summary: Some("updated file successfully".to_string()),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "Done! The bug is fixed.".to_string(),
        });
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_tool_heavy() {
        use crate::logs::ConversationEntry;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back(ConversationEntry::UserMessage {
            text: "Refactor the module".to_string(),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "Let me read the files first.".to_string(),
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Read".to_string(),
            details: Some("path=src/app.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/app.rs".to_string()],
            summary: None,
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Read".to_string(),
            details: Some("path=src/ui.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/ui.rs".to_string()],
            summary: None,
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Edit".to_string(),
            details: Some("id=t3 | file=src/app.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/app.rs".to_string(), "src/ui.rs".to_string()],
            summary: Some("2 files modified".to_string()),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "Refactoring complete.".to_string(),
        });
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_with_unparsed_logs() {
        use crate::logs::ConversationEntry;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back(ConversationEntry::UserMessage {
            text: "show me diagnostics".to_string(),
        });
        entries.push_back(ConversationEntry::Unparsed {
            reason: "Malformed JSONL".to_string(),
            raw: "{\"type\":\"assistant\" BROKEN".to_string(),
        });
        entries.push_back(ConversationEntry::QueueOperation {
            operation: "enqueue".to_string(),
            task_id: Some("task-1".to_string()),
        });
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_with_progress_system_and_snapshot() {
        use crate::logs::ConversationEntry;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back(ConversationEntry::Progress {
            kind: "waiting_for_task".to_string(),
            detail: "Run integration suite (local_bash)".to_string(),
        });
        entries.push_back(ConversationEntry::SystemEvent {
            subtype: "api_error".to_string(),
            detail: "API error | attempt 2/10 | retry in 536ms".to_string(),
        });
        entries.push_back(ConversationEntry::FileHistorySnapshot {
            tracked_files: 4,
            files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            is_update: true,
        });

        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("PROGRESS (waiting_for_task)"));
        assert!(rendered.contains("SYSTEM (api_error)"));
        assert!(rendered.contains("FILE SNAPSHOT"));
        assert!(rendered.contains("update: 4 tracked file(s)"));
        assert!(rendered.contains("... +2 more"));
    }
}
