use sha2::{Digest, Sha256};
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentType {
    Claude,
    Codex,
    Gemini,
}

impl AgentType {
    pub fn command(&self) -> &str {
        match self {
            AgentType::Claude => "claude --dangerously-skip-permissions",
            AgentType::Codex => "codex -c check_for_update_on_startup=false --yolo",
            AgentType::Gemini => "gemini --yolo",
        }
    }

    pub fn all() -> &'static [AgentType] {
        &[AgentType::Claude, AgentType::Codex, AgentType::Gemini]
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentType::Claude => write!(f, "Claude"),
            AgentType::Codex => write!(f, "Codex"),
            AgentType::Gemini => write!(f, "Gemini"),
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(AgentType::Claude),
            "codex" => Ok(AgentType::Codex),
            "gemini" => Ok(AgentType::Gemini),
            _ => Err(anyhow::anyhow!(
                "Unknown agent type: {s}. Use 'claude', 'codex', or 'gemini'."
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Running,
    Idle,
    Exited,
}

impl SessionStatus {
    /// Sort priority: Idle (needs input) first, then Running, then Exited.
    pub fn sort_order(&self) -> u8 {
        match self {
            SessionStatus::Idle => 0,
            SessionStatus::Running => 1,
            SessionStatus::Exited => 2,
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionStatus::Running => write!(f, "Running"),
            SessionStatus::Idle => write!(f, "Idle"),
            SessionStatus::Exited => write!(f, "Exited"),
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl Session {
    pub fn id(&self) -> SessionId {
        SessionId(self.tmux_name.clone())
    }
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

const AUTO_NAMES: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
    "uniform", "victor", "whiskey", "xray", "yankee", "zulu",
];

/// Generate the next available session name from the NATO phonetic alphabet.
pub fn generate_name(existing: &[String]) -> String {
    for name in AUTO_NAMES {
        if !existing.iter().any(|n| n == name) {
            return name.to_string();
        }
    }
    let mut i = AUTO_NAMES.len() + 1;
    loop {
        let name = format!("agent-{i}");
        if !existing.contains(&name) {
            return name;
        }
        i += 1;
    }
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

    #[test]
    fn session_id_new_and_display() {
        let id = SessionId::new("hydra-abcd1234-alpha");
        assert_eq!(id.as_str(), "hydra-abcd1234-alpha");
        assert_eq!(id.to_string(), "hydra-abcd1234-alpha");
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
        assert_eq!(
            AgentType::Claude.command(),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn agent_type_command_codex() {
        assert_eq!(
            AgentType::Codex.command(),
            "codex -c check_for_update_on_startup=false --yolo"
        );
    }

    #[test]
    fn agent_type_command_gemini() {
        assert_eq!(AgentType::Gemini.command(), "gemini --yolo");
    }

    // ── AgentType::all tests ──────────────────────────────────────────

    #[test]
    fn agent_type_all_returns_all_variants() {
        let all = AgentType::all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], AgentType::Claude);
        assert_eq!(all[1], AgentType::Codex);
        assert_eq!(all[2], AgentType::Gemini);
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

    #[test]
    fn agent_type_display_gemini() {
        assert_eq!(format!("{}", AgentType::Gemini), "Gemini");
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
    fn agent_type_from_str_gemini_lowercase() {
        let agent = AgentType::from_str("gemini").unwrap();
        assert_eq!(agent, AgentType::Gemini);
    }

    #[test]
    fn agent_type_from_str_gemini_mixed_case() {
        let agent = AgentType::from_str("Gemini").unwrap();
        assert_eq!(agent, AgentType::Gemini);
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

    // ── generate_name tests ──────────────────────────────────────────

    #[test]
    fn generate_name_first_is_alpha() {
        let name = generate_name(&[]);
        assert_eq!(name, "alpha");
    }

    #[test]
    fn generate_name_skips_existing() {
        let existing = vec!["alpha".to_string(), "bravo".to_string()];
        let name = generate_name(&existing);
        assert_eq!(name, "charlie");
    }

    #[test]
    fn generate_name_fills_gaps() {
        let existing = vec!["alpha".to_string(), "charlie".to_string()];
        let name = generate_name(&existing);
        assert_eq!(name, "bravo");
    }

    #[test]
    fn generate_name_fallback_when_all_taken() {
        let all_taken: Vec<String> = AUTO_NAMES.iter().map(|s| s.to_string()).collect();
        let name = generate_name(&all_taken);
        assert_eq!(name, "agent-27");
    }

    #[test]
    fn generate_name_fallback_skips_existing() {
        let mut names: Vec<String> = AUTO_NAMES.iter().map(|s| s.to_string()).collect();
        names.push("agent-27".to_string());
        let name = generate_name(&names);
        assert_eq!(name, "agent-28");
    }

    // ── format_duration tests ────────────────────────────────────────

    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(1)), "1s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 00s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_duration_hours_and_minutes() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 00m");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h 00m");
    }

    #[test]
    fn format_duration_ignores_subsecond() {
        assert_eq!(format_duration(Duration::from_millis(999)), "0s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1s");
    }

    // ── SessionStatus::sort_order tests ─────────────────────────────

    // ── proptest ──────────────────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn generate_name_always_returns_nonempty(
                existing in proptest::collection::vec("[a-z]{1,10}", 0..30)
            ) {
                let name = generate_name(&existing);
                prop_assert!(!name.is_empty());
            }

            #[test]
            fn generate_name_never_collides_with_existing(
                existing in proptest::collection::vec("[a-z]{1,10}", 0..30)
            ) {
                let name = generate_name(&existing);
                prop_assert!(
                    !existing.contains(&name),
                    "generated name '{}' collides with existing names",
                    name
                );
            }

            #[test]
            fn generate_name_is_deterministic(
                existing in proptest::collection::vec("[a-z]{1,10}", 0..30)
            ) {
                let name1 = generate_name(&existing);
                let name2 = generate_name(&existing);
                prop_assert_eq!(name1, name2);
            }

            #[test]
            fn project_id_is_8_hex_chars(path in ".*") {
                let id = project_id(&path);
                prop_assert_eq!(id.len(), 8);
                prop_assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
            }

            #[test]
            fn parse_session_name_roundtrips(
                path in ".{1,50}",
                name in "[a-z][a-z0-9-]{0,20}"
            ) {
                let pid = project_id(&path);
                let tmux = tmux_session_name(&pid, &name);
                let parsed = parse_session_name(&tmux, &pid);
                prop_assert_eq!(parsed, Some(name));
            }

            #[test]
            fn format_duration_never_panics(secs in 0u64..1_000_000) {
                let _ = format_duration(Duration::from_secs(secs));
            }
        }
    }

    #[test]
    fn sort_order_idle_is_lowest() {
        assert_eq!(SessionStatus::Idle.sort_order(), 0);
    }

    #[test]
    fn sort_order_running_is_middle() {
        assert_eq!(SessionStatus::Running.sort_order(), 1);
    }

    #[test]
    fn sort_order_exited_is_highest() {
        assert_eq!(SessionStatus::Exited.sort_order(), 2);
    }

    #[test]
    fn sort_order_produces_correct_ordering() {
        let mut statuses = vec![
            SessionStatus::Exited,
            SessionStatus::Running,
            SessionStatus::Idle,
        ];
        statuses.sort_by_key(|s| s.sort_order());
        assert_eq!(
            statuses,
            vec![
                SessionStatus::Idle,
                SessionStatus::Running,
                SessionStatus::Exited,
            ]
        );
    }
}
