use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};

use crate::app::{BackendCommand, PreviewUpdate, StateSnapshot};
use crate::logs::{GlobalStats, SessionStats};
use crate::session::{AgentType, Session, SessionStatus};
use crate::state::{BackgroundRefreshState, ConversationBuffer, OutputDetector, TaskTimers};
use crate::tmux::SessionManager;
use crate::tmux_control::{TmuxControlConnection, TmuxNotification};

// ── Backend actor ──────────────────────────────────────────────────

/// The backend actor runs in `tokio::spawn` and owns all I/O state.
/// It processes commands from the UI, handles `%output` notifications,
/// and periodically refreshes session state.
pub struct Backend {
    manager: Box<dyn SessionManager>,
    project_id: String,
    cwd: String,
    manifest_dir: PathBuf,

    // Session state
    sessions: Vec<Session>,
    output_detector: OutputDetector,
    timers: TaskTimers,
    dead_ticks: HashMap<String, u8>,

    // Data state
    last_messages: HashMap<String, String>,
    session_stats: HashMap<String, SessionStats>,
    global_stats: GlobalStats,
    diff_files: Vec<crate::models::DiffFile>,
    conversations: HashMap<String, ConversationBuffer>,
    bg: BackgroundRefreshState,

    // Preview state
    preview_capture_cache: HashMap<String, String>,
    dirty_preview_sessions: HashSet<String>,

    // Status message
    status_message: Option<String>,

    // Channels
    state_tx: watch::Sender<Arc<StateSnapshot>>,
    preview_tx: mpsc::Sender<PreviewUpdate>,

    // Control mode connection (for pane-to-session mapping)
    control_conn: Option<Arc<TmuxControlConnection>>,
}

impl Backend {
    const DEAD_TICK_THRESHOLD: u8 = 3;
    const DEAD_TICK_SUBAGENT_THRESHOLD: u8 = 15;

    pub fn new(
        manager: Box<dyn SessionManager>,
        project_id: String,
        cwd: String,
        manifest_dir: PathBuf,
        state_tx: watch::Sender<Arc<StateSnapshot>>,
        preview_tx: mpsc::Sender<PreviewUpdate>,
        control_conn: Option<Arc<TmuxControlConnection>>,
    ) -> Self {
        Self {
            manager,
            project_id,
            cwd,
            manifest_dir,
            sessions: Vec::new(),
            output_detector: OutputDetector::new(),
            timers: TaskTimers::new(),
            dead_ticks: HashMap::new(),
            last_messages: HashMap::new(),
            session_stats: HashMap::new(),
            global_stats: GlobalStats::default(),
            diff_files: Vec::new(),
            conversations: HashMap::new(),
            bg: BackgroundRefreshState::new(),
            preview_capture_cache: HashMap::new(),
            dirty_preview_sessions: HashSet::new(),
            status_message: None,
            state_tx,
            preview_tx,
            control_conn,
        }
    }

