use anyhow::{bail, Context, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use crate::session::{parse_session_name, AgentType, Session, SessionStatus};
use crate::tmux::SessionManager;

/// Timeout for control mode command responses.
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

// ── Protocol types ──────────────────────────────────────────────────

/// A parsed line from tmux control mode stdout.
#[derive(Debug, PartialEq)]
pub enum ControlLine {
    Begin,
    End,
    Error,
    Notification(String),
    Data(String),
}

/// The result of a command sent through control mode.
#[derive(Debug)]
pub struct CommandResponse {
    pub success: bool,
    pub output: String,
}

// ── Protocol parsing ────────────────────────────────────────────────

/// Parse a single line from tmux control mode output.
///
/// tmux control mode outputs:
/// - `%begin <timestamp> <cmd_number> <flags>` — start of command response
/// - `%end <timestamp> <cmd_number> <flags>` — successful end
/// - `%error <timestamp> <cmd_number> <flags>` — error end
/// - `%<notification> ...` — async notifications (%output, %session-changed, etc.)
/// - anything else — data lines within a %begin/%end block
pub fn parse_control_line(line: &str) -> ControlLine {
    if line.starts_with("%begin ") {
        return ControlLine::Begin;
    }
    if line.starts_with("%end ") {
        return ControlLine::End;
    }
    if line.starts_with("%error ") {
        return ControlLine::Error;
    }
    // Other % lines are notifications
    if line.starts_with('%') {
        return ControlLine::Notification(line.to_string());
    }
    // Everything else is data within a begin/end block
    ControlLine::Data(line.to_string())
}

/// Decode tmux control mode octal escape sequences.
/// `\012` → newline, `\134` → backslash, etc.
pub fn decode_octal_escapes(input: &str) -> String {
    let src = input.as_bytes();
    let len = src.len();
    let mut buf = Vec::with_capacity(len);
    let mut i = 0;

    while i < len {
        if src[i] == b'\\' && i + 3 < len {
            let d1 = src[i + 1];
            let d2 = src[i + 2];
            let d3 = src[i + 3];
            if (b'0'..=b'7').contains(&d1)
                && (b'0'..=b'7').contains(&d2)
                && (b'0'..=b'7').contains(&d3)
            {
                // Decode octal into a raw byte. tmux encodes each byte
                // individually, so multi-byte UTF-8 codepoints appear as
                // multiple consecutive octal escapes (e.g. \303\273 for ű).
                let val = (d1 - b'0') as u16 * 64 + (d2 - b'0') as u16 * 8 + (d3 - b'0') as u16;
                if let Ok(byte) = u8::try_from(val) {
                    buf.push(byte);
                    i += 4;
                    continue;
                }
                // > 255 (e.g. \777) — not a valid byte, fall through
            }
        }
        buf.push(src[i]);
        i += 1;
    }

    String::from_utf8_lossy(&buf).into_owned()
}

/// Quote a string for use as a tmux control mode argument.
/// Wraps in double quotes and escapes `\` and `"` inside.
fn quote_tmux_arg(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ── Connection manager ──────────────────────────────────────────────

/// Pending command awaiting a response from the reader task.
struct PendingCommand {
    sender: oneshot::Sender<CommandResponse>,
    output: String,
}

/// A persistent connection to tmux via control mode (`tmux -C`).
///
/// Command-response correlation uses FIFO ordering: commands are written
/// to stdin sequentially (protected by a tokio Mutex), and tmux sends
/// responses in the same order. The reader task pops pending entries
/// from the front of a VecDeque as `%begin` lines arrive.
pub struct TmuxControlConnection {
    /// Stdin handle for sending commands. Also serializes deque pushes
    /// so write order matches deque order.
    stdin: tokio::sync::Mutex<SenderState>,
    /// Whether the reader task is still alive.
    connected: Arc<AtomicBool>,
    /// The control session name (for cleanup).
    ctrl_session_name: String,
    /// Handle to the child process.
    _child: Child,
    /// Handle to the reader task.
    _reader_handle: tokio::task::JoinHandle<()>,
}

/// Bundled together under one lock so writes to stdin and pushes to the
/// pending deque are always in the same order.
struct SenderState {
    stdin: tokio::process::ChildStdin,
    pending: Arc<Mutex<VecDeque<PendingCommand>>>,
}

impl TmuxControlConnection {
    /// Spawn a tmux control mode session and start the reader task.
    pub async fn connect() -> Result<Self> {
        let pid = std::process::id();
        let ctrl_session_name = format!("_hydra_ctrl_{pid}");

        let mut child = Command::new("tmux")
            .args(["-C", "new-session", "-s", &ctrl_session_name])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn tmux control mode")?;

        let stdin = child.stdin.take().context("Failed to get tmux stdin")?;
        let stdout = child.stdout.take().context("Failed to get tmux stdout")?;

        let connected = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<VecDeque<PendingCommand>>> = Arc::new(Mutex::new(VecDeque::new()));

        let reader_connected = connected.clone();
        let reader_pending = pending.clone();

        let reader_handle = tokio::spawn(async move {
            Self::reader_loop(stdout, reader_pending, reader_connected).await;
        });

        // Give tmux a moment to initialize and emit startup notifications
        tokio::time::sleep(Duration::from_millis(100)).await;

        let conn = Self {
            stdin: tokio::sync::Mutex::new(SenderState { stdin, pending }),
            connected,
            ctrl_session_name,
            _child: child,
            _reader_handle: reader_handle,
        };

        // Quick health check — verifies the pipe is working end-to-end
        match tokio::time::timeout(
            Duration::from_secs(2),
            conn.send_command("display-message -p ok"),
        )
        .await
        {
            Ok(Ok(resp)) if resp.success => Ok(conn),
            Ok(Ok(resp)) => bail!("Control mode health check failed: {}", resp.output),
            Ok(Err(e)) => bail!("Control mode health check error: {e}"),
            Err(_) => bail!("Control mode health check timed out"),
        }
    }

    /// Background reader task that processes tmux control mode output.
    ///
    /// Uses FIFO ordering for command-response correlation: when `%begin`
    /// arrives, pops the next pending entry from the deque. Initial
    /// `%begin/%end` from the `new-session` command (before any entries
    /// are queued) is silently discarded.
    async fn reader_loop(
        stdout: tokio::process::ChildStdout,
        pending: Arc<Mutex<VecDeque<PendingCommand>>>,
        connected: Arc<AtomicBool>,
    ) {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        // The currently active command being accumulated (popped from deque on %begin)
        let mut active: Option<PendingCommand> = None;

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                    let parsed = parse_control_line(trimmed);

                    match parsed {
                        ControlLine::Begin => {
                            // Pop next pending command. If none (e.g. initial
                            // new-session response), active stays None and
                            // subsequent data/end lines are discarded.
                            active = pending.lock().unwrap().pop_front();
                        }
                        ControlLine::Data(data) => {
                            if let Some(ref mut cmd) = active {
                                if !cmd.output.is_empty() {
                                    cmd.output.push('\n');
                                }
                                cmd.output.push_str(&data);
                            }
                        }
                        ControlLine::End => {
                            if let Some(cmd) = active.take() {
                                let _ = cmd.sender.send(CommandResponse {
                                    success: true,
                                    output: cmd.output,
                                });
                            }
                        }
                        ControlLine::Error => {
                            if let Some(cmd) = active.take() {
                                let _ = cmd.sender.send(CommandResponse {
                                    success: false,
                                    output: cmd.output,
                                });
                            }
                        }
                        ControlLine::Notification(_) => {
                            // Notifications are ignored in Phase 1
                        }
                    }
                }
                Err(_) => break,
            }
        }

        // Mark as disconnected and fail all pending commands
        connected.store(false, Ordering::SeqCst);
        let mut deque = pending.lock().unwrap();
        for cmd in deque.drain(..) {
            let _ = cmd.sender.send(CommandResponse {
                success: false,
                output: "control mode disconnected".to_string(),
            });
        }
        // Also fail the active command if any
        if let Some(cmd) = active.take() {
            let _ = cmd.sender.send(CommandResponse {
                success: false,
                output: "control mode disconnected".to_string(),
            });
        }
    }

    /// Send a command and wait for the response.
    pub async fn send_command(&self, cmd: &str) -> Result<CommandResponse> {
        if !self.connected.load(Ordering::SeqCst) {
            bail!("control mode disconnected");
        }

        let (tx, rx) = oneshot::channel();

        // Hold the stdin lock while pushing to deque AND writing, so the
        // deque order exactly matches the stdin write order.
        {
            let mut state = self.stdin.lock().await;
            state.pending.lock().unwrap().push_back(PendingCommand {
                sender: tx,
                output: String::new(),
            });
            let write_result = state.stdin.write_all(format!("{cmd}\n").as_bytes()).await;
            if let Err(e) = write_result {
                bail!("failed to write command: {e}");
            }
            let _ = state.stdin.flush().await;
        }

        // Wait for response with timeout
        match tokio::time::timeout(CMD_TIMEOUT, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => bail!("command response channel closed"),
            Err(_) => bail!("command timed out after {}s", CMD_TIMEOUT.as_secs()),
        }
    }

    /// Send a command without waiting for the response.
    /// Must be async to maintain FIFO ordering with send_command.
    pub async fn send_command_fire_and_forget(&self, cmd: &str) {
        if !self.connected.load(Ordering::SeqCst) {
            return;
        }

        let (tx, _rx) = oneshot::channel(); // rx dropped → send() will fail silently

        let mut state = self.stdin.lock().await;
        state.pending.lock().unwrap().push_back(PendingCommand {
            sender: tx,
            output: String::new(),
        });
        let _ = state.stdin.write_all(format!("{cmd}\n").as_bytes()).await;
        let _ = state.stdin.flush().await;
    }

    /// Check if the connection is still alive.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Get the control session name for cleanup.
    pub fn ctrl_session_name(&self) -> &str {
        &self.ctrl_session_name
    }

    /// Shut down the control mode connection.
    pub async fn shutdown(&self) {
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &self.ctrl_session_name])
            .output()
            .await;
    }
}

