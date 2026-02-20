use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, Mode};
use crate::logs::{format_cost, format_tokens};
use crate::session::{format_duration, AgentType, SessionStatus};

pub fn draw(frame: &mut Frame, app: &App) {
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
        Mode::Browse | Mode::Attached => {}
    }
}

fn status_color(status: &SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::Green,
        SessionStatus::Running => Color::Red,
        SessionStatus::Exited => Color::Yellow,
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
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
        draw_diff_tree(frame, &tree_lines, tree_area);
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

fn draw_stats(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_preview(frame: &mut Frame, app: &App, area: Rect) {
    let title = if let Some(session) = app.sessions.get(app.selected) {
        format!(" {} ", session.name)
    } else {
        " Preview ".to_string()
    };

    let mut preview_area = area;
    let (border_style, border_type, border_title) = if app.mode == Mode::Attached {
        let active_style = Style::default()
            .fg(Color::LightGreen)
            .add_modifier(Modifier::BOLD);

        // Use three nested borders in attached mode so the active pane is obvious.
        if area.width >= 7 && area.height >= 7 {
            let attached_title = if let Some(session) = app.sessions.get(app.selected) {
                format!(" {} [ATTACHED] ", session.name)
            } else {
                " Preview [ATTACHED] ".to_string()
            };

            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Thick)
                    .title(attached_title)
                    .border_style(active_style),
                area,
            );

            let middle_area = inset_rect(area, 1);
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Double)
                    .border_style(active_style),
                middle_area,
            );

            preview_area = inset_rect(area, 2);
            (active_style, BorderType::Plain, String::new())
        } else {
            (active_style, BorderType::Double, title)
        }
    } else {
        (Style::default().fg(Color::Cyan), BorderType::Plain, title)
    };

    let inner_height = preview_area.height.saturating_sub(2);
    let total_lines = app.preview_line_count;
    let max_scroll_offset = total_lines.saturating_sub(inner_height);
    let capped_offset = app.preview_scroll_offset.min(max_scroll_offset);
    let scroll_y = max_scroll_offset.saturating_sub(capped_offset);

    let preview = Paragraph::new(app.preview.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border_type)
                .title(border_title)
                .border_style(border_style),
        )
        .scroll((scroll_y, 0));

    frame.render_widget(preview, preview_area);
}

fn draw_help_bar(frame: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.mode {
        Mode::Browse if !app.mouse_captured => "SELECT TEXT TO COPY  |  c: exit copy mode",
        Mode::Browse => "j/k: navigate  Enter: attach  n: new  d: delete  c: copy  q: quit",
        Mode::Attached => "Esc: detach  (keys forwarded to session)",
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

fn inset_rect(area: Rect, margin: u16) -> Rect {
    let double = margin.saturating_mul(2);
    Rect::new(
        area.x.saturating_add(margin),
        area.y.saturating_add(margin),
        area.width.saturating_sub(double),
        area.height.saturating_sub(double),
    )
}

fn draw_agent_select(frame: &mut Frame, app: &App) {
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

fn draw_confirm_delete(frame: &mut Frame, app: &App) {
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
    use ratatui::{backend::TestBackend, Terminal};

    use crate::app::{App, Mode};
    use crate::session::{AgentType, Session, SessionStatus};
    use crate::tmux::SessionManager;

    struct NoopSessionManager;

    #[async_trait::async_trait]
    impl SessionManager for NoopSessionManager {
        async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> {
            Ok(vec![])
        }
        async fn create_session(
            &self,
            _: &str,
            _: &str,
            _: &AgentType,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn capture_pane(&self, _: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        async fn kill_session(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    fn make_app() -> App {
        App::new_with_manager(
            "testproj".to_string(),
            "/tmp/test".to_string(),
            Box::new(NoopSessionManager),
        )
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
        app.set_preview_text("some preview content".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_empty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.set_preview_text("No sessions. Press 'n' to create one.".to_string());

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
    fn attached_mode() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![make_session("active-session", AgentType::Claude)];
        app.selected = 0;
        app.mode = Mode::Attached;
        app.set_preview_text("$ claude\nHello, how can I help?".to_string());

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
        app.set_preview_text("No sessions. Press 'n' to create one.".to_string());

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
        app.set_preview_text("running session output".to_string());

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
        app.set_preview_text("working...".to_string());

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
        app.set_preview_text("preview".to_string());
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
        app.set_preview_text("preview".to_string());
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
        app.set_preview_text(
            (0..50)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.preview_scroll_offset = 10;

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
        app.set_preview_text("preview content".to_string());

        let mut stats = crate::logs::SessionStats::default();
        stats.turns = 5;
        stats.tokens_in = 5000;
        stats.tokens_out = 1000;
        stats.edits = 2;
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
        app.set_preview_text("some preview content".to_string());

        // Populate global stats (machine-wide, drives stats block visibility)
        app.global_stats.tokens_in = 28200;
        app.global_stats.tokens_out = 6500;
        app.global_stats.tokens_cache_read = 500;
        app.global_stats.tokens_cache_write = 100;

        // Per-session stats (edits are still hydra-specific)
        let mut stats1 = crate::logs::SessionStats::default();
        stats1.turns = 12;
        stats1.edits = 5;
        stats1.bash_cmds = 3;
        app.session_stats
            .insert("hydra-testproj-worker-1".to_string(), stats1);

        let mut stats2 = crate::logs::SessionStats::default();
        stats2.turns = 8;
        stats2.edits = 3;
        stats2.bash_cmds = 2;
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
        app.set_preview_text("test output".to_string());
        app.mouse_captured = false; // copy mode enabled

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }
}