    /// Run the backend event loop.
    pub async fn run(mut self, mut cmd_rx: mpsc::Receiver<BackendCommand>) {
        // Initial setup
        self.revive_sessions().await;
        self.refresh_sessions().await;
        self.send_snapshot();

        // Subscribe to notifications if control mode is available
        let mut notif_rx: Option<broadcast::Receiver<TmuxNotification>> =
            self.control_conn.as_ref().map(|c| c.subscribe());

        // Status/preview refresh cadence.
        let mut session_tick = tokio::time::interval(Duration::from_millis(500));
        session_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Message/stats refresh every ~50ms (bg.tick() internally gates at 40-tick cadence)
        let mut message_tick = tokio::time::interval(Duration::from_millis(50));
        message_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if self.handle_command(cmd).await {
                        break;
                    }
                }
                notif = async {
                    match notif_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Ok(notif) = notif {
                        self.handle_notification(notif);
                    }
                }
                _ = session_tick.tick() => {
                    self.refresh_sessions().await;
                    self.send_snapshot();
                    self.send_preview_for_all().await;
                }
                _ = message_tick.tick() => {
                    self.refresh_messages();
                }
            }
        }
    }

    /// Handle a tmux notification.
    fn handle_notification(&mut self, notif: TmuxNotification) {
        match notif {
            TmuxNotification::PaneOutput { pane_id, .. } => {
                if let Some(session_name) = self
                    .control_conn
                    .as_ref()
                    .and_then(|c| c.pane_session_name(&pane_id))
                {
                    self.output_detector.record_output(&session_name);
                    self.dirty_preview_sessions.insert(session_name);
                    self.send_snapshot();
                }
            }
            TmuxNotification::PaneExited { pane_id } => {
                if let Some(session_name) = self
                    .control_conn
                    .as_ref()
                    .and_then(|c| c.pane_session_name(&pane_id))
                {
                    for session in &mut self.sessions {
                        if session.tmux_name == session_name {
                            session.status = SessionStatus::Exited;
                            break;
                        }
                    }
                    self.send_snapshot();
                }
            }
            TmuxNotification::SessionChanged { .. } => {
                // Session list may have changed, refresh on next tick.
            }
        }
    }

    /// Handle a command from the UI. Returns true if the backend should stop.
    async fn handle_command(&mut self, cmd: BackendCommand) -> bool {
        match cmd {
            BackendCommand::Quit => return true,
            BackendCommand::CreateSession { agent_type } => {
                self.create_session(agent_type).await;
                self.send_snapshot();
            }
            BackendCommand::DeleteSession { tmux_name, name } => {
                self.delete_session(&tmux_name, &name).await;
                self.send_snapshot();
            }
            BackendCommand::SendCompose { tmux_name, text } => {
                if let Err(e) = self.manager.send_text_enter(&tmux_name, &text).await {
                    self.status_message = Some(format!("Failed to send message: {e}"));
                    self.send_snapshot();
                }
            }
            BackendCommand::SendKeys { tmux_name, key } => {
                let _ = self.manager.send_keys(&tmux_name, &key).await;
            }
            BackendCommand::SendInterrupt { tmux_name } => {
                let _ = self.manager.send_keys(&tmux_name, "C-c").await;
            }
            BackendCommand::SendLiteralKeys { tmux_name, text } => {
                let _ = self.manager.send_keys_literal(&tmux_name, &text).await;
            }
            BackendCommand::RequestPreview {
                tmux_name,
                wants_scrollback,
            } => {
                self.send_preview(&tmux_name, wants_scrollback).await;
            }
        }
        false
    }

    async fn create_session(&mut self, agent_type: AgentType) {
        let existing: Vec<String> = self.sessions.iter().map(|s| s.name.clone()).collect();
        let name = crate::session::generate_name(&existing);
        let pid = self.project_id.clone();
        let cwd = self.cwd.clone();
        let manifest_dir = self.manifest_dir.clone();

        let record = crate::manifest::SessionRecord::for_new_session(&name, &agent_type, &cwd);
        let cmd = record.create_command();

        let result = self
            .manager
            .create_session(&pid, &name, &agent_type, &cwd, Some(&cmd))
            .await;
        match result {
            Ok(_) => {
                let mut msg = format!("Created session '{}' with {}", name, agent_type);
                if let Err(e) = crate::manifest::add_session(&manifest_dir, &pid, record).await {
                    msg.push_str(&format!(" (warning: manifest save failed: {e})"));
                }
                self.status_message = Some(msg);
                self.refresh_sessions().await;
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to create session: {e}"));
            }
        }
    }

    async fn delete_session(&mut self, tmux_name: &str, name: &str) {
        let pid = self.project_id.clone();
        let manifest_dir = self.manifest_dir.clone();
        let result = self.manager.kill_session(tmux_name).await;
        match result {
            Ok(_) => {
                let mut msg = format!("Killed session '{name}'");
                if let Err(e) = crate::manifest::remove_session(&manifest_dir, &pid, name).await {
                    msg.push_str(&format!(" (warning: manifest update failed: {e})"));
                }
                self.status_message = Some(msg);
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to kill session: {e}"));
            }
        }
        self.refresh_sessions().await;
    }

    async fn revive_sessions(&mut self) {
        let pid = self.project_id.clone();
        let manifest_dir = self.manifest_dir.clone();
        let mut manifest = crate::manifest::load_manifest(&manifest_dir, &pid).await;

        if manifest.sessions.is_empty() {
            return;
        }

        let agent_mapping: HashMap<String, AgentType> = manifest
            .sessions
            .iter()
            .filter_map(|(name, record)| {
                let agent: AgentType = record.agent_type.parse().ok()?;
                let tmux_name = crate::session::tmux_session_name(&pid, name);
                Some((tmux_name, agent))
            })
            .collect();
        self.manager.prepopulate_agent_cache(&agent_mapping);

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

    fn apply_exited_debounce(
        &mut self,
        tmux_name: &str,
        prev_statuses: &HashMap<String, SessionStatus>,
    ) -> SessionStatus {
        let has_active_subagents = self
            .session_stats
            .get(tmux_name)
            .map(|st| st.active_subagents > 0)
            .unwrap_or(false);

        let threshold = if has_active_subagents {
            Self::DEAD_TICK_SUBAGENT_THRESHOLD
        } else {
            Self::DEAD_TICK_THRESHOLD
        };

        let count = self.dead_ticks.entry(tmux_name.to_string()).or_insert(0);
        *count = count.saturating_add(1);

        if *count < threshold {
            prev_statuses
                .get(tmux_name)
                .filter(|s| **s != SessionStatus::Exited)
                .cloned()
                .unwrap_or(SessionStatus::Idle)
        } else {
            SessionStatus::Exited
        }
    }

    async fn refresh_sessions(&mut self) {
        let pid = self.project_id.clone();
        let result = self.manager.list_sessions(&pid).await;

        match result {
            Ok(mut sessions) => {
                let now = Instant::now();
                let prev_statuses: HashMap<String, SessionStatus> = self
                    .sessions
                    .iter()
                    .map(|s| (s.tmux_name.clone(), s.status.clone()))
                    .collect();

                let pane_status = self.manager.batch_pane_status().await;

                for session in sessions.iter_mut() {
                    let tmux_name = session.tmux_name.clone();
                    let is_dead = pane_status
                        .as_ref()
                        .and_then(|m| m.get(&tmux_name))
                        .map(|&(dead, _)| dead)
                        .unwrap_or(false);

                    if is_dead {
                        session.status = self.apply_exited_debounce(&tmux_name, &prev_statuses);
                        continue;
                    }

                    self.dead_ticks.insert(tmux_name.clone(), 0);

                    let log_running = self
                        .session_stats
                        .get(&tmux_name)
                        .and_then(|stats| stats.task_elapsed())
                        .is_some();
                    let recent_output =
                        self.output_detector.status(&tmux_name) == SessionStatus::Running;

                    session.status = if self.control_conn.is_some() {
                        if recent_output || log_running {
                            SessionStatus::Running
                        } else {
                            SessionStatus::Idle
                        }
                    } else if log_running || recent_output {
                        SessionStatus::Running
                    } else {
                        SessionStatus::Idle
                    };
                }

                self.timers.update(&mut sessions, &self.session_stats, now);

                sessions.sort_by(|a, b| {
                    a.status
                        .sort_order()
                        .cmp(&b.status.sort_order())
                        .then(a.name.cmp(&b.name))
                });

                self.sessions = sessions;
            }
            Err(e) => {
                self.preview_capture_cache.clear();
                self.status_message = Some(format!("Error listing sessions: {e}"));
            }
        }

        {
            let live_keys: HashSet<&String> = self.sessions.iter().map(|s| &s.tmux_name).collect();
            self.output_detector.prune(&live_keys);
            self.timers.prune(&live_keys);
            self.last_messages.retain(|k, _| live_keys.contains(k));
            self.session_stats.retain(|k, _| live_keys.contains(k));
            self.conversations.retain(|k, _| live_keys.contains(k));
            self.bg.prune(&live_keys);
            self.dead_ticks.retain(|k, _| live_keys.contains(k));
            self.preview_capture_cache
                .retain(|k, _| live_keys.contains(k));
            self.dirty_preview_sessions
                .retain(|k| live_keys.contains(k));
        }
    }

    fn refresh_messages(&mut self) {
        let sessions: Vec<(String, AgentType)> = self
            .sessions
            .iter()
            .map(|s| (s.tmux_name.clone(), s.agent_type.clone()))
            .collect();
        let conversation_offsets: HashMap<String, u64> = self
            .conversations
            .iter()
            .map(|(k, v)| (k.clone(), v.read_offset))
            .collect();

        if let Some(result) = self.bg.tick(
            &sessions,
            &self.session_stats,
            &self.global_stats,
            &self.cwd,
            conversation_offsets,
        ) {
            let changed_sessions: Vec<String> = result
                .conversation_offsets
                .iter()
                .filter_map(|(tmux_name, new_offset)| {
                    let old_offset = self
                        .conversations
                        .get(tmux_name)
                        .map(|buf| buf.read_offset)
                        .unwrap_or(0);
                    if *new_offset != old_offset {
                        Some(tmux_name.clone())
                    } else {
                        None
                    }
                })
                .collect();

            self.last_messages.extend(result.last_messages);
            self.session_stats = result.session_stats;
            self.global_stats = result.global_stats;
            self.diff_files = result.diff_files;

            for (tmux_name, new_entries) in result.conversations {
                let buf = self
                    .conversations
                    .entry(tmux_name.clone())
                    .or_insert_with(ConversationBuffer::new);
                if let Some(&new_offset) = result.conversation_offsets.get(&tmux_name) {
                    buf.read_offset = new_offset;
                }
                if result.conversation_replace.contains(&tmux_name) {
                    buf.entries.clear();
                }
                buf.extend(new_entries);
            }

            for tmux_name in changed_sessions {
                self.output_detector.record_output(&tmux_name);
                self.dirty_preview_sessions.insert(tmux_name);
            }

            self.send_snapshot();
        }
    }

    fn send_snapshot(&self) {
        let conversations = self
            .conversations
            .iter()
            .map(|(k, v)| (k.clone(), v.entries.clone()))
            .collect();

        let snapshot = StateSnapshot {
            sessions: self.sessions.clone(),
            last_messages: self.last_messages.clone(),
            session_stats: self.session_stats.clone(),
            global_stats: self.global_stats.clone(),
            diff_files: self.diff_files.clone(),
            conversations,
            status_message: self.status_message.clone(),
        };

        let _ = self.state_tx.send(Arc::new(snapshot));
    }

    fn build_preview_from_content(
        tmux_name: String,
        content: String,
        has_scrollback: bool,
    ) -> PreviewUpdate {
        let line_count = content.lines().count().min(u16::MAX as usize) as u16;
        let text = ansi_to_tui::IntoText::into_text(&content).ok();
        PreviewUpdate {
            tmux_name,
            text,
            content,
            line_count,
            has_scrollback,
        }
    }

    fn preview_from_conversation(&self, tmux_name: &str) -> Option<PreviewUpdate> {
        let conv = self.conversations.get(tmux_name)?;
        if conv.entries.is_empty() {
            return None;
        }

        let text = crate::ui::render_conversation(&conv.entries);
        let line_count = text.lines.len() as u16;
        Some(PreviewUpdate {
            tmux_name: tmux_name.to_string(),
            text: Some(text),
            content: String::new(),
            line_count,
            has_scrollback: false,
        })
    }

    /// Resolve preview content using a single fallback chain:
    /// 1. conversation entries
    /// 2. cached pane capture
    /// 3. live capture-pane
    async fn resolve_preview(
        &mut self,
        tmux_name: &str,
        wants_scrollback: bool,
        force_live_capture: bool,
    ) -> PreviewUpdate {
        if wants_scrollback {
            let content = self
                .manager
                .capture_pane_scrollback(tmux_name)
                .await
                .unwrap_or_else(|_| "[unable to capture pane]".to_string());
            return Self::build_preview_from_content(tmux_name.to_string(), content, true);
        }

        if let Some(update) = self.preview_from_conversation(tmux_name) {
            return update;
        }

        if !force_live_capture {
            if let Some(content) = self.preview_capture_cache.get(tmux_name) {
                return Self::build_preview_from_content(
                    tmux_name.to_string(),
                    content.clone(),
                    false,
                );
            }
        }

        let content = self
            .manager
            .capture_pane(tmux_name)
            .await
            .unwrap_or_else(|_| "[unable to capture pane]".to_string());
        self.preview_capture_cache
            .insert(tmux_name.to_string(), content.clone());
        Self::build_preview_from_content(tmux_name.to_string(), content, false)
    }

    async fn send_preview_for_all(&mut self) {
        let tmux_names: Vec<String> = self.sessions.iter().map(|s| s.tmux_name.clone()).collect();

        for tmux_name in tmux_names {
            let force_live_capture = self.dirty_preview_sessions.remove(&tmux_name);
            let update = self
                .resolve_preview(&tmux_name, false, force_live_capture)
                .await;
            let _ = self.preview_tx.try_send(update);
        }
    }

    async fn send_preview(&mut self, tmux_name: &str, wants_scrollback: bool) {
        let update = self
            .resolve_preview(tmux_name, wants_scrollback, false)
            .await;
        let _ = self.preview_tx.try_send(update);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_detector_new_session_is_idle() {
        let detector = OutputDetector::new();
        assert_eq!(detector.status("test-session"), SessionStatus::Idle);
        assert!(!detector.has_output("test-session"));
    }

    #[test]
    fn output_detector_recent_output_is_running() {
        let mut detector = OutputDetector::new();
        detector.record_output("test-session");
        assert_eq!(detector.status("test-session"), SessionStatus::Running);
        assert!(detector.has_output("test-session"));
    }

    #[test]
    fn output_detector_old_output_is_idle() {
        let mut detector = OutputDetector::new();
        detector.last_output.insert(
            "test-session".to_string(),
            Instant::now() - Duration::from_secs(10),
        );
        assert_eq!(detector.status("test-session"), SessionStatus::Idle);
        assert!(detector.has_output("test-session"));
    }

    #[test]
    fn output_detector_prune_removes_stale() {
        let mut detector = OutputDetector::new();
        detector.record_output("live-session");
        detector.record_output("dead-session");

        let live = "live-session".to_string();
        let live_keys: HashSet<&String> = [&live].into_iter().collect();
        detector.prune(&live_keys);

        assert!(detector.has_output("live-session"));
        assert!(!detector.has_output("dead-session"));
    }
}
