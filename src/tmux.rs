use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

use crate::session::{parse_session_name, AgentType, Session, SessionStatus};

/// Default timeout for subprocess calls (5 seconds).
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

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
    async fn send_mouse(
        &self,
        tmux_name: &str,
        kind: &str,
        button: u8,
        x: u16,
        y: u16,
    ) -> Result<()>;
    async fn capture_pane_scrollback(&self, tmux_name: &str) -> Result<String>;
}

pub struct TmuxSessionManager {
    agent_cache: Mutex<HashMap<String, AgentType>>,
}

impl TmuxSessionManager {
    pub fn new() -> Self {
        Self {
            agent_cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl SessionManager for TmuxSessionManager {
    async fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>> {
        let output = run_cmd_timeout(
            Command::new("tmux").args(["list-sessions", "-F", "#{session_name}"]),
        )
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
        let mut sessions = Vec::new();

        for line in stdout.lines() {
            let tmux_name = line.trim();
            if !tmux_name.starts_with(&prefix) {
                continue;
            }
            let name = match parse_session_name(tmux_name, project_id) {
                Some(n) => n,
                None => continue,
            };

            let agent_type = {
                let cached = self.agent_cache.lock().unwrap().get(tmux_name).cloned();
                match cached {
                    Some(a) => a,
                    None => {
                        let agent =
                            get_agent_type(tmux_name).await.unwrap_or(AgentType::Claude);
                        self.agent_cache
                            .lock()
                            .unwrap()
                            .insert(tmux_name.to_string(), agent.clone());
                        agent
                    }
                }
            };

            let status = if is_pane_dead(tmux_name).await {
                SessionStatus::Exited
            } else {
                // Default to Idle; App will upgrade to Running via content comparison
                SessionStatus::Idle
            };

            sessions.push(Session {
                name,
                tmux_name: tmux_name.to_string(),
                agent_type,
                status,
                task_elapsed: None,
                _alive: true,
            });
        }

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
            .unwrap()
            .insert(tmux_name.clone(), agent.clone());
        Ok(tmux_name)
    }

    async fn capture_pane(&self, tmux_name: &str) -> Result<String> {
        capture_pane(tmux_name).await
    }

    async fn kill_session(&self, tmux_name: &str) -> Result<()> {
        kill_session(tmux_name).await
    }

    async fn send_keys(&self, tmux_name: &str, key: &str) -> Result<()> {
        send_keys(tmux_name, key).await
    }

    async fn send_mouse(
        &self,
        tmux_name: &str,
        kind: &str,
        button: u8,
        x: u16,
        y: u16,
    ) -> Result<()> {
        send_mouse(tmux_name, kind, button, x, y).await
    }

    async fn capture_pane_scrollback(&self, tmux_name: &str) -> Result<String> {
        capture_pane_scrollback(tmux_name).await
    }
}

/// Check if the pane in a tmux session has exited (requires remain-on-exit).
/// Returns `true` when the session can't be queried (gone/dead) — a session
/// we can't reach is effectively dead rather than silently "Idle".
async fn is_pane_dead(tmux_name: &str) -> bool {
    let output = run_cmd_timeout(
        Command::new("tmux").args(["list-panes", "-t", tmux_name, "-F", "#{pane_dead}"]),
    )
    .await;

    match output {
        Ok(o) if o.status.success() => {
            // Only treat as alive when we get a definitive "not dead" answer
            String::from_utf8_lossy(&o.stdout).trim() != "0"
        }
        _ => true, // Can't reach session → treat as dead
    }
}

/// Read the HYDRA_AGENT_TYPE env var from the tmux session.
async fn get_agent_type(tmux_name: &str) -> Option<AgentType> {
    let output = run_cmd_timeout(
        Command::new("tmux").args(["show-environment", "-t", tmux_name, "HYDRA_AGENT_TYPE"]),
    )
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

    let status = run_status_timeout(Command::new("tmux").args([
        "new-session",
        "-d",
        "-s",
        &tmux_name,
        "-c",
        cwd,
        cmd,
    ]))
    .await
    .context("Failed to create tmux session")?;

    if !status.success() {
        bail!("tmux new-session failed for '{tmux_name}'");
    }

    // Keep pane alive after command exits so we can detect Exited status
    let _ = run_status_timeout(
        Command::new("tmux").args(["set-option", "-t", &tmux_name, "remain-on-exit", "on"]),
    )
    .await;

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
    let output = run_cmd_timeout(
        Command::new("tmux").args(["capture-pane", "-t", tmux_name, "-p"]),
    )
    .await
    .context("Failed to capture tmux pane")?;

    if !output.status.success() {
        return Ok(String::from("[session not available]"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Capture the full scrollback buffer of a tmux session.
pub async fn capture_pane_scrollback(tmux_name: &str) -> Result<String> {
    let output = run_cmd_timeout(
        Command::new("tmux").args(["capture-pane", "-t", tmux_name, "-p", "-S", "-"]),
    )
    .await
    .context("Failed to capture tmux pane scrollback")?;

    if !output.status.success() {
        return Ok(String::from("[session not available]"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Send a key to a tmux session via `tmux send-keys`.
pub async fn send_keys(tmux_name: &str, key: &str) -> Result<()> {
    let status = run_status_timeout(
        Command::new("tmux").args(["send-keys", "-t", tmux_name, key]),
    )
    .await
    .context("Failed to send keys to tmux session")?;

    if !status.success() {
        bail!("tmux send-keys failed for '{tmux_name}'");
    }

    Ok(())
}

/// Send a mouse event to a tmux session using SGR escape sequences.
///
/// `kind` is "press" or "release". Coordinates are 1-based for SGR encoding.
pub async fn send_mouse(
    tmux_name: &str,
    kind: &str,
    button: u8,
    x: u16,
    y: u16,
) -> Result<()> {
    // SGR mouse encoding: press = \x1b[<button;x;yM  release = \x1b[<button;x;ym
    let suffix = if kind == "press" { 'M' } else { 'm' };
    let seq = format!("\x1b[<{button};{x};{y}{suffix}");
    let status = run_status_timeout(
        Command::new("tmux").args(["send-keys", "-t", tmux_name, "-l", &seq]),
    )
    .await
    .context("Failed to send mouse event to tmux session")?;

    if !status.success() {
        bail!("tmux send-keys (mouse) failed for '{tmux_name}'");
    }

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
fn apply_tmux_modifiers(base: &str, modifiers: crossterm::event::KeyModifiers) -> String {
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
    let status = run_status_timeout(
        Command::new("tmux").args(["kill-session", "-t", tmux_name]),
    )
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
        assert_eq!(keycode_to_tmux(KeyCode::Char('a'), KeyModifiers::NONE), Some("a".into()));
    }

    #[test]
    fn char_key_uppercase() {
        assert_eq!(keycode_to_tmux(KeyCode::Char('A'), KeyModifiers::SHIFT), Some("A".into()));
    }

    #[test]
    fn char_key_ctrl() {
        assert_eq!(keycode_to_tmux(KeyCode::Char('c'), KeyModifiers::CONTROL), Some("C-c".into()));
    }

    #[test]
    fn char_key_alt() {
        assert_eq!(keycode_to_tmux(KeyCode::Char('x'), KeyModifiers::ALT), Some("M-x".into()));
    }

    // ── keycode_to_tmux: special keys ────────────────────────────────

    #[test]
    fn enter_key() {
        assert_eq!(keycode_to_tmux(KeyCode::Enter, KeyModifiers::NONE), Some("Enter".into()));
    }

    #[test]
    fn backspace_key() {
        assert_eq!(keycode_to_tmux(KeyCode::Backspace, KeyModifiers::NONE), Some("BSpace".into()));
    }

    #[test]
    fn tab_key() {
        assert_eq!(keycode_to_tmux(KeyCode::Tab, KeyModifiers::NONE), Some("Tab".into()));
    }

    #[test]
    fn backtab_key() {
        assert_eq!(keycode_to_tmux(KeyCode::BackTab, KeyModifiers::NONE), Some("BTab".into()));
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(keycode_to_tmux(KeyCode::Up, KeyModifiers::NONE), Some("Up".into()));
        assert_eq!(keycode_to_tmux(KeyCode::Down, KeyModifiers::NONE), Some("Down".into()));
        assert_eq!(keycode_to_tmux(KeyCode::Left, KeyModifiers::NONE), Some("Left".into()));
        assert_eq!(keycode_to_tmux(KeyCode::Right, KeyModifiers::NONE), Some("Right".into()));
    }

    #[test]
    fn home_end_keys() {
        assert_eq!(keycode_to_tmux(KeyCode::Home, KeyModifiers::NONE), Some("Home".into()));
        assert_eq!(keycode_to_tmux(KeyCode::End, KeyModifiers::NONE), Some("End".into()));
    }

    #[test]
    fn page_up_down_keys() {
        assert_eq!(keycode_to_tmux(KeyCode::PageUp, KeyModifiers::NONE), Some("PageUp".into()));
        assert_eq!(keycode_to_tmux(KeyCode::PageDown, KeyModifiers::NONE), Some("PageDown".into()));
    }

    #[test]
    fn delete_insert_keys() {
        assert_eq!(keycode_to_tmux(KeyCode::Delete, KeyModifiers::NONE), Some("DC".into()));
        assert_eq!(keycode_to_tmux(KeyCode::Insert, KeyModifiers::NONE), Some("IC".into()));
    }

    #[test]
    fn function_keys() {
        assert_eq!(keycode_to_tmux(KeyCode::F(1), KeyModifiers::NONE), Some("F1".into()));
        assert_eq!(keycode_to_tmux(KeyCode::F(12), KeyModifiers::NONE), Some("F12".into()));
    }

    #[test]
    fn esc_returns_none() {
        assert_eq!(keycode_to_tmux(KeyCode::Esc, KeyModifiers::NONE), None);
    }

    // ── keycode_to_tmux: modifiers on special keys ───────────────────

    #[test]
    fn ctrl_arrow() {
        assert_eq!(keycode_to_tmux(KeyCode::Up, KeyModifiers::CONTROL), Some("C-Up".into()));
    }

    #[test]
    fn alt_arrow() {
        assert_eq!(keycode_to_tmux(KeyCode::Left, KeyModifiers::ALT), Some("M-Left".into()));
    }

    #[test]
    fn shift_arrow() {
        assert_eq!(keycode_to_tmux(KeyCode::Right, KeyModifiers::SHIFT), Some("S-Right".into()));
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
        assert_eq!(apply_tmux_modifiers("Left", KeyModifiers::CONTROL), "C-Left");
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
}
