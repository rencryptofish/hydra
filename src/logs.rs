use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result as AnyhowResult};
use tokio::process::Command;

/// Default timeout for subprocess calls in log resolution (5 seconds).
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum depth for process tree walks (tmux shell → agent → subprocesses).
const MAX_TREE_DEPTH: usize = 5;

/// Maximum total PIDs collected during a process tree walk.
const MAX_TREE_PIDS: usize = 100;

/// Run a Command with a timeout, returning its Output.
async fn run_cmd_timeout(cmd: &mut Command) -> AnyhowResult<std::process::Output> {
    match tokio::time::timeout(CMD_TIMEOUT, cmd.output()).await {
        Ok(result) => result.context("subprocess failed to execute"),
        Err(_) => bail!("subprocess timed out after {}s", CMD_TIMEOUT.as_secs()),
    }
}

/// Per-session stats aggregated from Claude Code JSONL logs.
/// Updated incrementally — only new bytes are parsed on each refresh.
#[derive(Debug, Default)]
pub struct SessionStats {
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub edits: u16,
    pub bash_cmds: u16,
    pub files: HashSet<String>,
    /// Files in order of most recent edit (last = most recent).
    /// Deduplicated: each path appears at most once.
    pub recent_files: Vec<String>,
    /// ISO 8601 timestamp of the most recent user message (task start).
    pub last_user_ts: Option<String>,
    /// ISO 8601 timestamp of the most recent assistant message (task end).
    pub last_assistant_ts: Option<String>,
    pub read_offset: u64,
}

impl SessionStats {
    /// Estimated cost in USD using Sonnet pricing as default.
    /// Input: $3/MTok, Output: $15/MTok, Cache read: $0.30/MTok, Cache write: $3.75/MTok
    pub fn cost_usd(&self) -> f64 {
        let input = self.tokens_in as f64 * 3.0 / 1_000_000.0;
        let output = self.tokens_out as f64 * 15.0 / 1_000_000.0;
        let cache_read = self.tokens_cache_read as f64 * 0.30 / 1_000_000.0;
        let cache_write = self.tokens_cache_write as f64 * 3.75 / 1_000_000.0;
        input + output + cache_read + cache_write
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Compute task elapsed duration from log timestamps.
    /// Returns Some if the agent appears to be working (last user msg > last assistant msg,
    /// or no assistant response yet). Returns None if idle or no data.
    pub fn task_elapsed(&self) -> Option<std::time::Duration> {
        let user_ts = parse_iso_timestamp(self.last_user_ts.as_deref()?)?;
        let now = chrono::Utc::now();

        match &self.last_assistant_ts {
            Some(ast_str) => {
                let ast_ts = parse_iso_timestamp(ast_str)?;
                if user_ts > ast_ts {
                    // User sent a message after the last assistant reply — agent is working
                    Some((now - user_ts).to_std().unwrap_or_default())
                } else {
                    // Assistant replied after user — task complete, no elapsed
                    None
                }
            }
            None => {
                // No assistant response yet — agent is working on first message
                Some((now - user_ts).to_std().unwrap_or_default())
            }
        }
    }

    /// Record a file touch, updating both the dedup set and recency order.
    pub fn touch_file(&mut self, path: String) {
        self.files.insert(path.clone());
        // Move to end (most recent) by removing old position
        if let Some(pos) = self.recent_files.iter().position(|f| f == &path) {
            self.recent_files.remove(pos);
        }
        self.recent_files.push(path);
    }
}

/// Parse an ISO 8601 timestamp string into a chrono DateTime.
fn parse_iso_timestamp(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    s.parse::<chrono::DateTime<chrono::Utc>>().ok()
}

/// Format a token count compactly: 1234 → "1.2k", 1234567 → "1.2M"
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Format cost in USD compactly.
pub fn format_cost(usd: f64) -> String {
    if usd < 0.005 {
        "$0.00".to_string()
    } else if usd < 10.0 {
        format!("${:.2}", usd)
    } else {
        format!("${:.0}", usd)
    }
}

/// Incrementally update stats from a Claude JSONL log file.
/// Only reads bytes after `stats.read_offset`, making repeated calls cheap.
pub fn update_session_stats(cwd: &str, uuid: &str, stats: &mut SessionStats) {
    let escaped = escape_project_path(cwd);
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let path = PathBuf::from(&home)
        .join(".claude")
        .join("projects")
        .join(&escaped)
        .join(format!("{uuid}.jsonl"));

    update_session_stats_from_path(&path, stats);
}

/// Core stats parser — reads from a specific file path.
/// Separated from `update_session_stats` for testability (avoids HOME env var).
fn update_session_stats_from_path(path: &std::path::Path, stats: &mut SessionStats) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    // Nothing new to read
    if file_len <= stats.read_offset {
        return;
    }

