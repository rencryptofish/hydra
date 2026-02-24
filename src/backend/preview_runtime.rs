use std::collections::{HashMap, HashSet};

use tokio::sync::mpsc;

use crate::app::{PreviewData, PreviewUpdate};
use crate::backend::state::ConversationBuffer;
use crate::session::Session;
use crate::tmux::SessionManager;

const MAX_PREVIEW_UPDATES_PER_TICK: usize = 8;
const MAX_LIVE_CAPTURES_PER_TICK_CONTROL_MODE: usize = 2;
const MAX_LIVE_CAPTURES_PER_TICK_SUBPROCESS_MODE: usize = 1;

#[derive(Debug, Clone)]
struct PreviewCandidate {
    tmux_name: String,
    wants_scrollback: bool,
    requested: bool,
}

pub(crate) struct PreviewRuntime {
    preview_capture_cache: HashMap<String, String>,
    dirty_preview_sessions: HashSet<String>,
    requested_previews: HashMap<String, bool>,
    round_robin_cursor: usize,
}

impl PreviewRuntime {
    pub(crate) fn new() -> Self {
        Self {
            preview_capture_cache: HashMap::new(),
            dirty_preview_sessions: HashSet::new(),
            requested_previews: HashMap::new(),
            round_robin_cursor: 0,
        }
    }

    pub(crate) fn mark_dirty(&mut self, tmux_name: &str) {
        self.dirty_preview_sessions.insert(tmux_name.to_string());
    }

    pub(crate) fn queue_request(&mut self, tmux_name: &str, wants_scrollback: bool) {
        self.requested_previews
            .entry(tmux_name.to_string())
            .and_modify(|existing| *existing |= wants_scrollback)
            .or_insert(wants_scrollback);
    }

    pub(crate) fn prune(&mut self, live_keys: &HashSet<&String>) {
        self.preview_capture_cache
            .retain(|k, _| live_keys.contains(k));
        self.dirty_preview_sessions
            .retain(|k| live_keys.contains(k));
        self.requested_previews.retain(|k, _| live_keys.contains(k));
    }

    pub(crate) fn clear_cache(&mut self) {
        self.preview_capture_cache.clear();
        self.dirty_preview_sessions.clear();
        self.requested_previews.clear();
        self.round_robin_cursor = 0;
    }

    pub(crate) async fn send_preview_for_all(
        &mut self,
        manager: &dyn SessionManager,
        conversations: &HashMap<String, ConversationBuffer>,
        sessions: &[Session],
        preview_tx: &mpsc::Sender<PreviewUpdate>,
        control_mode: bool,
    ) {
        let tmux_names: Vec<String> = sessions
            .iter()
            .map(|session| session.tmux_name.clone())
            .collect();
        if tmux_names.is_empty() {
            self.round_robin_cursor = 0;
            return;
        }

        let candidates = self.plan_candidates(&tmux_names);
        let mut live_capture_budget = if control_mode {
            MAX_LIVE_CAPTURES_PER_TICK_CONTROL_MODE
        } else {
            MAX_LIVE_CAPTURES_PER_TICK_SUBPROCESS_MODE
        };

        for candidate in candidates {
            let was_dirty = self.dirty_preview_sessions.remove(&candidate.tmux_name);
            let allow_live_capture = candidate.wants_scrollback
                || candidate.requested
                || (was_dirty && take_budget(&mut live_capture_budget))
                // In subprocess mode there are no output notifications, so we
                // refresh one pane per tick even when cached.
                || (!control_mode && take_budget(&mut live_capture_budget));

            let Some(update) = self
                .resolve_preview(
                    manager,
                    conversations,
                    &candidate.tmux_name,
                    candidate.wants_scrollback,
                    allow_live_capture,
                )
                .await
            else {
                continue;
            };

            if preview_tx.try_send(update).is_err() {
                break;
            }
        }
    }

    fn plan_candidates(&mut self, tmux_names: &[String]) -> Vec<PreviewCandidate> {
        let max_candidates = MAX_PREVIEW_UPDATES_PER_TICK.min(tmux_names.len());
        let mut candidates = Vec::with_capacity(max_candidates);
        let mut seen: HashSet<String> = HashSet::with_capacity(max_candidates);

        // Explicit UI requests first.
        for tmux_name in tmux_names {
            if candidates.len() >= max_candidates {
                break;
            }
            if let Some(wants_scrollback) = self.requested_previews.remove(tmux_name) {
                if seen.insert(tmux_name.clone()) {
                    candidates.push(PreviewCandidate {
                        tmux_name: tmux_name.clone(),
                        wants_scrollback,
                        requested: true,
                    });
                }
            }
        }

        // Dirty sessions next.
        for tmux_name in tmux_names {
            if candidates.len() >= max_candidates {
                break;
            }
            if self.dirty_preview_sessions.contains(tmux_name) && seen.insert(tmux_name.clone()) {
                candidates.push(PreviewCandidate {
                    tmux_name: tmux_name.clone(),
                    wants_scrollback: false,
                    requested: false,
                });
            }
        }

        // Round-robin fill for fairness and cache warmup.
        let total = tmux_names.len();
        let start = self.round_robin_cursor % total;
        let mut visited = 0usize;
        while candidates.len() < max_candidates && visited < total {
            let idx = (start + visited) % total;
            let tmux_name = tmux_names[idx].clone();
            if seen.insert(tmux_name.clone()) {
                candidates.push(PreviewCandidate {
                    tmux_name,
                    wants_scrollback: false,
                    requested: false,
                });
            }
            visited += 1;
        }
        self.round_robin_cursor = (start + visited) % total;

        candidates
    }

