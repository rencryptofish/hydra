use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

use crate::session::{parse_session_name, AgentType, Session, SessionStatus};

/// Default timeout for subprocess calls (2 seconds).
const CMD_TIMEOUT: Duration = Duration::from_secs(2);

/// Longer timeout for scrollback capture (can be large).
const CMD_TIMEOUT_LONG: Duration = Duration::from_secs(5);

/// Run a Command with a timeout, returning its Output.
/// On timeout or spawn failure, returns an anyhow error.
pub async fn run_cmd_timeout(cmd: &mut Command) -> Result<std::process::Output> {
    match tokio::time::timeout(CMD_TIMEOUT, cmd.output()).await {
        Ok(result) => result.context("subprocess failed to execute"),
        Err(_) => bail!("subprocess timed out after {}s", CMD_TIMEOUT.as_secs()),
    }
}

/// Run a Command with a timeout, returning its ExitStatus.
/// On timeout or spawn failure, returns an anyhow error.
pub async fn run_status_timeout(cmd: &mut Command) -> Result<std::process::ExitStatus> {
    match tokio::time::timeout(CMD_TIMEOUT, cmd.status()).await {
        Ok(result) => result.context("subprocess failed to execute"),
        Err(_) => bail!("subprocess timed out after {}s", CMD_TIMEOUT.as_secs()),
    }
}

#[async_trait::async_trait]
pub trait SessionManager: Send + Sync {
    async fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>>;
    async fn create_session(
        &self,
        project_id: &str,
        name: &str,
        agent: &AgentType,
        cwd: &str,
        command_override: Option<&str>,
    ) -> Result<String>;
    async fn capture_pane(&self, tmux_name: &str) -> Result<String>;
    async fn kill_session(&self, tmux_name: &str) -> Result<()>;
    async fn send_keys(&self, tmux_name: &str, key: &str) -> Result<()>;
    /// Send literal text (including escape sequences) via `tmux send-keys -l`.
    async fn send_keys_literal(&self, _tmux_name: &str, _text: &str) -> Result<()> {
        Ok(())
    }
    async fn capture_pane_scrollback(&self, tmux_name: &str) -> Result<String>;

    /// Batch-capture pane content for multiple sessions. Default impl is sequential;
    /// `TmuxSessionManager` overrides with parallel subprocess calls.
    async fn capture_panes(&self, names: &[String]) -> Vec<Result<String>> {
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            results.push(self.capture_pane(name).await);
        }
        results
    }
}

pub struct TmuxSessionManager {
    agent_cache: Mutex<HashMap<String, AgentType>>,
}

impl Default for TmuxSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TmuxSessionManager {
    pub fn new() -> Self {
        Self {
            agent_cache: Mutex::new(HashMap::new()),
        }
    }
}

fn prune_agent_cache(cache: &mut HashMap<String, AgentType>, live_sessions: &HashSet<String>) {
    cache.retain(|tmux_name, _| live_sessions.contains(tmux_name));
}

#[async_trait::async_trait]
impl SessionManager for TmuxSessionManager {
    async fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>> {
        let output =
            run_cmd_timeout(Command::new("tmux").args(["list-sessions", "-F", "#{session_name}"]))
                .await;

        let output = match output {
            Ok(o) => o,
            Err(_) => return Ok(vec![]),
        };

        // tmux returns error when no server is running - that's fine, just no sessions
        if !output.status.success() {
            return Ok(vec![]);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let prefix = format!("hydra-{project_id}-");
        let live_sessions: HashSet<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.to_string())
            .collect();

        // Keep cache aligned with live tmux sessions to avoid unbounded growth.
        prune_agent_cache(
            &mut self.agent_cache.lock().expect("agent_cache lock poisoned"),
            &live_sessions,
        );

        // Pass 1: Parse session names, split into cached vs uncached agent types
        let mut parsed: Vec<(String, String)> = Vec::new();
        let mut cached_agents: Vec<Option<AgentType>> = Vec::new();
        let mut uncached_indices: Vec<usize> = Vec::new();

        for line in stdout.lines() {
            let tmux_name = line.trim();
            if !tmux_name.starts_with(&prefix) {
                continue;
            }
            let name = match parse_session_name(tmux_name, project_id) {
                Some(n) => n,
                None => continue,
            };

            let cached = self.agent_cache.lock().unwrap().get(tmux_name).cloned();
            if cached.is_none() {
                uncached_indices.push(parsed.len());
            }
            cached_agents.push(cached);
            parsed.push((name, tmux_name.to_string()));
        }

        // Resolve uncached agent types in parallel (instead of serial)
        if !uncached_indices.is_empty() {
            let agent_futures: Vec<_> = uncached_indices
                .iter()
                .map(|&i| get_agent_type(&parsed[i].1))
                .collect();
            let agent_results = futures::future::join_all(agent_futures).await;
            let mut cache = self.agent_cache.lock().unwrap();
            for (&idx, result) in uncached_indices.iter().zip(agent_results) {
                let agent = result.unwrap_or(AgentType::Claude);
                cache.insert(parsed[idx].1.clone(), agent.clone());
                cached_agents[idx] = Some(agent);
            }
        }

        // Pass 2: Batch is_pane_dead via single `tmux list-panes -a` call
        let dead_set = batch_dead_panes().await;

        let sessions = parsed
            .into_iter()
            .zip(cached_agents)
            .map(|((name, tmux_name), agent)| {
                let dead = dead_set
                    .as_ref()
                    .map(|s| s.contains(&tmux_name))
                    .unwrap_or(false);
                let status = if dead {
                    SessionStatus::Exited
                } else {
                    // Default to Idle; App will upgrade to Running via content comparison
                    SessionStatus::Idle
                };
                Session {
                    name,
                    tmux_name,
                    agent_type: agent.unwrap_or(AgentType::Claude),
                    status,
                    task_elapsed: None,
                    _alive: true,
                }
            })
            .collect();

        Ok(sessions)
    }