    // Seek to where we left off
    if stats.read_offset > 0 {
        if file.seek(SeekFrom::Start(stats.read_offset)).is_err() {
            return;
        }
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return;
    }
    let text = String::from_utf8_lossy(&buf);

    for line in text.lines() {
        // Skip empty lines
        if line.len() < 10 {
            continue;
        }

        // Fast path: user messages — track timestamp for task-start timing
        if line.contains("\"user\"") && line.contains("\"timestamp\"") && !line.contains("\"usage\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                    if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                        stats.last_user_ts = Some(ts.to_string());
                    }
                }
            }
            continue;
        }

        // Fast path: assistant messages with usage (token counts + tool calls)
        if line.contains("\"assistant\"") && line.contains("\"usage\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                    // Track timestamp for task-end timing
                    if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                        stats.last_assistant_ts = Some(ts.to_string());
                    }
                    // Extract token usage
                    if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                        stats.turns += 1;
                        stats.tokens_in += usage
                            .get("input_tokens")
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0);
                        stats.tokens_out += usage
                            .get("output_tokens")
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0);
                        stats.tokens_cache_read += usage
                            .get("cache_read_input_tokens")
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0);
                        stats.tokens_cache_write += usage
                            .get("cache_creation_input_tokens")
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0);
                    }

                    // Count tool calls from content array
                    if let Some(content) = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                    {
                        for item in content {
                            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                    match name {
                                        "Write" | "Edit" => stats.edits += 1,
                                        "Bash" => stats.bash_cmds += 1,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Fast path: tool results with filenames
        if line.contains("\"filenames\"") && line.contains("\"toolUseResult\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(filenames) = v
                    .get("toolUseResult")
                    .and_then(|r| r.get("filenames"))
                    .and_then(|f| f.as_array())
                {
                    for fname in filenames {
                        if let Some(s) = fname.as_str() {
                            stats.touch_file(s.to_string());
                        }
                    }
                }
            }
        }
    }

    stats.read_offset = file_len;
}

/// Get the pane PID for a tmux session.
pub async fn get_pane_pid(tmux_name: &str) -> Option<u32> {
    let output = run_cmd_timeout(
        Command::new("tmux").args(["list-panes", "-t", tmux_name, "-F", "#{pane_pid}"]),
    )
    .await
    .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()
}

/// Extract --session-id UUID from a command line string.
/// Handles both `--session-id <uuid>` and `--session-id=<uuid>` forms.
fn parse_session_id_from_cmdline(cmdline: &str) -> Option<String> {
    let mut args = cmdline.split_whitespace();
    while let Some(arg) = args.next() {
        if arg == "--session-id" {
            if let Some(value) = args.next() {
                if is_uuid(value) {
                    return Some(value.to_string());
                }
            }
        }
        // Also handle --session-id=<uuid> form
        if let Some(value) = arg.strip_prefix("--session-id=") {
            if is_uuid(value) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Extract --session-id from a process's command line arguments.
/// This is the most reliable way to get the Claude session UUID.
async fn resolve_uuid_from_cmdline(pid: u32) -> Option<String> {
    let output = run_cmd_timeout(
        Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]),
    )
    .await
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let cmdline = String::from_utf8_lossy(&output.stdout);
    parse_session_id_from_cmdline(&cmdline)
}

/// Collect all descendant PIDs of a process (children, grandchildren, etc.).
/// Bounded by `MAX_TREE_DEPTH` levels and `MAX_TREE_PIDS` total to prevent
/// runaway walks on pathological process trees.
async fn collect_descendant_pids(pid: u32) -> Vec<u32> {
    let mut all_pids = vec![pid];
    // Process level-by-level for depth tracking
    let mut current_level = vec![pid];
    let mut depth = 0;

    while !current_level.is_empty() && depth < MAX_TREE_DEPTH && all_pids.len() < MAX_TREE_PIDS {
        let mut next_level = Vec::new();

        for parent in &current_level {
            if all_pids.len() >= MAX_TREE_PIDS {
                break;
            }
            let output = run_cmd_timeout(
                Command::new("pgrep").args(["-P", &parent.to_string()]),
            )
            .await;

            if let Ok(output) = output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    if all_pids.len() >= MAX_TREE_PIDS {
                        break;
                    }
                    if let Ok(child_pid) = line.trim().parse::<u32>() {
                        all_pids.push(child_pid);
                        next_level.push(child_pid);
                    }
                }
            }
        }

        current_level = next_level;
        depth += 1;
    }

    all_pids
}