    /// Resolve preview content using a single fallback chain:
    /// 1. conversation entries
    /// 2. cached pane capture
    /// 3. live capture-pane (only when allowed)
    async fn resolve_preview(
        &mut self,
        manager: &dyn SessionManager,
        conversations: &HashMap<String, ConversationBuffer>,
        tmux_name: &str,
        wants_scrollback: bool,
        allow_live_capture: bool,
    ) -> Option<PreviewUpdate> {
        if wants_scrollback {
            let content = manager
                .capture_pane_scrollback(tmux_name)
                .await
                .unwrap_or_else(|_| "[unable to capture pane]".to_string());
            return Some(Self::build_preview_from_content(
                tmux_name.to_string(),
                content,
                true,
            ));
        }

        if let Some(update) = Self::preview_from_conversation(conversations, tmux_name) {
            return Some(update);
        }

        if allow_live_capture {
            let content = manager
                .capture_pane(tmux_name)
                .await
                .unwrap_or_else(|_| "[unable to capture pane]".to_string());
            self.preview_capture_cache
                .insert(tmux_name.to_string(), content.clone());
            return Some(Self::build_preview_from_content(
                tmux_name.to_string(),
                content,
                false,
            ));
        }

        self.preview_capture_cache.get(tmux_name).map(|content| {
            Self::build_preview_from_content(tmux_name.to_string(), content.clone(), false)
        })
    }

    fn preview_from_conversation(
        conversations: &HashMap<String, ConversationBuffer>,
        tmux_name: &str,
    ) -> Option<PreviewUpdate> {
        let conv = conversations.get(tmux_name)?;
        if conv.entries.is_empty() {
            return None;
        }

        Some(PreviewUpdate {
            tmux_name: tmux_name.to_string(),
            data: PreviewData::Conversation(conv.entries.clone()),
            has_scrollback: false,
        })
    }

    fn build_preview_from_content(
        tmux_name: String,
        content: String,
        has_scrollback: bool,
    ) -> PreviewUpdate {
        PreviewUpdate {
            tmux_name,
            data: PreviewData::PaneCapture(content),
            has_scrollback,
        }
    }
}

fn take_budget(budget: &mut usize) -> bool {
    if *budget == 0 {
        false
    } else {
        *budget -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use crate::session::{AgentType, SessionStatus};

    struct SequenceManager {
        captures: Mutex<VecDeque<String>>,
        capture_calls: Mutex<usize>,
    }

    impl SequenceManager {
        fn new(captures: &[&str]) -> Self {
            Self {
                captures: Mutex::new(captures.iter().map(|s| s.to_string()).collect()),
                capture_calls: Mutex::new(0),
            }
        }

        fn capture_calls(&self) -> usize {
            *self
                .capture_calls
                .lock()
                .expect("capture_calls lock poisoned")
        }
    }

    #[async_trait::async_trait]
    impl SessionManager for SequenceManager {
        async fn list_sessions(&self, _project_id: &str) -> Result<Vec<Session>> {
            Ok(Vec::new())
        }

        async fn create_session(
            &self,
            _project_id: &str,
            _name: &str,
            _agent: &AgentType,
            _cwd: &str,
            _command_override: Option<&str>,
        ) -> Result<String> {
            Ok(String::new())
        }

        async fn capture_pane(&self, _tmux_name: &str) -> Result<String> {
            *self
                .capture_calls
                .lock()
                .expect("capture_calls lock poisoned") += 1;
            let mut captures = self.captures.lock().expect("captures lock poisoned");
            Ok(captures
                .pop_front()
                .unwrap_or_else(|| "fallback".to_string()))
        }

        async fn kill_session(&self, _tmux_name: &str) -> Result<()> {
            Ok(())
        }

        async fn send_keys(&self, _tmux_name: &str, _key: &str) -> Result<()> {
            Ok(())
        }

        async fn capture_pane_scrollback(&self, _tmux_name: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    fn test_session(tmux_name: &str) -> Session {
        Session {
            name: "alpha".to_string(),
            tmux_name: tmux_name.to_string(),
            agent_type: AgentType::Codex,
            status: SessionStatus::Idle,
            task_elapsed: None,
            _alive: true,
        }
    }

    fn pane_content(update: PreviewUpdate) -> String {
        match update.data {
            PreviewData::PaneCapture(content) => content,
            PreviewData::Conversation(_) => String::new(),
        }
    }

    #[tokio::test]
    async fn subprocess_mode_refreshes_cached_capture_each_tick() {
        let manager = SequenceManager::new(&["first", "second"]);
        let mut runtime = PreviewRuntime::new();
        let conversations = HashMap::new();
        let sessions = vec![test_session("hydra-test-alpha")];
        let (preview_tx, mut preview_rx) = mpsc::channel(8);

        runtime
            .send_preview_for_all(&manager, &conversations, &sessions, &preview_tx, false)
            .await;
        let first = preview_rx.try_recv().expect("first preview missing");
        assert_eq!(pane_content(first), "first");

        runtime
            .send_preview_for_all(&manager, &conversations, &sessions, &preview_tx, false)
            .await;
        let second = preview_rx.try_recv().expect("second preview missing");
        assert_eq!(pane_content(second), "second");

        assert_eq!(manager.capture_calls(), 2);
    }
}
