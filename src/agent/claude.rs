use std::collections::HashSet;

use async_trait::async_trait;

use crate::agent::{AgentLogUpdate, AgentProvider, StatusStrategy};
use crate::logs::SessionStats;

pub struct ClaudeProvider;

#[async_trait]
impl AgentProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn create_command(&self, _session_name: &str, _cwd: &str) -> String {
        "claude --dangerously-skip-permissions".to_string()
    }

    async fn resolve_log_path(
        &self,
        tmux_name: &str,
        _cwd: &str,
        _claimed_paths: &HashSet<String>,
    ) -> Option<String> {
        crate::logs::resolve_session_uuid(tmux_name).await
    }

    fn update_from_log(
        &self,
        log_id: &str,
        cwd: &str,
        offset: u64,
        session_stats: &mut SessionStats,
    ) -> AgentLogUpdate {
        let last_message =
            crate::logs::update_session_stats_and_last_message(cwd, log_id, session_stats);
        let path = crate::logs::session_jsonl_path(cwd, log_id);
        let (entries, new_offset) = crate::logs::parse_conversation_entries(&path, offset);

        AgentLogUpdate {
            entries,
            new_offset,
            last_message,
            replace_conversation: false,
        }
    }

    fn preferred_status_strategy(&self) -> StatusStrategy {
        StatusStrategy::JsonlActivity
    }
}
