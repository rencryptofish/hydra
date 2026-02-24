use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::text::Text;

use crate::logs::{ConversationEntry, GlobalStats, SessionStats};
use crate::session::{AgentType, Session};
use crate::state::{ComposeState, PreviewState};

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
pub struct PreviewUpdate {
    pub tmux_name: String,
    pub text: Option<Text<'static>>,
    pub content: String,
    pub line_count: u16,
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
    pub sidebar_area: Cell<Rect>,
    pub preview_area: Cell<Rect>,
    pub mouse_captured: bool,
    pub diff_tree_cache: RefCell<(Vec<DiffFile>, usize, Vec<ratatui::text::Line<'static>>)>,

    // Preview cache (session → latest PreviewUpdate)
    preview_cache: HashMap<String, PreviewUpdate>,

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
            sidebar_area: Cell::new(Rect::default()),
            preview_area: Cell::new(Rect::default()),
            mouse_captured: true,
            diff_tree_cache: RefCell::new((Vec::new(), 0, Vec::new())),
            preview_cache: HashMap::new(),
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
            self.sessions = snapshot.sessions.clone();
            self.last_messages = snapshot.last_messages.clone();
            self.session_stats = snapshot.session_stats.clone();
            self.global_stats = snapshot.global_stats.clone();
            self.diff_files = snapshot.diff_files.clone();
            self.conversations = snapshot.conversations.clone();
            self.status_message = snapshot.status_message.clone();

            if self.sessions.is_empty() {
                self.selected = 0;
            } else if self.selected >= self.sessions.len() {
                self.selected = self.sessions.len() - 1;
            }
        }

        while let Ok(update) = self.preview_rx.try_recv() {
            self.preview_cache.insert(update.tmux_name.clone(), update);
        }

        self.refresh_preview_from_cache();
    }

    /// Update preview from cached data for the currently selected session.
    pub fn refresh_preview_from_cache(&mut self) {
        if let Some(session) = self.sessions.get(self.selected) {
            if let Some(update) = self.preview_cache.get(&session.tmux_name) {
                self.preview.text = update.text.clone();
                self.preview.content = update.content.clone();
                self.preview.line_count = update.line_count;
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
                let _ = self.cmd_tx.try_send(BackendCommand::Quit);
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
                    let _ = self.cmd_tx.try_send(BackendCommand::SendInterrupt {
                        tmux_name: session.tmux_name.clone(),
                    });
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
        let text = self.compose.text();
        if text.trim().is_empty() {
            self.exit_compose();
            return;
        }
        if let Some(session) = self.sessions.get(self.selected) {
            let _ = self.cmd_tx.try_send(BackendCommand::SendCompose {
                tmux_name: session.tmux_name.clone(),
                text,
            });
        }
        self.exit_compose();
    }

    fn handle_agent_select_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => {
                let agents = AgentType::all();
                if let Some(agent_type) = agents.get(self.agent_selection) {
                    let _ = self.cmd_tx.try_send(BackendCommand::CreateSession {
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
                    let _ = self.cmd_tx.try_send(BackendCommand::DeleteSession {
                        tmux_name: session.tmux_name.clone(),
                        name: session.name.clone(),
                    });
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
