use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::{App, Mode};
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
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(main_area);

    app.sidebar_area.set(panels[0]);
    app.preview_area.set(panels[1]);

    draw_sidebar(frame, app, panels[0]);
    draw_preview(frame, app, panels[1]);
    draw_help_bar(frame, app, help_area);

    // Draw modal overlays
    match app.mode {
        Mode::NewSessionName => draw_name_input(frame, app),
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

    frame.render_widget(list, area);
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
        Mode::NewSessionName => "Enter: confirm  Esc: cancel",
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

fn draw_name_input(frame: &mut Frame, app: &App) {
    let area = centered_rect(40, 5, frame.area());
    frame.render_widget(Clear, area);

    let text = format!(" > {}_", app.input);
    let input = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" New Session Name ")
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(input, area);
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
    fn new_session_name_modal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        app.mode = Mode::NewSessionName;
        app.input = "my-session".to_string();

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
}
