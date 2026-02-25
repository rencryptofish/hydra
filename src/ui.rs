pub mod state;

mod conversation;
mod diff;
mod help;
mod modals;
mod preview;
mod sidebar;
mod stats;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    Frame,
};

use crate::app::{Mode, UiApp};

// Re-exports for backward compatibility (benchmarks, lib.rs)
pub use conversation::render_conversation;
pub use diff::build_diff_tree_lines;
pub use preview::draw_preview;
pub use sidebar::draw_sidebar;
pub use stats::draw_stats;

#[derive(Clone, Copy, Debug, Default)]
pub struct UiLayout {
    pub main: Rect,
    pub help: Rect,
    pub sidebar: Rect,
    pub preview: Rect,
}

pub fn compute_layout(frame_area: Rect) -> UiLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame_area);

    let main = chunks[0];
    let help = chunks[1];
    let panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
        .split(main);

    UiLayout {
        main,
        help,
        sidebar: panels[0],
        preview: panels[1],
    }
}

pub fn draw(frame: &mut Frame, app: &UiApp) {
    let layout = compute_layout(frame.area());

    draw_sidebar(frame, app, layout.sidebar);
    draw_preview(frame, app, layout.preview);
    help::draw_help_bar(frame, app, layout.help);

    // Draw modal overlays
    match app.mode {
        Mode::NewSessionAgent => modals::draw_agent_select(frame, app),
        Mode::ConfirmDelete => modals::draw_confirm_delete(frame, app),
        _ => {}
    }
}

/// Truncate a string to at most `max` characters (Unicode-safe).
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{backend::TestBackend, Terminal};

    use crate::app::{Mode, StateSnapshot, UiApp};
    use crate::session::{AgentType, Session, SessionStatus};

    fn make_app() -> UiApp {
        UiApp::new_test()
    }

    /// Get a mutable reference to the snapshot for test setup.
    fn snap(app: &mut UiApp) -> &mut StateSnapshot {
        Arc::make_mut(&mut app.snapshot)
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
        snap(&mut app).sessions = vec![
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
        snap(&mut app).sessions = vec![make_session("doomed-session", AgentType::Claude)];
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
        snap(&mut app).sessions = vec![make_session("active-session", AgentType::Claude)];
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
        snap(&mut app).sessions = vec![
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
        snap(&mut app).sessions = vec![session];
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
        let s = snap(&mut app);
        s.sessions = vec![
            make_session("worker-1", AgentType::Claude),
            make_session("worker-2", AgentType::Codex),
        ];
        s.last_messages.insert(
            "hydra-testproj-worker-1".to_string(),
            "I'll help you with that task.".to_string(),
        );
        app.selected = 0;
        app.preview.set_text("preview".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_long_last_message_truncated() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        let s = snap(&mut app);
        s.sessions = vec![make_session("worker-1", AgentType::Claude)];
        s.last_messages.insert(
            "hydra-testproj-worker-1".to_string(),
            "This is a very long message that should be truncated at fifty characters to fit sidebar".to_string(),
        );
        app.selected = 0;
        app.preview.set_text("preview".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn preview_scrolling_renders() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        snap(&mut app).sessions = vec![make_session("s1", AgentType::Claude)];
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
    fn truncate_chars_ascii() {
        assert_eq!(super::truncate_chars("hello", 3), "hel");
        assert_eq!(super::truncate_chars("hi", 10), "hi");
    }

    #[test]
    fn truncate_chars_unicode() {
        assert_eq!(super::truncate_chars("café", 3), "caf");
        assert_eq!(super::truncate_chars("日本語テスト", 3), "日本語");
    }

    // ── Snapshot with deletion-only diff ─────────────────────────────

    #[test]
    fn browse_mode_with_deletion_only_diff() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        let s = snap(&mut app);
        s.sessions = vec![make_session("worker-1", AgentType::Claude)];
        s.session_stats.insert(
            "hydra-testproj-worker-1".to_string(),
            crate::logs::SessionStats {
                turns: 5,
                tokens_in: 5000,
                tokens_out: 1000,
                edits: 2,
                ..Default::default()
            },
        );
        s.diff_files = vec![crate::app::DiffFile {
            path: "old.rs".into(),
            insertions: 0,
            deletions: 20,
            untracked: false,
        }];
        app.selected = 0;
        app.preview.set_text("preview content".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_with_stats() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        let s = snap(&mut app);
        s.sessions = vec![make_session("worker-1", AgentType::Claude)];

        // Populate global stats (machine-wide, drives stats block visibility)
        s.global_stats.tokens_in = 28200;
        s.global_stats.tokens_out = 6500;
        s.global_stats.tokens_cache_read = 500;
        s.global_stats.tokens_cache_write = 100;

        // Per-session stats (edits are still hydra-specific)
        s.session_stats.insert(
            "hydra-testproj-worker-1".to_string(),
            crate::logs::SessionStats {
                turns: 12,
                edits: 5,
                bash_cmds: 3,
                ..Default::default()
            },
        );
        s.session_stats.insert(
            "hydra-testproj-worker-2".to_string(),
            crate::logs::SessionStats {
                turns: 8,
                edits: 3,
                bash_cmds: 2,
                ..Default::default()
            },
        );

        // Per-file git diff stats
        s.diff_files = vec![
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

        app.selected = 0;
        app.preview.set_text("some preview content".to_string());

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }

    #[test]
    fn browse_mode_copy_mode_help_bar() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut app = make_app();
        snap(&mut app).sessions = vec![make_session("s1", AgentType::Claude)];
        app.preview.set_text("test output".to_string());
        app.mouse_captured = false; // copy mode enabled

        terminal.draw(|f| super::draw(f, &app)).unwrap();
        let output = buffer_to_string(&terminal);

        insta::assert_snapshot!(output);
    }
}
