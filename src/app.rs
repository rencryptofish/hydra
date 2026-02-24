use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::logs::{ConversationEntry, GlobalStats, SessionStats};
use crate::session::{AgentType, Session};
use crate::ui::state::{ComposeState, PreviewState};
use crate::ui::UiLayout;

pub use crate::models::DiffFile;
pub use crate::system::git::parse_diff_numstat;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Browse,
    Compose,
    NewSessionAgent,
    ConfirmDelete,
}

/// Command from UI → Backend.
#[derive(Debug)]
pub enum BackendCommand {
    CreateSession {
        agent_type: AgentType,
    },
    DeleteSession {
        tmux_name: String,
        name: String,
    },
    SendCompose {
        tmux_name: String,
        text: String,
    },
    SendKeys {
        tmux_name: String,
        key: String,
    },
    SendInterrupt {
        tmux_name: String,
    },
    SendLiteralKeys {
        tmux_name: String,
        text: String,
    },
    RequestPreview {
        tmux_name: String,
        wants_scrollback: bool,
    },
    Quit,
}

/// Snapshot of backend state sent to UI for rendering.
/// Uses latest-value semantics via `watch` channel.
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot {
    pub sessions: Vec<Session>,
    pub last_messages: HashMap<String, String>,
    pub session_stats: HashMap<String, SessionStats>,
    pub global_stats: GlobalStats,
    pub diff_files: Vec<DiffFile>,
    pub conversations: HashMap<String, VecDeque<ConversationEntry>>,
    pub status_message: Option<String>,
}

/// Preview data sent from Backend → UI.
#[derive(Debug, Clone)]
pub enum PreviewData {
    Conversation(VecDeque<ConversationEntry>),
    PaneCapture(String),
}

/// Preview data sent from Backend → UI.
#[derive(Debug, Clone)]
pub struct PreviewUpdate {
    pub tmux_name: String,
    pub data: PreviewData,
    pub has_scrollback: bool,
}

/// UI-only application state, separated from I/O.
/// Receives state snapshots from the Backend actor via channels.
pub struct UiApp {
    // Snapshot-derived fields (updated via poll_state)
    pub sessions: Vec<Session>,
    pub last_messages: HashMap<String, String>,
    pub session_stats: HashMap<String, SessionStats>,
    pub global_stats: GlobalStats,
    pub diff_files: Vec<DiffFile>,
    pub conversations: HashMap<String, VecDeque<ConversationEntry>>,
    pub status_message: Option<String>,

    // Local UI state
    pub selected: usize,
    pub mode: Mode,
    pub agent_selection: usize,
    pub should_quit: bool,
    pub preview: PreviewState,
    pub compose: ComposeState,
    pub mouse_captured: bool,
    pub diff_tree_cache: RefCell<(Vec<DiffFile>, usize, Vec<ratatui::text::Line<'static>>)>,

    // Preview cache (session → latest PreviewUpdate)
    preview_cache: HashMap<String, PreviewUpdate>,
    requested_preview: Option<String>,

    // Channels
    cmd_tx: tokio::sync::mpsc::Sender<BackendCommand>,
    state_rx: tokio::sync::watch::Receiver<Arc<StateSnapshot>>,
    preview_rx: tokio::sync::mpsc::Receiver<PreviewUpdate>,
}

impl UiApp {
    pub fn new(
        state_rx: tokio::sync::watch::Receiver<Arc<StateSnapshot>>,
        preview_rx: tokio::sync::mpsc::Receiver<PreviewUpdate>,
        cmd_tx: tokio::sync::mpsc::Sender<BackendCommand>,
    ) -> Self {
        Self {
            sessions: Vec::new(),
            last_messages: HashMap::new(),
            session_stats: HashMap::new(),
            global_stats: GlobalStats::default(),
            diff_files: Vec::new(),
            conversations: HashMap::new(),
            status_message: None,
            selected: 0,
            mode: Mode::Browse,
            agent_selection: 0,
            should_quit: false,
            preview: PreviewState::new(),
            compose: ComposeState::new(),
            mouse_captured: true,
            diff_tree_cache: RefCell::new((Vec::new(), 0, Vec::new())),
            preview_cache: HashMap::new(),
            requested_preview: None,
            cmd_tx,
            state_rx,
            preview_rx,
        }
    }

