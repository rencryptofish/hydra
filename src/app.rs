use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::logs::{GlobalStats, SessionStats};
use crate::session::{AgentType, Session, SessionStatus};
use crate::tmux::{SessionManager, TmuxSessionManager};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Browse,
    Attached,
    NewSessionAgent,
    ConfirmDelete,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub preview: String,
    pub mode: Mode,
    pub agent_selection: usize,
    pub should_quit: bool,
    pub project_id: String,
    /// Which session the preview is currently showing (for skip-if-unchanged optimization).
    preview_session: Option<String>,
    pub cwd: String,
    pub status_message: Option<String>,
    pub sidebar_area: Cell<Rect>,
    pub preview_area: Cell<Rect>,
    pub preview_scroll_offset: u16,
    prev_captures: HashMap<String, String>,
    /// Consecutive ticks with unchanged pane content (for Running→Idle debounce).
    idle_ticks: HashMap<String, u8>,
    /// Consecutive ticks with changed pane content (for Idle→Running debounce).
    changed_ticks: HashMap<String, u8>,
    task_starts: HashMap<String, Instant>,
    task_last_active: HashMap<String, Instant>,
    pub last_messages: HashMap<String, String>,
    pub session_stats: HashMap<String, SessionStats>,
    pub global_stats: GlobalStats,
    /// Per-file git diff stats from `git diff --numstat`
    pub diff_files: Vec<DiffFile>,
    log_uuids: HashMap<String, String>,
    message_tick: u8,
    pub manifest_dir: PathBuf,
    manager: Box<dyn SessionManager>,
    /// Pending literal keys to send to tmux (tmux_name, text).
    /// Set by `handle_mouse` for forwarding clicks; consumed by the event loop.
    pub pending_literal_keys: Option<(String, String)>,
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
            agent_selection: 0,
            should_quit: false,
            project_id,
            cwd,
            preview_session: None,
            status_message: None,
            sidebar_area: Cell::new(Rect::default()),
            preview_area: Cell::new(Rect::default()),
            preview_scroll_offset: 0,
            prev_captures: HashMap::new(),
            idle_ticks: HashMap::new(),
            changed_ticks: HashMap::new(),
            task_starts: HashMap::new(),
            task_last_active: HashMap::new(),
            last_messages: HashMap::new(),
            session_stats: HashMap::new(),
            global_stats: GlobalStats::default(),
            diff_files: Vec::new(),
            log_uuids: HashMap::new(),
            message_tick: 0,
            manifest_dir: crate::manifest::default_base_dir(),
            manager,
            pending_literal_keys: None,
        }
    }

    pub async fn refresh_sessions(&mut self) {
        let pid = self.project_id.clone();
        let result = self.manager.list_sessions(&pid).await;
        match result {
            Ok(mut sessions) => {
                let now = Instant::now();

                // Batch-capture pane content in parallel for all non-exited sessions.
                // This turns N sequential subprocess waits into 1 parallel wait.
                let live_names: Vec<String> = sessions
                    .iter()
                    .filter(|s| s.status != SessionStatus::Exited)
                    .map(|s| s.tmux_name.clone())
                    .collect();
                let capture_results = self.manager.capture_panes(&live_names).await;
                let captures: HashMap<String, String> = live_names
                    .into_iter()
                    .zip(capture_results)
                    .map(|(name, res)| (name, res.unwrap_or_default()))
                    .collect();

                for session in &mut sessions {
                    let name = session.tmux_name.clone();

                    // Carry forward previous status so hysteresis "keep current" works.
                    // list_sessions() returns fresh Session objects; without this, the
                    // default status would defeat the debounce logic.
                    if let Some(prev_session) =
                        self.sessions.iter().find(|s| s.tmux_name == name)
                    {
                        if session.status != SessionStatus::Exited {
                            session.status = prev_session.status.clone();
                        }
                    }

                    // Determine Running vs Idle by comparing pane content.
                    //
                    // Two-layer detection:
                    // 1. Log-based (authoritative): if the JSONL log shows a pending
                    //    user message with no assistant reply, the agent is Running.
                    //    If the assistant has replied, the agent is Idle.
                    // 2. Pane-based (fallback): compare normalized pane content between
                    //    ticks. Spinners/cursor noise are stripped before comparison.
                    //
                    // Hysteresis thresholds (pane-based):
                    //   Running → Idle: 12 consecutive unchanged ticks (~3s)
                    //   Idle → Running: 2 consecutive changed ticks (~500ms)
                    // First capture (no previous): immediately set Running (new session).
                    let log_working = self
                        .session_stats
                        .get(&name)
                        .and_then(|st| st.task_elapsed())
                        .is_some();

                    if let Some(content) = captures.get(&name) {
                        let normalized = normalize_capture(content);
                        let prev = self.prev_captures.get(&name);
                        let first_capture = prev.is_none();
                        let unchanged = prev.is_some_and(|p| *p == normalized);
                        self.prev_captures.insert(name.clone(), normalized);

                        if first_capture {
                            // Brand-new session — assume Running until debounce says otherwise.
                            session.status = SessionStatus::Running;
                        } else if unchanged {
                            let count = self.idle_ticks.entry(name.clone()).or_insert(0);
                            *count = count.saturating_add(1);
                            self.changed_ticks.insert(name.clone(), 0);

                            if *count >= 12 {
                                session.status = SessionStatus::Idle;
                            } else if log_working {
                                // Log says agent is still processing but pane hasn't
                                // hit the idle threshold yet — keep Running. This avoids
                                // premature Idle during agent "thinking" pauses, but
                                // doesn't reset idle_ticks so the pane-based counter
                                // can still accumulate as a fallback for stale stats.
                                session.status = SessionStatus::Running;
                            }
                            // else: keep current status (hysteresis)
                        } else {
                            let count = self.changed_ticks.entry(name.clone()).or_insert(0);
                            *count = count.saturating_add(1);
                            self.idle_ticks.insert(name.clone(), 0);

                            if *count >= 2 || log_working {
                                session.status = SessionStatus::Running;
                            }
                            // else: keep current status (don't flip to Running on a single blip)
                        }
                    }

                    // Track task elapsed time.
                    // Prefer log-derived timestamps (survives Hydra restarts),
                    // fall back to in-memory Instant tracking for responsiveness.
                    let log_elapsed = self
                        .session_stats
                        .get(&name)
                        .and_then(|st| st.task_elapsed());

                    match session.status {
                        SessionStatus::Running => {
                            self.task_starts.entry(name.clone()).or_insert(now);
                            self.task_last_active.insert(name.clone(), now);
                            // Log elapsed is authoritative when available
                            session.task_elapsed = log_elapsed.or_else(|| {
                                let start = self.task_starts[&name];
                                Some(now.duration_since(start))
                            });
                        }
                        SessionStatus::Idle => {
                            // Log says agent is still working (e.g. thinking)
                            if log_elapsed.is_some() {
                                session.task_elapsed = log_elapsed;
                            } else if let (Some(&start), Some(&last)) = (
                                self.task_starts.get(&name),
                                self.task_last_active.get(&name),
                            ) {
                                if now.duration_since(last).as_secs() < 5 {
                                    session.task_elapsed = Some(last.duration_since(start));
                                } else {
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
                // Remember which session was selected before re-sorting
                let selected_name = self
                    .sessions
                    .get(self.selected)
                    .map(|s| s.tmux_name.clone());

                // Group by status (Idle → Running → Exited), then alphabetically
                // within each group. Headers make the grouping explicit so
                // reordering feels intentional rather than chaotic.
                sessions.sort_by(|a, b| {
                    a.status
                        .sort_order()
                        .cmp(&b.status.sort_order())
                        .then(a.name.cmp(&b.name))
                });

                self.sessions = sessions;

                // Restore selection to the same session after sort
                if let Some(name) = selected_name {
                    if let Some(idx) = self.sessions.iter().position(|s| s.tmux_name == name) {
                        self.selected = idx;
                    }
                }
            }
            Err(e) => {
                self.status_message = Some(format!("Error listing sessions: {e}"));
            }
        }
        // Prune stale entries from per-session HashMaps to prevent unbounded
        // memory growth when sessions are created and deleted over time.
        {
            let live_keys: std::collections::HashSet<&String> =
                self.sessions.iter().map(|s| &s.tmux_name).collect();
            self.prev_captures.retain(|k, _| live_keys.contains(k));
            self.idle_ticks.retain(|k, _| live_keys.contains(k));
            self.changed_ticks.retain(|k, _| live_keys.contains(k));
            self.task_starts.retain(|k, _| live_keys.contains(k));
            self.task_last_active.retain(|k, _| live_keys.contains(k));
            self.last_messages.retain(|k, _| live_keys.contains(k));
            self.session_stats.retain(|k, _| live_keys.contains(k));
            self.log_uuids.retain(|k, _| live_keys.contains(k));
        }

        // Keep selected index in bounds
        if self.sessions.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }
    }

    pub async fn refresh_preview(&mut self) {
        let tmux_name = self
            .sessions
            .get(self.selected)
            .map(|s| s.tmux_name.clone());
        if let Some(tmux_name) = tmux_name {
            // Skip the expensive full-scrollback capture when the selected session
            // hasn't changed and its pane content is confirmed unchanged (idle_ticks >= 1).
            if self.preview_session.as_ref() == Some(&tmux_name)
                && self.idle_ticks.get(&tmux_name).copied().unwrap_or(0) >= 1
            {
                return;
            }

            let result = self.manager.capture_pane_scrollback(&tmux_name).await;
            match result {
                Ok(content) => self.preview = content,
                Err(_) => self.preview = String::from("[unable to capture pane]"),
            }
            self.preview_session = Some(tmux_name);
        } else {
            self.preview = String::from("No sessions. Press 'n' to create one.");
            self.preview_session = None;
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

            // Read last message and update stats if UUID is known
            if let Some(uuid) = self.log_uuids.get(tmux_name).cloned() {
                if let Some(msg) = crate::logs::read_last_assistant_message(&self.cwd, &uuid) {
                    self.last_messages.insert(tmux_name.clone(), msg);
                }
                let stats = self
                    .session_stats
                    .entry(tmux_name.clone())
                    .or_default();
                crate::logs::update_session_stats(&self.cwd, &uuid, stats);
            }
        }

        // Refresh machine-wide stats for today
        crate::logs::update_global_stats(&mut self.global_stats);

        // Refresh per-file git diff stats
        self.diff_files = get_git_diff_numstat(&self.cwd).await;
    }

    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
            self.preview_scroll_offset = 0;
            self.preview_session = None;
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
            self.preview_session = None;
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
        self.mode = Mode::NewSessionAgent;
        self.agent_selection = 0;
        self.status_message = None;
    }

    pub async fn confirm_new_session(&mut self) {
        let agents = AgentType::all();
        let agent = agents[self.agent_selection].clone();
        let existing: Vec<String> = self.sessions.iter().map(|s| s.name.clone()).collect();
        let name = crate::session::generate_name(&existing);
        let pid = self.project_id.clone();
        let cwd = self.cwd.clone();
        let manifest_dir = self.manifest_dir.clone();

        let record = crate::manifest::SessionRecord::for_new_session(&name, &agent, &cwd);
        let cmd = record.create_command();

        let result = self
            .manager
            .create_session(&pid, &name, &agent, &cwd, Some(&cmd))
            .await;
        match result {
            Ok(_) => {
                let mut msg = format!("Created session '{}' with {}", name, agent);
                if let Err(e) =
                    crate::manifest::add_session(&manifest_dir, &pid, record).await
                {
                    msg.push_str(&format!(" (warning: manifest save failed: {e})"));
                }
                self.status_message = Some(msg);
                self.refresh_sessions().await;
                if let Some(idx) = self.sessions.iter().position(|s| s.name == name) {
                    self.selected = idx;
                }
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to create session: {e}"));
            }
        }
        self.mode = Mode::Browse;
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
            let pid = self.project_id.clone();
            let manifest_dir = self.manifest_dir.clone();
            let result = self.manager.kill_session(&tmux_name).await;
            match result {
                Ok(_) => {
                    let mut msg = format!("Killed session '{name}'");
                    if let Err(e) =
                        crate::manifest::remove_session(&manifest_dir, &pid, &name).await
                    {
                        msg.push_str(&format!(" (warning: manifest update failed: {e})"));
                    }
                    self.status_message = Some(msg);
                }
                Err(e) => {
                    self.status_message = Some(format!("Failed to kill session: {e}"));
                }
            }
        }
        self.mode = Mode::Browse;
        self.refresh_sessions().await;
    }

    pub async fn revive_sessions(&mut self) {
        let pid = self.project_id.clone();
        let manifest_dir = self.manifest_dir.clone();
        let mut manifest = crate::manifest::load_manifest(&manifest_dir, &pid).await;

        if manifest.sessions.is_empty() {
            return;
        }

        // Get live session names
        let live = self.manager.list_sessions(&pid).await.unwrap_or_default();
        let live_names: std::collections::HashSet<String> =
            live.iter().map(|s| s.name.clone()).collect();

        let mut revived = 0u32;
        let mut failed = 0u32;
        let mut manifest_dirty = false;

        let names: Vec<String> = manifest.sessions.keys().cloned().collect();
        for name in names {
            if live_names.contains(&name) {
                continue;
            }

            let record = manifest.sessions[&name].clone();

            let success = match record.agent_type.parse::<AgentType>() {
                Ok(agent) => {
                    let resume_cmd = record.resume_command();
                    self.manager
                        .create_session(&pid, &name, &agent, &record.cwd, Some(&resume_cmd))
                        .await
                        .is_ok()
                }
                Err(_) => false,
            };

            if success {
                if let Some(r) = manifest.sessions.get_mut(&name) {
                    if r.failed_attempts > 0 {
                        r.failed_attempts = 0;
                        manifest_dirty = true;
                    }
                }
                revived += 1;
            } else {
                failed += 1;
                manifest_dirty = true;
                let prune = manifest.sessions.get_mut(&name).map(|r| {
                    r.failed_attempts += 1;
                    r.failed_attempts >= crate::manifest::MAX_FAILED_ATTEMPTS
                });
                if prune == Some(true) {
                    manifest.sessions.remove(&name);
                }
            }
        }

        if manifest_dirty {
            let _ = crate::manifest::save_manifest(&manifest_dir, &pid, &manifest).await;
        }

        if revived > 0 || failed > 0 {
            let msg = if failed == 0 {
                format!("Revived {revived} session(s)")
            } else {
                format!("Revived {revived}, failed {failed} session(s)")
            };
            self.status_message = Some(msg);
        }
    }

    pub fn cancel_mode(&mut self) {
        self.mode = Mode::Browse;
        self.status_message = None;
    }

    /// Send any pending literal keys queued by `handle_mouse`.
    pub async fn flush_pending_keys(&mut self) {
        if let Some((tmux_name, text)) = self.pending_literal_keys.take() {
            let _ = self.manager.send_keys_literal(&tmux_name, &text).await;
        }
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
                        let mut current_group: Option<u8> = None;
                        for (i, session) in self.sessions.iter().enumerate() {
                            let group = session.status.sort_order();
                            if current_group != Some(group) {
                                current_group = Some(group);
                                // Sidebar renders a status header line before each group.
                                if row_offset == cumulative {
                                    break;
                                }
                                cumulative += 1;
                            }
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
                                self.preview_session = None;
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
                MouseEventKind::Down(MouseButton::Left) => {
                    let preview_inner = inner(preview);
                    if preview_inner.contains(pos) {
                        // Forward click to tmux pane so the agent can reposition its cursor.
                        // Coordinates are 1-based for SGR mouse encoding.
                        if let Some(session) = self.sessions.get(self.selected) {
                            let x = (pos.x - preview_inner.x) + 1;
                            let y = (pos.y - preview_inner.y) + 1;
                            let press = format!("\x1b[<0;{x};{y}M");
                            let release = format!("\x1b[<0;{x};{y}m");
                            self.pending_literal_keys =
                                Some((session.tmux_name.clone(), format!("{press}{release}")));
                        }
                        // Reset scroll to bottom so user sees live output after clicking.
                        self.preview_scroll_offset = 0;
                    } else {
                        self.detach();
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

    pub async fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Browse => self.handle_browse_key(key.code),
            Mode::Attached => self.handle_attached_key(key).await,
            Mode::NewSessionAgent => self.handle_agent_select_key(key.code).await,
            Mode::ConfirmDelete => self.handle_confirm_delete_key(key.code).await,
        }
    }

    pub fn handle_browse_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Enter => self.attach_selected(),
            KeyCode::Char('n') => self.start_new_session(),
            KeyCode::Char('d') => self.request_delete(),
            _ => {}
        }
    }

    pub async fn handle_attached_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.detach();
            return;
        }

        if let Some(session) = self.sessions.get(self.selected) {
            if let Some(tmux_key) = crate::tmux::keycode_to_tmux(key.code, key.modifiers) {
                let tmux_name = session.tmux_name.clone();
                let _ = self.manager.send_keys(&tmux_name, &tmux_key).await;
            }
        }
    }

    pub async fn handle_agent_select_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter => self.confirm_new_session().await,
            KeyCode::Esc => self.cancel_mode(),
            KeyCode::Char('j') | KeyCode::Down => self.agent_select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.agent_select_prev(),
            _ => {}
        }
    }

    pub async fn handle_confirm_delete_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') => self.confirm_delete().await,
            KeyCode::Esc | KeyCode::Char('n') => self.cancel_mode(),
            _ => {}
        }
    }
}

