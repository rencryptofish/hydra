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
    // Check if any session has stats to show
    let has_stats = app.session_stats.values().any(|st| st.turns > 0);

    let stats_height = if has_stats { 3 } else { 0 }; // 1 line + top/bottom border

    let tree_lines = build_diff_tree_lines(&app.diff_files, area.width.saturating_sub(2) as usize);
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

    // Draw session list
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, session)| {
            let marker = if i == app.selected { ">> " } else { "   " };
            let name_style = if i == app.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let mut spans = vec![
                Span::styled(marker, name_style),
                Span::styled("● ", Style::default().fg(status_color(&session.status))),
                Span::styled(format!("{} [{}]", session.name, session.agent_type), name_style),
            ];
            if let Some(elapsed) = session.task_elapsed {
                spans.push(Span::styled(
                    format!(" {}", format_duration(elapsed)),
                    Style::default().fg(Color::DarkGray),
                ));
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
                lines.push(Line::from(Span::styled(
                    display,
                    Style::default().fg(Color::DarkGray),
                )));
            }
            ListItem::new(lines)
        })
        .collect();

    let session_count = app.sessions.len();
    let title = format!(" Sessions ({session_count}) ");
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(list, list_area);

    // Draw aggregate stats across all sessions
    if has_stats {
        draw_stats(frame, app, stats_area);
    }

    // Draw diff tree
    if !tree_lines.is_empty() {
        draw_diff_tree(frame, &tree_lines, tree_area);
    }
}