    /// Test constructor with dummy channels.
    #[cfg(test)]
    pub fn new_test() -> Self {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(1);
        let (_state_tx, state_rx) = tokio::sync::watch::channel(Arc::new(StateSnapshot::default()));
        let (_preview_tx, preview_rx) = tokio::sync::mpsc::channel(1);
        Self::new(state_rx, preview_rx, cmd_tx)
    }

    /// Poll for new state from the backend. Call once per tick.
    pub fn poll_state(&mut self) {
        if self.state_rx.has_changed().unwrap_or(false) {
            let snapshot = self.state_rx.borrow_and_update().clone();
            self.apply_full_snapshot(&snapshot);
        }

        while let Ok(update) = self.preview_rx.try_recv() {
            if self.requested_preview.as_deref() == Some(update.tmux_name.as_str()) {
                self.requested_preview = None;
            }
            self.preview_cache.insert(update.tmux_name.clone(), update);
        }

        self.refresh_preview_from_cache();
    }

    fn apply_full_snapshot(&mut self, snapshot: &StateSnapshot) {
        self.sessions = snapshot.sessions.clone();
        self.last_messages = snapshot.last_messages.clone();
        self.session_stats = snapshot.session_stats.clone();
        self.global_stats = snapshot.global_stats.clone();
        self.diff_files = snapshot.diff_files.clone();
        self.conversations = snapshot.conversations.clone();
        self.status_message = snapshot.status_message.clone();
        self.prune_non_live_state();
    }

    fn prune_non_live_state(&mut self) {
        if self.sessions.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }

        let live_keys: HashSet<&String> = self.sessions.iter().map(|s| &s.tmux_name).collect();
        self.last_messages.retain(|k, _| live_keys.contains(k));
        self.session_stats.retain(|k, _| live_keys.contains(k));
        self.conversations.retain(|k, _| live_keys.contains(k));
        self.preview_cache.retain(|k, _| live_keys.contains(k));
        if self
            .requested_preview
            .as_ref()
            .is_some_and(|tmux_name| !live_keys.iter().any(|live| *live == tmux_name))
        {
            self.requested_preview = None;
        }
    }

    /// Update preview from cached data for the currently selected session.
    pub fn refresh_preview_from_cache(&mut self) {
        if let Some(session) = self.sessions.get(self.selected) {
            if let Some(update) = self.preview_cache.get(&session.tmux_name).cloned() {
                self.apply_preview_update(&update);
            } else {
                let tmux_name = session.tmux_name.clone();
                self.clear_preview();
                self.request_preview(&tmux_name, false);
            }
        } else {
            self.clear_preview();
        }
    }

    fn apply_preview_update(&mut self, update: &PreviewUpdate) {
        match &update.data {
            PreviewData::Conversation(entries) => {
                let text = crate::ui::render_conversation(entries);
                self.preview.line_count = text.lines.len() as u16;
                self.preview.text = Some(text);
                self.preview.content.clear();
            }
            PreviewData::PaneCapture(content) => {
                self.preview.line_count = content.lines().count().min(u16::MAX as usize) as u16;
                self.preview.text = ansi_to_tui::IntoText::into_text(content).ok();
                self.preview.content = content.clone();
            }
        }
    }

    fn clear_preview(&mut self) {
        self.preview.text = None;
        self.preview.content.clear();
        self.preview.line_count = 0;
    }

    fn request_preview(&mut self, tmux_name: &str, wants_scrollback: bool) {
        if !wants_scrollback && self.requested_preview.as_deref() == Some(tmux_name) {
            return;
        }

        self.queue_command(BackendCommand::RequestPreview {
            tmux_name: tmux_name.to_string(),
            wants_scrollback,
        });

        if !wants_scrollback {
            self.requested_preview = Some(tmux_name.to_string());
        }
    }

