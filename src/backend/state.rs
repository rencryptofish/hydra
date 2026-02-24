use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::agent::provider_for;
use crate::logs::{ConversationEntry, GlobalStats, SessionStats};
use crate::models::DiffFile;
use crate::session::{AgentType, Session, SessionStatus};
use crate::system::git::get_git_diff_numstat;

/// Per-session conversation buffer parsed from JSONL logs.
pub(crate) struct ConversationBuffer {
    pub(crate) entries: VecDeque<ConversationEntry>,
    pub(crate) read_offset: u64,
}

impl ConversationBuffer {
    const MAX_ENTRIES: usize = 500;

    pub(crate) fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            read_offset: 0,
        }
    }

    pub(crate) fn extend(&mut self, new_entries: Vec<ConversationEntry>) {
        for entry in new_entries {
            if self.entries.len() >= Self::MAX_ENTRIES {
                self.entries.pop_front();
            }
            self.entries.push_back(entry);
        }
    }
}

/// Results from a background message/stats refresh task.
pub(crate) struct MessageRefreshResult {
    pub(crate) log_uuids: HashMap<String, String>,
    pub(crate) uuid_retry_cooldowns: HashMap<String, u8>,
    pub(crate) last_messages: HashMap<String, String>,
    pub(crate) session_stats: HashMap<String, SessionStats>,
    pub(crate) global_stats: GlobalStats,
    pub(crate) diff_files: Vec<DiffFile>,
    pub(crate) conversations: HashMap<String, Vec<ConversationEntry>>,
    pub(crate) conversation_offsets: HashMap<String, u64>,
    /// Sessions whose conversation buffer should be fully replaced (not extended).
    /// Parsers can set this when they cannot provide append-only incremental entries.
    pub(crate) conversation_replace: HashSet<String>,
}

/// Detects session status from recent activity.
/// Sessions with recent output are Running; sessions silent for longer
/// than the idle threshold are Idle.
#[derive(Default)]
pub(crate) struct OutputDetector {
    last_output: HashMap<String, Instant>,
}

impl OutputDetector {
    /// How long after the last output before a session is considered Idle.
    const IDLE_THRESHOLD: Duration = Duration::from_secs(6);

    pub(crate) fn new() -> Self {
        Self {
            last_output: HashMap::new(),
        }
    }

    /// Record that a session produced output.
    pub(crate) fn record_output(&mut self, session: &str) {
        self.last_output.insert(session.to_string(), Instant::now());
    }

    /// Get the status of a session based on its output history.
    pub(crate) fn status(&self, session: &str) -> SessionStatus {
        match self.last_output.get(session) {
            Some(t) if t.elapsed() < Self::IDLE_THRESHOLD => SessionStatus::Running,
            _ => SessionStatus::Idle,
        }
    }

    /// Remove entries for sessions that no longer exist.
    pub(crate) fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.last_output.retain(|k, _| live_keys.contains(k));
    }
}

/// Tracks per-session task start/last-active timestamps for elapsed timer display.
pub(crate) struct TaskTimers {
    task_starts: HashMap<String, Instant>,
    task_last_active: HashMap<String, Instant>,
}

impl TaskTimers {
    pub(crate) fn new() -> Self {
        Self {
            task_starts: HashMap::new(),
            task_last_active: HashMap::new(),
        }
    }