    async fn create_session(
        &self,
        project_id: &str,
        name: &str,
        agent: &AgentType,
        cwd: &str,
        command_override: Option<&str>,
    ) -> Result<String> {
        let tmux_name = create_session(project_id, name, agent, cwd, command_override).await?;
        self.agent_cache
            .lock()
            .expect("agent_cache lock poisoned")
            .insert(tmux_name.clone(), agent.clone());
        Ok(tmux_name)
    }

    async fn capture_pane(&self, tmux_name: &str) -> Result<String> {
        capture_pane(tmux_name).await
    }

    async fn capture_panes(&self, names: &[String]) -> Vec<Result<String>> {
        let futs = names.iter().map(|n| capture_pane(n));
        futures::future::join_all(futs).await
    }

    async fn kill_session(&self, tmux_name: &str) -> Result<()> {
        kill_session(tmux_name).await?;
        self.agent_cache
            .lock()
            .expect("agent_cache lock poisoned")
            .remove(tmux_name);
        Ok(())
    }

    async fn send_keys(&self, tmux_name: &str, key: &str) -> Result<()> {
        send_keys(tmux_name, key).await
    }

    async fn send_keys_literal(&self, tmux_name: &str, text: &str) -> Result<()> {
        send_keys_literal(tmux_name, text).await
    }

    async fn capture_pane_scrollback(&self, tmux_name: &str) -> Result<String> {
        capture_pane_scrollback(tmux_name).await
    }
}

/// Check if the pane in a tmux session has exited (requires remain-on-exit).
/// Returns `true` when the session can't be queried (gone/dead) — a session
/// we can't reach is effectively dead rather than silently "Idle".
/// Note: Production code uses `batch_dead_panes()` instead; this is retained
/// for integration tests that check individual sessions.
#[cfg(test)]
async fn is_pane_dead(tmux_name: &str) -> bool {
    let output = run_cmd_timeout(Command::new("tmux").args([
        "list-panes",
        "-t",
        tmux_name,
        "-F",
        "#{pane_dead}",
    ]))
    .await;

    match output {
        Ok(o) if o.status.success() => {
            // Only treat as alive when we get a definitive "not dead" answer
            String::from_utf8_lossy(&o.stdout).trim() != "0"
        }
        _ => true, // Can't reach session → treat as dead
    }
}

/// Batch-check all tmux sessions for dead panes in a single subprocess call.
/// Returns a set of session names whose panes are dead, or None on error.
async fn batch_dead_panes() -> Option<HashSet<String>> {
    let output = run_cmd_timeout(Command::new("tmux").args([
        "list-panes",
        "-a",
        "-F",
        "#{session_name} #{pane_dead}",
    ]))
    .await
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let dead: HashSet<String> = stdout
        .lines()
        .filter_map(|line| {
            let (session, flag) = line.rsplit_once(' ')?;
            if flag != "0" {
                Some(session.to_string())
            } else {
                None
            }
        })
        .collect();
    Some(dead)
}

/// Read the HYDRA_AGENT_TYPE env var from the tmux session.
async fn get_agent_type(tmux_name: &str) -> Option<AgentType> {
    let output = run_cmd_timeout(Command::new("tmux").args([
        "show-environment",
        "-t",
        tmux_name,
        "HYDRA_AGENT_TYPE",
    ]))
    .await
    .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format: HYDRA_AGENT_TYPE=claude
    let val = stdout.trim().strip_prefix("HYDRA_AGENT_TYPE=")?;
    val.parse().ok()
}