    fn queue_command(&mut self, command: BackendCommand) {
        match self.cmd_tx.try_send(command) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
                let tx = self.cmd_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(command).await;
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.status_message = Some("Backend disconnected".to_string());
                self.should_quit = true;
            }
        }
    }

    /// Handle a key event. Synchronous — sends BackendCommand for I/O.
    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Compose => self.handle_compose_key(key),
            Mode::NewSessionAgent => self.handle_agent_select_key(key.code),
            Mode::ConfirmDelete => self.handle_confirm_delete_key(key.code),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyModifiers;
        match key.code {
            KeyCode::Char('q') => {
                self.queue_command(BackendCommand::Quit);
                self.should_quit = true;
            }
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Enter => self.enter_compose(),
            KeyCode::Char('n') => self.start_new_session(),
            KeyCode::Char('d') => self.request_delete(),
            KeyCode::Char('c') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mouse_captured = !self.mouse_captured;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(session) = self.sessions.get(self.selected) {
                    let tmux_name = session.tmux_name.clone();
                    self.queue_command(BackendCommand::SendInterrupt { tmux_name });
                }
            }
            _ => {}
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyModifiers;
        match key.code {
            KeyCode::Esc => self.exit_compose(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.exit_compose();
            }
            KeyCode::Enter => self.send_compose_message(),
            KeyCode::Backspace => self.compose.backspace(),
            KeyCode::Left => self.compose.move_left(),
            KeyCode::Right => self.compose.move_right(),
            KeyCode::Char(ch) => self.compose.insert_char(ch),
            _ => {}
        }
    }

    fn send_compose_message(&mut self) {
        if let Some(session) = self.sessions.get(self.selected) {
            let text = self.compose.text();
            let tmux_name = session.tmux_name.clone();
            let is_codex = session.agent_type == AgentType::Codex;

            if text.trim().is_empty() {
                // Codex startup and resume flows can require a bare Enter key.
                if is_codex {
                    self.queue_command(BackendCommand::SendKeys {
                        tmux_name,
                        key: "Enter".to_string(),
                    });
                }
                self.exit_compose();
                return;
            }

            self.queue_command(BackendCommand::SendCompose { tmux_name, text });
        }
        self.exit_compose();
    }

    fn handle_agent_select_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => {
                let agents = AgentType::all();
                if let Some(agent_type) = agents.get(self.agent_selection) {
                    self.queue_command(BackendCommand::CreateSession {
                        agent_type: agent_type.clone(),
                    });
                }
                self.mode = Mode::Browse;
            }
            KeyCode::Esc => self.cancel_mode(),
            KeyCode::Char('j') | KeyCode::Down => self.agent_select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.agent_select_prev(),
            _ => {}
        }
    }

    fn handle_confirm_delete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') => {
                if let Some(session) = self.sessions.get(self.selected) {
                    let tmux_name = session.tmux_name.clone();
                    let name = session.name.clone();
                    self.queue_command(BackendCommand::DeleteSession { tmux_name, name });
                }
                self.mode = Mode::Browse;
                if self.selected > 0 && self.selected >= self.sessions.len().saturating_sub(1) {
                    self.selected = self.sessions.len().saturating_sub(2);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => self.cancel_mode(),
            _ => {}
        }
    }

    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
            self.preview.reset_on_selection_change();
            self.refresh_preview_from_cache();
            if let Some(session) = self.sessions.get(self.selected) {
                let tmux_name = session.tmux_name.clone();
                self.request_preview(&tmux_name, false);
            }
        }
    }

    pub fn select_prev(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = if self.selected == 0 {
                self.sessions.len() - 1
            } else {
                self.selected - 1
            };
            self.preview.reset_on_selection_change();
            self.refresh_preview_from_cache();
            if let Some(session) = self.sessions.get(self.selected) {
                let tmux_name = session.tmux_name.clone();
                self.request_preview(&tmux_name, false);
            }
        }
    }

    pub fn enter_compose(&mut self) {
        if !self.sessions.is_empty() {
            self.compose.reset();
            self.mode = Mode::Compose;
        }
    }

    pub fn exit_compose(&mut self) {
        self.mode = Mode::Browse;
        self.compose.reset();
    }

    pub fn start_new_session(&mut self) {
        self.mode = Mode::NewSessionAgent;
        self.agent_selection = 0;
        self.status_message = None;
    }

    pub fn request_delete(&mut self) {
        if !self.sessions.is_empty() {
            self.mode = Mode::ConfirmDelete;
            self.status_message = None;
        }
    }

    pub fn cancel_mode(&mut self) {
        self.mode = Mode::Browse;
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

    pub fn scroll_preview_up(&mut self) {
        self.preview.scroll_up();
    }

    pub fn scroll_preview_down(&mut self) {
        self.preview.scroll_down();
    }

    /// Handle mouse events. Synchronous.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, layout: &UiLayout) {
        let pos = Position::new(mouse.column, mouse.row);
        let sidebar = layout.sidebar;
        let preview = layout.preview;

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
                        let mut current_group: Option<u8> = None;
                        for (i, session) in self.sessions.iter().enumerate() {
                            let group = session.status.sort_order();
                            if current_group != Some(group) {
                                current_group = Some(group);
                                if row_offset == cumulative {
                                    break;
                                }
                                cumulative += 1;
                            }
                            let item_height = if self.last_messages.contains_key(&session.tmux_name)
                            {
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
                                self.preview.reset_on_selection_change();
                                self.refresh_preview_from_cache();
                                if let Some(session) = self.sessions.get(self.selected) {
                                    let tmux_name = session.tmux_name.clone();
                                    self.request_preview(&tmux_name, false);
                                }
                            }
                        }
                    } else if preview.contains(pos) {
                        self.enter_compose();
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
            Mode::Compose => match mouse.kind {
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
                MouseEventKind::Down(MouseButton::Left) => {
                    if inner(preview).contains(pos) {
                        self.preview.scroll_offset = 0;
                    } else {
                        self.exit_compose();
                    }
                }
                MouseEventKind::Down(_) => {
                    if !inner(preview).contains(pos) {
                        self.exit_compose();
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn make_app() -> (UiApp, tokio::sync::mpsc::Receiver<BackendCommand>) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(8);
        let (_state_tx, state_rx) = tokio::sync::watch::channel(Arc::new(StateSnapshot::default()));
        let (_preview_tx, preview_rx) = tokio::sync::mpsc::channel(8);
        (UiApp::new(state_rx, preview_rx, cmd_tx), cmd_rx)
    }

    fn make_session(agent_type: AgentType) -> Session {
        Session {
            name: "alpha".to_string(),
            tmux_name: "hydra-test-alpha".to_string(),
            agent_type,
            status: crate::session::SessionStatus::Idle,
            task_elapsed: None,
            _alive: true,
        }
    }

    #[test]
    fn compose_enter_empty_codex_sends_enter_key() {
        let (mut app, mut cmd_rx) = make_app();
        app.sessions = vec![make_session(AgentType::Codex)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Browse);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::SendKeys { tmux_name, key }) => {
                assert_eq!(tmux_name, "hydra-test-alpha");
                assert_eq!(key, "Enter");
            }
            other => panic!("expected SendKeys Enter, got {other:?}"),
        }
    }

    #[test]
    fn compose_enter_with_text_sends_compose_command() {
        let (mut app, mut cmd_rx) = make_app();
        app.sessions = vec![make_session(AgentType::Codex)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Browse);
        match cmd_rx.try_recv() {
            Ok(BackendCommand::SendCompose { tmux_name, text }) => {
                assert_eq!(tmux_name, "hydra-test-alpha");
                assert_eq!(text, "hi");
            }
            other => panic!("expected SendCompose, got {other:?}"),
        }
    }

    #[test]
    fn preview_cache_miss_clears_preview_and_requests_update() {
        let (mut app, mut cmd_rx) = make_app();
        app.sessions = vec![make_session(AgentType::Claude)];
        app.preview.set_text("stale preview".to_string());

        app.refresh_preview_from_cache();

        assert!(app.preview.content.is_empty());
        assert_eq!(app.preview.line_count, 0);
        assert!(app.preview.text.is_none());

        match cmd_rx.try_recv() {
            Ok(BackendCommand::RequestPreview {
                tmux_name,
                wants_scrollback,
            }) => {
                assert_eq!(tmux_name, "hydra-test-alpha");
                assert!(!wants_scrollback);
            }
            other => panic!("expected RequestPreview, got {other:?}"),
        }
    }

    #[test]
    fn poll_state_prunes_preview_cache_for_removed_sessions() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(8);
        let (state_tx, state_rx) = tokio::sync::watch::channel(Arc::new(StateSnapshot::default()));
        let (preview_tx, preview_rx) = tokio::sync::mpsc::channel(8);
        let mut app = UiApp::new(state_rx, preview_rx, cmd_tx);

        let session = make_session(AgentType::Claude);
        state_tx
            .send(Arc::new(StateSnapshot {
                sessions: vec![session.clone()],
                ..StateSnapshot::default()
            }))
            .unwrap();
        preview_tx
            .try_send(PreviewUpdate {
                tmux_name: session.tmux_name.clone(),
                data: PreviewData::PaneCapture("hello".to_string()),
                has_scrollback: false,
            })
            .unwrap();
        app.poll_state();
        assert!(app.preview_cache.contains_key(&session.tmux_name));

        state_tx.send(Arc::new(StateSnapshot::default())).unwrap();
        app.poll_state();
        assert!(!app.preview_cache.contains_key(&session.tmux_name));
    }
}
