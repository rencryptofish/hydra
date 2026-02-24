use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};

use crate::app::{
    ActivityDetector, BackendCommand, BackgroundRefreshState, ConversationBuffer, DiffFile,
    PreviewUpdate, StateSnapshot, StatusDetector, TaskTimers,
};
use crate::logs::{GlobalStats, SessionStats};
use crate::session::{AgentType, Session, SessionStatus};
use crate::tmux::SessionManager;
use crate::tmux_control::{TmuxControlConnection, TmuxNotification};

// ── OutputDetector ─────────────────────────────────────────────────

/// Detects session status from `%output` notifications.
/// Sessions with recent output are Running; sessions silent for longer
/// than the idle threshold are Idle.
#[derive(Default)]
pub struct OutputDetector {
    last_output: HashMap<String, Instant>,
}

impl OutputDetector {
    /// How long after the last `%output` before a session is considered Idle.
    const IDLE_THRESHOLD: Duration = Duration::from_secs(6);

    pub fn new() -> Self {
        Self {
            last_output: HashMap::new(),
        }
    }

    /// Record that a session produced output.
    pub fn record_output(&mut self, session: &str) {
        self.last_output.insert(session.to_string(), Instant::now());
    }

    /// Get the status of a session based on its output history.
    pub fn status(&self, session: &str) -> SessionStatus {
        match self.last_output.get(session) {
            Some(t) if t.elapsed() < Self::IDLE_THRESHOLD => SessionStatus::Running,
            _ => SessionStatus::Idle,
        }
    }

    /// Returns true if this session has ever produced output.
    pub fn has_output(&self, session: &str) -> bool {
        self.last_output.contains_key(session)
    }

    /// Remove entries for sessions that no longer exist.
    pub fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.last_output.retain(|k, _| live_keys.contains(k));
    }
}

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
    status: StatusDetector,
    activity: ActivityDetector,
    output_detector: OutputDetector,
    timers: TaskTimers,

    // Data state
    last_messages: HashMap<String, String>,
    session_stats: HashMap<String, SessionStats>,
    global_stats: GlobalStats,
    diff_files: Vec<DiffFile>,
    conversations: HashMap<String, ConversationBuffer>,
    bg: BackgroundRefreshState,

    // Status message
    status_message: Option<String>,

    // Channels
    state_tx: watch::Sender<StateSnapshot>,
    preview_tx: mpsc::Sender<PreviewUpdate>,

    // Control mode connection (for pane-to-session mapping)
    control_conn: Option<Arc<TmuxControlConnection>>,
}