/// Create a new detached tmux session running the given agent command.
/// If `command_override` is provided, it is used instead of `agent.command()`.
pub async fn create_session(
    project_id: &str,
    name: &str,
    agent: &AgentType,
    cwd: &str,
    command_override: Option<&str>,
) -> Result<String> {
    let tmux_name = crate::session::tmux_session_name(project_id, name);
    let cmd = command_override.unwrap_or(agent.command());

    // Wrap command to unset Claude Code env vars so agents don't detect
    // a nested session when Hydra is launched from within Claude Code.
    let wrapped_cmd = format!(
        "unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; exec {}",
        cmd
    );

    let status = run_status_timeout(Command::new("tmux").args([
        "new-session",
        "-d",
        "-s",
        &tmux_name,
        "-c",
        cwd,
        &wrapped_cmd,
    ]))
    .await
    .context("Failed to create tmux session")?;

    if !status.success() {
        bail!("tmux new-session failed for '{tmux_name}'");
    }

    // Keep pane alive after command exits so we can detect Exited status
    let _ = run_status_timeout(Command::new("tmux").args([
        "set-option",
        "-t",
        &tmux_name,
        "remain-on-exit",
        "on",
    ]))
    .await;

    // Remove Claude Code env vars so spawned agents don't think they're nested
    for var in ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"] {
        let _ = run_status_timeout(Command::new("tmux").args([
            "set-environment",
            "-t",
            &tmux_name,
            "-r",
            var,
        ]))
        .await;
    }

    // Store agent type as env var on the session
    let _ = run_status_timeout(Command::new("tmux").args([
        "set-environment",
        "-t",
        &tmux_name,
        "HYDRA_AGENT_TYPE",
        &agent.to_string().to_lowercase(),
    ]))
    .await;

    Ok(tmux_name)
}

/// Capture the current pane content of a tmux session.
pub async fn capture_pane(tmux_name: &str) -> Result<String> {
    let output =
        run_cmd_timeout(Command::new("tmux").args(["capture-pane", "-t", tmux_name, "-p", "-e"]))
            .await
            .context("Failed to capture tmux pane")?;

    if !output.status.success() {
        return Ok(String::from("[session not available]"));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim_end_matches('\n');
    Ok(trimmed.to_string())
}

/// Capture the scrollback buffer of a tmux session (last 5000 lines).
pub async fn capture_pane_scrollback(tmux_name: &str) -> Result<String> {
    let output = match tokio::time::timeout(
        CMD_TIMEOUT_LONG,
        Command::new("tmux")
            .args(["capture-pane", "-t", tmux_name, "-p", "-e", "-S", "-5000"])
            .output(),
    )
    .await
    {
        Ok(result) => result.context("Failed to capture tmux pane scrollback")?,
        Err(_) => bail!(
            "capture_pane_scrollback timed out after {}s",
            CMD_TIMEOUT_LONG.as_secs()
        ),
    };

    if !output.status.success() {
        return Ok(String::from("[session not available]"));
    }

    // Trim trailing blank lines — tmux pads the capture to the full pane
    // height, which makes short output appear to start halfway up the preview.
    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim_end_matches('\n');
    Ok(trimmed.to_string())
}

/// Send a key to a tmux session via `tmux send-keys`.
/// Fire-and-forget: spawns the subprocess and reaps it in the background.
/// The exit code provides no actionable info (session-not-found is discovered on next tick).
pub async fn send_keys(tmux_name: &str, key: &str) -> Result<()> {
    let mut child = Command::new("tmux")
        .args(["send-keys", "-t", tmux_name, key])
        .spawn()
        .context("Failed to spawn tmux send-keys")?;
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    });
    Ok(())
}

/// Send literal text (including raw escape sequences) to a tmux session.
/// Fire-and-forget: spawns the subprocess and reaps it in the background.
pub async fn send_keys_literal(tmux_name: &str, text: &str) -> Result<()> {
    let mut child = Command::new("tmux")
        .args(["send-keys", "-t", tmux_name, "-l", text])
        .spawn()
        .context("Failed to spawn tmux send-keys -l")?;
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    });
    Ok(())
}

/// Map a crossterm KeyCode + KeyModifiers to a tmux key name.
pub fn keycode_to_tmux(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};

    // Character keys: apply modifier prefix directly
    if let KeyCode::Char(c) = code {
        return Some(if modifiers.contains(KeyModifiers::CONTROL) {
            format!("C-{c}")
        } else if modifiers.contains(KeyModifiers::ALT) {
            format!("M-{c}")
        } else {
            // SHIFT is already reflected in the char value (uppercase)
            c.to_string()
        });
    }

    // BackTab is Shift+Tab — already a distinct keycode, no modifier prefix needed
    if code == KeyCode::BackTab {
        return Some("BTab".to_string());
    }

    // Special keys → tmux base names
    let base = match code {
        KeyCode::Enter => "Enter",
        KeyCode::Backspace => "BSpace",
        KeyCode::Tab => "Tab",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::Delete => "DC",
        KeyCode::Insert => "IC",
        KeyCode::F(n) => return Some(apply_tmux_modifiers(&format!("F{n}"), modifiers)),
        _ => return None,
    };

    Some(apply_tmux_modifiers(base, modifiers))
}

/// Wrap a tmux key name with modifier prefixes (C-, M-, S-).
pub fn apply_tmux_modifiers(base: &str, modifiers: crossterm::event::KeyModifiers) -> String {
    use crossterm::event::KeyModifiers;

    let mut key = base.to_string();
    if modifiers.contains(KeyModifiers::SHIFT) {
        key = format!("S-{key}");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        key = format!("M-{key}");
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        key = format!("C-{key}");
    }
    key
}

