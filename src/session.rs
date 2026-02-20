use sha2::{Digest, Sha256};
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentType {
    Claude,
    Codex,
}

impl AgentType {
    pub fn command(&self) -> &str {
        match self {
            AgentType::Claude => "claude --dangerously-skip-permissions",
            AgentType::Codex => "codex --yolo",
        }
    }

    pub fn all() -> &'static [AgentType] {
        &[AgentType::Claude, AgentType::Codex]
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentType::Claude => write!(f, "Claude"),
            AgentType::Codex => write!(f, "Codex"),
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(AgentType::Claude),
            "codex" => Ok(AgentType::Codex),
            _ => Err(anyhow::anyhow!("Unknown agent type: {s}. Use 'claude' or 'codex'.")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Running,
    Idle,
    Exited,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    pub tmux_name: String,
    pub agent_type: AgentType,
    pub status: SessionStatus,
    pub task_elapsed: Option<Duration>,
    pub _alive: bool,
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Generate an 8-char hex hash from the absolute CWD path.
pub fn project_id(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..4])
}

/// Build the tmux session name: `hydra-<hash>-<name>`
pub fn tmux_session_name(project_id: &str, name: &str) -> String {
    format!("hydra-{project_id}-{name}")
}

/// Extract the user-facing session name from a tmux session name.
pub fn parse_session_name(tmux_name: &str, project_id: &str) -> Option<String> {
    let prefix = format!("hydra-{project_id}-");
    tmux_name.strip_prefix(&prefix).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // ── project_id tests ──────────────────────────────────────────────

    #[test]
    fn project_id_is_deterministic() {
        let id1 = project_id("/home/user/project");
        let id2 = project_id("/home/user/project");
        assert_eq!(id1, id2);
    }

    #[test]
    fn project_id_different_inputs_produce_different_ids() {
        let id1 = project_id("/home/user/project-a");
        let id2 = project_id("/home/user/project-b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn project_id_is_8_char_hex() {
        let id = project_id("/some/path");
        assert_eq!(id.len(), 8);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "project_id should only contain hex characters, got: {id}"
        );
    }

    #[test]
    fn project_id_empty_string_input() {
        let id = project_id("");
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── tmux_session_name tests ───────────────────────────────────────

    #[test]
    fn tmux_session_name_correct_format() {
        let name = tmux_session_name("abcd1234", "my-session");
        assert_eq!(name, "hydra-abcd1234-my-session");
    }

    #[test]
    fn tmux_session_name_with_empty_name() {
        let name = tmux_session_name("abcd1234", "");
        assert_eq!(name, "hydra-abcd1234-");
    }

    // ── parse_session_name tests ──────────────────────────────────────

    #[test]
    fn parse_session_name_roundtrip() {
        let pid = project_id("/home/user/my-project");
        let session = "worker-1";
        let tmux = tmux_session_name(&pid, session);
        let parsed = parse_session_name(&tmux, &pid);
        assert_eq!(parsed, Some(session.to_string()));
    }

    #[test]
    fn parse_session_name_wrong_prefix_returns_none() {
        let result = parse_session_name("other-prefix-session", "abcd1234");
        assert_eq!(result, None);
    }

    #[test]
    fn parse_session_name_wrong_project_id_returns_none() {
        let tmux = tmux_session_name("aaaaaaaa", "session");
        let result = parse_session_name(&tmux, "bbbbbbbb");
        assert_eq!(result, None);
    }

    #[test]
    fn parse_session_name_exact_prefix_no_name() {
        let tmux = "hydra-abcd1234-";
        let result = parse_session_name(tmux, "abcd1234");
        assert_eq!(result, Some(String::new()));
    }

    // ── AgentType::command tests ──────────────────────────────────────

    #[test]
    fn agent_type_command_claude() {
        assert_eq!(AgentType::Claude.command(), "claude --dangerously-skip-permissions");
    }

    #[test]
    fn agent_type_command_codex() {
        assert_eq!(AgentType::Codex.command(), "codex --yolo");
    }

    // ── AgentType::all tests ──────────────────────────────────────────

    #[test]
    fn agent_type_all_returns_both_variants() {
        let all = AgentType::all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], AgentType::Claude);
        assert_eq!(all[1], AgentType::Codex);
    }

    // ── AgentType Display tests ───────────────────────────────────────

    #[test]
    fn agent_type_display_claude() {
        assert_eq!(format!("{}", AgentType::Claude), "Claude");
    }

    #[test]
    fn agent_type_display_codex() {
        assert_eq!(format!("{}", AgentType::Codex), "Codex");
    }

    // ── AgentType FromStr tests ───────────────────────────────────────

    #[test]
    fn agent_type_from_str_claude_lowercase() {
        let agent = AgentType::from_str("claude").unwrap();
        assert_eq!(agent, AgentType::Claude);
    }

    #[test]
    fn agent_type_from_str_codex_lowercase() {
        let agent = AgentType::from_str("codex").unwrap();
        assert_eq!(agent, AgentType::Codex);
    }

    #[test]
    fn agent_type_from_str_case_insensitive_uppercase() {
        let agent = AgentType::from_str("CLAUDE").unwrap();
        assert_eq!(agent, AgentType::Claude);
    }

    #[test]
    fn agent_type_from_str_case_insensitive_mixed() {
        let agent = AgentType::from_str("Codex").unwrap();
        assert_eq!(agent, AgentType::Codex);
    }

    #[test]
    fn agent_type_from_str_invalid_returns_error() {
        let result = AgentType::from_str("gpt");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Unknown agent type"),
            "Error message should mention 'Unknown agent type', got: {err_msg}"
        );
    }

    #[test]
    fn agent_type_from_str_empty_returns_error() {
        let result = AgentType::from_str("");
        assert!(result.is_err());
    }
}