/// Parse lsof output to find a `.claude/tasks/<uuid>/` path.
fn parse_uuid_from_lsof_output(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(idx) = line.find(".claude/tasks/") {
            let rest = &line[idx + ".claude/tasks/".len()..];
            if rest.len() >= 36 {
                let candidate = &rest[..36];
                if is_uuid(candidate) {
                    return Some(candidate.to_string());
                }
            }
        }
    }
    None
}

/// Use lsof to find the Claude tasks UUID from a set of PIDs.
/// Fallback method — checks all provided PIDs for open .claude/tasks/ file descriptors.
async fn resolve_uuid_from_lsof_pids(pids: &[u32]) -> Option<String> {
    if pids.is_empty() {
        return None;
    }

    let pid_list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let output = run_cmd_timeout(Command::new("lsof").args(["-p", &pid_list]))
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_uuid_from_lsof_output(&stdout)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// Resolve the Claude session UUID for a tmux session.
/// Tries --session-id from process args first (reliable), then walks the process tree.
pub async fn resolve_session_uuid(tmux_name: &str) -> Option<String> {
    let pid = get_pane_pid(tmux_name).await?;

    // Try command line --session-id on pane PID and all descendants
    let all_pids = collect_descendant_pids(pid).await;
    for &p in &all_pids {
        if let Some(uuid) = resolve_uuid_from_cmdline(p).await {
            return Some(uuid);
        }
    }

    // Fall back to lsof on the full process tree
    resolve_uuid_from_lsof_pids(&all_pids).await
}

/// Convert a CWD path to the Claude projects directory escape format.
/// e.g. "/Users/monkey/hydra" → "-Users-monkey-hydra"
fn escape_project_path(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Read the last assistant message from a Claude JSONL log file.
/// Reads only the tail of the file for efficiency on large logs.
pub fn read_last_assistant_message(cwd: &str, uuid: &str) -> Option<String> {
    let escaped = escape_project_path(cwd);
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(&home)
        .join(".claude")
        .join("projects")
        .join(&escaped)
        .join(format!("{uuid}.jsonl"));

    let mut file = std::fs::File::open(&path).ok()?;
    let file_len = file.metadata().ok()?.len();

    // Read last 200KB — enough to find the most recent assistant message
    let chunk_size: u64 = 200 * 1024;
    let start = file_len.saturating_sub(chunk_size);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    let mut last_text: Option<String> = None;

    for line in text.lines() {
        // Quick filter before JSON parse
        if !line.contains("\"assistant\"") {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // partial line from mid-file seek
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(content) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            let mut parts = Vec::new();
            for item in content {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(t);
                }
            }
            if !parts.is_empty() {
                last_text = Some(parts.join(" "));
            }
        }
    }

    // Condense whitespace for display
    last_text.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock to serialize tests that modify the HOME environment variable.
    /// HOME is process-global, so parallel tests that set_var("HOME", ...) race.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── format_tokens tests ──────────────────────────────────────────

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(1000), "1.0k");
        assert_eq!(format_tokens(1234), "1.2k");
        assert_eq!(format_tokens(45300), "45.3k");
        assert_eq!(format_tokens(999999), "1000.0k");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(1_234_567), "1.2M");
    }

    // ── format_cost tests ────────────────────────────────────────────

    #[test]
    fn format_cost_zero() {
        assert_eq!(format_cost(0.0), "$0.00");
        assert_eq!(format_cost(0.004), "$0.00");
    }

    #[test]
    fn format_cost_normal() {
        assert_eq!(format_cost(0.42), "$0.42");
        assert_eq!(format_cost(1.23), "$1.23");
    }

    #[test]
    fn format_cost_large() {
        assert_eq!(format_cost(12.5), "$12");
    }

    // ── SessionStats cost tests ──────────────────────────────────────

    #[test]
    fn session_stats_cost_empty() {
        let stats = SessionStats::default();
        assert!((stats.cost_usd() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn session_stats_cost_calculation() {
        let stats = SessionStats {
            tokens_in: 1_000_000,  // $3.00
            tokens_out: 100_000,   // $1.50
            tokens_cache_read: 500_000,  // $0.15
            tokens_cache_write: 200_000, // $0.75
            ..Default::default()
        };
        let cost = stats.cost_usd();
        assert!((cost - 5.40).abs() < 0.01, "expected ~$5.40, got ${cost:.2}");
    }

    // ── update_session_stats tests ───────────────────────────────────
    // Tests use update_session_stats_from_path() directly to avoid
    // HOME env var races when tests run in parallel.

    fn write_tmp_jsonl(name: &str, lines: &[&str]) -> PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("hydra_test_{name}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path
    }

    #[test]
    fn update_session_stats_parses_tokens_and_turns() {
        let path = write_tmp_jsonl("stats_tokens", &[
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":100},"content":[{"type":"text","text":"hello"}]}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":2000,"output_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"world"}]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(stats.turns, 2);
        assert_eq!(stats.tokens_in, 3000);
        assert_eq!(stats.tokens_out, 500);
        assert_eq!(stats.tokens_cache_read, 500);
        assert_eq!(stats.tokens_cache_write, 100);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_session_stats_counts_tools() {
        let path = write_tmp_jsonl("stats_tools", &[
            r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","id":"t1","input":{}},{"type":"tool_use","name":"Bash","id":"t2","input":{}},{"type":"tool_use","name":"Write","id":"t3","input":{}}]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(stats.edits, 2, "Edit + Write = 2 edits");
        assert_eq!(stats.bash_cmds, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_session_stats_tracks_files() {
        let path = write_tmp_jsonl("stats_files", &[
            r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs","/src/app.rs"]}}"#,
            r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs"]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(stats.file_count(), 2, "should deduplicate filenames");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_session_stats_incremental() {
        use std::io::Write;

        let path = std::env::temp_dir().join(format!(
            "hydra_test_stats_incr_{:?}.jsonl",
            std::thread::current().id()
        ));
        // Clean up any leftover
        let _ = std::fs::remove_file(&path);

        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"first"}}]}}}}"#).unwrap();
        drop(f);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1);
        let offset_after_first = stats.read_offset;
        assert!(offset_after_first > 0);

        // Append more data
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":200,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"second"}}]}}}}"#).unwrap();
        drop(f);

        // Second call should only parse new data
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 2, "should accumulate from incremental read");
        assert_eq!(stats.tokens_in, 300);
        assert!(stats.read_offset > offset_after_first);

        // Third call with no new data should be a no-op
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 2, "no-op when no new data");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_session_stats_no_file() {
        let mut stats = SessionStats::default();
        update_session_stats_from_path(std::path::Path::new("/nonexistent/file.jsonl"), &mut stats);
        assert_eq!(stats.turns, 0);
    }

    #[test]
    fn stats_skips_short_lines() {
        let path = write_tmp_jsonl("stats_short", &[
            "short",  // < 10 chars, should be skipped
            "",       // empty, should be skipped
            r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1, "should skip short lines");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_unknown_tool_name_ignored() {
        let path = write_tmp_jsonl("stats_unknown_tool", &[
            r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"UnknownTool","id":"t1","input":{}}]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.edits, 0);
        assert_eq!(stats.bash_cmds, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_assistant_without_usage_not_counted() {
        let path = write_tmp_jsonl("stats_no_usage", &[
            // "assistant" in line but no "usage" — won't match fast path
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"no usage field here"}]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 0, "no usage = not counted as turn");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_file_count_deduplicates() {
        let mut stats = SessionStats::default();
        stats.touch_file("/a.rs".to_string());
        stats.touch_file("/a.rs".to_string());
        stats.touch_file("/b.rs".to_string());
        assert_eq!(stats.file_count(), 2);
    }

    #[test]
    fn touch_file_maintains_recency_order() {
        let mut stats = SessionStats::default();
        stats.touch_file("/a.rs".to_string());
        stats.touch_file("/b.rs".to_string());
        stats.touch_file("/c.rs".to_string());
        assert_eq!(stats.recent_files, vec!["/a.rs", "/b.rs", "/c.rs"]);

        // Re-touching /a.rs moves it to the end
        stats.touch_file("/a.rs".to_string());
        assert_eq!(stats.recent_files, vec!["/b.rs", "/c.rs", "/a.rs"]);
        assert_eq!(stats.file_count(), 3); // dedup set unchanged
    }

    #[test]
    fn update_session_stats_populates_recent_files() {
        let path = write_tmp_jsonl("stats_recent", &[
            r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs","/src/app.rs"]}}"#,
            r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs"]}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        // /src/main.rs was touched twice, so it should be last (most recent)
        assert_eq!(stats.recent_files, vec!["/src/app.rs", "/src/main.rs"]);
        let _ = std::fs::remove_file(&path);
    }

    // ── task_elapsed tests ────────────────────────────────────────

    #[test]
    fn task_elapsed_no_timestamps() {
        let stats = SessionStats::default();
        assert!(stats.task_elapsed().is_none());
    }

    #[test]
    fn task_elapsed_user_only_means_working() {
        let mut stats = SessionStats::default();
        // User sent a message 30 seconds ago, no response yet
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        stats.last_user_ts = Some(ts);

        let elapsed = stats.task_elapsed().expect("should be working");
        assert!(elapsed.as_secs() >= 29 && elapsed.as_secs() <= 31);
    }

    #[test]
    fn task_elapsed_assistant_replied_means_idle() {
        let mut stats = SessionStats::default();
        let now = chrono::Utc::now();
        stats.last_user_ts = Some(
            (now - chrono::Duration::seconds(60))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        stats.last_assistant_ts = Some(
            (now - chrono::Duration::seconds(30))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );

        assert!(stats.task_elapsed().is_none(), "assistant replied = task done");
    }

    #[test]
    fn task_elapsed_new_user_msg_after_assistant() {
        let mut stats = SessionStats::default();
        let now = chrono::Utc::now();
        // Assistant replied 60s ago, user sent new msg 10s ago
        stats.last_assistant_ts = Some(
            (now - chrono::Duration::seconds(60))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        stats.last_user_ts = Some(
            (now - chrono::Duration::seconds(10))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );

        let elapsed = stats.task_elapsed().expect("new user msg = working");
        assert!(elapsed.as_secs() >= 9 && elapsed.as_secs() <= 11);
    }

    #[test]
    fn task_elapsed_from_jsonl_parsing() {
        let path = write_tmp_jsonl("stats_timestamps", &[
            r#"{"type":"user","timestamp":"2026-01-15T10:00:00.000Z","message":{"role":"user","content":"do something"}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-15T10:00:30.000Z","message":{"role":"assistant","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"user","timestamp":"2026-01-15T10:01:00.000Z","message":{"role":"user","content":"now do this"}}"#,
        ]);

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(stats.last_user_ts.as_deref(), Some("2026-01-15T10:01:00.000Z"));
        assert_eq!(stats.last_assistant_ts.as_deref(), Some("2026-01-15T10:00:30.000Z"));
        // User message is after assistant → agent should be working
        assert!(stats.task_elapsed().is_some());
        let _ = std::fs::remove_file(&path);
    }

    // ── parse_session_id_from_cmdline tests ────────────────────────

    #[test]
    fn parse_cmdline_separate_args() {
        let cmdline = "claude --dangerously-skip-permissions --session-id 7c04c22f-796f-403a-9521-d83ad13fd60d";
        assert_eq!(
            parse_session_id_from_cmdline(cmdline),
            Some("7c04c22f-796f-403a-9521-d83ad13fd60d".to_string())
        );
    }

    #[test]
    fn parse_cmdline_equals_form() {
        let cmdline = "claude --session-id=7c04c22f-796f-403a-9521-d83ad13fd60d --other-flag";
        assert_eq!(
            parse_session_id_from_cmdline(cmdline),
            Some("7c04c22f-796f-403a-9521-d83ad13fd60d".to_string())
        );
    }

    #[test]
    fn parse_cmdline_no_session_id() {
        let cmdline = "claude --dangerously-skip-permissions";
        assert_eq!(parse_session_id_from_cmdline(cmdline), None);
    }

    #[test]
    fn parse_cmdline_invalid_uuid_after_flag() {
        let cmdline = "claude --session-id not-a-uuid";
        assert_eq!(parse_session_id_from_cmdline(cmdline), None);
    }

    #[test]
    fn parse_cmdline_empty() {
        assert_eq!(parse_session_id_from_cmdline(""), None);
    }

    #[test]
    fn parse_cmdline_session_id_at_end_no_value() {
        let cmdline = "claude --session-id";
        assert_eq!(parse_session_id_from_cmdline(cmdline), None);
    }

    // ── parse_uuid_from_lsof_output tests ───────────────────────────

    #[test]
    fn parse_lsof_finds_uuid() {
        let output = "claude  12345  user  cwd  DIR  1,20  640  /Users/test\n\
                       claude  12345  user  txt  REG  1,20  123  /Users/test/.claude/tasks/7c04c22f-796f-403a-9521-d83ad13fd60d/output.jsonl\n\
                       claude  12345  user  3u   REG  1,20  456  /tmp/some-file";
        assert_eq!(
            parse_uuid_from_lsof_output(output),
            Some("7c04c22f-796f-403a-9521-d83ad13fd60d".to_string())
        );
    }

    #[test]
    fn parse_lsof_no_uuid() {
        let output = "claude  12345  user  cwd  DIR  1,20  640  /Users/test\n\
                       claude  12345  user  txt  REG  1,20  123  /usr/bin/claude";
        assert_eq!(parse_uuid_from_lsof_output(output), None);
    }

    #[test]
    fn parse_lsof_empty() {
        assert_eq!(parse_uuid_from_lsof_output(""), None);
    }

    #[test]
    fn parse_lsof_invalid_uuid_after_tasks() {
        let output = "claude  12345  user  txt  REG  1,20  123  /Users/test/.claude/tasks/not-a-valid-uuid/file";
        assert_eq!(parse_uuid_from_lsof_output(output), None);
    }

    // ── is_uuid tests ────────────────────────────────────────────────

    #[test]
    fn is_uuid_valid() {
        assert!(is_uuid("7c04c22f-796f-403a-9521-d83ad13fd60d"));
    }

    #[test]
    fn is_uuid_invalid_length() {
        assert!(!is_uuid("7c04c22f-796f-403a-9521"));
    }

    #[test]
    fn is_uuid_invalid_chars() {
        assert!(!is_uuid("zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"));
    }

    #[test]
    fn is_uuid_missing_dashes() {
        assert!(!is_uuid("7c04c22f0796f0403a09521od83ad13fd60d"));
    }

    #[test]
    fn escape_project_path_basic() {
        assert_eq!(escape_project_path("/Users/monkey/hydra"), "-Users-monkey-hydra");
    }

    #[test]
    fn escape_project_path_root() {
        assert_eq!(escape_project_path("/"), "-");
    }

    #[test]
    fn escape_project_path_no_slashes() {
        assert_eq!(escape_project_path("projects"), "projects");
    }

    #[test]
    fn escape_project_path_nested() {
        assert_eq!(
            escape_project_path("/Users/cat/code/my-project"),
            "-Users-cat-code-my-project"
        );
    }

    #[test]
    fn is_uuid_all_zeros() {
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));
    }

    #[test]
    fn is_uuid_all_f() {
        assert!(is_uuid("ffffffff-ffff-ffff-ffff-ffffffffffff"));
    }

    #[test]
    fn is_uuid_empty() {
        assert!(!is_uuid(""));
    }

    #[test]
    fn is_uuid_too_long() {
        assert!(!is_uuid("7c04c22f-796f-403a-9521-d83ad13fd60d0"));
    }

    #[test]
    fn is_uuid_wrong_dash_positions() {
        // Dashes at wrong positions
        assert!(!is_uuid("7c04c22f0796f-403a-9521-d83ad13fd60d"));
    }

    // ── read_last_assistant_message tests ─────────────────────────────

    #[test]
    fn read_last_assistant_message_with_valid_jsonl() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs");
        let escaped = escape_project_path("/tmp/test-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));

        let mut f = std::fs::File::create(&log_file).unwrap();
        // Write some non-assistant lines
        writeln!(f, r#"{{"type":"user","message":{{"content":"hello"}}}}"#).unwrap();
        // Write an assistant message
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"text":"I can help with that!"}}]}}}}"#
        )
        .unwrap();
        // Write another assistant message (should be the one returned)
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"text":"Here is the answer."}}]}}}}"#
        )
        .unwrap();

        // Temporarily override HOME
        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/test-project", uuid);
        assert_eq!(msg, Some("Here is the answer.".to_string()));

        // Cleanup
        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }

    #[test]
    fn read_last_assistant_message_multiple_text_parts() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs_parts");
        let escaped = escape_project_path("/tmp/parts-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "11111111-2222-3333-4444-555555555555";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));

        let mut f = std::fs::File::create(&log_file).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"text":"Part one."}},{{"text":"Part two."}}]}}}}"#
        )
        .unwrap();

        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/parts-project", uuid);
        assert_eq!(msg, Some("Part one. Part two.".to_string()));

        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }

    #[test]
    fn read_last_assistant_message_no_file() {
        let msg = read_last_assistant_message(
            "/nonexistent/path",
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        );
        assert_eq!(msg, None);
    }

    #[test]
    fn read_last_assistant_message_empty_file() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs_empty");
        let escaped = escape_project_path("/tmp/empty-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "cccccccc-dddd-eeee-ffff-000000000000";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));
        let _ = std::fs::File::create(&log_file).unwrap();

        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/empty-project", uuid);
        assert_eq!(msg, None);

        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }

    #[test]
    fn read_last_assistant_message_no_assistant_lines() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs_noassist");
        let escaped = escape_project_path("/tmp/noassist-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "dddddddd-eeee-ffff-0000-111111111111";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));

        let mut f = std::fs::File::create(&log_file).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hello"}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"system","message":{{"content":"info"}}}}"#).unwrap();

        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/noassist-project", uuid);
        assert_eq!(msg, None);

        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }

    #[test]
    fn read_last_assistant_message_condenses_whitespace() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs_ws");
        let escaped = escape_project_path("/tmp/ws-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "eeeeeeee-ffff-0000-1111-222222222222";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));

        let mut f = std::fs::File::create(&log_file).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"text":"  hello   world  \n  foo  "}}]}}}}"#
        )
        .unwrap();

        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/ws-project", uuid);
        assert_eq!(msg, Some("hello world foo".to_string()));

        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }

    #[test]
    fn read_last_assistant_message_skips_invalid_json() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs_invalid");
        let escaped = escape_project_path("/tmp/invalid-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "ffffffff-0000-1111-2222-333333333333";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));

        let mut f = std::fs::File::create(&log_file).unwrap();
        writeln!(f, "this is not json but contains assistant").unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"text":"valid line"}}]}}}}"#
        )
        .unwrap();

        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/invalid-project", uuid);
        assert_eq!(msg, Some("valid line".to_string()));

        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }

    #[test]
    fn read_last_assistant_message_empty_content_array() {
        use std::io::Write;

        let tmp_dir = std::env::temp_dir().join("hydra_test_logs_emptycontent");
        let escaped = escape_project_path("/tmp/emptycontent-project");
        let projects_dir = tmp_dir.join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "00000000-1111-2222-3333-444444444444";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));

        let mut f = std::fs::File::create(&log_file).unwrap();
        // Empty content array — no text items
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[]}}}}"#
        )
        .unwrap();

        let _lock = HOME_LOCK.lock().unwrap();
        let orig_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &tmp_dir);

        let msg = read_last_assistant_message("/tmp/emptycontent-project", uuid);
        assert_eq!(msg, None);

        if let Some(home) = orig_home {
            std::env::set_var("HOME", home);
        }
        let _ = std::fs::remove_dir_all(&tmp_dir.join(".claude"));
    }
}