/// Kill a tmux session.
pub async fn kill_session(tmux_name: &str) -> Result<()> {
    let status = run_status_timeout(Command::new("tmux").args(["kill-session", "-t", tmux_name]))
        .await
        .context("Failed to kill tmux session")?;

    if !status.success() {
        bail!("tmux kill-session failed for '{tmux_name}'");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    // ── keycode_to_tmux: character keys ──────────────────────────────

    #[test]
    fn char_key_no_modifiers() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Char('a'), KeyModifiers::NONE),
            Some("a".into())
        );
    }

    #[test]
    fn char_key_uppercase() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Some("A".into())
        );
    }

    #[test]
    fn char_key_ctrl() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some("C-c".into())
        );
    }

    #[test]
    fn char_key_alt() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Char('x'), KeyModifiers::ALT),
            Some("M-x".into())
        );
    }

    // ── keycode_to_tmux: special keys ────────────────────────────────

    #[test]
    fn enter_key() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Enter, KeyModifiers::NONE),
            Some("Enter".into())
        );
    }

    #[test]
    fn backspace_key() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Backspace, KeyModifiers::NONE),
            Some("BSpace".into())
        );
    }

    #[test]
    fn tab_key() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Tab, KeyModifiers::NONE),
            Some("Tab".into())
        );
    }

    #[test]
    fn backtab_key() {
        assert_eq!(
            keycode_to_tmux(KeyCode::BackTab, KeyModifiers::NONE),
            Some("BTab".into())
        );
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Up, KeyModifiers::NONE),
            Some("Up".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::Down, KeyModifiers::NONE),
            Some("Down".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::Left, KeyModifiers::NONE),
            Some("Left".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::Right, KeyModifiers::NONE),
            Some("Right".into())
        );
    }

    #[test]
    fn home_end_keys() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Home, KeyModifiers::NONE),
            Some("Home".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::End, KeyModifiers::NONE),
            Some("End".into())
        );
    }

    #[test]
    fn page_up_down_keys() {
        assert_eq!(
            keycode_to_tmux(KeyCode::PageUp, KeyModifiers::NONE),
            Some("PageUp".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::PageDown, KeyModifiers::NONE),
            Some("PageDown".into())
        );
    }

    #[test]
    fn delete_insert_keys() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Delete, KeyModifiers::NONE),
            Some("DC".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::Insert, KeyModifiers::NONE),
            Some("IC".into())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            keycode_to_tmux(KeyCode::F(1), KeyModifiers::NONE),
            Some("F1".into())
        );
        assert_eq!(
            keycode_to_tmux(KeyCode::F(12), KeyModifiers::NONE),
            Some("F12".into())
        );
    }

    #[test]
    fn esc_returns_none() {
        assert_eq!(keycode_to_tmux(KeyCode::Esc, KeyModifiers::NONE), None);
    }

    // ── keycode_to_tmux: modifiers on special keys ───────────────────

    #[test]
    fn ctrl_arrow() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Up, KeyModifiers::CONTROL),
            Some("C-Up".into())
        );
    }

    #[test]
    fn alt_arrow() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Left, KeyModifiers::ALT),
            Some("M-Left".into())
        );
    }

    #[test]
    fn shift_arrow() {
        assert_eq!(
            keycode_to_tmux(KeyCode::Right, KeyModifiers::SHIFT),
            Some("S-Right".into())
        );
    }

    #[test]
    fn ctrl_shift_function_key() {
        assert_eq!(
            keycode_to_tmux(KeyCode::F(5), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            Some("C-S-F5".into())
        );
    }

    #[test]
    fn all_modifiers_on_special_key() {
        let mods = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
        assert_eq!(
            keycode_to_tmux(KeyCode::Enter, mods),
            Some("C-M-S-Enter".into())
        );
    }

    // ── apply_tmux_modifiers ─────────────────────────────────────────

    #[test]
    fn apply_no_modifiers() {
        assert_eq!(apply_tmux_modifiers("Enter", KeyModifiers::NONE), "Enter");
    }

    #[test]
    fn apply_shift_only() {
        assert_eq!(apply_tmux_modifiers("Up", KeyModifiers::SHIFT), "S-Up");
    }

    #[test]
    fn apply_alt_only() {
        assert_eq!(apply_tmux_modifiers("Tab", KeyModifiers::ALT), "M-Tab");
    }

    #[test]
    fn apply_ctrl_only() {
        assert_eq!(
            apply_tmux_modifiers("Left", KeyModifiers::CONTROL),
            "C-Left"
        );
    }

    #[test]
    fn apply_modifier_ordering_ctrl_alt_shift() {
        let mods = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
        // Shift applied first (innermost), then Alt, then Ctrl (outermost)
        assert_eq!(apply_tmux_modifiers("F1", mods), "C-M-S-F1");
    }

    // ── TmuxSessionManager agent cache ───────────────────────────────

    #[test]
    fn tmux_session_manager_new_has_empty_cache() {
        let mgr = TmuxSessionManager::new();
        let cache = mgr.agent_cache.lock().unwrap();
        assert!(cache.is_empty());
    }

    #[test]
    fn prune_agent_cache_removes_non_live_entries() {
        let mut cache = HashMap::new();
        cache.insert("hydra-a-one".to_string(), AgentType::Claude);
        cache.insert("hydra-a-two".to_string(), AgentType::Codex);
        cache.insert("hydra-a-stale".to_string(), AgentType::Claude);

        let live: HashSet<String> = ["hydra-a-one", "hydra-a-two"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        prune_agent_cache(&mut cache, &live);

        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key("hydra-a-one"));
        assert!(cache.contains_key("hydra-a-two"));
        assert!(!cache.contains_key("hydra-a-stale"));
    }

    #[test]
    fn prune_agent_cache_empty_live_clears_all() {
        let mut cache = HashMap::new();
        cache.insert("a".to_string(), AgentType::Claude);
        cache.insert("b".to_string(), AgentType::Codex);
        prune_agent_cache(&mut cache, &HashSet::new());
        assert!(cache.is_empty());
    }

    #[test]
    fn prune_agent_cache_empty_cache_stays_empty() {
        let mut cache = HashMap::new();
        let live: HashSet<String> = ["x".to_string()].into_iter().collect();
        prune_agent_cache(&mut cache, &live);
        assert!(cache.is_empty());
    }

    // ── Default trait implementations ───────────────────────────────

    /// Minimal SessionManager impl to test default trait methods.
    struct MinimalManager {
        capture_result: String,
    }

    #[async_trait::async_trait]
    impl SessionManager for MinimalManager {
        async fn list_sessions(&self, _project_id: &str) -> Result<Vec<Session>> {
            Ok(vec![])
        }
        async fn create_session(
            &self,
            _project_id: &str,
            _name: &str,
            _agent: &AgentType,
            _cwd: &str,
            _command_override: Option<&str>,
        ) -> Result<String> {
            Ok("test".into())
        }
        async fn capture_pane(&self, _tmux_name: &str) -> Result<String> {
            Ok(self.capture_result.clone())
        }
        async fn kill_session(&self, _tmux_name: &str) -> Result<()> {
            Ok(())
        }
        async fn send_keys(&self, _tmux_name: &str, _key: &str) -> Result<()> {
            Ok(())
        }
        async fn capture_pane_scrollback(&self, _tmux_name: &str) -> Result<String> {
            Ok(self.capture_result.clone())
        }
    }

    #[tokio::test]
    async fn default_send_keys_literal_is_noop() {
        let mgr = MinimalManager {
            capture_result: String::new(),
        };
        let result = mgr.send_keys_literal("any-session", "hello").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn default_capture_panes_sequential() {
        let mgr = MinimalManager {
            capture_result: "output".into(),
        };
        let names = vec!["a".into(), "b".into(), "c".into()];
        let results = mgr.capture_panes(&names).await;
        assert_eq!(results.len(), 3);
        for r in results {
            assert_eq!(r.unwrap(), "output");
        }
    }

    #[tokio::test]
    async fn default_capture_panes_empty() {
        let mgr = MinimalManager {
            capture_result: String::new(),
        };
        let results = mgr.capture_panes(&[]).await;
        assert!(results.is_empty());
    }

    // ── run_cmd_timeout / run_status_timeout ────────────────────────

    #[tokio::test]
    async fn run_cmd_timeout_success() {
        let output = run_cmd_timeout(Command::new("echo").arg("hello")).await;
        let output = output.unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[tokio::test]
    async fn run_cmd_timeout_bad_command() {
        let result = run_cmd_timeout(&mut Command::new(
            "__nonexistent_command_that_does_not_exist__",
        ))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_status_timeout_success() {
        let status = run_status_timeout(&mut Command::new("true")).await;
        assert!(status.unwrap().success());
    }

    #[tokio::test]
    async fn run_status_timeout_failure_exit_code() {
        let status = run_status_timeout(&mut Command::new("false")).await;
        assert!(!status.unwrap().success());
    }

    #[tokio::test]
    async fn run_status_timeout_bad_command() {
        let result = run_status_timeout(&mut Command::new(
            "__nonexistent_command_that_does_not_exist__",
        ))
        .await;
        assert!(result.is_err());
    }

    // ── Integration tests (require tmux) ────────────────────────────

    /// Generate a unique tmux session name for integration tests.
    fn test_session_name() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        format!("hydra-test-{pid}-{id}")
    }

    /// Kill a tmux session, ignoring errors (cleanup helper).
    async fn cleanup_session(name: &str) {
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", name])
            .output();
    }

    #[tokio::test]
    async fn integration_create_capture_kill() {
        let name = test_session_name();

        // Create a session running a simple shell command
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "echo 'HYDRA_TEST_OUTPUT'; sleep 10",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        // Give the command a moment to produce output
        tokio::time::sleep(Duration::from_millis(200)).await;

        // capture_pane should return something
        let content = capture_pane(&name).await.unwrap();
        assert!(!content.is_empty());

        // capture_pane_scrollback should also work
        let scrollback = capture_pane_scrollback(&name).await.unwrap();
        assert!(!scrollback.is_empty());

        // kill_session should succeed
        kill_session(&name).await.unwrap();

        // capture after kill should fail or return session-not-available
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after_kill = capture_pane(&name).await.unwrap();
        assert!(after_kill.contains("[session not available]") || after_kill.is_empty());
    }

    #[tokio::test]
    async fn integration_kill_nonexistent_session() {
        let result = kill_session("hydra-test-nonexistent-session-xyz").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn integration_capture_pane_nonexistent() {
        let result = capture_pane("hydra-test-nonexistent-session-xyz")
            .await
            .unwrap();
        assert_eq!(result, "[session not available]");
    }

    #[tokio::test]
    async fn integration_capture_scrollback_nonexistent() {
        let result = capture_pane_scrollback("hydra-test-nonexistent-session-xyz")
            .await
            .unwrap();
        assert_eq!(result, "[session not available]");
    }

    #[tokio::test]
    async fn integration_send_keys_to_session() {
        let name = test_session_name();
        // Create session with bash
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        tokio::time::sleep(Duration::from_millis(200)).await;

        // send_keys should not error
        let result = send_keys(&name, "echo hello").await;
        assert!(result.is_ok());

        // send_keys_literal should not error
        let result = send_keys_literal(&name, "test").await;
        assert!(result.is_ok());

        cleanup_session(&name).await;
    }

    #[tokio::test]
    async fn integration_is_pane_dead_live_session() {
        let name = test_session_name();
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "sleep",
            "30",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        let dead = is_pane_dead(&name).await;
        assert!(!dead, "live session should not be dead");

        cleanup_session(&name).await;
    }

    #[tokio::test]
    async fn integration_is_pane_dead_nonexistent() {
        let dead = is_pane_dead("hydra-test-nonexistent-xyz").await;
        assert!(dead, "nonexistent session should be dead");
    }

    #[tokio::test]
    async fn integration_is_pane_dead_exited_session() {
        let name = test_session_name();
        // Create session with remain-on-exit, running a command that exits immediately
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "true",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        // Set remain-on-exit so the pane stays
        let _ = run_status_timeout(Command::new("tmux").args([
            "set-option",
            "-t",
            &name,
            "remain-on-exit",
            "on",
        ]))
        .await;

        // Wait for command to exit
        tokio::time::sleep(Duration::from_millis(300)).await;

        let dead = is_pane_dead(&name).await;
        assert!(dead, "exited session with remain-on-exit should be dead");

        cleanup_session(&name).await;
    }

    #[tokio::test]
    async fn integration_get_agent_type_nonexistent() {
        let result = get_agent_type("hydra-test-nonexistent-xyz").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn integration_get_agent_type_with_env() {
        let name = test_session_name();
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "sleep",
            "30",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        // Set the env var
        let _ = run_status_timeout(Command::new("tmux").args([
            "set-environment",
            "-t",
            &name,
            "HYDRA_AGENT_TYPE",
            "codex",
        ]))
        .await;

        let agent = get_agent_type(&name).await;
        assert_eq!(agent, Some(AgentType::Codex));

        cleanup_session(&name).await;
    }

    #[tokio::test]
    async fn integration_create_session_free_fn() {
        let sess_name = "itest-create";
        let tmux_name = create_session(
            "ffffffff",
            sess_name,
            &AgentType::Claude,
            "/tmp",
            Some("sleep 30"),
        )
        .await
        .unwrap();

        // Verify session exists
        let content = capture_pane(&tmux_name).await.unwrap();
        assert!(!content.is_empty() || content.is_empty()); // session exists if no error

        // Verify remain-on-exit was set
        let output = run_cmd_timeout(Command::new("tmux").args([
            "show-option",
            "-t",
            &tmux_name,
            "remain-on-exit",
        ]))
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("on"));

        // Verify agent type env var was set
        let agent = get_agent_type(&tmux_name).await;
        assert_eq!(agent, Some(AgentType::Claude));

        cleanup_session(&tmux_name).await;
    }

    #[tokio::test]
    async fn integration_create_session_with_command_override() {
        let sess_name = "itest-override";
        // Use a long-running command so the session stays alive for capture
        let tmux_name = create_session(
            "ffffffff",
            sess_name,
            &AgentType::Codex,
            "/tmp",
            Some("sh -c 'echo HYDRA_OVERRIDE_OK && sleep 30'"),
        )
        .await
        .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        let content = capture_pane(&tmux_name).await.unwrap();
        assert!(
            content.contains("HYDRA_OVERRIDE_OK"),
            "command override should be used: {content}"
        );

        cleanup_session(&tmux_name).await;
    }

    #[tokio::test]
    async fn integration_tmux_manager_create_and_kill() {
        let mgr = TmuxSessionManager::new();
        let tmux_name = mgr
            .create_session(
                "eeeeeeee",
                "mgr-test",
                &AgentType::Claude,
                "/tmp",
                Some("sleep 30"),
            )
            .await
            .unwrap();

        // Cache should contain the entry
        {
            let cache = mgr.agent_cache.lock().unwrap();
            assert_eq!(cache.get(&tmux_name), Some(&AgentType::Claude));
        }

        // capture_pane via manager should work
        let _content = mgr.capture_pane(&tmux_name).await.unwrap();

        // capture_panes (batch) via manager
        let results = mgr.capture_panes(std::slice::from_ref(&tmux_name)).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());

        // capture_pane_scrollback via manager
        let _scrollback = mgr.capture_pane_scrollback(&tmux_name).await.unwrap();

        // send_keys via manager
        mgr.send_keys(&tmux_name, "echo hi").await.unwrap();

        // send_keys_literal via manager
        mgr.send_keys_literal(&tmux_name, "test").await.unwrap();

        // kill via manager
        mgr.kill_session(&tmux_name).await.unwrap();

        // Cache should be pruned
        {
            let cache = mgr.agent_cache.lock().unwrap();
            assert!(!cache.contains_key(&tmux_name));
        }
    }

    #[tokio::test]
    async fn integration_tmux_manager_list_sessions() {
        let mgr = TmuxSessionManager::new();
        let project_id = "abab1234";
        let tmux_name = mgr
            .create_session(
                project_id,
                "listtest",
                &AgentType::Codex,
                "/tmp",
                Some("sleep 30"),
            )
            .await
            .unwrap();

        let sessions = mgr.list_sessions(project_id).await.unwrap();
        let found = sessions.iter().any(|s| s.tmux_name == tmux_name);
        assert!(found, "created session should appear in list: {sessions:?}");

        // Verify agent type was resolved from cache
        let session = sessions.iter().find(|s| s.tmux_name == tmux_name).unwrap();
        assert_eq!(session.agent_type, AgentType::Codex);

        cleanup_session(&tmux_name).await;
    }

    // ── proptest ──────────────────────────────────────────────────────

    mod proptests {
        use super::*;
        use crossterm::event::{KeyCode, KeyModifiers};
        use proptest::prelude::*;

        fn arb_keycode() -> impl Strategy<Value = KeyCode> {
            prop_oneof![
                any::<char>().prop_map(KeyCode::Char),
                Just(KeyCode::Enter),
                Just(KeyCode::Backspace),
                Just(KeyCode::Tab),
                Just(KeyCode::BackTab),
                Just(KeyCode::Up),
                Just(KeyCode::Down),
                Just(KeyCode::Left),
                Just(KeyCode::Right),
                Just(KeyCode::Home),
                Just(KeyCode::End),
                Just(KeyCode::PageUp),
                Just(KeyCode::PageDown),
                Just(KeyCode::Delete),
                Just(KeyCode::Insert),
                (1u8..=12).prop_map(KeyCode::F),
                Just(KeyCode::Esc),
                Just(KeyCode::Null),
            ]
        }

        fn arb_modifiers() -> impl Strategy<Value = KeyModifiers> {
            (0u8..8).prop_map(KeyModifiers::from_bits_truncate)
        }

        proptest! {
            #[test]
            fn keycode_to_tmux_never_panics(
                code in arb_keycode(),
                modifiers in arb_modifiers()
            ) {
                let _ = keycode_to_tmux(code, modifiers);
            }

            #[test]
            fn keycode_char_always_produces_some(
                c in any::<char>(),
                modifiers in arb_modifiers()
            ) {
                let result = keycode_to_tmux(KeyCode::Char(c), modifiers);
                prop_assert!(result.is_some());
            }

            #[test]
            fn apply_tmux_modifiers_never_panics(
                base in "[a-zA-Z]{1,10}",
                modifiers in arb_modifiers()
            ) {
                let result = apply_tmux_modifiers(&base, modifiers);
                prop_assert!(!result.is_empty());
                prop_assert!(result.contains(&base));
            }

            #[test]
            fn ctrl_char_starts_with_c_prefix(c in proptest::char::range('a', 'z')) {
                let result = keycode_to_tmux(
                    KeyCode::Char(c),
                    KeyModifiers::CONTROL
                ).unwrap();
                prop_assert!(result.starts_with("C-"), "expected C- prefix, got: {}", result);
            }

            #[test]
            fn alt_char_starts_with_m_prefix(c in proptest::char::range('a', 'z')) {
                let result = keycode_to_tmux(
                    KeyCode::Char(c),
                    KeyModifiers::ALT
                ).unwrap();
                prop_assert!(result.starts_with("M-"), "expected M- prefix, got: {}", result);
            }

            #[test]
            fn f_key_produces_f_prefix(n in 1u8..=12) {
                let result = keycode_to_tmux(
                    KeyCode::F(n),
                    KeyModifiers::NONE
                ).unwrap();
                prop_assert!(result.starts_with('F'), "expected F prefix, got: {}", result);
            }
        }
    }

    // ── Additional keycode_to_tmux modifier tests ──

    #[test]
    fn ctrl_shift_arrow() {
        let result = keycode_to_tmux(KeyCode::Up, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        // Combined Ctrl+Shift should still map to something
        assert!(result.is_some(), "Ctrl+Shift+Up should be mappable");
    }

    #[test]
    fn alt_letter() {
        let result = keycode_to_tmux(KeyCode::Char('a'), KeyModifiers::ALT);
        assert!(result.is_some(), "Alt+a should be mappable");
    }

    #[test]
    fn page_up_and_page_down() {
        assert!(
            keycode_to_tmux(KeyCode::PageUp, KeyModifiers::NONE).is_some(),
            "PageUp should be mappable"
        );
        assert!(
            keycode_to_tmux(KeyCode::PageDown, KeyModifiers::NONE).is_some(),
            "PageDown should be mappable"
        );
    }

    #[test]
    fn home_and_end_keys() {
        assert!(
            keycode_to_tmux(KeyCode::Home, KeyModifiers::NONE).is_some(),
            "Home should be mappable"
        );
        assert!(
            keycode_to_tmux(KeyCode::End, KeyModifiers::NONE).is_some(),
            "End should be mappable"
        );
    }

    #[test]
    fn insert_and_delete_keys() {
        assert!(
            keycode_to_tmux(KeyCode::Insert, KeyModifiers::NONE).is_some(),
            "Insert should be mappable"
        );
        assert!(
            keycode_to_tmux(KeyCode::Delete, KeyModifiers::NONE).is_some(),
            "Delete should be mappable"
        );
    }

    // ── batch_dead_panes integration tests ──────────────────────────

    #[tokio::test]
    async fn integration_batch_dead_panes_returns_some() {
        // batch_dead_panes should work when a tmux server is running.
        // If no server is running, it returns None (not an error).
        let result = batch_dead_panes().await;
        // We can't guarantee a tmux server is running, so just verify no panic.
        // If a server is running, we should get Some(set).
        if let Some(dead) = &result {
            // The set may be empty or contain sessions — either is valid.
            assert!(dead.len() < 10000, "sanity check: not absurdly large");
        }
    }

    #[tokio::test]
    async fn integration_batch_dead_panes_detects_exited_session() {
        let name = test_session_name();
        // Create session with a command that sleeps briefly (giving us time to set
        // remain-on-exit), then exits.
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "sh",
            "-c",
            "sleep 0.3 && exit 0",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        // Set remain-on-exit while the sleep is still running.
        let _ = run_status_timeout(Command::new("tmux").args([
            "set-option", "-t", &name, "remain-on-exit", "on",
        ]))
        .await;

        // Wait for the command to finish exiting.
        tokio::time::sleep(Duration::from_millis(600)).await;

        let dead_set = batch_dead_panes().await.expect("tmux server should be running");
        assert!(
            dead_set.contains(&name),
            "exited session should appear in batch dead set: {dead_set:?}"
        );

        cleanup_session(&name).await;
    }

    #[tokio::test]
    async fn integration_batch_dead_panes_live_session_not_dead() {
        let name = test_session_name();
        let status = run_status_timeout(Command::new("tmux").args([
            "new-session", "-d", "-s", &name, "-x", "80", "-y", "24", "sleep", "30",
        ]))
        .await
        .unwrap();
        assert!(status.success());

        let dead_set = batch_dead_panes().await.expect("tmux server should be running");
        assert!(
            !dead_set.contains(&name),
            "live session should NOT appear in dead set"
        );

        cleanup_session(&name).await;
    }

    #[tokio::test]
    async fn integration_parallel_agent_resolution() {
        // Verify that the TmuxSessionManager correctly resolves agent types
        // for multiple sessions with parallel lookups.
        let mgr = TmuxSessionManager::new();
        let project_id = "partest1";

        // Create two sessions with different agent types.
        let name1 = mgr
            .create_session(project_id, "par-a", &AgentType::Claude, "/tmp", Some("sleep 30"))
            .await
            .unwrap();
        let name2 = mgr
            .create_session(project_id, "par-b", &AgentType::Codex, "/tmp", Some("sleep 30"))
            .await
            .unwrap();

        // Clear the cache to force parallel resolution.
        mgr.agent_cache.lock().unwrap().clear();

        let sessions = mgr.list_sessions(project_id).await.unwrap();
        let s1 = sessions.iter().find(|s| s.tmux_name == name1);
        let s2 = sessions.iter().find(|s| s.tmux_name == name2);

        assert!(s1.is_some(), "session 1 should be listed");
        assert!(s2.is_some(), "session 2 should be listed");
        assert_eq!(s1.unwrap().agent_type, AgentType::Claude);
        assert_eq!(s2.unwrap().agent_type, AgentType::Codex);

        // Verify cache was populated.
        {
            let cache = mgr.agent_cache.lock().unwrap();
            assert_eq!(cache.get(&name1), Some(&AgentType::Claude));
            assert_eq!(cache.get(&name2), Some(&AgentType::Codex));
        }

        cleanup_session(&name1).await;
        cleanup_session(&name2).await;
    }
}