impl Drop for TmuxControlConnection {
    fn drop(&mut self) {
        // Best-effort cleanup — kill the control session
        let name = self.ctrl_session_name.clone();
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &name])
            .output();
    }
}

// ── ControlModeSessionManager ───────────────────────────────────────

/// SessionManager implementation that uses tmux control mode for all operations.
pub struct ControlModeSessionManager {
    conn: Arc<TmuxControlConnection>,
    agent_cache: Mutex<HashMap<String, AgentType>>,
}

impl ControlModeSessionManager {
    pub fn new(conn: TmuxControlConnection) -> Self {
        Self {
            conn: Arc::new(conn),
            agent_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn connection(&self) -> &TmuxControlConnection {
        &self.conn
    }

    /// Check if a session's pane is dead via control mode.
    async fn is_pane_dead(&self, tmux_name: &str) -> bool {
        let cmd = format!("list-panes -t {tmux_name} -F '#{{pane_dead}}'");
        match self.conn.send_command(&cmd).await {
            Ok(resp) if resp.success => resp.output.trim() != "0",
            _ => true,
        }
    }

    /// Get the agent type from tmux environment via control mode.
    async fn get_agent_type(&self, tmux_name: &str) -> Option<AgentType> {
        let cmd = format!("show-environment -t {tmux_name} HYDRA_AGENT_TYPE");
        let resp = self.conn.send_command(&cmd).await.ok()?;
        if !resp.success {
            return None;
        }
        let val = resp.output.trim().strip_prefix("HYDRA_AGENT_TYPE=")?;
        val.parse().ok()
    }

    fn prune_agent_cache(&self, live_sessions: &std::collections::HashSet<String>) {
        let mut cache = self.agent_cache.lock().unwrap();
        cache.retain(|tmux_name, _| live_sessions.contains(tmux_name));
    }
}

#[async_trait::async_trait]
impl SessionManager for ControlModeSessionManager {
    async fn list_sessions(&self, project_id: &str) -> Result<Vec<Session>> {
        let resp = self
            .conn
            .send_command("list-sessions -F '#{session_name}'")
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(_) => return Ok(vec![]),
        };

        if !resp.success {
            return Ok(vec![]);
        }

        let prefix = format!("hydra-{project_id}-");
        let ctrl_prefix = "_hydra_ctrl_";

        let live_sessions: std::collections::HashSet<String> = resp
            .output
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        self.prune_agent_cache(&live_sessions);

        // Parse session names, filter out control sessions
        let mut parsed: Vec<(String, String, AgentType)> = Vec::new();
        for line in resp.output.lines() {
            let tmux_name = line.trim();
            if tmux_name.is_empty()
                || tmux_name.starts_with(ctrl_prefix)
                || !tmux_name.starts_with(&prefix)
            {
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
                        let agent = self
                            .get_agent_type(tmux_name)
                            .await
                            .unwrap_or(AgentType::Claude);
                        self.agent_cache
                            .lock()
                            .unwrap()
                            .insert(tmux_name.to_string(), agent.clone());
                        agent
                    }
                }
            };

            parsed.push((name, tmux_name.to_string(), agent_type));
        }

