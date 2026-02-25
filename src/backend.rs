use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};

use crate::agent::provider_for;
use crate::app::{BackendCommand, PreviewUpdate, StateSnapshot};
use crate::session::{AgentType, Session, VisualStatus, ProcessState, AgentState};
use crate::tmux::SessionManager;
use crate::tmux_control::{TmuxControlConnection, TmuxNotification};

mod message_runtime;
mod preview_runtime;
mod session_runtime;
pub mod state;

use message_runtime::MessageRuntime;
use preview_runtime::PreviewRuntime;
use session_runtime::SessionRuntime;

/// The backend actor runs in `tokio::spawn` and owns all I/O state.
/// It processes commands from the UI, handles `%output` notifications,
/// and periodically refreshes session state.
pub struct Backend {
    manager: Box<dyn SessionManager>,
    project_id: String,
    cwd: String,
    manifest_dir: PathBuf,

    sessions: Vec<Session>,
    session_runtime: SessionRuntime,
    message_runtime: MessageRuntime,
    preview_runtime: PreviewRuntime,

    status_message: Option<String>,
    status_message_set_at: Option<Instant>,

    state_tx: watch::Sender<Arc<StateSnapshot>>,
    preview_tx: mpsc::Sender<PreviewUpdate>,

    control_conn: Option<Arc<TmuxControlConnection>>,
}

