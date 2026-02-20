use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::process::Command;

use crate::session::{parse_session_name, AgentType, Session, SessionStatus};

#[async_trait::async_trait]
pub trait SessionManager: Send + Sync {
    async fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>>;
    async fn create_session(
        &self,
        project_id: &str,
        name: &str,
        agent: &AgentType,
        cwd: &str,
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
        let output = Command::new("tmux")
            .args(["list-sessions", "-F", "#{session_name}"])
            .output()
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
    ) -> Result<String> {
        let tmux_name = create_session(project_id, name, agent, cwd).await?;
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
async fn is_pane_dead(tmux_name: &str) -> bool {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", tmux_name, "-F", "#{pane_dead}"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim() == "1",
        _ => false,
    }
}

/// Read the HYDRA_AGENT_TYPE env var from the tmux session.
async fn get_agent_type(tmux_name: &str) -> Option<AgentType> {
    let output = Command::new("tmux")
        .args(["show-environment", "-t", tmux_name, "HYDRA_AGENT_TYPE"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format: HYDRA_AGENT_TYPE=claude
    let val = stdout.trim().strip_prefix("HYDRA_AGENT_TYPE=")?;
    val.parse().ok()
}

/// Create a new detached tmux session running the given agent command.
pub async fn create_session(
    project_id: &str,
    name: &str,
    agent: &AgentType,
    cwd: &str,
) -> Result<String> {
    let tmux_name = crate::session::tmux_session_name(project_id, name);

    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-c",
            cwd,
            agent.command(),
        ])
        .status()
        .await
        .context("Failed to create tmux session")?;

    if !status.success() {
        bail!("tmux new-session failed for '{tmux_name}'");
    }

    // Keep pane alive after command exits so we can detect Exited status
    let _ = Command::new("tmux")
        .args(["set-option", "-t", &tmux_name, "remain-on-exit", "on"])
        .status()
        .await;

    // Store agent type as env var on the session
    let _ = Command::new("tmux")
        .args([
            "set-environment",
            "-t",
            &tmux_name,
            "HYDRA_AGENT_TYPE",
            &agent.to_string().to_lowercase(),
        ])
        .status()
        .await;

    Ok(tmux_name)
}

/// Capture the current pane content of a tmux session.
pub async fn capture_pane(tmux_name: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", tmux_name, "-p"])
        .output()
        .await
        .context("Failed to capture tmux pane")?;

    if !output.status.success() {
        return Ok(String::from("[session not available]"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Capture the full scrollback buffer of a tmux session.
pub async fn capture_pane_scrollback(tmux_name: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", tmux_name, "-p", "-S", "-"])
        .output()
        .await
        .context("Failed to capture tmux pane scrollback")?;

    if !output.status.success() {
        return Ok(String::from("[session not available]"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Send a key to a tmux session via `tmux send-keys`.
pub async fn send_keys(tmux_name: &str, key: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["send-keys", "-t", tmux_name, key])
        .status()
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
    let status = Command::new("tmux")
        .args(["send-keys", "-t", tmux_name, "-l", &seq])
        .status()
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
    let status = Command::new("tmux")
        .args(["kill-session", "-t", tmux_name])
        .status()
        .await
        .context("Failed to kill tmux session")?;

    if !status.success() {
        bail!("tmux kill-session failed for '{tmux_name}'");
    }

    Ok(())
}
