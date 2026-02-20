use std::cell::Cell;
use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::session::{AgentType, Session, SessionStatus};
use crate::tmux::{SessionManager, TmuxSessionManager};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Browse,
    Attached,
    NewSessionName,
    NewSessionAgent,
    ConfirmDelete,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub preview: String,
    pub mode: Mode,
    pub input: String,
    pub agent_selection: usize,
    pub new_session_name: String,
    pub should_quit: bool,
    pub project_id: String,
    pub cwd: String,
    pub status_message: Option<String>,
    pub sidebar_area: Cell<Rect>,
    pub preview_area: Cell<Rect>,
    pub preview_scroll_offset: u16,
    prev_captures: HashMap<String, String>,
    task_starts: HashMap<String, Instant>,
    task_last_active: HashMap<String, Instant>,
    pub last_messages: HashMap<String, String>,
    log_uuids: HashMap<String, String>,
    message_tick: u8,
    manager: Box<dyn SessionManager>,
}

impl App {
    pub fn new(project_id: String, cwd: String) -> Self {
        Self::new_with_manager(project_id, cwd, Box::new(TmuxSessionManager::new()))
    }

    pub fn new_with_manager(
        project_id: String,
        cwd: String,
        manager: Box<dyn SessionManager>,
    ) -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            preview: String::new(),
            mode: Mode::Browse,
            input: String::new(),
            agent_selection: 0,
            new_session_name: String::new(),
            should_quit: false,
            project_id,
            cwd,
            status_message: None,
            sidebar_area: Cell::new(Rect::default()),
            preview_area: Cell::new(Rect::default()),
            preview_scroll_offset: 0,
            prev_captures: HashMap::new(),
            task_starts: HashMap::new(),
            task_last_active: HashMap::new(),
            last_messages: HashMap::new(),
            log_uuids: HashMap::new(),
            message_tick: 0,
            manager,
        }
    }

    pub async fn refresh_sessions(&mut self) {
        let pid = self.project_id.clone();
        let result = self.manager.list_sessions(&pid).await;
        match result {
            Ok(mut sessions) => {
                let now = Instant::now();

                for session in &mut sessions {
                    let name = session.tmux_name.clone();

                    // Determine Running vs Idle by comparing pane content
                    if session.status != SessionStatus::Exited {
                        let content = self
                            .manager
                            .capture_pane(&name)
                            .await
                            .unwrap_or_default();
                        let prev = self.prev_captures.get(&name);
                        session.status = if prev.is_some_and(|p| p == &content) {
                            SessionStatus::Idle
                        } else {
                            SessionStatus::Running
                        };
                        self.prev_captures.insert(name.clone(), content);
                    }

                    // Track task elapsed time
                    match session.status {
                        SessionStatus::Running => {
                            self.task_starts.entry(name.clone()).or_insert(now);
                            self.task_last_active.insert(name.clone(), now);
                            let start = self.task_starts[&name];
                            session.task_elapsed = Some(now.duration_since(start));
                        }
                        SessionStatus::Idle => {
                            if let (Some(&start), Some(&last)) = (
                                self.task_starts.get(&name),
                                self.task_last_active.get(&name),
                            ) {
                                if now.duration_since(last).as_secs() < 5 {
                                    // Brief pause — show frozen duration from last activity
                                    session.task_elapsed = Some(last.duration_since(start));
                                } else {
                                    // Task is done — clear timer
                                    self.task_starts.remove(&name);
                                    self.task_last_active.remove(&name);
                                }
                            }
                        }
                        SessionStatus::Exited => {
                            self.task_starts.remove(&name);
                            self.task_last_active.remove(&name);
                        }
                    }
                }
                self.sessions = sessions;
            }
            Err(e) => {
                self.status_message = Some(format!("Error listing sessions: {e}"));
            }
        }
        // Keep selected index in bounds
        if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
            self.selected = self.sessions.len() - 1;
        }
    }

    pub async fn refresh_preview(&mut self) {
        let tmux_name = self
            .sessions
            .get(self.selected)
            .map(|s| s.tmux_name.clone());
        if let Some(tmux_name) = tmux_name {
            let result = self.manager.capture_pane_scrollback(&tmux_name).await;
            match result {
                Ok(content) => self.preview = content,
                Err(_) => self.preview = String::from("[unable to capture pane]"),
            }
        } else {
            self.preview = String::from("No sessions. Press 'n' to create one.");
        }
    }

    pub async fn refresh_messages(&mut self) {
        self.message_tick = self.message_tick.wrapping_add(1);
        // Run every 20 ticks (~5 seconds at 250ms interval)
        if self.message_tick % 20 != 0 {
            return;
        }

        for session in &self.sessions {
            let tmux_name = &session.tmux_name;

            // Try to resolve UUID if not cached
            if !self.log_uuids.contains_key(tmux_name) {
                if let Some(uuid) = crate::logs::resolve_session_uuid(tmux_name).await {
                    self.log_uuids.insert(tmux_name.clone(), uuid);
                }
            }

            // Read last message if UUID is known
            if let Some(uuid) = self.log_uuids.get(tmux_name).cloned() {
                if let Some(msg) = crate::logs::read_last_assistant_message(&self.cwd, &uuid) {
                    self.last_messages.insert(tmux_name.clone(), msg);
                }
            }
        }
    }

    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
            self.preview_scroll_offset = 0;
        }
    }

    pub fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = if self.selected == 0 {
                self.sessions.len() - 1
            } else {
                self.selected - 1
            };
            self.preview_scroll_offset = 0;
        }
    }

    pub fn scroll_preview_up(&mut self) {
        self.preview_scroll_offset = self.preview_scroll_offset.saturating_add(3);
    }

    pub fn scroll_preview_down(&mut self) {
        self.preview_scroll_offset = self.preview_scroll_offset.saturating_sub(3);
    }

    pub fn attach_selected(&mut self) {
        if !self.sessions.is_empty() {
            self.mode = Mode::Attached;
        }
    }

    pub fn detach(&mut self) {
        self.mode = Mode::Browse;
    }

    pub fn start_new_session(&mut self) {
        self.mode = Mode::NewSessionName;
        self.input.clear();
        self.status_message = None;
    }

    pub fn submit_session_name(&mut self) {
        let name = self.input.trim().to_string();
        if name.is_empty() {
            self.status_message = Some("Session name cannot be empty".to_string());
            return;
        }
        // Check for name conflicts
        if self.sessions.iter().any(|s| s.name == name) {
            self.status_message = Some(format!("Session '{name}' already exists"));
            return;
        }
        self.new_session_name = name;
        self.mode = Mode::NewSessionAgent;
        self.agent_selection = 0;
        self.input.clear();
        self.status_message = None;
    }

    pub async fn confirm_new_session(&mut self) {
        let agents = AgentType::all();
        let agent = agents[self.agent_selection].clone();
        let pid = self.project_id.clone();
        let name = self.new_session_name.clone();
        let cwd = self.cwd.clone();

        let result = self.manager.create_session(&pid, &name, &agent, &cwd).await;
        match result {
            Ok(_) => {
                self.status_message = Some(format!(
                    "Created session '{}' with {}",
                    name, agent
                ));
                self.refresh_sessions().await;
                // Select the newly created session
                if let Some(idx) = self.sessions.iter().position(|s| s.name == name) {
                    self.selected = idx;
                }
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to create session: {e}"));
            }
        }
        self.mode = Mode::Browse;
        self.new_session_name.clear();
    }

    pub fn request_delete(&mut self) {
        if !self.sessions.is_empty() {
            self.mode = Mode::ConfirmDelete;
            self.status_message = None;
        }
    }

    pub async fn confirm_delete(&mut self) {
        if let Some(session) = self.sessions.get(self.selected) {
            let name = session.name.clone();
            let tmux_name = session.tmux_name.clone();
            let result = self.manager.kill_session(&tmux_name).await;
            match result {
                Ok(_) => {
                    self.status_message = Some(format!("Killed session '{name}'"));
                }
                Err(e) => {
                    self.status_message = Some(format!("Failed to kill session: {e}"));
                }
            }
        }
        self.mode = Mode::Browse;
        self.refresh_sessions().await;
    }

    pub fn cancel_mode(&mut self) {
        self.mode = Mode::Browse;
        self.input.clear();
        self.status_message = None;
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        let pos = Position::new(mouse.column, mouse.row);
        let sidebar = self.sidebar_area.get();
        let preview = self.preview_area.get();

        fn inner(r: Rect) -> Rect {
            if r.width < 2 || r.height < 2 {
                Rect::default()
            } else {
                Rect::new(r.x + 1, r.y + 1, r.width - 2, r.height - 2)
            }
        }

        match self.mode {
            Mode::Browse => match mouse.kind {
                MouseEventKind::Down(_) => {
                    let sidebar_inner = inner(sidebar);
                    if sidebar_inner.contains(pos) {
                        let row_offset = (mouse.row - sidebar_inner.y) as usize;
                        let mut cumulative = 0usize;
                        let mut target_idx = None;
                        for (i, session) in self.sessions.iter().enumerate() {
                            let item_height =
                                if self.last_messages.contains_key(&session.tmux_name) {
                                    2
                                } else {
                                    1
                                };
                            if row_offset < cumulative + item_height {
                                target_idx = Some(i);
                                break;
                            }
                            cumulative += item_height;
                        }
                        if let Some(idx) = target_idx {
                            if self.selected != idx {
                                self.selected = idx;
                                self.preview_scroll_offset = 0;
                            }
                        }
                    } else if preview.contains(pos) {
                        self.attach_selected();
                    }
                }
                MouseEventKind::ScrollUp => {
                    if preview.contains(pos) {
                        self.scroll_preview_up();
                    } else if sidebar.contains(pos) {
                        self.select_prev();
                    }
                }
                MouseEventKind::ScrollDown => {
                    if preview.contains(pos) {
                        self.scroll_preview_down();
                    } else if sidebar.contains(pos) {
                        self.select_next();
                    }
                }
                _ => {}
            },
            Mode::Attached => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if preview.contains(pos) {
                        self.scroll_preview_up();
                    }
                }
                MouseEventKind::ScrollDown => {
                    if preview.contains(pos) {
                        self.scroll_preview_down();
                    }
                }
                MouseEventKind::Down(_) => {
                    if !inner(preview).contains(pos) {
                        self.detach();
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    pub fn agent_select_next(&mut self) {
        let count = AgentType::all().len();
        self.agent_selection = (self.agent_selection + 1) % count;
    }

    pub fn agent_select_prev(&mut self) {
        let count = AgentType::all().len();
        self.agent_selection = if self.agent_selection == 0 {
            count - 1
        } else {
            self.agent_selection - 1
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentType, Session};
    use crate::tmux::SessionManager;

    // ── Mock and helpers ─────────────────────────────────────────────

    struct MockSessionManager {
        sessions: Vec<Session>,
        create_result: Result<String, String>,
    }

    impl MockSessionManager {
        fn new() -> Self {
            Self {
                sessions: vec![],
                create_result: Ok("mock-session".to_string()),
            }
        }
        fn with_sessions(sessions: Vec<Session>) -> Self {
            Self {
                sessions,
                create_result: Ok("mock-session".to_string()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionManager for MockSessionManager {
        async fn list_sessions(&self, _project_id: &str) -> anyhow::Result<Vec<Session>> {
            Ok(self.sessions.clone())
        }
        async fn create_session(
            &self,
            _project_id: &str,
            _name: &str,
            _agent: &AgentType,
            _cwd: &str,
        ) -> anyhow::Result<String> {
            self.create_result.clone().map_err(|e| anyhow::anyhow!(e))
        }
        async fn capture_pane(&self, _tmux_name: &str) -> anyhow::Result<String> {
            Ok("mock pane content".to_string())
        }
        async fn kill_session(&self, _tmux_name: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_keys(&self, _tmux_name: &str, _key: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn send_mouse(
            &self,
            _tmux_name: &str,
            _kind: &str,
            _button: u8,
            _x: u16,
            _y: u16,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn capture_pane_scrollback(&self, _tmux_name: &str) -> anyhow::Result<String> {
            Ok("mock pane content".to_string())
        }
    }

    fn test_app() -> App {
        App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        )
    }

    fn test_app_with_sessions(sessions: Vec<Session>) -> App {
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions.clone())),
        );
        app.sessions = sessions;
        app
    }

    fn make_session(name: &str, agent: AgentType) -> Session {
        Session {
            name: name.to_string(),
            tmux_name: format!("hydra-testid-{name}"),
            agent_type: agent,
            status: crate::session::SessionStatus::Idle,
            task_elapsed: None,
            _alive: true,
        }
    }

    // ── Navigation tests ─────────────────────────────────────────────

    #[test]
    fn select_next_wraps_around() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
            make_session("c", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.selected = 2; // last item
        app.select_next();
        assert_eq!(app.selected, 0, "select_next should wrap from last to first");
    }

    #[test]
    fn select_prev_wraps_around() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
            make_session("c", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.selected = 0; // first item
        app.select_prev();
        assert_eq!(app.selected, 2, "select_prev should wrap from first to last");
    }

    #[test]
    fn select_next_with_empty_sessions_does_nothing() {
        let mut app = test_app();
        assert!(app.sessions.is_empty());
        app.selected = 0;
        app.select_next();
        assert_eq!(
            app.selected, 0,
            "select_next on empty sessions should not change selected"
        );
    }

    #[test]
    fn select_prev_with_empty_sessions_does_nothing() {
        let mut app = test_app();
        assert!(app.sessions.is_empty());
        app.selected = 0;
        app.select_prev();
        assert_eq!(
            app.selected, 0,
            "select_prev on empty sessions should not change selected"
        );
    }

    #[test]
    fn select_next_updates_index_correctly() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
            make_session("c", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        assert_eq!(app.selected, 0);
        app.select_next();
        assert_eq!(app.selected, 1);
        app.select_next();
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn select_prev_updates_index_correctly() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
            make_session("c", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.selected = 2;
        app.select_prev();
        assert_eq!(app.selected, 1);
        app.select_prev();
        assert_eq!(app.selected, 0);
    }

    // ── Mode transition tests ────────────────────────────────────────

    #[test]
    fn start_new_session_sets_mode_and_clears_input() {
        let mut app = test_app();
        app.input = "leftover".to_string();
        app.status_message = Some("old status".to_string());
        app.start_new_session();
        assert_eq!(app.mode, Mode::NewSessionName);
        assert!(app.input.is_empty(), "input should be cleared");
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared"
        );
    }

    #[test]
    fn submit_session_name_valid_transitions_to_new_session_agent() {
        let mut app = test_app();
        app.mode = Mode::NewSessionName;
        app.input = "my-session".to_string();
        app.submit_session_name();
        assert_eq!(app.mode, Mode::NewSessionAgent);
        assert_eq!(app.new_session_name, "my-session");
        assert!(
            app.input.is_empty(),
            "input should be cleared after valid submission"
        );
        assert_eq!(app.agent_selection, 0, "agent_selection should be reset to 0");
    }

    #[test]
    fn submit_session_name_empty_stays_in_new_session_name() {
        let mut app = test_app();
        app.mode = Mode::NewSessionName;
        app.input = "".to_string();
        app.submit_session_name();
        assert_eq!(
            app.mode,
            Mode::NewSessionName,
            "mode should stay NewSessionName for empty input"
        );
        assert!(
            app.status_message.is_some(),
            "status_message should be set for empty name"
        );
        assert!(
            app.status_message.as_ref().unwrap().contains("empty"),
            "error message should mention empty: got {:?}",
            app.status_message
        );
    }

    #[test]
    fn submit_session_name_whitespace_only_stays_in_new_session_name() {
        let mut app = test_app();
        app.mode = Mode::NewSessionName;
        app.input = "   ".to_string();
        app.submit_session_name();
        assert_eq!(
            app.mode,
            Mode::NewSessionName,
            "mode should stay NewSessionName for whitespace-only input"
        );
        assert!(app.status_message.is_some(), "status_message should be set");
    }

    #[test]
    fn submit_session_name_duplicate_stays_in_new_session_name() {
        let sessions = vec![make_session("existing", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::NewSessionName;
        app.input = "existing".to_string();
        app.submit_session_name();
        assert_eq!(
            app.mode,
            Mode::NewSessionName,
            "mode should stay NewSessionName for duplicate name"
        );
        assert!(
            app.status_message.is_some(),
            "status_message should be set for duplicate"
        );
        assert!(
            app.status_message.as_ref().unwrap().contains("already exists"),
            "error should mention 'already exists': got {:?}",
            app.status_message
        );
    }

    #[test]
    fn cancel_mode_returns_to_browse_and_clears_input() {
        let mut app = test_app();
        app.mode = Mode::NewSessionName;
        app.input = "some-input".to_string();
        app.status_message = Some("error message".to_string());
        app.cancel_mode();
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.input.is_empty(), "input should be cleared");
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared"
        );
    }

    #[test]
    fn cancel_mode_from_new_session_agent_returns_to_browse() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.cancel_mode();
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn cancel_mode_from_confirm_delete_returns_to_browse() {
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::ConfirmDelete;
        app.cancel_mode();
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn request_delete_with_sessions_transitions_to_confirm_delete() {
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.status_message = Some("old".to_string());
        app.request_delete();
        assert_eq!(app.mode, Mode::ConfirmDelete);
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared"
        );
    }

    #[test]
    fn request_delete_with_no_sessions_stays_in_browse() {
        let mut app = test_app();
        assert!(app.sessions.is_empty());
        app.request_delete();
        assert_eq!(
            app.mode,
            Mode::Browse,
            "mode should remain Browse when no sessions"
        );
    }

    #[test]
    fn attach_selected_with_sessions_transitions_to_attached() {
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.attach_selected();
        assert_eq!(app.mode, Mode::Attached);
    }

    #[test]
    fn attach_selected_with_no_sessions_stays_in_browse() {
        let mut app = test_app();
        assert!(app.sessions.is_empty());
        app.attach_selected();
        assert_eq!(
            app.mode,
            Mode::Browse,
            "mode should remain Browse when no sessions"
        );
    }

    #[test]
    fn detach_transitions_to_browse() {
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.detach();
        assert_eq!(app.mode, Mode::Browse);
    }

    // ── Agent selection tests ────────────────────────────────────────

    #[test]
    fn agent_select_next_wraps_around() {
        let mut app = test_app();
        let agent_count = AgentType::all().len();
        app.agent_selection = agent_count - 1; // last agent
        app.agent_select_next();
        assert_eq!(
            app.agent_selection, 0,
            "agent_select_next should wrap from last to first"
        );
    }

    #[test]
    fn agent_select_prev_wraps_around() {
        let mut app = test_app();
        app.agent_selection = 0; // first agent
        app.agent_select_prev();
        let agent_count = AgentType::all().len();
        assert_eq!(
            app.agent_selection,
            agent_count - 1,
            "agent_select_prev should wrap from first to last"
        );
    }

    #[test]
    fn agent_select_next_increments() {
        let mut app = test_app();
        app.agent_selection = 0;
        app.agent_select_next();
        assert_eq!(app.agent_selection, 1);
    }

    #[test]
    fn agent_select_prev_decrements() {
        let mut app = test_app();
        let agent_count = AgentType::all().len();
        app.agent_selection = agent_count - 1;
        app.agent_select_prev();
        assert_eq!(app.agent_selection, agent_count - 2);
    }

    // ── Session creation flow tests ──────────────────────────────────

    #[test]
    fn full_new_session_flow_name_then_agent() {
        let mut app = test_app();

        // Step 1: start new session
        app.start_new_session();
        assert_eq!(app.mode, Mode::NewSessionName);
        assert!(app.input.is_empty());

        // Step 2: type a name
        app.input = "my-new-session".to_string();

        // Step 3: submit the name
        app.submit_session_name();
        assert_eq!(app.mode, Mode::NewSessionAgent);
        assert_eq!(app.new_session_name, "my-new-session");
        assert!(app.input.is_empty());
        assert_eq!(app.agent_selection, 0);

        // Step 4: select an agent (optional cycle)
        app.agent_select_next();
        assert_eq!(app.agent_selection, 1);
    }

    #[tokio::test]
    async fn confirm_new_session_returns_to_browse_and_clears_name() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.new_session_name = "test-session".to_string();
        app.agent_selection = 0;

        app.confirm_new_session().await;

        assert_eq!(
            app.mode,
            Mode::Browse,
            "mode should return to Browse after confirm"
        );
        assert!(
            app.new_session_name.is_empty(),
            "new_session_name should be cleared after confirm"
        );
        // The mock create_session returns Ok, so status should reflect success
        assert!(app.status_message.is_some());
        assert!(
            app.status_message.as_ref().unwrap().contains("Created session"),
            "status should indicate session was created: got {:?}",
            app.status_message
        );
    }

    // ── Delete flow tests ────────────────────────────────────────────

    #[tokio::test]
    async fn delete_flow_request_then_confirm() {
        let sessions = vec![make_session("doomed", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);

        // Step 1: request delete
        app.request_delete();
        assert_eq!(app.mode, Mode::ConfirmDelete);

        // Step 2: confirm delete (mock kill_session returns Ok)
        app.confirm_delete().await;
        assert_eq!(
            app.mode,
            Mode::Browse,
            "mode should return to Browse after confirm_delete"
        );
        assert!(app.status_message.is_some());
        assert!(
            app.status_message.as_ref().unwrap().contains("Killed session"),
            "status should indicate session was killed: got {:?}",
            app.status_message
        );
    }

    #[test]
    fn delete_flow_request_then_cancel() {
        let sessions = vec![make_session("safe", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);

        app.request_delete();
        assert_eq!(app.mode, Mode::ConfirmDelete);

        app.cancel_mode();
        assert_eq!(app.mode, Mode::Browse, "cancel should return to Browse");
        assert!(app.status_message.is_none());
    }

    // ── Quit tests ───────────────────────────────────────────────────

    #[test]
    fn should_quit_starts_false() {
        let app = test_app();
        assert!(!app.should_quit, "should_quit should start as false");
    }

    #[test]
    fn should_quit_stays_true_once_set() {
        let mut app = test_app();
        app.should_quit = true;
        assert!(app.should_quit, "should_quit should remain true once set");
        // Verify it doesn't reset unexpectedly after other operations
        app.select_next();
        assert!(
            app.should_quit,
            "should_quit should still be true after other operations"
        );
    }

    // ── Additional edge-case tests ───────────────────────────────────

    #[test]
    fn select_next_with_single_session_stays_at_zero() {
        let sessions = vec![make_session("only", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        assert_eq!(app.selected, 0);
        app.select_next();
        assert_eq!(app.selected, 0, "single session: next should wrap to 0");
    }

    #[test]
    fn select_prev_with_single_session_stays_at_zero() {
        let sessions = vec![make_session("only", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        assert_eq!(app.selected, 0);
        app.select_prev();
        assert_eq!(app.selected, 0, "single session: prev should wrap to 0");
    }

    #[test]
    fn new_app_starts_in_browse_mode() {
        let app = test_app();
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn new_app_has_empty_sessions() {
        let app = test_app();
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn new_app_has_zero_selected() {
        let app = test_app();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn new_app_has_empty_input() {
        let app = test_app();
        assert!(app.input.is_empty());
    }

    #[test]
    fn new_app_has_no_status_message() {
        let app = test_app();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn submit_session_name_clears_status_on_valid_name() {
        let mut app = test_app();
        app.mode = Mode::NewSessionName;
        app.status_message = Some("previous error".to_string());
        app.input = "valid-name".to_string();
        app.submit_session_name();
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared on valid submission"
        );
    }

    #[test]
    fn multiple_cancel_mode_calls_remain_in_browse() {
        let mut app = test_app();
        app.mode = Mode::NewSessionName;
        app.cancel_mode();
        assert_eq!(app.mode, Mode::Browse);
        app.cancel_mode();
        assert_eq!(
            app.mode,
            Mode::Browse,
            "repeated cancel should stay in Browse"
        );
    }

    #[test]
    fn request_delete_clears_status_message() {
        let sessions = vec![make_session("s", AgentType::Codex)];
        let mut app = test_app_with_sessions(sessions);
        app.status_message = Some("old msg".to_string());
        app.request_delete();
        assert!(
            app.status_message.is_none(),
            "request_delete should clear status_message"
        );
    }

    #[test]
    fn detach_from_already_browse_stays_browse() {
        let mut app = test_app();
        assert_eq!(app.mode, Mode::Browse);
        app.detach();
        assert_eq!(
            app.mode,
            Mode::Browse,
            "detach from Browse should remain Browse"
        );
    }

    #[tokio::test]
    async fn refresh_preview_with_no_sessions_shows_placeholder() {
        let mut app = test_app();
        app.refresh_preview().await;
        assert!(
            app.preview.contains("No sessions"),
            "preview with no sessions should show placeholder, got: {:?}",
            app.preview
        );
    }

    #[tokio::test]
    async fn refresh_preview_with_session_captures_pane() {
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.refresh_preview().await;
        assert_eq!(
            app.preview, "mock pane content",
            "preview should contain mock pane content"
        );
    }

    #[tokio::test]
    async fn refresh_sessions_populates_from_manager() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Codex),
        ];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions.clone())),
        );
        assert!(app.sessions.is_empty(), "sessions should start empty");
        app.refresh_sessions().await;
        assert_eq!(
            app.sessions.len(),
            2,
            "refresh_sessions should populate from manager"
        );
    }
}