/// A single file's diff stats from `git diff --numstat` or untracked listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
    pub untracked: bool,
}

/// Normalize captured pane content to reduce noise from spinners and cursors.
/// Strips braille spinner characters (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏), common line-drawing
/// spinners (|/-\), trailing whitespace, and ANSI escape sequences so that
/// cosmetic animation doesn't trigger Running/Idle status changes.
fn normalize_capture(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        // Skip ANSI escape sequences: ESC [ ... final_byte
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Consume until we hit a letter (the final byte of the CSI sequence)
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        // Skip braille spinner characters (U+2800..U+28FF)
        if ('\u{2800}'..='\u{28FF}').contains(&ch) {
            continue;
        }
        result.push(ch);
    }
    // Trim trailing whitespace from each line
    result
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse `git diff --numstat` output into per-file stats.
/// Each line: `<insertions>\t<deletions>\t<path>`
/// Binary files show `-\t-\t<path>` — we skip those.
fn parse_diff_numstat(output: &str) -> Vec<DiffFile> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let ins_str = parts.next()?;
            let del_str = parts.next()?;
            let path = parts.next()?.to_string();
            if path.is_empty() {
                return None;
            }
            let insertions = ins_str.parse().ok()?; // skips binary "-"
            let deletions = del_str.parse().ok()?;
            Some(DiffFile {
                path,
                insertions,
                deletions,
                untracked: false,
            })
        })
        .collect()
}