impl Backend {
    pub fn new(
        manager: Box<dyn SessionManager>,
        project_id: String,
        cwd: String,
        manifest_dir: PathBuf,
        state_tx: watch::Sender<StateSnapshot>,
        preview_tx: mpsc::Sender<PreviewUpdate>,
        control_conn: Option<Arc<TmuxControlConnection>>,
    ) -> Self {
        Self {
            manager,
            project_id,
            cwd,
            manifest_dir,
            sessions: Vec::new(),
            status: StatusDetector::new(),
            activity: ActivityDetector::new(),
            output_detector: OutputDetector::new(),
            timers: TaskTimers::new(),
            last_messages: HashMap::new(),
            session_stats: HashMap::new(),
            global_stats: GlobalStats::default(),
            diff_files: Vec::new(),
            conversations: HashMap::new(),
            bg: BackgroundRefreshState::new(),
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

        // Session refresh interval: 1s with control mode (status comes from %output),
        // 200ms without (capture-based status detection needs fast polling).
        let session_interval = if self.control_conn.is_some() {
            std::time::Duration::from_millis(1000)
        } else {
            std::time::Duration::from_millis(200)
        };
        let mut session_tick = tokio::time::interval(session_interval);
        session_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Message/stats refresh every ~50ms (bg.tick() internally gates at 40-tick cadence)
        let mut message_tick = tokio::time::interval(std::time::Duration::from_millis(50));
        message_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if self.handle_command(cmd).await {
                        break; // Quit received
                    }
                }
                // Handle tmux %output notifications for event-driven status
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
                // Resolve pane ID to session name
                if let Some(session_name) = self
                    .control_conn
                    .as_ref()
                    .and_then(|c| c.pane_session_name(&pane_id))
                {
                    self.output_detector.record_output(&session_name);
                    // Immediate snapshot so UI sees Running status quickly
                    self.send_snapshot();
                }
            }
            TmuxNotification::PaneExited { pane_id } => {
                // Mark session as exited immediately
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
                // Session list may have changed, refresh on next tick
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
                let _ = self.manager.send_text_enter(&tmux_name, &text).await;
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

        // Pre-populate agent type cache from manifest
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

    async fn refresh_sessions(&mut self) {
        let pid = self.project_id.clone();
        let result = self.manager.list_sessions(&pid).await;
        let has_output_detector = self.control_conn.is_some();

        match result {
            Ok(mut sessions) => {
                let now = Instant::now();
                let prev_statuses: HashMap<String, SessionStatus> = self
                    .sessions
                    .iter()
                    .map(|s| (s.tmux_name.clone(), s.status.clone()))
                    .collect();

                let pane_status = self.manager.batch_pane_status().await;

                // ── Status detection priority ──
                // 1. OutputDetector (from %output notifications, if control mode)
                // 2. ActivityDetector (pane_activity timestamps, for Claude sessions)
                // 3. StatusDetector (capture-based, for Codex sessions without %output)

                // Track which sessions have been handled by higher-priority detectors
                let mut handled: HashSet<String> = HashSet::new();

                // Phase 1: OutputDetector for sessions with recent %output events
                if has_output_detector {
                    for session in sessions.iter_mut() {
                        if self.output_detector.has_output(&session.tmux_name) {
                            // Check dead status from pane_status first
                            let is_dead = pane_status
                                .as_ref()
                                .and_then(|m| m.get(&session.tmux_name))
                                .map(|&(dead, _)| dead)
                                .unwrap_or(false);

                            if is_dead {
                                session.status = SessionStatus::Exited;
                            } else {
                                // Use JSONL log-based status as primary for Claude sessions
                                // (task_elapsed is more accurate than %output timing)
                                if session.agent_type == AgentType::Claude {
                                    if let Some(stats) = self.session_stats.get(&session.tmux_name)
                                    {
                                        if stats.task_elapsed().is_some() {
                                            session.status = SessionStatus::Running;
                                        } else {
                                            session.status =
                                                self.output_detector.status(&session.tmux_name);
                                        }
                                    } else {
                                        session.status =
                                            self.output_detector.status(&session.tmux_name);
                                    }
                                } else {
                                    session.status =
                                        self.output_detector.status(&session.tmux_name);
                                }
                            }
                            handled.insert(session.tmux_name.clone());
                        }
                    }
                }

                // Phase 2: ActivityDetector for Claude sessions not handled by OutputDetector
                if let Some(ref status_map) = pane_status {
                    for session in sessions.iter_mut() {
                        if handled.contains(&session.tmux_name) {
                            continue;
                        }
                        if session.agent_type != AgentType::Claude {
                            continue;
                        }
                        if let Some(&(is_dead, activity)) = status_map.get(&session.tmux_name) {
                            if is_dead {
                                session.status = SessionStatus::Exited;
                            } else {
                                let stats = self.session_stats.get(&session.tmux_name);
                                session.status = self.activity.update_claude_status(
                                    &session.tmux_name,
                                    activity,
                                    stats,
                                );
                            }
                            handled.insert(session.tmux_name.clone());
                        }
                    }
                }

                // Dead-tick debounce for sessions handled by output/activity detectors
                for session in sessions.iter_mut() {
                    if !handled.contains(&session.tmux_name) {
                        continue;
                    }
                    let name = session.tmux_name.clone();
                    let has_active_subagents = self
                        .session_stats
                        .get(&name)
                        .map(|st| st.active_subagents > 0)
                        .unwrap_or(false);

                    if session.status == SessionStatus::Exited {
                        let count = self.status.dead_ticks.entry(name.clone()).or_insert(0);
                        *count = count.saturating_add(1);
                        let threshold = if has_active_subagents {
                            StatusDetector::DEAD_TICK_SUBAGENT_THRESHOLD
                        } else {
                            StatusDetector::DEAD_TICK_THRESHOLD
                        };
                        if *count < threshold {
                            session.status = prev_statuses
                                .get(&name)
                                .filter(|s| **s != SessionStatus::Exited)
                                .cloned()
                                .unwrap_or(SessionStatus::Idle);
                        }
                    } else {
                        self.status.dead_ticks.insert(name, 0);
                    }
                }

                // Phase 3: Capture-based status for remaining sessions (fallback only).
                // When control mode is active, %output notifications handle all sessions.
                // Only run capture-based detection when no control mode.
                if !has_output_detector {
                    const IDLE_CAPTURE_SKIP_THRESHOLD: u8 = 40;
                    let codex_names: Vec<String> = sessions
                        .iter()
                        .filter(|s| !handled.contains(&s.tmux_name))
                        .filter(|s| s.status != SessionStatus::Exited)
                        .filter(|s| {
                            self.status.idle_ticks_for(&s.tmux_name) < IDLE_CAPTURE_SKIP_THRESHOLD
                        })
                        .map(|s| s.tmux_name.clone())
                        .collect();
                    let capture_results = self.manager.capture_panes(&codex_names).await;
                    let mut captures: HashMap<String, String> = codex_names
                        .into_iter()
                        .zip(capture_results)
                        .map(|(name, res)| (name, res.unwrap_or_default()))
                        .collect();
                    for s in sessions.iter() {
                        if !handled.contains(&s.tmux_name)
                            && s.status != SessionStatus::Exited
                            && !captures.contains_key(&s.tmux_name)
                        {
                            if let Some(prev) = self.status.raw_captures.get(&s.tmux_name) {
                                captures.insert(s.tmux_name.clone(), prev.clone());
                            }
                        }
                    }

                    self.status.update_statuses(
                        &mut sessions,
                        &captures,
                        &prev_statuses,
                        &self.session_stats,
                    );

                    self.status.latest_pane_captures = captures;
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
                self.status.latest_pane_captures.clear();
                self.status_message = Some(format!("Error listing sessions: {e}"));
            }
        }

        // Prune stale entries
        {
            let live_keys: HashSet<&String> = self.sessions.iter().map(|s| &s.tmux_name).collect();
            self.status.prune(&live_keys);
            self.activity.prune(&live_keys);
            self.output_detector.prune(&live_keys);
            self.timers.prune(&live_keys);
            self.last_messages.retain(|k, _| live_keys.contains(k));
            self.session_stats.retain(|k, _| live_keys.contains(k));
            self.conversations.retain(|k, _| live_keys.contains(k));
            self.bg.prune(&live_keys);
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
                buf.extend(new_entries);
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
        let _ = self.state_tx.send(snapshot);
    }

    async fn send_preview_for_all(&mut self) {
        for session in &self.sessions {
            let tmux_name = &session.tmux_name;

            // Check if there's conversation data
            if let Some(conv) = self.conversations.get(tmux_name) {
                if !conv.entries.is_empty() {
                    let text = crate::ui::render_conversation(&conv.entries);
                    let line_count = text.lines.len() as u16;
                    let _ = self.preview_tx.try_send(PreviewUpdate {
                        tmux_name: tmux_name.clone(),
                        text: Some(text),
                        content: String::new(),
                        line_count,
                        has_scrollback: false,
                    });
                    continue;
                }
            }

            // Fallback to pane capture (cached or live)
            let content = if let Some(cached) = self.status.latest_pane_captures.get(tmux_name) {
                cached.clone()
            } else {
                // Live capture for sessions without cached data (e.g. control mode Codex)
                self.manager
                    .capture_pane(tmux_name)
                    .await
                    .unwrap_or_default()
            };
            if !content.is_empty() {
                let line_count = content.lines().count().min(u16::MAX as usize) as u16;
                let text = ansi_to_tui::IntoText::into_text(&content).ok();
                let _ = self.preview_tx.try_send(PreviewUpdate {
                    tmux_name: tmux_name.clone(),
                    text,
                    content,
                    line_count,
                    has_scrollback: false,
                });
            }
        }
    }

    async fn send_preview(&mut self, tmux_name: &str, wants_scrollback: bool) {
        if wants_scrollback {
            let result = self.manager.capture_pane_scrollback(tmux_name).await;
            let content = result.unwrap_or_else(|_| "[unable to capture pane]".to_string());
            let line_count = content.lines().count().min(u16::MAX as usize) as u16;
            let text = ansi_to_tui::IntoText::into_text(&content).ok();
            let _ = self.preview_tx.try_send(PreviewUpdate {
                tmux_name: tmux_name.to_string(),
                text,
                content,
                line_count,
                has_scrollback: true,
            });
        } else {
            // Check for conversation data first
            if let Some(conv) = self.conversations.get(tmux_name) {
                if !conv.entries.is_empty() {
                    let text = crate::ui::render_conversation(&conv.entries);
                    let line_count = text.lines.len() as u16;
                    let _ = self.preview_tx.try_send(PreviewUpdate {
                        tmux_name: tmux_name.to_string(),
                        text: Some(text),
                        content: String::new(),
                        line_count,
                        has_scrollback: false,
                    });
                    return;
                }
            }

            // Use cached capture
            if let Some(content) = self.status.latest_pane_captures.get(tmux_name) {
                let line_count = content.lines().count().min(u16::MAX as usize) as u16;
                let text = ansi_to_tui::IntoText::into_text(content).ok();
                let _ = self.preview_tx.try_send(PreviewUpdate {
                    tmux_name: tmux_name.to_string(),
                    text,
                    content: content.clone(),
                    line_count,
                    has_scrollback: false,
                });
            } else {
                // Live capture as last resort
                let result = self.manager.capture_pane(tmux_name).await;
                let content = result.unwrap_or_else(|_| "[unable to capture pane]".to_string());
                let line_count = content.lines().count().min(u16::MAX as usize) as u16;
                let text = ansi_to_tui::IntoText::into_text(&content).ok();
                let _ = self.preview_tx.try_send(PreviewUpdate {
                    tmux_name: tmux_name.to_string(),
                    text,
                    content,
                    line_count,
                    has_scrollback: false,
                });
            }
        }
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
        // Manually insert an old timestamp
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