        // Check pane_dead status sequentially (serialized pipe is fast)
        let mut sessions = Vec::with_capacity(parsed.len());
        for (name, tmux_name, agent_type) in parsed {
            let dead = self.is_pane_dead(&tmux_name).await;
            let status = if dead {
                SessionStatus::Exited
            } else {
                SessionStatus::Idle
            };
            sessions.push(Session {
                name,
                tmux_name,
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
        let tmux_name = crate::session::tmux_session_name(project_id, name);
        let cmd = command_override.unwrap_or(agent.command());

        // Create the session
        let resp = self
            .conn
            .send_command(&format!("new-session -d -s {tmux_name} -c {cwd} {cmd}"))
            .await
            .context("Failed to create tmux session")?;

        if !resp.success {
            bail!("tmux new-session failed for '{tmux_name}': {}", resp.output);
        }

        // Set remain-on-exit
        let _ = self
            .conn
            .send_command(&format!("set-option -t {tmux_name} remain-on-exit on"))
            .await;

        // Set agent type env var
        let agent_str = agent.to_string().to_lowercase();
        let _ = self
            .conn
            .send_command(&format!(
                "set-environment -t {tmux_name} HYDRA_AGENT_TYPE {agent_str}"
            ))
            .await;

        // Pre-populate agent cache
        self.agent_cache
            .lock()
            .unwrap()
            .insert(tmux_name.clone(), agent.clone());

        Ok(tmux_name)
    }

    async fn capture_pane(&self, tmux_name: &str) -> Result<String> {
        let resp = self
            .conn
            .send_command(&format!("capture-pane -t {tmux_name} -p -e"))
            .await
            .context("Failed to capture tmux pane")?;

        if !resp.success {
            return Ok(String::from("[session not available]"));
        }

        // Decode octal escapes from control mode output
        let decoded = decode_octal_escapes(&resp.output);
        let trimmed = decoded.trim_end_matches('\n');
        Ok(trimmed.to_string())
    }

    async fn capture_panes(&self, names: &[String]) -> Vec<Result<String>> {
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            results.push(self.capture_pane(name).await);
        }
        results
    }

    async fn capture_pane_scrollback(&self, tmux_name: &str) -> Result<String> {
        let resp = self
            .conn
            .send_command(&format!("capture-pane -t {tmux_name} -p -e -S -5000"))
            .await
            .context("Failed to capture tmux pane scrollback")?;

        if !resp.success {
            return Ok(String::from("[session not available]"));
        }

        let decoded = decode_octal_escapes(&resp.output);
        let trimmed = decoded.trim_end_matches('\n');
        Ok(trimmed.to_string())
    }

    async fn kill_session(&self, tmux_name: &str) -> Result<()> {
        let resp = self
            .conn
            .send_command(&format!("kill-session -t {tmux_name}"))
            .await
            .context("Failed to kill tmux session")?;

        if !resp.success {
            bail!(
                "tmux kill-session failed for '{tmux_name}': {}",
                resp.output
            );
        }

        self.agent_cache.lock().unwrap().remove(tmux_name);
        Ok(())
    }

    async fn send_keys(&self, tmux_name: &str, key: &str) -> Result<()> {
        let quoted = quote_tmux_arg(key);
        self.conn
            .send_command_fire_and_forget(&format!("send-keys -t {tmux_name} {quoted}"))
            .await;
        Ok(())
    }

    async fn send_keys_literal(&self, tmux_name: &str, text: &str) -> Result<()> {
        let quoted = quote_tmux_arg(text);
        self.conn
            .send_command_fire_and_forget(&format!("send-keys -t {tmux_name} -l {quoted}"))
            .await;
        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── decode_octal_escapes ────────────────────────────────────────

    #[test]
    fn decode_octal_newline() {
        assert_eq!(decode_octal_escapes("hello\\012world"), "hello\nworld");
    }

    #[test]
    fn decode_octal_backslash() {
        assert_eq!(decode_octal_escapes("path\\134file"), "path\\file");
    }

    #[test]
    fn decode_octal_tab() {
        assert_eq!(decode_octal_escapes("col1\\011col2"), "col1\tcol2");
    }

    #[test]
    fn decode_octal_multiple() {
        assert_eq!(decode_octal_escapes("a\\012b\\012c"), "a\nb\nc");
    }

    #[test]
    fn decode_octal_no_escapes() {
        assert_eq!(decode_octal_escapes("plain text"), "plain text");
    }

    #[test]
    fn decode_octal_empty() {
        assert_eq!(decode_octal_escapes(""), "");
    }

    #[test]
    fn decode_octal_trailing_backslash() {
        assert_eq!(decode_octal_escapes("end\\"), "end\\");
    }

    #[test]
    fn decode_octal_partial_escape() {
        assert_eq!(decode_octal_escapes("end\\01"), "end\\01");
    }

    #[test]
    fn decode_octal_non_octal_digits() {
        assert_eq!(decode_octal_escapes("x\\089y"), "x\\089y");
    }

    #[test]
    fn decode_octal_null_byte() {
        assert_eq!(decode_octal_escapes("a\\000b"), "a\0b");
    }

    #[test]
    fn decode_octal_multibyte_utf8() {
        // » is U+00BB, UTF-8 bytes: 0xC2 0xBB = octal \302\273
        assert_eq!(decode_octal_escapes("\\302\\273"), "»");
    }

    #[test]
    fn decode_octal_3byte_utf8() {
        // ● is U+25CF, UTF-8 bytes: 0xE2 0x97 0x8F = octal \342\227\217
        assert_eq!(decode_octal_escapes("\\342\\227\\217"), "●");
    }

    #[test]
    fn decode_octal_4byte_utf8_emoji() {
        // 🔒 is U+1F512, UTF-8 bytes: 0xF0 0x9F 0x94 0x92 = octal \360\237\224\222
        assert_eq!(decode_octal_escapes("\\360\\237\\224\\222"), "\u{1F512}");
    }

    #[test]
    fn decode_octal_mixed_ascii_and_utf8() {
        // "hello » world" with » encoded as octal
        assert_eq!(
            decode_octal_escapes("hello \\302\\273 world"),
            "hello » world"
        );
    }

    // ── quote_tmux_arg ───────────────────────────────────────────────

    #[test]
    fn quote_simple_text() {
        assert_eq!(quote_tmux_arg("hello"), "\"hello\"");
    }

    #[test]
    fn quote_text_with_spaces() {
        assert_eq!(quote_tmux_arg("hello world"), "\"hello world\"");
    }

    #[test]
    fn quote_text_with_backslash() {
        assert_eq!(quote_tmux_arg("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn quote_text_with_double_quote() {
        assert_eq!(quote_tmux_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn quote_empty_string() {
        assert_eq!(quote_tmux_arg(""), "\"\"");
    }

    // ── parse_control_line ──────────────────────────────────────────

    #[test]
    fn parse_begin() {
        assert_eq!(
            parse_control_line("%begin 1234567890 42 1"),
            ControlLine::Begin
        );
    }

    #[test]
    fn parse_end() {
        assert_eq!(parse_control_line("%end 1234567890 42 1"), ControlLine::End);
    }

    #[test]
    fn parse_error() {
        assert_eq!(
            parse_control_line("%error 1234567890 42 1"),
            ControlLine::Error
        );
    }

    #[test]
    fn parse_notification() {
        let line = "%output %5 some output text";
        assert_eq!(
            parse_control_line(line),
            ControlLine::Notification(line.to_string())
        );
    }

    #[test]
    fn parse_session_changed_notification() {
        let line = "%session-changed $1 mysession";
        assert_eq!(
            parse_control_line(line),
            ControlLine::Notification(line.to_string())
        );
    }

    #[test]
    fn parse_data_line() {
        let line = "some output data here";
        assert_eq!(
            parse_control_line(line),
            ControlLine::Data(line.to_string())
        );
    }

    #[test]
    fn parse_empty_data() {
        assert_eq!(parse_control_line(""), ControlLine::Data(String::new()));
    }

    #[test]
    fn parse_begin_various_ids() {
        // Command ID doesn't matter for parsing — we use FIFO ordering
        assert_eq!(
            parse_control_line("%begin 1234567890 999999 1"),
            ControlLine::Begin
        );
        assert_eq!(parse_control_line("%begin 0 0 0"), ControlLine::Begin);
    }

    // ── proptest ────────────────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn decode_octal_never_panics(input in ".*") {
                let _ = decode_octal_escapes(&input);
            }

            #[test]
            fn parse_control_line_never_panics(input in ".*") {
                let _ = parse_control_line(&input);
            }

            #[test]
            fn decode_octal_preserves_ascii_without_backslash(
                input in "[a-zA-Z0-9 ]{0,100}"
            ) {
                let result = decode_octal_escapes(&input);
                prop_assert_eq!(result, input);
            }
        }
    }
}