    /// Update task_elapsed on each session based on its status and timestamps.
    pub(crate) fn update(
        &mut self,
        sessions: &mut [Session],
        session_stats: &HashMap<String, SessionStats>,
        now: Instant,
    ) {
        for session in sessions.iter_mut() {
            let name = session.tmux_name.clone();

            let log_elapsed = session_stats.get(&name).and_then(|st| st.task_elapsed());

            match session.status {
                SessionStatus::Running => {
                    self.task_starts.entry(name.clone()).or_insert(now);
                    self.task_last_active.insert(name.clone(), now);
                    session.task_elapsed = log_elapsed.or_else(|| {
                        let start = self.task_starts[&name];
                        Some(now.duration_since(start))
                    });
                }
                SessionStatus::Idle => {
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
    }

    /// Remove entries for sessions that no longer exist.
    pub(crate) fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.task_starts.retain(|k, _| live_keys.contains(k));
        self.task_last_active.retain(|k, _| live_keys.contains(k));
    }
}

/// Background task state for async message/stats/diff refresh.
pub(crate) struct BackgroundRefreshState {
    log_uuids: HashMap<String, String>,
    uuid_retry_cooldowns: HashMap<String, u8>,
    message_tick: u8,
    bg_refresh_rx: Option<tokio::sync::oneshot::Receiver<MessageRefreshResult>>,
}

impl BackgroundRefreshState {
    pub(crate) fn new() -> Self {
        Self {
            log_uuids: HashMap::new(),
            uuid_retry_cooldowns: HashMap::new(),
            message_tick: 0,
            bg_refresh_rx: None,
        }
    }

    /// Poll for completed background results and spawn new tasks on cadence.
    /// Returns `Some(result)` when a background task completes.
    pub(crate) fn tick(
        &mut self,
        sessions: &[(String, AgentType)],
        session_stats: &HashMap<String, SessionStats>,
        global_stats: &GlobalStats,
        cwd: &str,
        conversation_offsets: HashMap<String, u64>,
    ) -> Option<MessageRefreshResult> {
        let mut completed = None;

        // Always poll for completed background results.
        if let Some(mut rx) = self.bg_refresh_rx.take() {
            match rx.try_recv() {
                Ok(result) => {
                    self.log_uuids.extend(result.log_uuids.clone());
                    self.uuid_retry_cooldowns = result.uuid_retry_cooldowns.clone();
                    completed = Some(result);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    self.bg_refresh_rx = Some(rx);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
            }
        }

        self.message_tick = self.message_tick.wrapping_add(1);
        // Run every 40 ticks (~2 seconds at 50ms tick rate).
        if !self.message_tick.is_multiple_of(40) {
            return completed;
        }

        // Don't start a new background task if one is already running.
        if self.bg_refresh_rx.is_some() {
            return completed;
        }

        // Clone data for background task.
        let sessions = sessions.to_vec();
        let log_uuids = self.log_uuids.clone();
        let uuid_retry_cooldowns = self.uuid_retry_cooldowns.clone();
        let session_stats = session_stats.clone();
        let global_stats = global_stats.clone();
        let cwd = cwd.to_string();

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.bg_refresh_rx = Some(rx);

        tokio::spawn(async move {
            let result = compute_message_refresh(
                sessions,
                log_uuids,
                uuid_retry_cooldowns,
                session_stats,
                global_stats,
                cwd,
                conversation_offsets,
            )
            .await;
            let _ = tx.send(result);
        });

        completed
    }

    /// Remove entries for sessions that no longer exist.
    pub(crate) fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.log_uuids.retain(|k, _| live_keys.contains(k));
        self.uuid_retry_cooldowns
            .retain(|k, _| live_keys.contains(k));
    }
}

/// Background task: compute message refresh results off the main event loop.
/// Runs UUID/rollout resolution, JSONL parsing, global stats, and git diff in a background task.
async fn compute_message_refresh(
    sessions: Vec<(String, AgentType)>,
    mut log_uuids: HashMap<String, String>,
    mut uuid_retry_cooldowns: HashMap<String, u8>,
    mut session_stats: HashMap<String, SessionStats>,
    mut global_stats: GlobalStats,
    cwd: String,
    mut conversation_offsets: HashMap<String, u64>,
) -> MessageRefreshResult {
    /// Retry unresolved UUID discovery every ~30s (6 refresh cycles at 5s each).
    const UUID_RETRY_COOLDOWN_CYCLES: u8 = 6;

    let mut last_messages = HashMap::new();
    let mut conversations: HashMap<String, Vec<ConversationEntry>> = HashMap::new();
    let mut new_conversation_offsets: HashMap<String, u64> = HashMap::new();
    let mut conversation_replace = HashSet::new();

    for (tmux_name, agent_type) in &sessions {
        let provider = provider_for(agent_type);

        // Try to resolve log path if not cached.
        if !log_uuids.contains_key(tmux_name) {
            let should_attempt_resolve = match uuid_retry_cooldowns.get_mut(tmux_name) {
                Some(cooldown) if *cooldown > 0 => {
                    *cooldown -= 1;
                    false
                }
                _ => true,
            };

            if should_attempt_resolve {
                let claimed_paths: HashSet<String> = log_uuids.values().cloned().collect();
                let resolved = provider
                    .resolve_log_path(tmux_name, &cwd, &claimed_paths)
                    .await;

                if let Some(id) = resolved {
                    log_uuids.insert(tmux_name.clone(), id);
                    uuid_retry_cooldowns.remove(tmux_name);
                } else {
                    uuid_retry_cooldowns.insert(tmux_name.clone(), UUID_RETRY_COOLDOWN_CYCLES);
                }
            }
        }

        // Read last message, update stats, and parse conversation.
        if let Some(log_id) = log_uuids.get(tmux_name).cloned() {
            uuid_retry_cooldowns.remove(tmux_name);
            let stats = session_stats.entry(tmux_name.clone()).or_default();
            let conv_offset = conversation_offsets.remove(tmux_name).unwrap_or(0);
            let update = provider.update_from_log(&log_id, &cwd, conv_offset, stats);

            if let Some(msg) = update.last_message {
                last_messages.insert(tmux_name.clone(), msg);
            }
            if !update.entries.is_empty() {
                conversations.insert(tmux_name.clone(), update.entries);
            }
            if update.replace_conversation {
                conversation_replace.insert(tmux_name.clone());
            }
            new_conversation_offsets.insert(tmux_name.clone(), update.new_offset);
        }
    }

    // Refresh machine-wide stats for today.
    crate::logs::update_global_stats(&mut global_stats);

    // Refresh per-file git diff stats.
    let diff_files = get_git_diff_numstat(&cwd).await;

    MessageRefreshResult {
        log_uuids,
        uuid_retry_cooldowns,
        last_messages,
        session_stats,
        global_stats,
        diff_files,
        conversations,
        conversation_offsets: new_conversation_offsets,
        conversation_replace,
    }
}