/// Get per-file git diff stats for the working tree, including untracked files.
async fn get_git_diff_numstat(cwd: &str) -> Vec<DiffFile> {
    let (diff_out, untracked_out) = tokio::join!(
        tokio::process::Command::new("git")
            .args(["diff", "--numstat"])
            .current_dir(cwd)
            .output(),
        tokio::process::Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard"])
            .current_dir(cwd)
            .output(),
    );

    let mut files = match diff_out {
        Ok(o) if o.status.success() => {
            parse_diff_numstat(&String::from_utf8_lossy(&o.stdout))
        }
        _ => Vec::new(),
    };

    if let Ok(o) = untracked_out {
        if o.status.success() {
            for path in String::from_utf8_lossy(&o.stdout).lines() {
                let path = path.trim();
                if !path.is_empty() {
                    files.push(DiffFile {
                        path: path.to_string(),
                        insertions: 0,
                        deletions: 0,
                        untracked: true,
                    });
                }
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentType, Session};
    use crate::tmux::SessionManager;
    use crossterm::event::MouseButton;

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
            _command_override: Option<&str>,
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
        async fn capture_pane_scrollback(&self, _tmux_name: &str) -> anyhow::Result<String> {
            Ok("mock pane content".to_string())
        }
    }

    fn test_app() -> App {
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        // Use a per-thread temp dir to avoid writing to the real home directory.
        // The dir is created lazily by manifest functions when needed.
        app.manifest_dir = std::env::temp_dir()
            .join("hydra-test")
            .join(format!("{:?}", std::thread::current().id()));
        app
    }

    fn test_app_with_sessions(sessions: Vec<Session>) -> App {
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions.clone())),
        );
        app.sessions = sessions;
        app.manifest_dir = std::env::temp_dir()
            .join("hydra-test")
            .join(format!("{:?}", std::thread::current().id()));
        app
    }

    fn make_session(name: &str, agent: AgentType) -> Session {
        make_session_with_status(name, agent, crate::session::SessionStatus::Idle)
    }

    fn make_session_with_status(name: &str, agent: AgentType, status: SessionStatus) -> Session {
        Session {
            name: name.to_string(),
            tmux_name: format!("hydra-testid-{name}"),
            agent_type: agent,
            status,
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
    fn start_new_session_goes_to_agent_select() {
        let mut app = test_app();
        app.status_message = Some("old status".to_string());
        app.start_new_session();
        assert_eq!(app.mode, Mode::NewSessionAgent);
        assert_eq!(app.agent_selection, 0);
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared"
        );
    }

    #[test]
    fn cancel_mode_returns_to_browse() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.status_message = Some("error message".to_string());
        app.cancel_mode();
        assert_eq!(app.mode, Mode::Browse);
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared"
        );
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
    fn full_new_session_flow() {
        let mut app = test_app();

        // Step 1: start new session — goes straight to agent select
        app.start_new_session();
        assert_eq!(app.mode, Mode::NewSessionAgent);
        assert_eq!(app.agent_selection, 0);

        // Step 2: cycle agent selection
        app.agent_select_next();
        assert_eq!(app.agent_selection, 1);
    }

    #[tokio::test]
    async fn confirm_new_session_auto_generates_name() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 0;

        app.confirm_new_session().await;

        assert_eq!(
            app.mode,
            Mode::Browse,
            "mode should return to Browse after confirm"
        );
        assert!(app.status_message.is_some());
        assert!(
            app.status_message.as_ref().unwrap().contains("Created session 'alpha'"),
            "should auto-generate name 'alpha': got {:?}",
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

    // ── parse_diff_numstat tests ──────────────────────────────────

    #[test]
    fn parse_diff_numstat_multiple_files() {
        let out = "45\t12\tsrc/app.rs\n30\t5\tsrc/ui.rs\n3\t0\tREADME.md\n";
        let files = super::parse_diff_numstat(out);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0], super::DiffFile { path: "src/app.rs".into(), insertions: 45, deletions: 12, untracked: false });
        assert_eq!(files[1], super::DiffFile { path: "src/ui.rs".into(), insertions: 30, deletions: 5, untracked: false });
        assert_eq!(files[2], super::DiffFile { path: "README.md".into(), insertions: 3, deletions: 0, untracked: false });
    }

    #[test]
    fn parse_diff_numstat_skips_binary() {
        let out = "-\t-\timage.png\n10\t2\tsrc/main.rs\n";
        let files = super::parse_diff_numstat(out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
    }

    #[test]
    fn parse_diff_numstat_empty() {
        assert!(super::parse_diff_numstat("").is_empty());
        assert!(super::parse_diff_numstat("\n").is_empty());
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
    fn new_app_has_no_status_message() {
        let app = test_app();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn multiple_cancel_mode_calls_remain_in_browse() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
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

    // ── Scroll tests ─────────────────────────────────────────────────

    #[test]
    fn scroll_preview_up_increases_offset() {
        let mut app = test_app();
        assert_eq!(app.preview_scroll_offset, 0);
        app.scroll_preview_up();
        assert_eq!(app.preview_scroll_offset, 3);
        app.scroll_preview_up();
        assert_eq!(app.preview_scroll_offset, 6);
    }

    #[test]
    fn scroll_preview_down_decreases_offset() {
        let mut app = test_app();
        app.preview_scroll_offset = 6;
        app.scroll_preview_down();
        assert_eq!(app.preview_scroll_offset, 3);
        app.scroll_preview_down();
        assert_eq!(app.preview_scroll_offset, 0);
    }

    #[test]
    fn scroll_preview_down_saturates_at_zero() {
        let mut app = test_app();
        assert_eq!(app.preview_scroll_offset, 0);
        app.scroll_preview_down();
        assert_eq!(app.preview_scroll_offset, 0, "should not go below 0");
    }

    #[test]
    fn scroll_preview_up_saturates_at_max() {
        let mut app = test_app();
        app.preview_scroll_offset = u16::MAX - 1;
        app.scroll_preview_up();
        assert_eq!(app.preview_scroll_offset, u16::MAX);
    }

    #[test]
    fn select_next_resets_scroll_offset() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.preview_scroll_offset = 10;
        app.select_next();
        assert_eq!(app.preview_scroll_offset, 0, "scroll should reset on nav");
    }

    #[test]
    fn select_prev_resets_scroll_offset() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.selected = 1;
        app.preview_scroll_offset = 10;
        app.select_prev();
        assert_eq!(app.preview_scroll_offset, 0);
    }

    // ── Mouse handling tests ─────────────────────────────────────────

    #[test]
    fn mouse_click_sidebar_selects_session() {

        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
            make_session("c", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        // Set sidebar area to simulate layout (x=0, y=0, w=24, h=20)
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Sidebar always has a status header row, so second session is row y=3.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 1, "clicking second row should select session 1");
    }

    #[test]
    fn mouse_click_sidebar_status_header_does_not_change_selection() {

        let sessions = vec![
            make_session_with_status("a", AgentType::Claude, SessionStatus::Idle),
            make_session_with_status("b", AgentType::Claude, SessionStatus::Running),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        app.selected = 1;

        // Click first status header row (top row inside border).
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 1, "header rows should not select a session");
    }

    #[test]
    fn mouse_click_sidebar_with_multiple_status_groups() {

        let sessions = vec![
            make_session_with_status("a", AgentType::Claude, SessionStatus::Idle),
            make_session_with_status("b", AgentType::Claude, SessionStatus::Running),
            make_session_with_status("c", AgentType::Claude, SessionStatus::Exited),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        app.selected = 0;

        // Running group has its own header; session "b" is at y=4.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 1, "click should map to running session row");
    }

    #[test]
    fn mouse_click_preview_attaches() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Click inside the preview area border
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.mode, Mode::Attached);
    }

    #[test]
    fn mouse_scroll_up_preview_scrolls() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.preview_scroll_offset, 3);
    }

    #[test]
    fn mouse_scroll_down_preview_scrolls() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        app.preview_scroll_offset = 6;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.preview_scroll_offset, 3);
    }

    #[test]
    fn mouse_scroll_sidebar_navigates() {

        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
            make_session("c", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Scroll down in sidebar = select next
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 1);

        // Scroll up in sidebar = select prev
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn mouse_attached_click_outside_detaches() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Click outside preview inner area (on the border)
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 24, // on border
            row: 0,     // on border
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.mode, Mode::Browse);
    }

    #[test]
    fn mouse_attached_scroll_up_in_preview() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.preview_scroll_offset, 3);
        assert_eq!(app.mode, Mode::Attached, "should stay attached");
    }

    #[test]
    fn mouse_attached_scroll_down_in_preview() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        app.preview_scroll_offset = 6;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.preview_scroll_offset, 3);
    }

    #[test]
    fn mouse_other_mode_is_noop() {

        let mut app = test_app();
        app.mode = Mode::ConfirmDelete;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Mouse events in ConfirmDelete mode should be no-ops
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.mode, Mode::ConfirmDelete);
    }

    // ── Refresh edge cases ───────────────────────────────────────────

    #[tokio::test]
    async fn refresh_sessions_error_sets_status_message() {
        struct ErrorManager;
        #[async_trait::async_trait]
        impl SessionManager for ErrorManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> {
                Err(anyhow::anyhow!("tmux not running"))
            }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(ErrorManager),
        );
        app.refresh_sessions().await;
        assert!(app.status_message.is_some());
        assert!(app.status_message.as_ref().unwrap().contains("Error listing sessions"));
    }

    #[tokio::test]
    async fn refresh_preview_error_shows_error_message() {
        struct ErrorCapture;
        #[async_trait::async_trait]
        impl SessionManager for ErrorCapture {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("capture failed"))
            }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(ErrorCapture),
        );
        app.sessions = vec![make_session("s1", AgentType::Claude)];
        app.refresh_preview().await;
        assert_eq!(app.preview, "[unable to capture pane]");
    }

    #[tokio::test]
    async fn confirm_new_session_error_sets_status_message() {
        struct ErrorCreate;
        #[async_trait::async_trait]
        impl SessionManager for ErrorCreate {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("creation failed"))
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(ErrorCreate),
        );
        app.mode = Mode::NewSessionAgent;
        app.confirm_new_session().await;
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.as_ref().unwrap().contains("Failed to create session"));
    }

    #[tokio::test]
    async fn confirm_delete_error_sets_status_message() {
        struct ErrorKill;
        #[async_trait::async_trait]
        impl SessionManager for ErrorKill {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> {
                Err(anyhow::anyhow!("kill failed"))
            }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(ErrorKill),
        );
        app.sessions = vec![make_session("s1", AgentType::Claude)];
        app.mode = Mode::ConfirmDelete;
        app.confirm_delete().await;
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.as_ref().unwrap().contains("Failed to kill session"));
    }

    #[tokio::test]
    async fn confirm_delete_with_no_sessions_returns_to_browse() {
        let mut app = test_app();
        app.mode = Mode::ConfirmDelete;
        app.confirm_delete().await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn refresh_sessions_selected_stays_in_bounds() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Codex),
        ];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );
        app.selected = 99; // out of bounds
        app.refresh_sessions().await;
        assert!(
            app.selected < app.sessions.len(),
            "selected should be clamped to valid range"
        );
    }

    #[tokio::test]
    async fn refresh_sessions_detects_running_status() {
        // First capture (no prev) → immediately Running (first_capture branch).
        // Mock returns constant "mock pane content" each tick via capture_panes.
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );
        app.refresh_sessions().await;
        assert_eq!(app.sessions[0].status, SessionStatus::Running, "first tick = Running (first capture)");

        // With debouncing, need 12 consecutive unchanged ticks (~3s) to become Idle
        for i in 2..=12 {
            app.refresh_sessions().await;
            assert_eq!(app.sessions[0].status, SessionStatus::Running, "tick {i} still Running");
        }
        // 13th tick: 12 consecutive unchanged → Idle
        app.refresh_sessions().await;
        assert_eq!(app.sessions[0].status, SessionStatus::Idle, "tick 13 = Idle (12 consecutive unchanged)");
    }

    #[tokio::test]
    async fn refresh_sessions_sorts_alphabetically() {
        let sessions = vec![
            make_session("charlie", AgentType::Claude),
            make_session("alpha", AgentType::Claude),
            make_session("bravo", AgentType::Claude),
        ];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );

        app.refresh_sessions().await;

        let names: Vec<_> = app.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[tokio::test]
    async fn refresh_sessions_preserves_selection_across_sort() {
        // Manager returns sessions in reverse order of their names
        let sessions = vec![
            make_session("bravo", AgentType::Claude),
            make_session("alpha", AgentType::Claude),
        ];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions.clone())),
        );
        // Pre-set: user has "bravo" selected (index 0 before sort)
        app.sessions = sessions;
        app.selected = 0; // "bravo" selected

        app.refresh_sessions().await;

        // After alphabetical sort, "alpha" is 0 and "bravo" is 1
        // Selection should follow "bravo" to its new index
        let selected_name = &app.sessions[app.selected].name;
        assert_eq!(selected_name, "bravo", "selection should follow session across sort");
    }

    #[tokio::test]
    async fn refresh_messages_only_runs_every_20_ticks() {
        let mut app = test_app();
        // message_tick starts at 0, first call increments to 1
        app.refresh_messages().await;
        // No panic, no messages (no sessions)
        assert_eq!(app.message_tick, 1);
    }

    // ── Mouse with last_messages (2-line items) ──────────────────────

    #[test]
    fn mouse_click_sidebar_with_two_line_items() {

        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        // First session has a message (2 lines), second doesn't (1 line)
        app.last_messages.insert("hydra-testid-a".to_string(), "some msg".to_string());

        // Rows: header, a, a-msg, b. Session b is at y=4.
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 1);
    }

    // ── Mouse with tiny sidebar ──────────────────────────────────────

    #[test]
    fn mouse_click_tiny_sidebar_no_panic() {

        let mut app = test_app();
        // Sidebar too small (width < 2)
        app.sidebar_area.set(Rect::new(0, 0, 1, 1));
        app.preview_area.set(Rect::new(1, 0, 1, 1));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        // Should not panic
    }

    #[test]
    fn mouse_move_is_noop() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        let old_selected = app.selected;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, old_selected);
        assert_eq!(app.mode, Mode::Browse);
    }

    // ── Mode enum ────────────────────────────────────────────────────

    #[test]
    fn mode_clone_and_debug() {
        let mode = Mode::Browse;
        let cloned = mode.clone();
        assert_eq!(mode, cloned);
        let debug = format!("{:?}", mode);
        assert!(debug.contains("Browse"));
    }

    // ── revive_sessions tests ────────────────────────────────────────
    // Each test uses a unique project ID to avoid parallel test interference.

    fn make_manifest_record(name: &str, agent_type: &str) -> crate::manifest::SessionRecord {
        crate::manifest::SessionRecord {
            name: name.to_string(),
            agent_type: agent_type.to_string(),
            agent_session_id: if agent_type == "claude" {
                Some("test-uuid".to_string())
            } else {
                None
            },
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        }
    }

    #[tokio::test]
    async fn revive_sessions_empty_manifest_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;
        assert!(app.status_message.is_none());
    }

    #[tokio::test]
    async fn revive_sessions_creates_dead_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";
        let mut manifest = crate::manifest::Manifest::default();
        manifest.sessions.insert("alpha".to_string(), make_manifest_record("alpha", "claude"));
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;

        assert!(app.status_message.is_some());
        let msg = app.status_message.as_ref().unwrap();
        assert!(msg.contains("Revived 1"), "should revive 1 session, got: {msg}");
    }

    #[tokio::test]
    async fn revive_sessions_skips_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";
        let mut manifest = crate::manifest::Manifest::default();
        manifest.sessions.insert("alpha".to_string(), make_manifest_record("alpha", "claude"));
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        let sessions = vec![Session {
            name: "alpha".to_string(),
            tmux_name: format!("hydra-{pid}-alpha"),
            agent_type: AgentType::Claude,
            status: SessionStatus::Idle,
            task_elapsed: None,
            _alive: true,
        }];
        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;

        // No status message because no sessions were revived (all already live)
        assert!(app.status_message.is_none());
    }

    #[tokio::test]
    async fn revive_sessions_invalid_agent_type_counts_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";
        let mut manifest = crate::manifest::Manifest::default();
        manifest.sessions.insert("bad".to_string(), make_manifest_record("bad", "unknown_agent"));
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;

        assert!(app.status_message.is_some());
        let msg = app.status_message.as_ref().unwrap();
        assert!(msg.contains("failed 1"), "should report 1 failed, got: {msg}");
    }

    #[tokio::test]
    async fn revive_sessions_create_error_counts_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";
        let mut manifest = crate::manifest::Manifest::default();
        manifest.sessions.insert("alpha".to_string(), make_manifest_record("alpha", "claude"));
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        struct FailCreate;
        #[async_trait::async_trait]
        impl SessionManager for FailCreate {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("tmux error"))
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(FailCreate),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;

        assert!(app.status_message.is_some());
        let msg = app.status_message.as_ref().unwrap();
        assert!(msg.contains("failed 1"), "got: {msg}");
    }

    #[tokio::test]
    async fn revive_sessions_prunes_after_max_failed_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";
        let mut manifest = crate::manifest::Manifest::default();
        let mut record = make_manifest_record("doomed", "unknown_agent");
        // Set failed_attempts to one below the threshold
        record.failed_attempts = crate::manifest::MAX_FAILED_ATTEMPTS - 1;
        manifest.sessions.insert("doomed".to_string(), record);
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;

        // After this failure, failed_attempts reaches MAX_FAILED_ATTEMPTS and gets pruned
        let loaded = crate::manifest::load_manifest(dir.path(), pid).await;
        assert!(
            !loaded.sessions.contains_key("doomed"),
            "session should be pruned after reaching MAX_FAILED_ATTEMPTS"
        );
    }

    // ── Task timer edge cases ───────────────────────────────────────

    #[tokio::test]
    async fn refresh_sessions_exited_clears_task_timer() {
        // Create a manager that returns an Exited session
        struct ExitedManager;
        #[async_trait::async_trait]
        impl SessionManager for ExitedManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> {
                Ok(vec![Session {
                    name: "dead".to_string(),
                    tmux_name: "hydra-testid-dead".to_string(),
                    agent_type: AgentType::Claude,
                    status: SessionStatus::Exited,
                    task_elapsed: None,
                    _alive: true,
                }])
            }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(ExitedManager),
        );

        // Pre-set a task start to verify it gets cleaned up
        app.task_starts.insert("hydra-testid-dead".to_string(), std::time::Instant::now());
        app.task_last_active.insert("hydra-testid-dead".to_string(), std::time::Instant::now());

        app.refresh_sessions().await;
        assert_eq!(app.sessions[0].status, SessionStatus::Exited);
        assert!(
            !app.task_starts.contains_key("hydra-testid-dead"),
            "exited session should clear task_starts"
        );
        assert!(
            !app.task_last_active.contains_key("hydra-testid-dead"),
            "exited session should clear task_last_active"
        );
    }

    #[tokio::test]
    async fn refresh_sessions_idle_after_long_pause_clears_timer() {
        // In test env, tmux capture returns empty/error → content is static.
        // Session stays Idle throughout (hysteresis never sees 2 changed ticks).
        let sessions = vec![make_session("worker", AgentType::Claude)];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );

        // Run enough ticks to stabilize at Idle (1 first-capture + 12 unchanged = 13)
        for _ in 0..13 {
            app.refresh_sessions().await;
        }
        assert_eq!(app.sessions[0].status, SessionStatus::Idle);

        // Simulate long idle: move task_last_active back >5 seconds
        let five_secs_ago = std::time::Instant::now() - std::time::Duration::from_secs(6);
        let key = "hydra-testid-worker".to_string();
        app.task_last_active.insert(key.clone(), five_secs_ago);
        app.task_starts.insert(
            key.clone(),
            five_secs_ago - std::time::Duration::from_secs(10),
        );

        // Next refresh: still Idle, but last_active > 5s ago = clear timer
        app.refresh_sessions().await;
        assert_eq!(app.sessions[0].status, SessionStatus::Idle);
        assert!(
            !app.task_starts.contains_key(&key),
            "long idle should clear task_starts"
        );
    }

    // ── confirm_new_session with Codex agent ────────────────────────

    #[tokio::test]
    async fn confirm_new_session_with_codex() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 1; // Codex

        app.confirm_new_session().await;

        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.is_some());
        let msg = app.status_message.as_ref().unwrap();
        assert!(
            msg.contains("Codex"),
            "status should mention Codex: got {msg}"
        );
    }

    // ── confirm_new_session selects newly created session ────────────

    #[tokio::test]
    async fn confirm_new_session_selects_new_session() {
        let sessions = vec![make_session("alpha", AgentType::Claude)];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );
        // Pre-populate sessions
        app.sessions = vec![make_session("alpha", AgentType::Claude)];
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 0;

        app.confirm_new_session().await;

        assert_eq!(app.mode, Mode::Browse);
        // The new session name would be "bravo" (since "alpha" exists)
        assert!(app.status_message.as_ref().unwrap().contains("bravo"));
    }

    // ── Key handler tests ───────────────────────────────────────────

    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_key_with_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // ── Browse mode key handler ─────────────────────────────────────

    #[test]
    fn browse_key_q_sets_quit() {
        let mut app = test_app();
        app.handle_browse_key(KeyCode::Char('q'));
        assert!(app.should_quit);
    }

    #[test]
    fn browse_key_j_selects_next() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.handle_browse_key(KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn browse_key_down_selects_next() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.handle_browse_key(KeyCode::Down);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn browse_key_k_selects_prev() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.selected = 1;
        app.handle_browse_key(KeyCode::Char('k'));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn browse_key_up_selects_prev() {
        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.selected = 1;
        app.handle_browse_key(KeyCode::Up);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn browse_key_enter_attaches() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.handle_browse_key(KeyCode::Enter);
        assert_eq!(app.mode, Mode::Attached);
    }

    #[test]
    fn browse_key_n_starts_new_session() {
        let mut app = test_app();
        app.handle_browse_key(KeyCode::Char('n'));
        assert_eq!(app.mode, Mode::NewSessionAgent);
    }

    #[test]
    fn browse_key_d_requests_delete() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.handle_browse_key(KeyCode::Char('d'));
        assert_eq!(app.mode, Mode::ConfirmDelete);
    }

    #[test]
    fn browse_key_unknown_is_noop() {
        let mut app = test_app();
        app.handle_browse_key(KeyCode::Char('x'));
        assert_eq!(app.mode, Mode::Browse);
        assert!(!app.should_quit);
    }

    // ── Attached mode key handler ───────────────────────────────────

    #[tokio::test]
    async fn attached_key_esc_detaches() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.handle_attached_key(make_key(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn attached_key_sends_to_tmux() {
        use std::sync::{Arc, Mutex};

        struct TrackingManager {
            sent_keys: Arc<Mutex<Vec<(String, String)>>>,
        }
        #[async_trait::async_trait]
        impl SessionManager for TrackingManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, tmux_name: &str, key: &str) -> anyhow::Result<()> {
                self.sent_keys.lock().unwrap().push((tmux_name.to_string(), key.to_string()));
                Ok(())
            }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let sent_keys = Arc::new(Mutex::new(Vec::new()));
        let manager = TrackingManager { sent_keys: sent_keys.clone() };

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(manager),
        );
        app.sessions = vec![make_session("worker", AgentType::Claude)];
        app.mode = Mode::Attached;

        // Send 'a' key
        app.handle_attached_key(make_key(KeyCode::Char('a'))).await;

        let keys = sent_keys.lock().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "hydra-testid-worker");
        assert_eq!(keys[0].1, "a");
    }

    #[tokio::test]
    async fn attached_key_ctrl_c_sends_ctrl_key() {
        use std::sync::{Arc, Mutex};

        struct TrackingManager {
            sent_keys: Arc<Mutex<Vec<(String, String)>>>,
        }
        #[async_trait::async_trait]
        impl SessionManager for TrackingManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, tmux_name: &str, key: &str) -> anyhow::Result<()> {
                self.sent_keys.lock().unwrap().push((tmux_name.to_string(), key.to_string()));
                Ok(())
            }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let sent_keys = Arc::new(Mutex::new(Vec::new()));
        let manager = TrackingManager { sent_keys: sent_keys.clone() };

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(manager),
        );
        app.sessions = vec![make_session("worker", AgentType::Claude)];
        app.mode = Mode::Attached;

        // Send Ctrl+C
        app.handle_attached_key(make_key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)).await;

        let keys = sent_keys.lock().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].1, "C-c");
    }

    #[tokio::test]
    async fn attached_key_no_session_is_noop() {
        let mut app = test_app();
        app.mode = Mode::Attached;
        // No sessions, should not panic
        app.handle_attached_key(make_key(KeyCode::Char('a'))).await;
        assert_eq!(app.mode, Mode::Attached);
    }

    // ── Agent select mode key handler ───────────────────────────────

    #[tokio::test]
    async fn agent_select_key_enter_confirms() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 0;
        app.handle_agent_select_key(KeyCode::Enter).await;
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.is_some());
    }

    #[tokio::test]
    async fn agent_select_key_esc_cancels() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.handle_agent_select_key(KeyCode::Esc).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn agent_select_key_j_moves_down() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 0;
        app.handle_agent_select_key(KeyCode::Char('j')).await;
        assert_eq!(app.agent_selection, 1);
    }

    #[tokio::test]
    async fn agent_select_key_k_moves_up() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 1;
        app.handle_agent_select_key(KeyCode::Char('k')).await;
        assert_eq!(app.agent_selection, 0);
    }

    #[tokio::test]
    async fn agent_select_key_unknown_is_noop() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.agent_selection = 0;
        app.handle_agent_select_key(KeyCode::Char('x')).await;
        assert_eq!(app.agent_selection, 0);
        assert_eq!(app.mode, Mode::NewSessionAgent);
    }

    // ── Confirm delete mode key handler ─────────────────────────────

    #[tokio::test]
    async fn confirm_delete_key_y_confirms() {
        let sessions = vec![make_session("doomed", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::ConfirmDelete;
        app.handle_confirm_delete_key(KeyCode::Char('y')).await;
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.as_ref().unwrap().contains("Killed"));
    }

    #[tokio::test]
    async fn confirm_delete_key_esc_cancels() {
        let sessions = vec![make_session("safe", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::ConfirmDelete;
        app.handle_confirm_delete_key(KeyCode::Esc).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn confirm_delete_key_n_cancels() {
        let sessions = vec![make_session("safe", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::ConfirmDelete;
        app.handle_confirm_delete_key(KeyCode::Char('n')).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn confirm_delete_key_unknown_is_noop() {
        let sessions = vec![make_session("safe", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::ConfirmDelete;
        app.handle_confirm_delete_key(KeyCode::Char('x')).await;
        assert_eq!(app.mode, Mode::ConfirmDelete);
    }

    // ── handle_key dispatch tests ───────────────────────────────────

    #[tokio::test]
    async fn handle_key_dispatches_browse_mode() {
        let mut app = test_app();
        app.mode = Mode::Browse;
        app.handle_key(make_key(KeyCode::Char('q'))).await;
        assert!(app.should_quit);
    }

    #[tokio::test]
    async fn handle_key_dispatches_attached_mode() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.handle_key(make_key(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn handle_key_dispatches_agent_select_mode() {
        let mut app = test_app();
        app.mode = Mode::NewSessionAgent;
        app.handle_key(make_key(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    #[tokio::test]
    async fn handle_key_dispatches_confirm_delete_mode() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::ConfirmDelete;
        app.handle_key(make_key(KeyCode::Esc)).await;
        assert_eq!(app.mode, Mode::Browse);
    }

    // ── Additional coverage tests ────────────────────────────────────

    #[test]
    fn app_new_creates_instance() {
        // Covers App::new() which delegates to new_with_manager with TmuxSessionManager
        let app = App::new("testid".to_string(), "/tmp/test".to_string());
        assert_eq!(app.project_id, "testid");
        assert_eq!(app.cwd, "/tmp/test");
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.sessions.is_empty());
    }

    #[tokio::test]
    async fn attached_key_unmappable_key_is_noop() {
        // Keys that keycode_to_tmux returns None for (e.g., CapsLock, Null)
        // should not panic or send anything
        use std::sync::{Arc, Mutex};

        struct TrackingManager {
            sent_keys: Arc<Mutex<Vec<(String, String)>>>,
        }
        #[async_trait::async_trait]
        impl SessionManager for TrackingManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> { Ok(vec![]) }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, tmux_name: &str, key: &str) -> anyhow::Result<()> {
                self.sent_keys.lock().unwrap().push((tmux_name.to_string(), key.to_string()));
                Ok(())
            }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let sent_keys = Arc::new(Mutex::new(Vec::new()));
        let manager = TrackingManager { sent_keys: sent_keys.clone() };

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(manager),
        );
        app.sessions = vec![make_session("worker", AgentType::Claude)];
        app.mode = Mode::Attached;

        // Send a key that doesn't map to tmux (e.g., CapsLock)
        app.handle_attached_key(make_key(KeyCode::CapsLock)).await;

        let keys = sent_keys.lock().unwrap();
        assert!(keys.is_empty(), "unmappable key should not send anything");
    }

    #[test]
    fn mouse_click_already_selected_session_stays() {

        let sessions = vec![
            make_session("a", AgentType::Claude),
            make_session("b", AgentType::Claude),
        ];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        app.selected = 0;
        app.preview_scroll_offset = 5; // non-zero offset

        // Click on first session (already selected)
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1, // inner area row 0 = first session
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.selected, 0);
        // Scroll offset should NOT reset since session didn't change
        assert_eq!(app.preview_scroll_offset, 5);
    }

    #[test]
    fn mouse_attached_scroll_outside_preview_is_noop() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Scroll up outside the preview area (in sidebar)
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.preview_scroll_offset, 0, "scroll outside preview should be noop");
        assert_eq!(app.mode, Mode::Attached);
    }

    #[test]
    fn parse_diff_numstat_empty_path_skipped() {
        // A line with insertions/deletions but empty path should be skipped
        let out = "10\t5\t\n";
        let files = super::parse_diff_numstat(out);
        assert!(files.is_empty());
    }

    #[test]
    fn parse_diff_numstat_malformed_line() {
        // Lines without enough tab-separated parts
        let out = "only_one_column\n";
        let files = super::parse_diff_numstat(out);
        assert!(files.is_empty());
    }

    #[test]
    fn parse_diff_numstat_untracked_field_is_false() {
        let out = "10\t5\tsrc/main.rs\n";
        let files = super::parse_diff_numstat(out);
        assert_eq!(files.len(), 1);
        assert!(!files[0].untracked, "parsed files should not be untracked");
    }

    #[tokio::test]
    async fn refresh_messages_runs_on_20th_tick() {
        let mut app = test_app();
        // Start at tick 19 so the next call (tick 20) triggers the inner loop
        app.message_tick = 19;
        app.refresh_messages().await;
        assert_eq!(app.message_tick, 20);
        // No panic and no sessions to process — this just covers the tick check
    }

    #[tokio::test]
    async fn refresh_messages_wraps_tick_counter() {
        let mut app = test_app();
        app.message_tick = 255; // u8::MAX
        app.refresh_messages().await;
        assert_eq!(app.message_tick, 0, "tick counter should wrap around");
    }

    #[test]
    fn mouse_attached_click_inside_preview_stays_attached() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Click inside the preview inner area (not on border)
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.mode, Mode::Attached, "clicking inside preview should stay attached");
    }

    #[test]
    fn mouse_attached_click_forwards_to_tmux() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        // Preview at x=24, width=56, height=20 → inner starts at (25,1)
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // Click at column=30, row=5 → inner x = 30-25 = 5, inner y = 5-1 = 4
        // SGR coords are 1-based: x=6, y=5
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });

        let (tmux_name, text) = app.pending_literal_keys.take().expect("should queue literal keys");
        assert_eq!(tmux_name, "hydra-testid-a");
        assert!(text.contains("\x1b[<0;6;5M"), "should contain SGR press: {text:?}");
        assert!(text.contains("\x1b[<0;6;5m"), "should contain SGR release: {text:?}");
    }

    #[test]
    fn mouse_attached_click_resets_scroll_offset() {
        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        app.preview_scroll_offset = 10;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });

        assert_eq!(app.preview_scroll_offset, 0, "click should reset scroll to bottom");
    }

    #[test]
    fn mouse_attached_other_event_is_noop() {

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.mode = Mode::Attached;
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));

        // MouseMove in attached mode
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 30,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(app.mode, Mode::Attached);
    }

    // ── Resilience: selected resets to 0 when all sessions deleted ──

    #[tokio::test]
    async fn refresh_sessions_selected_resets_to_zero_when_empty() {
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()), // returns empty sessions
        );
        app.selected = 5; // out-of-bounds for an empty list
        app.refresh_sessions().await;
        assert!(app.sessions.is_empty());
        assert_eq!(app.selected, 0, "selected should reset to 0 when sessions list is empty");
    }

    // ── Resilience: HashMap pruning removes stale keys ──────────────

    #[tokio::test]
    async fn refresh_sessions_prunes_stale_hashmap_entries() {
        let sessions = vec![make_session("alpha", AgentType::Claude)];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );

        // Pre-populate HashMaps with a stale key that won't appear in live sessions
        let stale = "hydra-testid-deleted".to_string();
        app.prev_captures.insert(stale.clone(), "old".into());
        app.idle_ticks.insert(stale.clone(), 3);
        app.changed_ticks.insert(stale.clone(), 2);
        app.task_starts.insert(stale.clone(), Instant::now());
        app.task_last_active.insert(stale.clone(), Instant::now());
        app.last_messages.insert(stale.clone(), "msg".into());
        app.session_stats.insert(stale.clone(), SessionStats::default());
        app.log_uuids.insert(stale.clone(), "uuid".into());

        app.refresh_sessions().await;

        // Live session key should still exist in prev_captures (inserted during refresh)
        let live = "hydra-testid-alpha".to_string();
        assert!(app.prev_captures.contains_key(&live), "live key should remain");

        // Stale key should be pruned from all maps
        assert!(!app.prev_captures.contains_key(&stale), "stale prev_captures should be pruned");
        assert!(!app.idle_ticks.contains_key(&stale), "stale idle_ticks should be pruned");
        assert!(!app.changed_ticks.contains_key(&stale), "stale changed_ticks should be pruned");
        assert!(!app.task_starts.contains_key(&stale), "stale task_starts should be pruned");
        assert!(!app.task_last_active.contains_key(&stale), "stale task_last_active should be pruned");
        assert!(!app.last_messages.contains_key(&stale), "stale last_messages should be pruned");
        assert!(!app.session_stats.contains_key(&stale), "stale session_stats should be pruned");
        assert!(!app.log_uuids.contains_key(&stale), "stale log_uuids should be pruned");
    }

    // ── Revival: success resets failed_attempts ──────────────────

    #[tokio::test]
    async fn revive_sessions_success_resets_failed_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";
        let mut manifest = crate::manifest::Manifest::default();
        let mut record = make_manifest_record("alpha", "claude");
        record.failed_attempts = 2; // Previously failed twice
        manifest.sessions.insert("alpha".to_string(), record);
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.revive_sessions().await;

        // Verify failed_attempts was reset to 0
        let loaded = crate::manifest::load_manifest(dir.path(), pid).await;
        assert_eq!(
            loaded.sessions["alpha"].failed_attempts, 0,
            "successful revival should reset failed_attempts"
        );
    }

    // ── confirm_new_session success path ──────────────────────────

    #[tokio::test]
    async fn confirm_new_session_success_saves_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";

        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::new()),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.mode = Mode::NewSessionAgent;
        app.confirm_new_session().await;

        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.as_ref().unwrap().contains("Created session"));

        // Verify manifest was saved (name is auto-generated)
        let loaded = crate::manifest::load_manifest(dir.path(), pid).await;
        assert!(!loaded.sessions.is_empty(), "manifest should have the new session");
    }

    // ── confirm_delete success path ──────────────────────────────

    #[tokio::test]
    async fn confirm_delete_success_updates_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let pid = "testid";

        // Pre-populate manifest with a session
        let mut manifest = crate::manifest::Manifest::default();
        manifest.sessions.insert("s1".to_string(), make_manifest_record("s1", "claude"));
        crate::manifest::save_manifest(dir.path(), pid, &manifest).await.unwrap();

        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = App::new_with_manager(
            pid.to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions.clone())),
        );
        app.manifest_dir = dir.path().to_path_buf();
        app.sessions = sessions;
        app.mode = Mode::ConfirmDelete;
        app.confirm_delete().await;

        assert_eq!(app.mode, Mode::Browse);
        assert!(app.status_message.as_ref().unwrap().contains("Killed session"));

        // Verify manifest entry was removed
        let loaded = crate::manifest::load_manifest(dir.path(), pid).await;
        assert!(!loaded.sessions.contains_key("s1"), "session should be removed from manifest");
    }

    // ── Mouse: scroll in preview scrolls viewport ────────────────

    #[test]
    fn mouse_scroll_preview_changes_offset() {
        use crossterm::event::{MouseEvent, MouseEventKind};

        let sessions = vec![make_session("a", AgentType::Claude)];
        let mut app = test_app_with_sessions(sessions);
        app.sidebar_area.set(Rect::new(0, 0, 24, 20));
        app.preview_area.set(Rect::new(24, 0, 56, 20));
        assert_eq!(app.preview_scroll_offset, 0);

        // Scroll up in preview
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 40,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(app.preview_scroll_offset > 0);

        let offset = app.preview_scroll_offset;

        // Scroll down should decrease offset
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(app.preview_scroll_offset < offset);
    }

    // ── Task timer: Running starts clock ─────────────────────────

    #[tokio::test]
    async fn refresh_sessions_running_starts_task_timer() {
        struct RunningManager;
        #[async_trait::async_trait]
        impl SessionManager for RunningManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> {
                Ok(vec![Session {
                    name: "worker".to_string(),
                    tmux_name: "hydra-testid-worker".to_string(),
                    agent_type: AgentType::Claude,
                    status: SessionStatus::Running,
                    task_elapsed: None,
                    _alive: true,
                }])
            }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(RunningManager),
        );
        app.refresh_sessions().await;

        let key = "hydra-testid-worker";
        assert!(app.task_starts.contains_key(key), "Running session should start task timer");
        assert!(app.task_last_active.contains_key(key), "Running session should set last_active");
    }

    // ── Task timer: Idle with recent activity keeps frozen timer ──

    #[tokio::test]
    async fn refresh_sessions_idle_recent_keeps_frozen_timer() {
        struct IdleManager;
        #[async_trait::async_trait]
        impl SessionManager for IdleManager {
            async fn list_sessions(&self, _: &str) -> anyhow::Result<Vec<Session>> {
                Ok(vec![Session {
                    name: "worker".to_string(),
                    tmux_name: "hydra-testid-worker".to_string(),
                    agent_type: AgentType::Claude,
                    status: SessionStatus::Idle,
                    task_elapsed: None,
                    _alive: true,
                }])
            }
            async fn create_session(&self, _: &str, _: &str, _: &AgentType, _: &str, _: Option<&str>) -> anyhow::Result<String> {
                Ok(String::new())
            }
            async fn capture_pane(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
            async fn kill_session(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn send_keys(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            async fn capture_pane_scrollback(&self, _: &str) -> anyhow::Result<String> { Ok(String::new()) }
        }

        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(IdleManager),
        );

        let key = "hydra-testid-worker".to_string();
        // Pre-populate task_starts and task_last_active with recent timestamps
        let now = Instant::now();
        app.task_starts.insert(key.clone(), now);
        app.task_last_active.insert(key.clone(), now);

        app.refresh_sessions().await;

        // Timer should still be set (within 5s window)
        assert!(app.task_starts.contains_key(&key), "recent idle should keep task timer");
        // Session should have a task_elapsed value
        assert!(app.sessions[0].task_elapsed.is_some(), "idle within 5s should show frozen timer");
    }

    // ── normalize_capture tests ─────────────────────────────────────

    #[test]
    fn normalize_capture_strips_braille_spinners() {
        let input = "Loading \u{280B}\u{2819}\u{2839} done";
        let result = super::normalize_capture(input);
        assert_eq!(result, "Loading  done");
    }

    #[test]
    fn normalize_capture_strips_ansi_escapes() {
        let input = "hello \x1b[31mred\x1b[0m world";
        let result = super::normalize_capture(input);
        assert_eq!(result, "hello red world");
    }

    #[test]
    fn normalize_capture_trims_trailing_whitespace() {
        let input = "line one   \nline two  \n";
        let result = super::normalize_capture(input);
        // lines() drops the trailing \n, join produces no trailing newline
        assert_eq!(result, "line one\nline two");
    }

    #[test]
    fn normalize_capture_preserves_normal_content() {
        let input = "$ claude\nHello, how can I help?";
        let result = super::normalize_capture(input);
        assert_eq!(result, input);
    }

    #[test]
    fn normalize_capture_empty_string() {
        assert_eq!(super::normalize_capture(""), "");
    }

    #[test]
    fn normalize_capture_combined_noise() {
        // ANSI cursor move + braille spinner + trailing spaces
        let input = "\x1b[2Kworking \u{2807}   ";
        let result = super::normalize_capture(input);
        assert_eq!(result, "working");
    }

    // ── log-based status override test ──────────────────────────────

    #[tokio::test]
    async fn refresh_sessions_log_working_keeps_running_before_idle_threshold() {
        // When session_stats.task_elapsed() returns Some (agent is working),
        // status should stay Running during the pane-based debounce window
        // (idle_ticks < 12), but once idle_ticks >= 12, pane-based Idle wins
        // over potentially stale log data.
        let sessions = vec![make_session("s1", AgentType::Claude)];
        let mut app = App::new_with_manager(
            "testid".to_string(),
            "/tmp/test".to_string(),
            Box::new(MockSessionManager::with_sessions(sessions)),
        );

        // Inject session_stats with a pending user message (agent is working)
        let mut stats = crate::logs::SessionStats::default();
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(5))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        stats.last_user_ts = Some(ts);
        app.session_stats.insert("hydra-testid-s1".to_string(), stats);

        // First tick: first_capture, sets Running
        app.refresh_sessions().await;
        assert_eq!(app.sessions[0].status, SessionStatus::Running, "first tick = Running");

        // Ticks 2-12: unchanged content, but log_working keeps it Running
        for i in 2..=12 {
            app.refresh_sessions().await;
            assert_eq!(
                app.sessions[0].status,
                SessionStatus::Running,
                "tick {i}: log_working should keep Running before idle threshold"
            );
        }

        // Tick 13: idle_ticks reaches 12, pane-based Idle overrides stale log
        app.refresh_sessions().await;
        assert_eq!(
            app.sessions[0].status,
            SessionStatus::Idle,
            "tick 13: pane-based Idle should override stale log data"
        );
    }

}