/// Build lines for a compact diff tree grouped by directory.
/// Files sorted by path, grouped under directory headers.
/// Output example:
///   src/
///    app.rs       +45-12
///    ui.rs        +30-5
///   README.md     +3
fn build_diff_tree_lines<'a>(diff_files: &[crate::app::DiffFile], width: usize) -> Vec<Line<'a>> {
    if diff_files.is_empty() {
        return vec![];
    }

    let dim = Style::default().fg(Color::DarkGray);
    let green = Style::default().fg(Color::Green);
    let red = Style::default().fg(Color::Red);

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

        // Emit directory header if changed
        let dir_str = dir.unwrap_or("");
        let show_dir = match current_dir {
            Some(prev) => prev != dir_str,
            None => dir.is_some(),
        };
        if show_dir {
            if let Some(d) = dir {
                let display: String = if d.len() > inner_w {
                    d[..inner_w].to_string()
                } else {
                    d.to_string()
                };
                lines.push(Line::from(Span::styled(format!(" {display}"), dim)));
            }
            current_dir = Some(dir_str);
        }

        // Build diff stat string
        let stat = format_compact_diff(f.insertions, f.deletions);
        let indent = if dir.is_some() { "  " } else { " " };

        // Compute available space for filename
        let stat_len = stat.chars().count();
        let prefix_len = indent.len();
        let available = inner_w.saturating_sub(prefix_len + stat_len + 1);

        let name: String = if basename.len() > available && available > 3 {
            format!("{}…", &basename[..available - 1])
        } else if basename.len() > available {
            basename[..available.min(basename.len())].to_string()
        } else {
            basename.to_string()
        };

        let padding = inner_w.saturating_sub(prefix_len + name.len() + stat_len);
        let pad_str: String = " ".repeat(padding);

        let mut spans = vec![
            Span::styled(format!("{indent}{name}{pad_str}"), dim),
        ];

        // Color the stat: green for +, red for -
        if f.insertions > 0 {
            spans.push(Span::styled(format!("+{}", f.insertions), green));
        }
        if f.deletions > 0 {
            spans.push(Span::styled(format!("-{}", f.deletions), red));
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
    // Aggregate across all sessions
    let mut total_cost = 0.0;
    let mut total_tokens = 0u64;
    let mut total_edits = 0u16;

    for stats in app.session_stats.values() {
        total_cost += stats.cost_usd();
        total_tokens += stats.tokens_in + stats.tokens_out;
        total_edits += stats.edits;
    }

    let dim = Style::default().fg(Color::DarkGray);
    let val = Style::default().fg(Color::White);

    // Total diff across all files
    let total_diff: u32 = app.diff_files.iter().map(|f| f.insertions + f.deletions).sum();

    let mut spans = vec![
        Span::styled(format_cost(total_cost), Style::default().fg(Color::Green)),
        Span::styled(format!(" {}", format_tokens(total_tokens)), val),
        Span::styled(format!(" {}✎", total_edits), val),
    ];

    if total_diff > 0 {
        spans.push(Span::styled(format!(" Δ{total_diff}"), dim));
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

    let (border_style, border_type) = if app.mode == Mode::Attached {
        (
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            BorderType::Thick,
        )
    } else {
        (Style::default().fg(Color::Cyan), BorderType::Plain)
    };

    let inner_height = area.height.saturating_sub(2) as u16;
    let total_lines = app.preview.lines().count() as u16;
    let max_scroll_offset = total_lines.saturating_sub(inner_height);
    let capped_offset = app.preview_scroll_offset.min(max_scroll_offset);
    let scroll_y = max_scroll_offset.saturating_sub(capped_offset);

    let preview = Paragraph::new(app.preview.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border_type)
                .title(title)
                .border_style(border_style),
        )
        .scroll((scroll_y, 0));

    frame.render_widget(preview, area);
}

fn draw_help_bar(frame: &mut Frame, app: &App, area: Rect) {
    let help_text = match app.mode {
        Mode::Browse => "j/k: navigate  Enter: attach  n: new  d: delete  q: quit",
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
                Style::default().fg(Color::White)
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
        async fn send_mouse(&self, _: &str, _: &str, _: u8, _: u16, _: u16) -> anyhow::Result<()> {
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
        app.preview = "some preview content".to_string();

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_empty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.preview = "No sessions. Press 'n' to create one.".to_string();

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
        app.sessions = vec![
            make_session("doomed-session", AgentType::Claude),
        ];
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
        app.sessions = vec![
            make_session("active-session", AgentType::Claude),
        ];
        app.selected = 0;
        app.mode = Mode::Attached;
        app.preview = "$ claude\nHello, how can I help?".to_string();

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
        app.preview = "No sessions. Press 'n' to create one.".to_string();

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
        app.preview = "running session output".to_string();

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
        app.preview = "working...".to_string();

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
        app.preview = "preview".to_string();
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
        app.preview = "preview".to_string();
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
        app.preview = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
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
        assert_eq!(super::status_color(&SessionStatus::Idle), ratatui::style::Color::Green);
        assert_eq!(super::status_color(&SessionStatus::Running), ratatui::style::Color::Red);
        assert_eq!(super::status_color(&SessionStatus::Exited), ratatui::style::Color::Yellow);
    }

    #[test]
    fn centered_rect_normal() {
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let result = super::centered_rect(40, 10, area);
        assert_eq!(result.width, 40);
        assert_eq!(result.height, 10);
        assert_eq!(result.x, 20); // (80 - 40) / 2
        assert_eq!(result.y, 7);  // (24 - 10) / 2
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

    #[test]
    fn browse_mode_with_stats() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.sessions = vec![
            make_session("worker-1", AgentType::Claude),
        ];
        app.selected = 0;
        app.preview = "some preview content".to_string();

        // Populate stats for multiple sessions (aggregated in display)
        let mut stats1 = crate::logs::SessionStats::default();
        stats1.turns = 12;
        stats1.tokens_in = 18200;
        stats1.tokens_out = 4500;
        stats1.edits = 5;
        stats1.bash_cmds = 3;
        stats1.touch_file("/src/main.rs".to_string());
        stats1.touch_file("/src/app.rs".to_string());
        app.session_stats.insert("hydra-testproj-worker-1".to_string(), stats1);

        let mut stats2 = crate::logs::SessionStats::default();
        stats2.turns = 8;
        stats2.tokens_in = 10000;
        stats2.tokens_out = 2000;
        stats2.edits = 3;
        stats2.bash_cmds = 2;
        stats2.touch_file("/src/main.rs".to_string()); // overlaps with worker-1
        stats2.touch_file("/src/ui.rs".to_string());
        app.session_stats.insert("hydra-testproj-worker-2".to_string(), stats2);

        // Per-file git diff stats
        app.diff_files = vec![
            crate::app::DiffFile { path: "src/app.rs".into(), insertions: 45, deletions: 12 },
            crate::app::DiffFile { path: "src/ui.rs".into(), insertions: 30, deletions: 5 },
            crate::app::DiffFile { path: "README.md".into(), insertions: 8, deletions: 0 },
        ];

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }
}