impl Backend {
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
            session_runtime: SessionRuntime::new(),
            message_runtime: MessageRuntime::new(),
            preview_runtime: PreviewRuntime::new(),
            status_message: None,
            status_message_set_at: None,
            state_tx,
            preview_tx,
            control_conn,
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status_message = Some(msg);
        self.status_message_set_at = Some(Instant::now());
    }

    /// Run the backend event loop.
    pub async fn run(mut self, mut cmd_rx: mpsc::Receiver<BackendCommand>) {
        // Initial setup.
        self.revive_sessions().await;
        self.refresh_sessions().await;
        self.send_snapshot();

        // Subscribe to notifications if control mode is available.
        let mut notif_rx: Option<broadcast::Receiver<TmuxNotification>> =
            self.control_conn.as_ref().map(|c| c.subscribe());

        // Status/preview refresh cadence.
        let mut session_tick = tokio::time::interval(Duration::from_millis(500));
        session_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Message/stats refresh every ~50ms (runtime internally gates cadence).
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
                    let prev_sessions = self.sessions.clone();
                    let prev_status_message = self.status_message.clone();

                    // Auto-clear status messages after 4.5s (UI clears at 5s)
                    if let Some(set_at) = self.status_message_set_at {
                        if set_at.elapsed() > Duration::from_millis(4500) {
                            self.status_message = None;
                            self.status_message_set_at = None;
                        }
                    }

                    self.refresh_sessions().await;
                    if sessions_changed(&prev_sessions, &self.sessions)
                        || self.status_message != prev_status_message
                    {
                        self.send_snapshot();
                    }
                    self.send_preview_for_all().await;
                }
                _ = message_tick.tick() => {
                    self.refresh_messages();
                }
            }
        }
    }

    fn handle_notification(&mut self, notif: TmuxNotification) {
        match notif {
            TmuxNotification::PaneOutput { pane_id, .. } => {
                if let Some(session_name) = self
                    .control_conn
                    .as_ref()
                    .and_then(|c| c.pane_session_name(&pane_id))
                {
                    self.session_runtime.record_output(&session_name);
                    self.preview_runtime.mark_dirty(&session_name);

                    let mut changed = false;
                    for session in &mut self.sessions {
                        if session.tmux_name == session_name
                            && session.visual_status() != VisualStatus::Running("Thinking".to_string())
                        {
                            session.process_state = ProcessState::Alive;
                            session.agent_state = AgentState::Thinking;
                            changed = true;
                            break;
                        }
                    }
                    if changed {
                        self.send_snapshot();
                    }
                }
            }
            TmuxNotification::PaneExited { pane_id } => {
                if let Some(session_name) = self
                    .control_conn
                    .as_ref()
                    .and_then(|c| c.pane_session_name(&pane_id))
                {
                    let mut changed = false;
                    for session in &mut self.sessions {
                        if session.tmux_name == session_name
                            && session.process_state != (ProcessState::Exited { exit_code: None, reason: None })
                        {
                            session.process_state = ProcessState::Exited { exit_code: None, reason: None };
                            changed = true;
                            break;
                        }
                    }
                    if changed {
                        self.send_snapshot();
                    }
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
                    self.set_status(format!("Failed to send message: {e}"));
                    self.send_snapshot();
                } else {
                    // In subprocess mode there are no output notifications.
                    // Mark dirty so preview refreshes after user sends input.
                    self.preview_runtime.mark_dirty(&tmux_name);
                }
            }
            BackendCommand::SendKeys { tmux_name, key } => {
                let _ = self.manager.send_keys(&tmux_name, &key).await;
                self.preview_runtime.mark_dirty(&tmux_name);
            }
            BackendCommand::SendInterrupt { tmux_name } => {
                let _ = self.manager.send_keys(&tmux_name, "C-c").await;
                self.preview_runtime.mark_dirty(&tmux_name);
            }
            BackendCommand::SendLiteralKeys { tmux_name, text } => {
                let _ = self.manager.send_keys_literal(&tmux_name, &text).await;
                self.preview_runtime.mark_dirty(&tmux_name);
            }
            BackendCommand::RequestPreview {
                tmux_name,
                wants_scrollback,
            } => {
                self.preview_runtime
                    .queue_request(&tmux_name, wants_scrollback);
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
        let provider = provider_for(&agent_type);
        let cmd = provider.create_command(&name, &cwd);

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
                self.set_status(msg);
                self.refresh_sessions().await;
            }
            Err(e) => {
                self.set_status(format!("Failed to create session: {e}"));
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
                self.set_status(msg);
            }
            Err(e) => {
                self.set_status(format!("Failed to kill session: {e}"));
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
            self.set_status(msg);
        }
    }

    async fn refresh_sessions(&mut self) {
        let pid = self.project_id.clone();
        let result = self.manager.list_sessions(&pid).await;

        match result {
            Ok(mut sessions) => {
                let now = Instant::now();
                let prev_statuses: HashMap<String, VisualStatus> = self
                    .sessions
                    .iter()
                    .map(|s| (s.tmux_name.clone(), s.visual_status()))
                    .collect();

                let pane_status = self.manager.batch_pane_status().await;

                self.session_runtime.apply_statuses(
                    &mut sessions,
                    &prev_statuses,
                    self.message_runtime.session_stats(),
                    pane_status.as_ref(),
                    self.control_conn.is_some(),
                    now,
                );

                sessions.sort_by(|a, b| {
                    a.sort_order()
                        .cmp(&b.sort_order())
                        .then(a.name.cmp(&b.name))
                });

                self.sessions = sessions;
            }
            Err(e) => {
                self.preview_runtime.clear_cache();
                self.set_status(format!("Error listing sessions: {e}"));
            }
        }

        let live_keys: HashSet<&String> = self.sessions.iter().map(|s| &s.tmux_name).collect();
        self.session_runtime.prune(&live_keys);
        self.message_runtime.prune(&live_keys);
        self.preview_runtime.prune(&live_keys);
    }

    fn refresh_messages(&mut self) {
        let sessions: Vec<(String, AgentType)> = self
            .sessions
            .iter()
            .map(|session| (session.tmux_name.clone(), session.agent_type.clone()))
            .collect();

        if let Some(update) = self.message_runtime.tick(&sessions, &self.cwd) {
            for tmux_name in update.changed_sessions {
                self.session_runtime.record_output(&tmux_name);
                self.preview_runtime.mark_dirty(&tmux_name);
            }
            self.send_snapshot();
        }
    }

    fn send_snapshot(&self) {
        let snapshot = StateSnapshot {
            sessions: self.sessions.clone(),
            last_messages: self.message_runtime.last_messages().clone(),
            session_stats: self.message_runtime.session_stats().clone(),
            global_stats: self.message_runtime.global_stats().clone(),
            diff_files: self.message_runtime.diff_files().to_vec(),
            conversations: self.message_runtime.snapshot_conversations(),
            status_message: self.status_message.clone(),
        };

        let _ = self.state_tx.send(Arc::new(snapshot));
    }

    async fn send_preview_for_all(&mut self) {
        self.preview_runtime
            .send_preview_for_all(
                self.manager.as_ref(),
                self.message_runtime.conversations(),
                &self.sessions,
                &self.preview_tx,
                self.control_conn.is_some(),
            )
            .await;
    }
}

fn sessions_changed(previous: &[Session], current: &[Session]) -> bool {
    if previous.len() != current.len() {
        return true;
    }

    previous
        .iter()
        .zip(current)
        .any(|(old_session, new_session)| {
            old_session.tmux_name != new_session.tmux_name
                || old_session.name != new_session.name
                || old_session.agent_type != new_session.agent_type
                || old_session.visual_status() != new_session.visual_status()
                || old_session.task_elapsed != new_session.task_elapsed
        })
}
