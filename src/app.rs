use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

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

#[derive(Debug, Clone)]
struct PendingDelete {
    tmux_name: String,
    name: String,
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
    // Backend state (shared via Arc — no per-field cloning)
    pub snapshot: Arc<StateSnapshot>,
    // Local copy of status_message (needs local mutation)
    pub status_message: Option<String>,
    status_message_set_at: Option<Instant>,

    // Local UI state
    pub selected: usize,
    pub mode: Mode,
    pub agent_selection: usize,
    pub should_quit: bool,
    pub preview: PreviewState,
    pub compose: ComposeState,
    compose_target_tmux: Option<String>,
    compose_target_name: Option<String>,
    compose_target_missing: bool,
    pending_delete: Option<PendingDelete>,
    pub mouse_captured: bool,
    pub needs_redraw: bool,
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
            snapshot: Arc::new(StateSnapshot::default()),
            status_message: None,
            status_message_set_at: None,
            selected: 0,
            mode: Mode::Browse,
            agent_selection: 0,
            should_quit: false,
            preview: PreviewState::new(),
            compose: ComposeState::new(),
            compose_target_tmux: None,
            compose_target_name: None,
            compose_target_missing: false,
            pending_delete: None,
            mouse_captured: true,
            needs_redraw: true,
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

    /// Set a status message with auto-clear timer.
    fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_message_set_at = Some(Instant::now());
    }

    /// Clear the status message and its timer.
    fn clear_status(&mut self) {
        self.status_message = None;
        self.status_message_set_at = None;
    }

    /// Poll for new state from the backend. Call once per tick.
    pub fn poll_state(&mut self) {
        if self.state_rx.has_changed().unwrap_or(false) {
            let snapshot = self.state_rx.borrow_and_update().clone();
            self.apply_snapshot(snapshot);
            self.needs_redraw = true;
        }

        let mut got_preview = false;
        while let Ok(update) = self.preview_rx.try_recv() {
            if self.requested_preview.as_deref() == Some(update.tmux_name.as_str()) {
                self.requested_preview = None;
            }
            self.preview_cache.insert(update.tmux_name.clone(), update);
            got_preview = true;
        }
        if got_preview {
            self.needs_redraw = true;
        }

        self.refresh_preview_from_cache();

        // Auto-clear status messages after 5 seconds
        if let Some(set_at) = self.status_message_set_at {
            if set_at.elapsed() > std::time::Duration::from_secs(5) {
                self.clear_status();
                self.needs_redraw = true;
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: Arc<StateSnapshot>) {
        let previous_selected_tmux = self
            .snapshot
            .sessions
            .get(self.selected)
            .map(|session| session.tmux_name.clone());

        // Only accept backend status when it has a new message.
        // Let the timer handle clearing (don't let backend's None stomp local messages).
        if let Some(msg) = &snapshot.status_message {
            if self.status_message.as_ref() != Some(msg) {
                self.set_status(msg.clone());
            }
        }
        self.snapshot = snapshot;
        self.prune_non_live_state(previous_selected_tmux.as_deref());
    }

    fn prune_non_live_state(&mut self, previous_selected_tmux: Option<&str>) {
        // Own the keys so we don't hold an immutable borrow on self.snapshot
        // across the mutable self.set_status() calls below.
        let live_keys: HashSet<String> = self
            .snapshot
            .sessions
            .iter()
            .map(|s| s.tmux_name.clone())
            .collect();
        let session_count = self.snapshot.sessions.len();
        let preferred_tmux = match self.mode {
            Mode::Compose => self.compose_target_tmux.as_deref(),
            Mode::ConfirmDelete => self
                .pending_delete
                .as_ref()
                .map(|target| target.tmux_name.as_str()),
            Mode::Browse | Mode::NewSessionAgent => previous_selected_tmux,
        };

        if let Some(tmux_name) = preferred_tmux {
            if let Some(idx) = self
                .snapshot
                .sessions
                .iter()
                .position(|session| session.tmux_name == tmux_name)
            {
                self.selected = idx;
            } else if session_count == 0 {
                self.selected = 0;
            } else if self.selected >= session_count {
                self.selected = session_count - 1;
            }
        } else if session_count == 0 {
            self.selected = 0;
        } else if self.selected >= session_count {
            self.selected = session_count - 1;
        }

        if let Some(target) = self.pending_delete.as_ref() {
            if !live_keys.contains(&target.tmux_name) {
                self.pending_delete = None;
                if self.mode == Mode::ConfirmDelete {
                    self.mode = Mode::Browse;
                    self.set_status("Delete target no longer exists".to_string());
                }
            }
        }

        if self.mode == Mode::Compose {
            if let Some(target_tmux) = self.compose_target_tmux.as_deref() {
                if live_keys.contains(target_tmux) {
                    self.compose_target_missing = false;
                } else if !self.compose_target_missing {
                    let name = self.compose_target_name.as_deref().unwrap_or(target_tmux);
                    self.set_status(format!(
                        "Compose target '{name}' is no longer available; draft preserved"
                    ));
                    self.compose_target_missing = true;
                }
            }
        } else {
            self.compose_target_missing = false;
        }

        self.preview_cache.retain(|k, _| live_keys.contains(k));
        if self
            .requested_preview
            .as_ref()
            .is_some_and(|tmux_name| !live_keys.contains(tmux_name))
        {
            self.requested_preview = None;
        }
    }

    /// Update preview from cached data for the currently selected session.
    pub fn refresh_preview_from_cache(&mut self) {
        if let Some(tmux_name) = self.active_preview_tmux() {
            if let Some(update) = self.preview_cache.get(&tmux_name).cloned() {
                self.apply_preview_update(&update);
            } else {
                self.clear_preview();
                if self
                    .snapshot
                    .sessions
                    .iter()
                    .any(|session| session.tmux_name == tmux_name)
                {
                    self.request_preview(&tmux_name, false);
                }
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

    fn active_preview_tmux(&self) -> Option<String> {
        match self.mode {
            Mode::Compose => self.compose_target_tmux.clone(),
            Mode::Browse | Mode::NewSessionAgent | Mode::ConfirmDelete => self
                .snapshot
                .sessions
                .get(self.selected)
                .map(|s| s.tmux_name.clone()),
        }
    }

    pub fn active_preview_name(&self) -> Option<&str> {
        if self.mode == Mode::Compose {
            if let Some(tmux_name) = self.compose_target_tmux.as_deref() {
                return self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|session| session.tmux_name == tmux_name)
                    .map(|session| session.name.as_str())
                    .or(self.compose_target_name.as_deref());
            }
            return self.compose_target_name.as_deref();
        }

        self.snapshot
            .sessions
            .get(self.selected)
            .map(|session| session.name.as_str())
    }

    pub fn confirm_delete_target_name(&self) -> Option<&str> {
        self.pending_delete
            .as_ref()
            .map(|target| target.name.as_str())
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
                self.set_status("Backend disconnected".to_string());
                self.should_quit = true;
            }
        }
    }

    /// Handle a key event. Synchronous — sends BackendCommand for I/O.
    pub fn handle_key(&mut self, key: KeyEvent) {
        self.needs_redraw = true;
        match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Compose => self.handle_compose_key(key),
            Mode::NewSessionAgent => self.handle_agent_select_key(key.code),
            Mode::ConfirmDelete => self.handle_confirm_delete_key(key.code),
        }
    }

    /// Handle a bracketed paste event. Only active in Compose mode.
    pub fn handle_paste(&mut self, text: String) {
        if self.mode == Mode::Compose {
            self.compose.insert_text(&text);
            self.needs_redraw = true;
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
                if let Some(session) = self.snapshot.sessions.get(self.selected) {
                    let tmux_name = session.tmux_name.clone();
                    self.queue_command(BackendCommand::SendInterrupt { tmux_name });
                } else {
                    self.set_status("No sessions".to_string());
                }
            }
            KeyCode::PageUp => self.preview.scroll_page_up(),
            KeyCode::PageDown => self.preview.scroll_page_down(),
            KeyCode::Home => self.preview.scroll_to_top(),
            KeyCode::End => self.preview.scroll_to_bottom(),
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
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.compose.insert_newline();
            }
            KeyCode::Enter => self.send_compose_message(),
            KeyCode::Backspace => self.compose.backspace(),
            KeyCode::Delete => self.compose.delete_forward(),
            KeyCode::Left => self.compose.move_left(),
            KeyCode::Right => self.compose.move_right(),
            KeyCode::Up => {
                // On the first line, Up navigates history instead of moving cursor.
                if self.compose.cursor_row == 0 {
                    self.compose.history_prev();
                } else {
                    self.compose.move_up();
                }
            }
            KeyCode::Down => {
                // On the last line while browsing history, Down navigates forward.
                if self.compose.cursor_row + 1 >= self.compose.lines.len()
                    && self.compose.history_index.is_some()
                {
                    self.compose.history_next();
                } else {
                    self.compose.move_down();
                }
            }
            KeyCode::Home => self.compose.move_home(),
            KeyCode::End => self.compose.move_end(),
            KeyCode::PageUp => self.preview.scroll_page_up(),
            KeyCode::PageDown => self.preview.scroll_page_down(),
            KeyCode::Char(ch) => self.compose.insert_char(ch),
            _ => {}
        }
    }

    fn send_compose_message(&mut self) {
        let Some(target_tmux) = self.compose_target_tmux.as_deref() else {
            self.set_status("Compose target is unavailable; draft preserved".to_string());
            return;
        };
        let Some(session) = self
            .snapshot
            .sessions
            .iter()
            .find(|session| session.tmux_name == target_tmux)
        else {
            self.set_status("Compose target is no longer available; draft preserved".to_string());
            self.compose_target_missing = true;
            return;
        };

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
            self.compose.reset();
            self.exit_compose();
            return;
        }

        self.compose.push_history(text.clone());
        self.queue_command(BackendCommand::SendCompose { tmux_name, text });
        self.compose.reset();
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
                if let Some(target) = self.pending_delete.take() {
                    self.queue_command(BackendCommand::DeleteSession {
                        tmux_name: target.tmux_name,
                        name: target.name,
                    });
                } else {
                    self.set_status("Delete target no longer exists".to_string());
                }
                self.mode = Mode::Browse;
                if self.selected > 0
                    && self.selected >= self.snapshot.sessions.len().saturating_sub(1)
                {
                    self.selected = self.snapshot.sessions.len().saturating_sub(2);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.pending_delete = None;
                self.cancel_mode();
            }
            _ => {}
        }
    }

    pub fn select_next(&mut self) {
        if !self.snapshot.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.snapshot.sessions.len();
            self.preview.reset_on_selection_change();
            self.refresh_preview_from_cache();
            if let Some(session) = self.snapshot.sessions.get(self.selected) {
                let tmux_name = session.tmux_name.clone();
                self.request_preview(&tmux_name, false);
            }
        }
    }

    pub fn select_prev(&mut self) {
        if !self.snapshot.sessions.is_empty() {
            self.selected = if self.selected == 0 {
                self.snapshot.sessions.len() - 1
            } else {
                self.selected - 1
            };
            self.preview.reset_on_selection_change();
            self.refresh_preview_from_cache();
            if let Some(session) = self.snapshot.sessions.get(self.selected) {
                let tmux_name = session.tmux_name.clone();
                self.request_preview(&tmux_name, false);
            }
        }
    }

    pub fn enter_compose(&mut self) {
        if self.snapshot.sessions.is_empty() {
            self.set_status("No sessions. Press 'n' to create one.".to_string());
            return;
        }
        if let Some(session) = self.snapshot.sessions.get(self.selected) {
            // Preserve draft across enter/exit cycles — only reset on successful send.
            self.compose_target_tmux = Some(session.tmux_name.clone());
            self.compose_target_name = Some(session.name.clone());
            self.compose_target_missing = false;
            self.mode = Mode::Compose;
        }
    }

    pub fn exit_compose(&mut self) {
        self.mode = Mode::Browse;
        // Draft is intentionally NOT cleared — preserved for the next enter_compose().
        self.compose_target_tmux = None;
        self.compose_target_name = None;
        self.compose_target_missing = false;
    }

    pub fn start_new_session(&mut self) {
        self.mode = Mode::NewSessionAgent;
        self.agent_selection = 0;
        self.clear_status();
    }

    pub fn request_delete(&mut self) {
        if self.snapshot.sessions.is_empty() {
            self.set_status("No sessions to delete".to_string());
            return;
        }
        if let Some(session) = self.snapshot.sessions.get(self.selected) {
            self.mode = Mode::ConfirmDelete;
            self.pending_delete = Some(PendingDelete {
                tmux_name: session.tmux_name.clone(),
                name: session.name.clone(),
            });
            self.clear_status();
        }
    }

    pub fn cancel_mode(&mut self) {
        if self.mode == Mode::ConfirmDelete {
            self.pending_delete = None;
        }
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
        self.needs_redraw = true;
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
                        for (i, session) in self.snapshot.sessions.iter().enumerate() {
                            let group = session.sort_order();
                            if current_group != Some(group) {
                                current_group = Some(group);
                                if row_offset == cumulative {
                                    break;
                                }
                                cumulative += 1;
                            }
                            let item_height =
                                if self.snapshot.last_messages.contains_key(&session.tmux_name) {
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
                                if let Some(session) = self.snapshot.sessions.get(self.selected) {
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
                    }
                }
                MouseEventKind::Down(_) => {}
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
impl UiApp {
    /// Test helper: get mutable access to the inner StateSnapshot.
    /// Only works when there's a single owner (always true in tests).
    pub fn snapshot_mut(&mut self) -> &mut StateSnapshot {
        Arc::make_mut(&mut self.snapshot)
    }

    pub fn apply_full_snapshot(&mut self, snapshot: &StateSnapshot) {
        self.apply_snapshot(Arc::new(snapshot.clone()));
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
        make_named_session("alpha", "hydra-test-alpha", agent_type)
    }

    fn make_named_session(name: &str, tmux_name: &str, agent_type: AgentType) -> Session {
        Session {
            name: name.to_string(),
            tmux_name: tmux_name.to_string(),
            agent_type,
            process_state: crate::session::ProcessState::Alive,
            agent_state: crate::session::AgentState::Idle,
            last_activity_at: std::time::Instant::now(),
            task_elapsed: None,
            _alive: true,
        }
    }

    #[test]
    fn compose_enter_empty_codex_sends_enter_key() {
        let (mut app, mut cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Codex)];
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
        app.snapshot_mut().sessions = vec![make_session(AgentType::Codex)];
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
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
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

    #[test]
    fn selection_tracks_same_session_when_order_changes() {
        let (mut app, _cmd_rx) = make_app();
        let alpha = make_named_session("alpha", "hydra-test-alpha", AgentType::Codex);
        let bravo = make_named_session("bravo", "hydra-test-bravo", AgentType::Claude);

        app.snapshot_mut().sessions = vec![alpha.clone(), bravo.clone()];
        app.selected = 1;

        app.apply_full_snapshot(&StateSnapshot {
            sessions: vec![bravo.clone(), alpha.clone()],
            ..StateSnapshot::default()
        });

        assert_eq!(app.selected, 0);
        assert_eq!(
            app.snapshot.sessions[app.selected].tmux_name,
            "hydra-test-bravo"
        );
    }

    #[test]
    fn compose_target_stays_bound_when_order_changes() {
        let (mut app, mut cmd_rx) = make_app();
        let alpha = make_named_session("alpha", "hydra-test-alpha", AgentType::Codex);
        let bravo = make_named_session("bravo", "hydra-test-bravo", AgentType::Claude);

        app.snapshot_mut().sessions = vec![alpha.clone(), bravo.clone()];
        app.selected = 1;
        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));

        app.apply_full_snapshot(&StateSnapshot {
            sessions: vec![bravo.clone(), alpha.clone()],
            ..StateSnapshot::default()
        });
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        match cmd_rx.try_recv() {
            Ok(BackendCommand::SendCompose { tmux_name, text }) => {
                assert_eq!(tmux_name, "hydra-test-bravo");
                assert_eq!(text, "hi");
            }
            other => panic!("expected SendCompose, got {other:?}"),
        }
    }

    #[test]
    fn confirm_delete_target_stays_bound_when_order_changes() {
        let (mut app, mut cmd_rx) = make_app();
        let alpha = make_named_session("alpha", "hydra-test-alpha", AgentType::Codex);
        let bravo = make_named_session("bravo", "hydra-test-bravo", AgentType::Claude);

        app.snapshot_mut().sessions = vec![alpha.clone(), bravo.clone()];
        app.selected = 1;
        app.request_delete();
        assert_eq!(app.confirm_delete_target_name(), Some("bravo"));

        app.apply_full_snapshot(&StateSnapshot {
            sessions: vec![bravo.clone(), alpha.clone()],
            ..StateSnapshot::default()
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        match cmd_rx.try_recv() {
            Ok(BackendCommand::DeleteSession { tmux_name, name }) => {
                assert_eq!(tmux_name, "hydra-test-bravo");
                assert_eq!(name, "bravo");
            }
            other => panic!("expected DeleteSession, got {other:?}"),
        }
    }

    #[test]
    fn compose_shift_enter_inserts_newline() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Codex)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));

        assert_eq!(app.compose.text(), "a\nb");
        assert_eq!(app.mode, Mode::Compose);
    }

    #[test]
    fn compose_send_preserves_draft_when_target_disappears() {
        let (mut app, mut cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Codex)];
        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));

        app.apply_full_snapshot(&StateSnapshot::default());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Compose);
        assert_eq!(app.compose.text(), "hi");
        assert!(matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|msg| msg.contains("draft preserved")));
    }

    // ── Feature 1: Keyboard preview scrolling ────────────────────────

    #[test]
    fn browse_page_up_down_scrolls_preview() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.preview.scroll_offset, 15);

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.preview.scroll_offset, 0);
    }

    #[test]
    fn browse_home_end_scrolls_preview() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.preview.scroll_offset, u16::MAX);

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.preview.scroll_offset, 0);
    }

    #[test]
    fn compose_page_up_down_scrolls_preview() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.preview.scroll_offset, 15);

        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.preview.scroll_offset, 0);
    }

    // ── Feature 2: Bracketed paste ───────────────────────────────────

    #[test]
    fn paste_in_compose_inserts_text() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
        app.enter_compose();

        app.handle_paste("hello\nworld".to_string());
        assert_eq!(app.compose.text(), "hello\nworld");
        assert_eq!(app.compose.cursor_row, 1);
        assert_eq!(app.compose.cursor_col, 5);
    }

    #[test]
    fn paste_in_browse_mode_ignored() {
        let (mut app, _cmd_rx) = make_app();
        app.handle_paste("should be ignored".to_string());
        assert_eq!(app.mode, Mode::Browse);
    }

    // ── Feature 4: Compose editing ───────────────────────────────────

    #[test]
    fn compose_delete_key() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "a");
    }

    #[test]
    fn compose_home_end_keys() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.compose.cursor_col, 0);

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.compose.cursor_col, 2);
    }

    // ── Feature 5: Empty session feedback ────────────────────────────

    #[test]
    fn enter_compose_empty_sessions_shows_status() {
        let (mut app, _cmd_rx) = make_app();
        // No sessions

        app.enter_compose();
        assert_eq!(app.mode, Mode::Browse);
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|msg| msg.contains("No sessions")));
    }

    #[test]
    fn request_delete_empty_sessions_shows_status() {
        let (mut app, _cmd_rx) = make_app();
        // No sessions

        app.request_delete();
        assert_eq!(app.mode, Mode::Browse);
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|msg| msg.contains("No sessions")));
    }

    #[test]
    fn ctrl_c_empty_sessions_shows_status() {
        let (mut app, _cmd_rx) = make_app();
        // No sessions

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app
            .status_message
            .as_deref()
            .is_some_and(|msg| msg.contains("No sessions")));
    }

    // ── Feature 3: Status auto-clear ─────────────────────────────────

    #[test]
    fn set_status_records_timestamp() {
        let (mut app, _cmd_rx) = make_app();
        assert!(app.status_message_set_at.is_none());

        app.set_status("test".to_string());
        assert!(app.status_message.is_some());
        assert!(app.status_message_set_at.is_some());
    }

    #[test]
    fn clear_status_clears_both() {
        let (mut app, _cmd_rx) = make_app();
        app.set_status("test".to_string());

        app.clear_status();
        assert!(app.status_message.is_none());
        assert!(app.status_message_set_at.is_none());
    }

    #[test]
    fn apply_snapshot_only_accepts_new_backend_status() {
        let (mut app, _cmd_rx) = make_app();
        app.set_status("local message".to_string());

        // Backend snapshot with None status should NOT clear local message
        app.apply_full_snapshot(&StateSnapshot::default());
        assert_eq!(app.status_message.as_deref(), Some("local message"));

        // Backend snapshot with new status should override
        app.apply_full_snapshot(&StateSnapshot {
            status_message: Some("backend msg".to_string()),
            ..StateSnapshot::default()
        });
        assert_eq!(app.status_message.as_deref(), Some("backend msg"));
    }

    // ── Draft preservation ────────────────────────────────────────────

    #[test]
    fn esc_preserves_compose_draft() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Browse);
        // Draft is preserved
        assert_eq!(app.compose.text(), "hi");

        // Re-entering compose restores the draft
        app.enter_compose();
        assert_eq!(app.compose.text(), "hi");
    }

    #[test]
    fn successful_send_clears_compose_draft() {
        let (mut app, mut cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];
        app.enter_compose();

        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.mode, Mode::Browse);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(BackendCommand::SendCompose { .. })
        ));
        // Buffer is cleared after successful send
        assert_eq!(app.compose.text(), "");
    }

    // ── Prompt history ────────────────────────────────────────────────

    #[test]
    fn sent_messages_are_added_to_history() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];

        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.compose.history.len(), 2);
        assert_eq!(app.compose.history[0], "a");
        assert_eq!(app.compose.history[1], "b");
    }

    #[test]
    fn up_arrow_recalls_history_in_empty_compose() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];

        // Send two messages to build history
        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // Enter compose with empty buffer, press Up
        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "b"); // most recent

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "a"); // older

        // Down returns to "b"
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "b");

        // Down again returns to empty draft
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "");
    }

    #[test]
    fn history_stashes_and_restores_in_progress_draft() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];

        // Build history
        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        // Start typing a new message
        app.enter_compose();
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));

        // Navigate up into history — draft is stashed
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "x");

        // Navigate back down — draft is restored
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.compose.text(), "draft");
    }

    #[test]
    fn duplicate_history_entries_are_deduplicated() {
        let (mut app, _cmd_rx) = make_app();
        app.snapshot_mut().sessions = vec![make_session(AgentType::Claude)];

        for _ in 0..3 {
            app.enter_compose();
            app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }

        assert_eq!(app.compose.history.len(), 1);
    }
}
