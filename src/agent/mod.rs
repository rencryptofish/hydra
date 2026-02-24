use std::collections::HashSet;

use async_trait::async_trait;

use crate::logs::{ConversationEntry, SessionStats};
use crate::session::AgentType;

mod claude;
mod codex;
mod gemini;

pub use claude::ClaudeProvider;
pub use codex::CodexProvider;
pub use gemini::GeminiProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusStrategy {
    OutputEvent,
    JsonlActivity,
}

#[derive(Debug, Default)]
pub struct AgentLogUpdate {
    pub entries: Vec<ConversationEntry>,
    pub new_offset: u64,
    pub last_message: Option<String>,
    pub replace_conversation: bool,
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &'static str;

    fn create_command(&self, _session_name: &str, _cwd: &str) -> String;

    async fn resolve_log_path(
        &self,
        tmux_name: &str,
        cwd: &str,
        claimed_paths: &HashSet<String>,
    ) -> Option<String>;

    fn update_from_log(
        &self,
        log_id: &str,
        cwd: &str,
        offset: u64,
        session_stats: &mut SessionStats,
    ) -> AgentLogUpdate;

    fn preferred_status_strategy(&self) -> StatusStrategy {
        StatusStrategy::OutputEvent
    }
}

static CLAUDE_PROVIDER: ClaudeProvider = ClaudeProvider;
static CODEX_PROVIDER: CodexProvider = CodexProvider;
static GEMINI_PROVIDER: GeminiProvider = GeminiProvider;

pub fn provider_for(agent_type: &AgentType) -> &'static dyn AgentProvider {
    match agent_type {
        AgentType::Claude => &CLAUDE_PROVIDER,
        AgentType::Codex => &CODEX_PROVIDER,
        AgentType::Gemini => &GEMINI_PROVIDER,
    }
}
