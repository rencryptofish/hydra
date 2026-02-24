use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::agent::{AgentLogUpdate, AgentProvider, StatusStrategy};
use crate::logs::SessionStats;

pub struct GeminiProvider;

#[async_trait]
impl AgentProvider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn create_command(&self, _session_name: &str, _cwd: &str) -> String {
        "gemini --yolo".to_string()
    }

    async fn resolve_log_path(
        &self,
        tmux_name: &str,
        cwd: &str,
        claimed_paths: &HashSet<String>,
    ) -> Option<String> {
        crate::logs::resolve_gemini_session_path(tmux_name, cwd, claimed_paths).await
    }

    fn update_from_log(
        &self,
        log_id: &str,
        _cwd: &str,
        offset: u64,
        session_stats: &mut SessionStats,
    ) -> AgentLogUpdate {
        let path = PathBuf::from(log_id);
        let (entries, new_offset, last_message, gemini_stats) =
            crate::logs::parse_gemini_session_entries(&path, offset);
        crate::logs::apply_gemini_stats(session_stats, &gemini_stats);

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
