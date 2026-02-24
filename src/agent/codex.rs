use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::agent::{AgentLogUpdate, AgentProvider};
use crate::logs::{ConversationEntry, SessionStats};

pub struct CodexProvider;

#[async_trait]
impl AgentProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn create_command(&self, _session_name: &str, _cwd: &str) -> String {
        "codex -c check_for_update_on_startup=false --yolo".to_string()
    }

    async fn resolve_log_path(
        &self,
        tmux_name: &str,
        _cwd: &str,
        _claimed_paths: &HashSet<String>,
    ) -> Option<String> {
        crate::logs::resolve_codex_rollout_path(tmux_name)
            .await
            .map(|p| p.to_string_lossy().to_string())
    }

    fn update_from_log(
        &self,
        log_id: &str,
        _cwd: &str,
        offset: u64,
        _session_stats: &mut SessionStats,
    ) -> AgentLogUpdate {
        let path = PathBuf::from(log_id);
        let (entries, new_offset) = crate::logs::parse_codex_conversation_entries(&path, offset);

        let last_message = entries.iter().rev().find_map(|entry| match entry {
            ConversationEntry::AssistantText { text } => Some(text.clone()),
            _ => None,
        });

        AgentLogUpdate {
            entries,
            new_offset,
            last_message,
            replace_conversation: false,
        }
    }
}
