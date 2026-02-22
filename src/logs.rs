use std::collections::{HashMap, HashSet};
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
#[derive(Debug, Default, Clone)]
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
    /// Active subagent count (from queue-operation enqueue/remove entries).
    pub active_subagents: u16,
}

/// Upper bound for per-session touched file history.
/// Keeps enough history for real projects while preventing unbounded growth.
const MAX_SESSION_TRACKED_FILES: usize = 4096;

impl SessionStats {
    #[cfg(test)]
    pub fn cost_usd(&self) -> f64 {
        let input = self.tokens_in as f64 * 3.0 / 1_000_000.0;
        let output = self.tokens_out as f64 * 15.0 / 1_000_000.0;
        let cache_read = self.tokens_cache_read as f64 * 0.30 / 1_000_000.0;
        let cache_write = self.tokens_cache_write as f64 * 3.75 / 1_000_000.0;
        input + output + cache_read + cache_write
    }

    #[cfg(test)]
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
        // Existing path: move it to the end (most recent).
        if let Some(pos) = self.recent_files.iter().position(|f| f == &path) {
            self.recent_files.remove(pos);
            self.recent_files.push(path);
            return;
        }

        // New path: evict oldest entries when at capacity.
        while self.recent_files.len() >= MAX_SESSION_TRACKED_FILES {
            if let Some(evicted) = self.recent_files.first().cloned() {
                self.recent_files.remove(0);
                self.files.remove(&evicted);
            } else {
                break;
            }
        }

        self.files.insert(path.clone());
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
#[allow(dead_code)]
pub fn update_session_stats(cwd: &str, uuid: &str, stats: &mut SessionStats) {
    let _ = update_session_stats_and_last_message(cwd, uuid, stats);
}

/// Incrementally update stats and return the most recent assistant text seen
/// in newly-read bytes (if any).
pub fn update_session_stats_and_last_message(
    cwd: &str,
    uuid: &str,
    stats: &mut SessionStats,
) -> Option<String> {
    let escaped = escape_project_path(cwd);
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return None,
    };
    let path = PathBuf::from(&home)
        .join(".claude")
        .join("projects")
        .join(&escaped)
        .join(format!("{uuid}.jsonl"));

    update_session_stats_from_path_and_last_message(&path, stats)
}

/// Core stats parser — reads from a specific file path.
/// Separated from `update_session_stats` for testability (avoids HOME env var).
#[allow(dead_code)]
fn update_session_stats_from_path(path: &std::path::Path, stats: &mut SessionStats) {
    let _ = update_session_stats_from_path_and_last_message(path, stats);
}

pub fn update_session_stats_from_path_and_last_message(
    path: &std::path::Path,
    stats: &mut SessionStats,
) -> Option<String> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return None,
    };
    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return None,
    };

    // Nothing new to read
    if file_len <= stats.read_offset {
        return None;
    }

    // Seek to where we left off
    if stats.read_offset > 0 {
        if file.seek(SeekFrom::Start(stats.read_offset)).is_err() {
            return None;
        }
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return None;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut last_text: Option<String> = None;

    for line in text.lines() {
        // Skip empty lines
        if line.len() < 10 {
            continue;
        }

        // Fast path: assistant messages. Parse once and update both stats + last text.
        if line.contains("\"assistant\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                    if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                        stats.last_assistant_ts = Some(ts.to_string());
                    }

                    if let Some(text) = extract_assistant_message_text(&v) {
                        last_text = Some(text);
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

        // Fast path: user messages — track timestamp for task-start timing
        if line.contains("\"user\"")
            && line.contains("\"timestamp\"")
            && !line.contains("\"usage\"")
        {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("user") {
                    if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
                        stats.last_user_ts = Some(ts.to_string());
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
            continue;
        }

        // Fast path: queue-operation entries for subagent tracking
        if line.contains("\"queue-operation\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("type").and_then(|t| t.as_str()) == Some("queue-operation") {
                    match v.get("operation").and_then(|o| o.as_str()) {
                        Some("enqueue") => {
                            stats.active_subagents = stats.active_subagents.saturating_add(1);
                        }
                        Some("remove") => {
                            stats.active_subagents = stats.active_subagents.saturating_sub(1);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    stats.read_offset = file_len;
    last_text
}

const FILE_DISCOVERY_INTERVAL_SECS: i64 = 30;

// Uses OpenAI's published GPT-5 Codex token pricing as an estimate.
const CODEX_INPUT_USD_PER_MTOK: f64 = 1.25;
const CODEX_OUTPUT_USD_PER_MTOK: f64 = 10.0;
const CODEX_CACHE_READ_USD_PER_MTOK: f64 = 0.125;

#[derive(Debug, Clone, Default)]
struct CodexFileState {
    read_offset: u64,
    last_total_tokens: u64,
    last_input_tokens: u64,
    last_output_tokens: u64,
    last_cached_input_tokens: u64,
}

/// Machine-wide stats for today, aggregated across Claude and Codex logs.
/// Updated incrementally — only new bytes are parsed on each refresh.
#[derive(Debug, Clone)]
pub struct GlobalStats {
    // Aggregate totals displayed in the UI.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    // Provider breakdown used for cost calculations.
    pub claude_tokens_in: u64,
    pub claude_tokens_out: u64,
    pub claude_tokens_cache_read: u64,
    pub claude_tokens_cache_write: u64,
    pub codex_tokens_in: u64,
    pub codex_tokens_out: u64,
    pub codex_tokens_cache_read: u64,
    /// Per-file read offsets for incremental Claude log reading.
    file_offsets: HashMap<PathBuf, u64>,
    /// Per-file incremental state for Codex token_count parsing.
    codex_file_states: HashMap<PathBuf, CodexFileState>,
    /// Cached file list to avoid recursive scans on every refresh.
    known_claude_files: Vec<PathBuf>,
    /// Cached file list to avoid recursive scans on every refresh.
    known_codex_files: Vec<PathBuf>,
    /// Unix timestamp of last recursive file discovery.
    last_file_discovery_ts: i64,
    /// Date string (YYYY-MM-DD) these stats are for; reset when date changes.
    date: String,
}

impl Default for GlobalStats {
    fn default() -> Self {
        Self {
            tokens_in: 0,
            tokens_out: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            claude_tokens_in: 0,
            claude_tokens_out: 0,
            claude_tokens_cache_read: 0,
            claude_tokens_cache_write: 0,
            codex_tokens_in: 0,
            codex_tokens_out: 0,
            codex_tokens_cache_read: 0,
            file_offsets: HashMap::new(),
            codex_file_states: HashMap::new(),
            known_claude_files: Vec::new(),
            known_codex_files: Vec::new(),
            last_file_discovery_ts: 0,
            date: String::new(),
        }
    }
}

impl GlobalStats {
    /// Estimated cost in USD using provider-specific pricing.
    /// Claude: Sonnet ($3 in / $15 out / $0.30 cache-read / $3.75 cache-write per MTok).
    /// Codex: GPT-5 Codex estimate ($1.25 in / $10 out / $0.125 cache-read per MTok).
    pub fn cost_usd(&self) -> f64 {
        let has_breakdown = self.claude_tokens_in > 0
            || self.claude_tokens_out > 0
            || self.claude_tokens_cache_read > 0
            || self.claude_tokens_cache_write > 0
            || self.codex_tokens_in > 0
            || self.codex_tokens_out > 0
            || self.codex_tokens_cache_read > 0;

        // Backward compatibility for tests/older state that only set aggregate fields.
        if !has_breakdown {
            let input = self.tokens_in as f64 * 3.0 / 1_000_000.0;
            let output = self.tokens_out as f64 * 15.0 / 1_000_000.0;
            let cache_read = self.tokens_cache_read as f64 * 0.30 / 1_000_000.0;
            let cache_write = self.tokens_cache_write as f64 * 3.75 / 1_000_000.0;
            return input + output + cache_read + cache_write;
        }

        let claude_input = self.claude_tokens_in as f64 * 3.0 / 1_000_000.0;
        let claude_output = self.claude_tokens_out as f64 * 15.0 / 1_000_000.0;
        let claude_cache_read = self.claude_tokens_cache_read as f64 * 0.30 / 1_000_000.0;
        let claude_cache_write = self.claude_tokens_cache_write as f64 * 3.75 / 1_000_000.0;

        let codex_uncached_input_tokens = self
            .codex_tokens_in
            .saturating_sub(self.codex_tokens_cache_read);
        let codex_input =
            codex_uncached_input_tokens as f64 * CODEX_INPUT_USD_PER_MTOK / 1_000_000.0;
        let codex_output = self.codex_tokens_out as f64 * CODEX_OUTPUT_USD_PER_MTOK / 1_000_000.0;
        let codex_cache_read =
            self.codex_tokens_cache_read as f64 * CODEX_CACHE_READ_USD_PER_MTOK / 1_000_000.0;

        claude_input
            + claude_output
            + claude_cache_read
            + claude_cache_write
            + codex_input
            + codex_output
            + codex_cache_read
    }
}

/// Scan Claude + Codex logs and sum today's token usage.
/// Incremental: only reads new bytes per file after the first call.
/// Resets at midnight (date change).
pub fn update_global_stats(stats: &mut GlobalStats) {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Reset on date change
    if stats.date != today {
        stats.tokens_in = 0;
        stats.tokens_out = 0;
        stats.tokens_cache_read = 0;
        stats.tokens_cache_write = 0;
        stats.claude_tokens_in = 0;
        stats.claude_tokens_out = 0;
        stats.claude_tokens_cache_read = 0;
        stats.claude_tokens_cache_write = 0;
        stats.codex_tokens_in = 0;
        stats.codex_tokens_out = 0;
        stats.codex_tokens_cache_read = 0;
        stats.file_offsets.clear();
        stats.codex_file_states.clear();
        stats.known_claude_files.clear();
        stats.known_codex_files.clear();
        stats.last_file_discovery_ts = 0;
        stats.date = today.clone();
    }

    update_global_stats_inner(stats, &today, None);
}

/// Inner implementation that accepts an optional base_dir for testability.
fn update_global_stats_inner(
    stats: &mut GlobalStats,
    today: &str,
    base_dir: Option<&std::path::Path>,
) {
    let (claude_projects_dir, codex_sessions_dir) = match base_dir {
        Some(dir) => (dir.to_path_buf(), dir.join(".codex").join("sessions")),
        None => {
            let home = match std::env::var("HOME") {
                Ok(h) => h,
                Err(_) => return,
            };
            (
                PathBuf::from(&home).join(".claude").join("projects"),
                PathBuf::from(&home).join(".codex").join("sessions"),
            )
        }
    };

    let now_ts = chrono::Utc::now().timestamp();
    let needs_discovery = stats.last_file_discovery_ts == 0
        || now_ts - stats.last_file_discovery_ts >= FILE_DISCOVERY_INTERVAL_SECS;

    if needs_discovery {
        let mut claude_files = Vec::new();
        collect_jsonl_files(&claude_projects_dir, &mut claude_files, 0);
        stats.known_claude_files = claude_files;

        let mut codex_files = Vec::new();
        collect_jsonl_files(&codex_sessions_dir, &mut codex_files, 0);
        stats.known_codex_files = codex_files;

        let claude_file_set: HashSet<PathBuf> = stats.known_claude_files.iter().cloned().collect();
        stats
            .file_offsets
            .retain(|p, _| claude_file_set.contains(p));

        let codex_file_set: HashSet<PathBuf> = stats.known_codex_files.iter().cloned().collect();
        stats
            .codex_file_states
            .retain(|p, _| codex_file_set.contains(p));

        stats.last_file_discovery_ts = now_ts;
    }

    // Process Claude files incrementally.
    // Index-based iteration avoids cloning the entire Vec<PathBuf> — we can't
    // iterate by reference because process_*_global_file takes &mut stats.
    for i in 0..stats.known_claude_files.len() {
        let path = stats.known_claude_files[i].clone();
        process_claude_global_file(&path, stats, today);
    }

    // Process Codex files incrementally.
    for i in 0..stats.known_codex_files.len() {
        let path = stats.known_codex_files[i].clone();
        process_codex_global_file(&path, stats, today);
    }
}

fn add_claude_usage(
    stats: &mut GlobalStats,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) {
    stats.tokens_in += input_tokens;
    stats.tokens_out += output_tokens;
    stats.tokens_cache_read += cache_read_tokens;
    stats.tokens_cache_write += cache_write_tokens;

    stats.claude_tokens_in += input_tokens;
    stats.claude_tokens_out += output_tokens;
    stats.claude_tokens_cache_read += cache_read_tokens;
    stats.claude_tokens_cache_write += cache_write_tokens;
}

fn add_codex_usage(
    stats: &mut GlobalStats,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
) {
    stats.tokens_in += input_tokens;
    stats.tokens_out += output_tokens;
    stats.tokens_cache_read += cache_read_tokens;

    stats.codex_tokens_in += input_tokens;
    stats.codex_tokens_out += output_tokens;
    stats.codex_tokens_cache_read += cache_read_tokens;
}

fn process_claude_global_file(path: &PathBuf, stats: &mut GlobalStats, today: &str) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    let offset = stats.file_offsets.get(path).copied().unwrap_or(0);
    if file_len <= offset {
        return;
    }

    if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
        return;
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return;
    }
    let text = String::from_utf8_lossy(&buf);

    for line in text.lines() {
        if line.len() < 10 {
            continue;
        }
        if !line.contains(today) || !line.contains("\"assistant\"") || !line.contains("\"usage\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            if let Some(usage) = v.get("message").and_then(|m| m.get("usage")) {
                add_claude_usage(
                    stats,
                    usage
                        .get("input_tokens")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0),
                    usage
                        .get("output_tokens")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0),
                    usage
                        .get("cache_read_input_tokens")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0),
                    usage
                        .get("cache_creation_input_tokens")
                        .and_then(|t| t.as_u64())
                        .unwrap_or(0),
                );
            }
        }
    }

    stats.file_offsets.insert(path.clone(), file_len);
}

fn process_codex_global_file(path: &PathBuf, stats: &mut GlobalStats, today: &str) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    let mut last_total_tokens = stats
        .codex_file_states
        .get(path)
        .map(|s| s.last_total_tokens)
        .unwrap_or(0);
    let mut last_input_tokens = stats
        .codex_file_states
        .get(path)
        .map(|s| s.last_input_tokens)
        .unwrap_or(0);
    let mut last_output_tokens = stats
        .codex_file_states
        .get(path)
        .map(|s| s.last_output_tokens)
        .unwrap_or(0);
    let mut last_cached_input_tokens = stats
        .codex_file_states
        .get(path)
        .map(|s| s.last_cached_input_tokens)
        .unwrap_or(0);
    let offset = stats
        .codex_file_states
        .get(path)
        .map(|s| s.read_offset)
        .unwrap_or(0);

    if file_len <= offset {
        return;
    }

    if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
        return;
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return;
    }
    let text = String::from_utf8_lossy(&buf);

    for line in text.lines() {
        if line.len() < 20 {
            continue;
        }
        if !line.contains("\"token_count\"") || !line.contains("\"total_token_usage\"") {
            continue;
        }

        let v = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
            continue;
        }

        let totals = match payload.get("info").and_then(|i| i.get("total_token_usage")) {
            Some(t) => t,
            None => continue,
        };

        let total_input_tokens = totals
            .get("input_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let total_output_tokens = totals
            .get("output_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let total_cached_input_tokens = totals
            .get("cached_input_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let total_tokens = totals
            .get("total_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(total_input_tokens.saturating_add(total_output_tokens));

        let is_today = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .is_some_and(|ts| ts.starts_with(today));

        if total_tokens > last_total_tokens {
            if is_today {
                let delta_input = total_input_tokens.saturating_sub(last_input_tokens);
                let delta_output = total_output_tokens.saturating_sub(last_output_tokens);
                let delta_cache_read =
                    total_cached_input_tokens.saturating_sub(last_cached_input_tokens);
                add_codex_usage(stats, delta_input, delta_output, delta_cache_read);
            }
        }

        // Always advance snapshot state; duplicate totals are ignored by the delta check above.
        last_total_tokens = total_tokens;
        last_input_tokens = total_input_tokens;
        last_output_tokens = total_output_tokens;
        last_cached_input_tokens = total_cached_input_tokens;
    }

    stats.codex_file_states.insert(
        path.clone(),
        CodexFileState {
            read_offset: file_len,
            last_total_tokens,
            last_input_tokens,
            last_output_tokens,
            last_cached_input_tokens,
        },
    );
}

/// Recursively collect all `.jsonl` files under a directory.
/// Bounded to 4 levels deep to avoid runaway walks.
fn collect_jsonl_files(dir: &std::path::Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out, depth + 1);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Get the pane PID for a tmux session.
pub async fn get_pane_pid(tmux_name: &str) -> Option<u32> {
    let output = run_cmd_timeout(Command::new("tmux").args([
        "list-panes",
        "-t",
        tmux_name,
        "-F",
        "#{pane_pid}",
    ]))
    .await
    .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
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
    let output =
        run_cmd_timeout(Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]))
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
            let output =
                run_cmd_timeout(Command::new("pgrep").args(["-P", &parent.to_string()])).await;

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
/// e.g. "/home/user/project" → "-home-user-project"
fn escape_project_path(cwd: &str) -> String {
    cwd.replace('/', "-")
}

pub fn extract_assistant_message_text(v: &serde_json::Value) -> Option<String> {
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;

    let mut parts = Vec::new();
    for item in content {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            parts.push(text);
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Read the last assistant message from a Claude JSONL log file.
/// Reads only the tail of the file for efficiency on large logs.
#[allow(dead_code)]
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
        if let Some(text) = extract_assistant_message_text(&v) {
            last_text = Some(text);
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

    /// RAII guard that saves HOME, sets it to a new value, and restores on drop.
    /// Also acquires HOME_LOCK for thread safety.
    struct HomeGuard {
        orig: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        /// Save current HOME, set to new path, and acquire the HOME_LOCK.
        fn set(path: &std::path::Path) -> Self {
            let lock = HOME_LOCK.lock().unwrap();
            let orig = std::env::var("HOME").ok();
            std::env::set_var("HOME", path);
            Self { orig, _lock: lock }
        }

        /// Save current HOME, remove it, and acquire the HOME_LOCK.
        fn remove() -> Self {
            let lock = HOME_LOCK.lock().unwrap();
            let orig = std::env::var("HOME").ok();
            std::env::remove_var("HOME");
            Self { orig, _lock: lock }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            if let Some(h) = &self.orig {
                std::env::set_var("HOME", h);
            }
        }
    }

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
            tokens_in: 1_000_000,        // $3.00
            tokens_out: 100_000,         // $1.50
            tokens_cache_read: 500_000,  // $0.15
            tokens_cache_write: 200_000, // $0.75
            ..Default::default()
        };
        let cost = stats.cost_usd();
        assert!(
            (cost - 5.40).abs() < 0.01,
            "expected ~$5.40, got ${cost:.2}"
        );
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
        let path = write_tmp_jsonl(
            "stats_tokens",
            &[
                r#"{"type":"assistant","message":{"usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":100},"content":[{"type":"text","text":"hello"}]}}"#,
                r#"{"type":"assistant","message":{"usage":{"input_tokens":2000,"output_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"world"}]}}"#,
            ],
        );

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
        let path = write_tmp_jsonl(
            "stats_tools",
            &[
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","id":"t1","input":{}},{"type":"tool_use","name":"Bash","id":"t2","input":{}},{"type":"tool_use","name":"Write","id":"t3","input":{}}]}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(stats.edits, 2, "Edit + Write = 2 edits");
        assert_eq!(stats.bash_cmds, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_session_stats_tracks_files() {
        let path = write_tmp_jsonl(
            "stats_files",
            &[
                r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs","/src/app.rs"]}}"#,
                r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs"]}}"#,
            ],
        );

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
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
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
        let path = write_tmp_jsonl(
            "stats_short",
            &[
                "short", // < 10 chars, should be skipped
                "",      // empty, should be skipped
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1, "should skip short lines");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_unknown_tool_name_ignored() {
        let path = write_tmp_jsonl(
            "stats_unknown_tool",
            &[
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"UnknownTool","id":"t1","input":{}}]}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.edits, 0);
        assert_eq!(stats.bash_cmds, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_assistant_without_usage_not_counted() {
        let path = write_tmp_jsonl(
            "stats_no_usage",
            &[
                // "assistant" in line but no "usage" — won't match fast path
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"no usage field here"}]}}"#,
            ],
        );

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
    fn touch_file_caps_history_size() {
        let mut stats = SessionStats::default();
        let total = MAX_SESSION_TRACKED_FILES + 50;
        for i in 0..total {
            stats.touch_file(format!("/file-{i}.rs"));
        }

        assert_eq!(stats.files.len(), MAX_SESSION_TRACKED_FILES);
        assert_eq!(stats.recent_files.len(), MAX_SESSION_TRACKED_FILES);
        assert!(
            !stats.files.contains("/file-0.rs"),
            "oldest files should be evicted at capacity"
        );
        assert!(
            stats.files.contains(&format!("/file-{}.rs", total - 1)),
            "newest file should remain tracked"
        );
    }

    #[test]
    fn touch_file_retouch_at_capacity_keeps_size_bounded() {
        let mut stats = SessionStats::default();
        for i in 0..MAX_SESSION_TRACKED_FILES {
            stats.touch_file(format!("/file-{i}.rs"));
        }

        stats.touch_file("/file-0.rs".to_string());
        assert_eq!(stats.files.len(), MAX_SESSION_TRACKED_FILES);
        assert_eq!(stats.recent_files.len(), MAX_SESSION_TRACKED_FILES);
        assert_eq!(
            stats.recent_files.last().map(|s| s.as_str()),
            Some("/file-0.rs")
        );
    }

    #[test]
    fn update_session_stats_populates_recent_files() {
        let path = write_tmp_jsonl(
            "stats_recent",
            &[
                r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs","/src/app.rs"]}}"#,
                r#"{"type":"user","toolUseResult":{"filenames":["/src/main.rs"]}}"#,
            ],
        );

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

        assert!(
            stats.task_elapsed().is_none(),
            "assistant replied = task done"
        );
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
        let path = write_tmp_jsonl(
            "stats_timestamps",
            &[
                r#"{"type":"user","timestamp":"2026-01-15T10:00:00.000Z","message":{"role":"user","content":"do something"}}"#,
                r#"{"type":"assistant","timestamp":"2026-01-15T10:00:30.000Z","message":{"role":"assistant","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}"#,
                r#"{"type":"user","timestamp":"2026-01-15T10:01:00.000Z","message":{"role":"user","content":"now do this"}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(
            stats.last_user_ts.as_deref(),
            Some("2026-01-15T10:01:00.000Z")
        );
        assert_eq!(
            stats.last_assistant_ts.as_deref(),
            Some("2026-01-15T10:00:30.000Z")
        );
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
        assert_eq!(
            escape_project_path("/home/user/project"),
            "-home-user-project"
        );
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
            escape_project_path("/home/dev/code/my-project"),
            "-home-dev-code-my-project"
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
        writeln!(f, r#"{{"type":"assistant","message":{{"content":[]}}}}"#).unwrap();

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

    // ── update_session_stats (HOME wrapper) tests ──────────────────

    #[test]
    fn update_session_stats_via_home_env() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let escaped = escape_project_path("/tmp/stats-home-project");
        let projects_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&projects_dir).unwrap();

        let uuid = "aabbccdd-1122-3344-5566-778899aabbcc";
        let log_file = projects_dir.join(format!("{uuid}.jsonl"));
        let mut f = std::fs::File::create(&log_file).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":500,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"done"}}]}}}}"#).unwrap();
        drop(f);

        let mut stats = SessionStats::default();
        update_session_stats("/tmp/stats-home-project", uuid, &mut stats);
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.tokens_in, 500);
    }

    #[test]
    fn update_session_stats_missing_home_is_noop() {
        let _guard = HomeGuard::remove();

        let mut stats = SessionStats::default();
        update_session_stats(
            "/tmp/whatever",
            "some-uuid-value-here-1234567890ab",
            &mut stats,
        );
        assert_eq!(stats.turns, 0, "missing HOME should be no-op");
    }

    // ── parse_iso_timestamp tests ─────────────────────────────────

    #[test]
    fn parse_iso_timestamp_valid() {
        let ts = parse_iso_timestamp("2026-01-15T10:00:00.000Z");
        assert!(ts.is_some());
    }

    #[test]
    fn parse_iso_timestamp_invalid() {
        assert!(parse_iso_timestamp("not a timestamp").is_none());
        assert!(parse_iso_timestamp("").is_none());
        assert!(parse_iso_timestamp("2026-13-45T99:99:99Z").is_none());
    }

    // ── JSONL timestamp branch coverage ──────────────────────────

    #[test]
    fn stats_user_message_without_timestamp_field() {
        // User message that contains "user" but no "timestamp" key —
        // should not match the timestamp fast path
        let path = write_tmp_jsonl(
            "stats_user_no_ts",
            &[r#"{"type":"user","message":{"role":"user","content":"hi"}}"#],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert!(stats.last_user_ts.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_user_message_with_usage_skips_user_fast_path() {
        // A line with both "user" and "timestamp" AND "usage" should
        // not match the user timestamp fast path (line 181 condition)
        let path = write_tmp_jsonl(
            "stats_user_usage",
            &[
                r#"{"type":"user","timestamp":"2026-01-15T10:00:00.000Z","usage":{"input_tokens":100},"message":{"content":"hi"}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        // Should not set last_user_ts because "usage" is present
        assert!(stats.last_user_ts.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_assistant_timestamp_tracked() {
        let path = write_tmp_jsonl(
            "stats_ast_ts",
            &[
                r#"{"type":"assistant","timestamp":"2026-01-15T10:00:30.000Z","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(
            stats.last_assistant_ts.as_deref(),
            Some("2026-01-15T10:00:30.000Z")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_assistant_without_timestamp_field() {
        let path = write_tmp_jsonl(
            "stats_ast_no_ts",
            &[
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert!(stats.last_assistant_ts.is_none());
        assert_eq!(stats.turns, 1); // Still counted as a turn
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_tool_result_without_filenames() {
        // toolUseResult without filenames array — should not crash
        let path = write_tmp_jsonl(
            "stats_tool_no_files",
            &[r#"{"type":"user","toolUseResult":{"content":"some result"}}"#],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.file_count(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_tool_result_with_non_string_filename() {
        // filenames array with non-string entries — should skip gracefully
        let path = write_tmp_jsonl(
            "stats_tool_bad_fname",
            &[r#"{"type":"user","toolUseResult":{"filenames":["/valid.rs", 123, null]}}"#],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.file_count(), 1, "only string filenames counted");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_malformed_json_line_skipped() {
        // Valid assistant + malformed line — should not interfere
        let path = write_tmp_jsonl(
            "stats_malformed",
            &[
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"ok"}]}}"#,
                r#"{"type":"assistant","usage" this is broken json"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1, "only valid lines counted");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stats_mixed_message_types_full_coverage() {
        // A comprehensive test with user timestamps, assistant timestamps,
        // tool use results, and file tracking
        let path = write_tmp_jsonl(
            "stats_mixed_full",
            &[
                r#"{"type":"user","timestamp":"2026-01-15T10:00:00.000Z","message":{"role":"user","content":"start"}}"#,
                r#"{"type":"assistant","timestamp":"2026-01-15T10:00:15.000Z","message":{"usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":50,"cache_creation_input_tokens":25},"content":[{"type":"text","text":"thinking..."},{"type":"tool_use","name":"Edit","id":"t1","input":{}},{"type":"tool_use","name":"Bash","id":"t2","input":{}}]}}"#,
                r#"{"filenames":["/src/main.rs"],"toolUseResult":{"filenames":["/src/main.rs"]}}"#,
                r#"{"type":"user","timestamp":"2026-01-15T10:01:00.000Z","message":{"role":"user","content":"next task"}}"#,
                r#"{"type":"assistant","timestamp":"2026-01-15T10:01:30.000Z","message":{"usage":{"input_tokens":500,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Write","id":"t3","input":{}}]}}"#,
            ],
        );

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);

        assert_eq!(stats.turns, 2);
        assert_eq!(stats.tokens_in, 1500);
        assert_eq!(stats.tokens_out, 300);
        assert_eq!(stats.tokens_cache_read, 50);
        assert_eq!(stats.tokens_cache_write, 25);
        assert_eq!(stats.edits, 2, "Edit + Write = 2");
        assert_eq!(stats.bash_cmds, 1);
        assert_eq!(stats.file_count(), 1);
        assert_eq!(
            stats.last_user_ts.as_deref(),
            Some("2026-01-15T10:01:00.000Z")
        );
        assert_eq!(
            stats.last_assistant_ts.as_deref(),
            Some("2026-01-15T10:01:30.000Z")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_lsof_uuid_too_short_after_tasks() {
        let output =
            "claude  12345  user  txt  REG  1,20  123  /Users/test/.claude/tasks/short/file";
        assert_eq!(parse_uuid_from_lsof_output(output), None);
    }

    #[test]
    fn parse_cmdline_equals_invalid_uuid() {
        let cmdline = "claude --session-id=not-a-valid-uuid";
        assert_eq!(parse_session_id_from_cmdline(cmdline), None);
    }

    // ── GlobalStats tests ───────────────────────────────────────────

    #[test]
    fn global_stats_cost_calculation() {
        let stats = GlobalStats {
            tokens_in: 1_000_000,
            tokens_out: 100_000,
            tokens_cache_read: 500_000,
            tokens_cache_write: 200_000,
            ..Default::default()
        };
        let cost = stats.cost_usd();
        assert!(
            (cost - 5.40).abs() < 0.01,
            "expected ~$5.40, got ${cost:.2}"
        );
    }

    #[test]
    fn global_stats_default_is_zero() {
        let stats = GlobalStats::default();
        assert_eq!(stats.tokens_in, 0);
        assert_eq!(stats.tokens_out, 0);
        assert!((stats.cost_usd() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn global_stats_cost_calculation_with_codex_breakdown() {
        let stats = GlobalStats {
            codex_tokens_in: 1_000_000,
            codex_tokens_out: 100_000,
            codex_tokens_cache_read: 200_000,
            ..Default::default()
        };
        let cost = stats.cost_usd();
        assert!(
            (cost - 2.025).abs() < 0.01,
            "expected ~$2.03, got ${cost:.2}"
        );
    }

    #[test]
    fn global_stats_codex_cost_saturates_when_cache_exceeds_input() {
        let stats = GlobalStats {
            codex_tokens_in: 100,
            codex_tokens_out: 0,
            codex_tokens_cache_read: 200,
            ..Default::default()
        };
        let cost = stats.cost_usd();
        // Uncached input should saturate at 0, so only cached pricing applies.
        let expected = 200.0 * CODEX_CACHE_READ_USD_PER_MTOK / 1_000_000.0;
        assert!((cost - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn update_global_stats_scans_jsonl_files() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("proj-a");
        std::fs::create_dir_all(&projects).unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let log = projects.join("session1.jsonl");
        let mut f = std::fs::File::create(&log).unwrap();
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:00:00.000Z","message":{{"usage":{{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":50,"cache_creation_input_tokens":10}},"content":[{{"type":"text","text":"hello"}}]}}}}"#,
            today = today
        ).unwrap();
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:01:00.000Z","message":{{"usage":{{"input_tokens":2000,"output_tokens":300,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"world"}}]}}}}"#,
            today = today
        ).unwrap();
        drop(f);

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));

        assert_eq!(stats.tokens_in, 3000);
        assert_eq!(stats.tokens_out, 500);
        assert_eq!(stats.tokens_cache_read, 50);
        assert_eq!(stats.tokens_cache_write, 10);
    }

    #[test]
    fn update_global_stats_incremental_reads() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("proj-b");
        std::fs::create_dir_all(&projects).unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let log = projects.join("session2.jsonl");
        let mut f = std::fs::File::create(&log).unwrap();
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:00:00.000Z","message":{{"usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"first"}}]}}}}"#,
            today = today
        ).unwrap();
        drop(f);

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));
        assert_eq!(stats.tokens_in, 100);

        // Append more data
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:01:00.000Z","message":{{"usage":{{"input_tokens":200,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"second"}}]}}}}"#,
            today = today
        ).unwrap();
        drop(f);

        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));
        assert_eq!(stats.tokens_in, 300, "should accumulate incrementally");
        assert_eq!(stats.tokens_out, 150);
    }

    #[test]
    fn update_global_stats_skips_other_dates() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("proj-c");
        std::fs::create_dir_all(&projects).unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let log = projects.join("session3.jsonl");
        let mut f = std::fs::File::create(&log).unwrap();
        // Write an entry from a different date — should be skipped
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"2020-01-01T10:00:00.000Z","message":{{"usage":{{"input_tokens":5000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"old"}}]}}}}"#
        ).unwrap();
        // Write an entry from today — should be counted
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:00:00.000Z","message":{{"usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"new"}}]}}}}"#,
            today = today
        ).unwrap();
        drop(f);

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));

        assert_eq!(stats.tokens_in, 100, "should only count today's entries");
        assert_eq!(stats.tokens_out, 50);
    }

    #[test]
    fn update_global_stats_resets_on_date_change() {
        let mut stats = GlobalStats::default();
        stats.tokens_in = 5000;
        stats.tokens_out = 1000;
        stats.date = "2020-01-01".to_string();
        stats.file_offsets.insert(PathBuf::from("/fake"), 999);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Use a nonexistent base_dir so no real files are scanned.
        // The date mismatch logic is in update_global_stats (public),
        // so we replicate the reset check + call inner with empty dir.
        if stats.date != today {
            stats.tokens_in = 0;
            stats.tokens_out = 0;
            stats.tokens_cache_read = 0;
            stats.tokens_cache_write = 0;
            stats.file_offsets.clear();
            stats.date = today.clone();
        }
        update_global_stats_inner(
            &mut stats,
            &today,
            Some(std::path::Path::new("/nonexistent/path")),
        );

        assert_eq!(stats.date, today);
        assert_eq!(stats.tokens_in, 0, "should reset on date change");
        assert_eq!(stats.tokens_out, 0);
        assert!(stats.file_offsets.is_empty());
    }

    #[test]
    fn update_global_stats_no_projects_dir() {
        let mut stats = GlobalStats::default();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        stats.date = today.clone();
        // Point at nonexistent dir — should not panic
        update_global_stats_inner(
            &mut stats,
            &today,
            Some(std::path::Path::new("/nonexistent/path")),
        );
        assert_eq!(stats.tokens_in, 0);
    }

    #[test]
    fn update_global_stats_includes_nested_subagent_files() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Direct JSONL under project dir
        let project = tmp.path().join("proj-x");
        std::fs::create_dir_all(&project).unwrap();
        let mut f = std::fs::File::create(project.join("main.jsonl")).unwrap();
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:00:00.000Z","message":{{"usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"main"}}]}}}}"#,
            today = today
        ).unwrap();
        drop(f);

        // Nested subagent JSONL (simulating <project>/<uuid>/subagents/<agent>.jsonl)
        let subagents = project.join("some-uuid").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let mut f = std::fs::File::create(subagents.join("agent-1.jsonl")).unwrap();
        writeln!(f,
            r#"{{"type":"assistant","timestamp":"{today}T10:01:00.000Z","message":{{"usage":{{"input_tokens":200,"output_tokens":80,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}},"content":[{{"type":"text","text":"subagent"}}]}}}}"#,
            today = today
        ).unwrap();
        drop(f);

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));

        assert_eq!(
            stats.tokens_in, 300,
            "should include both direct and subagent entries"
        );
        assert_eq!(stats.tokens_out, 130);
    }

    #[test]
    fn update_global_stats_parses_codex_token_count_incrementally() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let codex_dir = tmp
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("02")
            .join("20");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let log = codex_dir.join("rollout.jsonl");

        let mut f = std::fs::File::create(&log).unwrap();
        // Baseline from an older date.
        writeln!(
            f,
            r#"{{"timestamp":"2020-01-01T23:59:59Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":90,"cached_input_tokens":0,"output_tokens":10,"total_tokens":100}}}}}}}}"#
        )
        .unwrap();
        // First token_count for today.
        writeln!(
            f,
            r#"{{"timestamp":"{today}T10:00:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":150,"cached_input_tokens":20,"output_tokens":10,"total_tokens":160}}}}}}}}"#
        )
        .unwrap();
        // Duplicate snapshot (should be ignored by delta logic).
        writeln!(
            f,
            r#"{{"timestamp":"{today}T10:00:01Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":150,"cached_input_tokens":20,"output_tokens":10,"total_tokens":160}}}}}}}}"#
        )
        .unwrap();
        // Second token_count for today.
        writeln!(
            f,
            r#"{{"timestamp":"{today}T10:01:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":230,"cached_input_tokens":40,"output_tokens":30,"total_tokens":260}}}}}}}}"#
        )
        .unwrap();
        drop(f);

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));

        assert_eq!(stats.codex_tokens_in, 140);
        assert_eq!(stats.codex_tokens_out, 20);
        assert_eq!(stats.codex_tokens_cache_read, 40);
        assert_eq!(
            stats.tokens_in, 140,
            "aggregate totals should include codex"
        );
        assert_eq!(
            stats.tokens_out, 20,
            "aggregate totals should include codex"
        );
        assert_eq!(
            stats.tokens_cache_read, 40,
            "aggregate totals should include codex"
        );

        // Append one more today snapshot and verify incremental accumulation.
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(
            f,
            r#"{{"timestamp":"{today}T10:02:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":260,"cached_input_tokens":50,"output_tokens":40,"total_tokens":300}}}}}}}}"#
        )
        .unwrap();
        drop(f);

        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));
        assert_eq!(stats.codex_tokens_in, 170);
        assert_eq!(stats.codex_tokens_out, 30);
        assert_eq!(stats.codex_tokens_cache_read, 50);
    }

    // ── update_session_stats_from_path: assistant tool_use counting ──

    #[test]
    fn stats_assistant_tool_use_edit_and_bash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = concat!(
            r#"{"type":"assistant","timestamp":"2025-01-01T00:00:00Z","message":{"usage":{"input_tokens":100,"output_tokens":50},"content":[{"type":"tool_use","name":"Edit"},{"type":"tool_use","name":"Bash"},{"type":"tool_use","name":"Write"},{"type":"tool_use","name":"Read"}]}}"#,
            "\n",
        );
        std::fs::write(&path, content).unwrap();
        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.edits, 2, "Edit + Write = 2 edits");
        assert_eq!(stats.bash_cmds, 1, "1 Bash command");
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.tokens_in, 100);
        assert_eq!(stats.tokens_out, 50);
    }

    #[test]
    fn stats_assistant_cache_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = r#"{"type":"assistant","timestamp":"2025-01-01T00:00:00Z","message":{"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":300},"content":[]}}"#;
        std::fs::write(&path, format!("{content}\n")).unwrap();
        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.tokens_cache_read, 200);
        assert_eq!(stats.tokens_cache_write, 300);
    }

    // ── update_session_stats_from_path: incremental reading ──

    #[test]
    fn stats_incremental_reads_only_new_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        // Write first batch
        let line1 = r#"{"type":"assistant","timestamp":"2025-01-01T00:00:00Z","message":{"usage":{"input_tokens":100,"output_tokens":50},"content":[]}}"#;
        std::fs::write(&path, format!("{line1}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.tokens_in, 100);
        assert_eq!(stats.turns, 1);
        let offset_after_first = stats.read_offset;
        assert!(offset_after_first > 0);

        // Append second batch
        let line2 = r#"{"type":"assistant","timestamp":"2025-01-01T00:01:00Z","message":{"usage":{"input_tokens":200,"output_tokens":100},"content":[]}}"#;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{line2}").unwrap();

        // Second read should only parse the new bytes
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.tokens_in, 300, "should accumulate: 100 + 200");
        assert_eq!(stats.turns, 2);
        assert!(stats.read_offset > offset_after_first);
    }

    #[test]
    fn stats_incremental_no_reread_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let line = r#"{"type":"assistant","timestamp":"2025-01-01T00:00:00Z","message":{"usage":{"input_tokens":100,"output_tokens":50},"content":[]}}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1);

        // Second call with same file — should be a noop
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1, "should not re-parse unchanged file");
    }

    #[test]
    fn stats_seek_error_returns_early() {
        // A non-existent path should simply return without error
        let path = std::path::Path::new("/nonexistent/file.jsonl");
        let mut stats = SessionStats::default();
        update_session_stats_from_path(path, &mut stats);
        assert_eq!(stats.turns, 0);
    }

    // ── update_session_stats_from_path: tool results with filenames ──

    #[test]
    fn stats_tool_result_filenames_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = concat!(
            r#"{"toolUseResult":{"filenames":["src/main.rs","src/lib.rs"]}}"#,
            "\n",
            r#"{"toolUseResult":{"filenames":["src/main.rs","src/app.rs"]}}"#,
            "\n",
        );
        std::fs::write(&path, content).unwrap();
        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.files.len(), 3, "3 unique files");
        assert!(stats.recent_files.contains(&"src/main.rs".to_string()));
        assert!(stats.recent_files.contains(&"src/lib.rs".to_string()));
        assert!(stats.recent_files.contains(&"src/app.rs".to_string()));
    }

    // ── update_global_stats_inner with real temp files ──

    #[test]
    fn global_stats_inner_reads_jsonl_files() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Create structure: dir/subproject/session.jsonl
        let subdir = dir.path().join("my-project");
        std::fs::create_dir_all(&subdir).unwrap();
        let jsonl_path = subdir.join("session.jsonl");
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{today}T12:00:00Z","message":{{"usage":{{"input_tokens":500,"output_tokens":250,"cache_read_input_tokens":100,"cache_creation_input_tokens":50}},"content":[]}}}}"#,
        );
        std::fs::write(&jsonl_path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));

        assert_eq!(stats.tokens_in, 500);
        assert_eq!(stats.tokens_out, 250);
        assert_eq!(stats.tokens_cache_read, 100);
        assert_eq!(stats.tokens_cache_write, 50);
    }

    #[test]
    fn global_stats_inner_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        let jsonl_path = subdir.join("s.jsonl");
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{today}T12:00:00Z","message":{{"usage":{{"input_tokens":100,"output_tokens":50}},"content":[]}}}}"#,
        );
        std::fs::write(&jsonl_path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 100);

        // Append more data
        let line2 = format!(
            r#"{{"type":"assistant","timestamp":"{today}T13:00:00Z","message":{{"usage":{{"input_tokens":200,"output_tokens":100}},"content":[]}}}}"#,
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{line2}").unwrap();

        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 300, "should accumulate incrementally");
    }

    #[test]
    fn global_stats_inner_skips_non_jsonl_files() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        // Write a .txt file (should be ignored)
        std::fs::write(subdir.join("notes.txt"), "not a jsonl").unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 0, "should skip non-jsonl files");
    }

    #[test]
    fn global_stats_inner_skips_lines_without_today() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        let line = r#"{"type":"assistant","timestamp":"1999-01-01T12:00:00Z","message":{"usage":{"input_tokens":100,"output_tokens":50},"content":[]}}"#;
        std::fs::write(subdir.join("old.jsonl"), format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 0, "should skip lines from other dates");
    }

    #[test]
    fn collect_jsonl_files_respects_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        // Create a directory tree 6 levels deep (limit is 4)
        let mut deep = dir.path().to_path_buf();
        for i in 0..6 {
            deep = deep.join(format!("level{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        // Place a jsonl file at depth 6 — should be ignored
        std::fs::write(deep.join("deep.jsonl"), "ignored\n").unwrap();
        // Also place one at depth 2 — should be found
        let shallow = dir.path().join("level0").join("level1");
        std::fs::write(shallow.join("shallow.jsonl"), "found\n").unwrap();

        let mut files = Vec::new();
        collect_jsonl_files(dir.path(), &mut files, 0);
        assert_eq!(files.len(), 1, "only shallow file should be collected");
        assert!(files[0].ends_with("shallow.jsonl"));
    }

    #[test]
    fn global_stats_inner_none_base_dir_without_home_is_noop() {
        let _guard = HomeGuard::remove();

        let mut stats = GlobalStats::default();
        let today = "2026-01-01";
        update_global_stats_inner(&mut stats, today, None);
        assert_eq!(stats.tokens_in, 0, "should be noop when HOME is unset");
    }

    #[test]
    fn update_session_stats_user_message_without_type_field_skipped() {
        // A line that matches the user fast-path heuristic but has wrong JSON structure
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // Has "user" and "timestamp" but type is not "user"
        let line = r#"{"type":"system","role":"user","timestamp":"2026-01-15T10:00:00.000Z"}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert!(
            stats.last_user_ts.is_none(),
            "should not set last_user_ts for non-user type"
        );
    }

    #[test]
    fn update_session_stats_assistant_message_without_usage_key_skipped() {
        // A line that matches assistant fast-path heuristic but has no usage
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // Contains "assistant" and "usage" in text, but type is "result"
        let line = r#"{"type":"result","message":"assistant usage info"}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 0);
    }

    #[test]
    fn parse_uuid_from_lsof_output_valid() {
        let output = "node  12345 user  txt  REG  /home/user/.claude/tasks/a1b2c3d4-e5f6-7890-abcd-ef1234567890/file.jsonl\n";
        let result = parse_uuid_from_lsof_output(output);
        assert_eq!(
            result.as_deref(),
            Some("a1b2c3d4-e5f6-7890-abcd-ef1234567890")
        );
    }

    #[test]
    fn parse_uuid_from_lsof_output_no_match() {
        let output = "node  12345 user  txt  REG  /home/user/.config/something\n";
        let result = parse_uuid_from_lsof_output(output);
        assert!(result.is_none());
    }

    #[test]
    fn parse_uuid_from_lsof_output_short_rest() {
        // The path after .claude/tasks/ is shorter than 36 chars
        let output = "node  12345 user  txt  REG  /home/.claude/tasks/short/file\n";
        let result = parse_uuid_from_lsof_output(output);
        assert!(result.is_none());
    }

    #[test]
    fn parse_uuid_from_lsof_output_invalid_uuid() {
        // 36 chars but not a valid UUID format
        let output = "node  12345 user  txt  REG  /home/.claude/tasks/not-a-valid-uuid-at-all-really-nope/file\n";
        let result = parse_uuid_from_lsof_output(output);
        assert!(result.is_none());
    }

    #[test]
    fn update_global_stats_outer_covers_today_and_delegates() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        // Create a projects dir with a jsonl file for today
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let projects_dir = dir.path().join(".claude").join("projects").join("proj");
        std::fs::create_dir_all(&projects_dir).unwrap();
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{today}T12:00:00Z","message":{{"usage":{{"input_tokens":100,"output_tokens":50}},"content":[]}}}}"#,
        );
        std::fs::write(projects_dir.join("s.jsonl"), format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        update_global_stats(&mut stats);

        assert_eq!(
            stats.tokens_in, 100,
            "should read tokens from HOME-based path"
        );
        assert_eq!(stats.date, today, "should set today's date");
    }

    #[test]
    fn update_global_stats_outer_resets_on_date_change() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let mut stats = GlobalStats::default();
        stats.date = "1999-01-01".to_string(); // old date
        stats.tokens_in = 500;
        stats.tokens_out = 200;
        stats.tokens_cache_read = 100;
        stats.tokens_cache_write = 50;

        update_global_stats(&mut stats);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(stats.date, today, "date should be updated to today");
        assert_eq!(
            stats.tokens_in, 0,
            "tokens_in should be reset on date change"
        );
        assert_eq!(
            stats.tokens_out, 0,
            "tokens_out should be reset on date change"
        );
    }

    #[test]
    fn global_stats_inner_false_positive_assistant_line_skipped() {
        // Line passes ALL quick filters (contains today's date, "assistant" as
        // a JSON key, and "usage") but the top-level type is NOT "assistant",
        // so it must be rejected at the JSON type check (line 383-384).
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        // "assistant" appears as a key name (passes contains check),
        // "usage" appears as a key, and today's date is in the timestamp.
        let line = format!(
            r#"{{"type":"system","assistant":"yes","usage":"yes","timestamp":"{today}","message":{{"usage":{{"input_tokens":999}}}}}}"#,
        );
        std::fs::write(subdir.join("s.jsonl"), format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(
            stats.tokens_in, 0,
            "should skip lines where type != assistant"
        );
    }

    // ── read_last_assistant_message via temp files ──

    #[test]
    fn read_last_assistant_message_returns_text() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let cwd = "/test/project";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let escaped = escape_project_path(cwd);
        let jsonl_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&jsonl_dir).unwrap();

        let jsonl_path = jsonl_dir.join(format!("{uuid}.jsonl"));
        let content = concat!(
            r#"{"type":"user","message":{"content":[{"text":"hello"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"text":"Hi there!"},{"text":"How can I help?"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"text":"bye"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"text":"Goodbye!"}]}}"#,
            "\n",
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let result = read_last_assistant_message(cwd, uuid);
        assert_eq!(result, Some("Goodbye!".to_string()));
    }

    #[test]
    fn read_last_assistant_message_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let result =
            read_last_assistant_message("/nonexistent", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(result, None);
    }

    #[test]
    fn read_last_assistant_message_multi_part_content() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let cwd = "/test/multi";
        let uuid = "11111111-2222-3333-4444-555555555555";
        let escaped = escape_project_path(cwd);
        let jsonl_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&jsonl_dir).unwrap();

        let jsonl_path = jsonl_dir.join(format!("{uuid}.jsonl"));
        let content = r#"{"type":"assistant","message":{"content":[{"text":"Part one."},{"text":"Part two."}]}}"#;
        std::fs::write(&jsonl_path, format!("{content}\n")).unwrap();

        let result = read_last_assistant_message(cwd, uuid);
        assert_eq!(result, Some("Part one. Part two.".to_string()));
    }

    // ── parse_session_id_from_cmdline ──

    #[test]
    fn parse_session_id_space_form() {
        let cmdline = "node claude --session-id aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee --other";
        let result = parse_session_id_from_cmdline(cmdline);
        assert_eq!(
            result,
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string())
        );
    }

    #[test]
    fn parse_session_id_equals_form() {
        let cmdline = "node claude --session-id=aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee --other";
        let result = parse_session_id_from_cmdline(cmdline);
        assert_eq!(
            result,
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string())
        );
    }

    #[test]
    fn parse_session_id_not_present() {
        let cmdline = "node claude --no-session-id";
        let result = parse_session_id_from_cmdline(cmdline);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_session_id_invalid_uuid_value() {
        let cmdline = "node claude --session-id not-a-uuid";
        let result = parse_session_id_from_cmdline(cmdline);
        assert_eq!(result, None);
    }

    #[test]
    fn parse_session_id_missing_value() {
        let cmdline = "node claude --session-id";
        let result = parse_session_id_from_cmdline(cmdline);
        assert_eq!(result, None);
    }

    // ── is_uuid ──

    #[test]
    fn is_uuid_correct_format() {
        assert!(is_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert!(is_uuid("12345678-1234-1234-1234-123456789abc"));
    }

    #[test]
    fn is_uuid_rejects_bad_format() {
        assert!(!is_uuid("too-short"));
        assert!(!is_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeee")); // 35 chars
        assert!(!is_uuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeeee")); // 37 chars
        assert!(!is_uuid("gaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")); // non-hex
        assert!(!is_uuid("aaaaaaaaabbbb-cccc-dddd-eeeeeeeeeeee")); // missing dash at pos 8
    }

    // ── parse_uuid_from_lsof_output ──

    #[test]
    fn parse_lsof_output_valid() {
        let output = "node    1234  user  txt  REG  /Users/me/.claude/tasks/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/foo.json\n";
        assert_eq!(
            parse_uuid_from_lsof_output(output),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string())
        );
    }

    #[test]
    fn parse_lsof_output_no_match() {
        let output = "node    1234  user  txt  REG  /Users/me/.claude/other/file.json\n";
        assert_eq!(parse_uuid_from_lsof_output(output), None);
    }

    // ── update_global_stats_inner: no-new-bytes (incremental skip) ──

    #[test]
    fn global_stats_inner_no_new_bytes_skips_reread() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{today}T10:00:00Z","message":{{"usage":{{"input_tokens":50,"output_tokens":10}},"content":[]}}}}"#,
        );
        std::fs::write(subdir.join("log.jsonl"), format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 50);

        // Second call without changes — should hit file_len <= offset path
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 50, "should not re-count on unchanged file");
    }

    // ── update_global_stats_inner: short lines skipped ──

    #[test]
    fn global_stats_inner_short_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        // Short lines (<10 chars) should be skipped
        std::fs::write(subdir.join("log.jsonl"), "short\n{}\n\n").unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 0, "short lines should not parse");
    }

    // ── update_global_stats_inner: file open error ──

    #[test]
    fn global_stats_inner_unreadable_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Create a subdir with a symlink to a nonexistent file
        let subdir = dir.path().join("proj");
        std::fs::create_dir_all(&subdir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/nonexistent/path", subdir.join("bad.jsonl")).unwrap();
        }

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        // Should not panic — the broken symlink triggers Err on File::open
        update_global_stats_inner(&mut stats, &today, Some(dir.path()));
        assert_eq!(stats.tokens_in, 0);
    }

    // ── update_session_stats: unknown tool name hits default arm ──

    #[test]
    fn update_session_stats_unknown_tool_name_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // tool_use with an unrecognized tool name
        let line = r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"tool_use","name":"UnknownTool","input":{}}]}}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1, "should still count the turn");
        assert_eq!(stats.edits, 0, "unknown tool should not count as edit");
        assert_eq!(stats.bash_cmds, 0, "unknown tool should not count as bash");
    }

    // ── read_last_assistant_message: non-assistant lines only ──

    #[test]
    fn read_last_assistant_message_no_assistant_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let cwd = "/test/noassist";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let escaped = escape_project_path(cwd);
        let jsonl_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&jsonl_dir).unwrap();

        // Only user messages, no assistant messages
        let content = concat!(
            r#"{"type":"user","message":{"content":[{"text":"hello"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"text":"bye"}]}}"#,
            "\n",
        );
        std::fs::write(jsonl_dir.join(format!("{uuid}.jsonl")), content).unwrap();

        let result = read_last_assistant_message(cwd, uuid);
        assert_eq!(result, None, "no assistant messages should return None");
    }

    // ── read_last_assistant_message: malformed JSON line ──

    #[test]
    fn read_last_assistant_message_malformed_json_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let cwd = "/test/malformed";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let escaped = escape_project_path(cwd);
        let jsonl_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&jsonl_dir).unwrap();

        // Malformed line with "assistant" keyword, followed by a valid assistant message
        let content = concat!(
            r#"{"type":"assistant" BROKEN JSON"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"text":"Valid msg"}]}}"#,
            "\n",
        );
        std::fs::write(jsonl_dir.join(format!("{uuid}.jsonl")), content).unwrap();

        let result = read_last_assistant_message(cwd, uuid);
        assert_eq!(
            result,
            Some("Valid msg".to_string()),
            "should skip malformed and return valid"
        );
    }

    // ── read_last_assistant_message: false positive (type != assistant) ──

    #[test]
    fn read_last_assistant_message_non_assistant_type_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let cwd = "/test/falsepos";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let escaped = escape_project_path(cwd);
        let jsonl_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&jsonl_dir).unwrap();

        // Line passes the quick filter (contains "assistant" as a JSON key)
        // but has type != "assistant", hitting the continue at the type check
        let content = concat!(
            r#"{"type":"system","assistant":"yes","message":{"content":[{"text":"should not appear"}]}}"#,
            "\n",
        );
        std::fs::write(jsonl_dir.join(format!("{uuid}.jsonl")), content).unwrap();

        let result = read_last_assistant_message(cwd, uuid);
        assert_eq!(result, None, "non-assistant type should be skipped");
    }

    // ── read_last_assistant_message: content with no text items ──

    #[test]
    fn read_last_assistant_message_no_text_content_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());

        let cwd = "/test/notext";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let escaped = escape_project_path(cwd);
        let jsonl_dir = dir.path().join(".claude").join("projects").join(&escaped);
        std::fs::create_dir_all(&jsonl_dir).unwrap();

        // Assistant message with only tool_use, no text
        let content = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        std::fs::write(
            jsonl_dir.join(format!("{uuid}.jsonl")),
            format!("{content}\n"),
        )
        .unwrap();

        let result = read_last_assistant_message(cwd, uuid);
        assert_eq!(
            result, None,
            "assistant with no text content should return None"
        );
    }

    // ── update_session_stats: incremental seek path ──

    #[test]
    fn update_session_stats_from_path_incremental_seek() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");

        // Write initial content
        let line1 = r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":5},"content":[]}}"#;
        std::fs::write(&path, format!("{line1}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 1);

        // Append more and read again — exercises the seek path (offset > 0)
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let line2 = r#"{"type":"assistant","message":{"usage":{"input_tokens":20,"output_tokens":10},"content":[]}}"#;
        writeln!(file, "{line2}").unwrap();

        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.turns, 2);
        assert_eq!(stats.tokens_in, 30);
    }

    // ── update_session_stats: tool_use with multiple tool types including unknown ──

    #[test]
    fn update_session_stats_mixed_tool_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let line = r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"tool_use","name":"Write","input":{}},{"type":"tool_use","name":"Bash","input":{}},{"type":"tool_use","name":"Read","input":{}},{"type":"tool_use","name":"Edit","input":{}}]}}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.edits, 2, "Write + Edit = 2 edits");
        assert_eq!(stats.bash_cmds, 1, "1 Bash command");
        // Read and other tools don't increment any counter
    }

    // ── queue-operation subagent tracking ──

    #[test]
    fn stats_queue_operation_enqueue_increments_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let lines = [
            r#"{"type":"queue-operation","operation":"enqueue","taskId":"a"}"#,
            r#"{"type":"queue-operation","operation":"enqueue","taskId":"b"}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.active_subagents, 2);
    }

    #[test]
    fn stats_queue_operation_remove_decrements_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let lines = [
            r#"{"type":"queue-operation","operation":"enqueue","taskId":"a"}"#,
            r#"{"type":"queue-operation","operation":"enqueue","taskId":"b"}"#,
            r#"{"type":"queue-operation","operation":"remove","taskId":"a"}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.active_subagents, 1);
    }

    #[test]
    fn stats_queue_operation_remove_saturates_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let lines = [
            r#"{"type":"queue-operation","operation":"remove","taskId":"x"}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let mut stats = SessionStats::default();
        update_session_stats_from_path(&path, &mut stats);
        assert_eq!(stats.active_subagents, 0);
    }

    // ── escape_project_path ──

    #[test]
    fn escape_project_path_replaces_slashes() {
        assert_eq!(
            escape_project_path("/Users/me/project"),
            "-Users-me-project"
        );
        assert_eq!(escape_project_path("no-slashes"), "no-slashes");
    }

    // ── extract_assistant_message_text edge cases ──

    #[test]
    fn extract_assistant_message_text_no_content_field() {
        let v = serde_json::json!({"message": {"role": "assistant"}});
        assert_eq!(extract_assistant_message_text(&v), None);
    }

    #[test]
    fn extract_assistant_message_text_no_text_items() {
        let v = serde_json::json!({
            "message": {
                "content": [
                    {"type": "tool_use", "name": "Bash", "input": {}}
                ]
            }
        });
        assert_eq!(extract_assistant_message_text(&v), None);
    }

    #[test]
    fn extract_assistant_message_text_multiple_parts() {
        let v = serde_json::json!({
            "message": {
                "content": [
                    {"type": "text", "text": "Hello"},
                    {"type": "tool_use", "name": "Bash", "input": {}},
                    {"type": "text", "text": "World"}
                ]
            }
        });
        assert_eq!(extract_assistant_message_text(&v), Some("Hello World".to_string()));
    }

    #[test]
    fn extract_assistant_message_text_empty_content_array() {
        let v = serde_json::json!({"message": {"content": []}});
        assert_eq!(extract_assistant_message_text(&v), None);
    }

    #[test]
    fn extract_assistant_message_text_no_message_field() {
        let v = serde_json::json!({"type": "assistant"});
        assert_eq!(extract_assistant_message_text(&v), None);
    }

    // ── add_claude_usage / add_codex_usage direct tests ──

    #[test]
    fn add_claude_usage_accumulates() {
        let mut stats = GlobalStats::default();
        add_claude_usage(&mut stats, 100, 50, 20, 10);
        add_claude_usage(&mut stats, 200, 100, 30, 20);
        assert_eq!(stats.tokens_in, 300);
        assert_eq!(stats.tokens_out, 150);
        assert_eq!(stats.tokens_cache_read, 50);
        assert_eq!(stats.tokens_cache_write, 30);
        assert_eq!(stats.claude_tokens_in, 300);
        assert_eq!(stats.claude_tokens_out, 150);
        assert_eq!(stats.claude_tokens_cache_read, 50);
        assert_eq!(stats.claude_tokens_cache_write, 30);
    }

    #[test]
    fn add_codex_usage_accumulates() {
        let mut stats = GlobalStats::default();
        add_codex_usage(&mut stats, 100, 50, 20);
        add_codex_usage(&mut stats, 200, 100, 30);
        assert_eq!(stats.tokens_in, 300);
        assert_eq!(stats.tokens_out, 150);
        assert_eq!(stats.tokens_cache_read, 50);
        assert_eq!(stats.codex_tokens_in, 300);
        assert_eq!(stats.codex_tokens_out, 150);
        assert_eq!(stats.codex_tokens_cache_read, 50);
    }

    #[test]
    fn add_mixed_usage_separates_providers() {
        let mut stats = GlobalStats::default();
        add_claude_usage(&mut stats, 100, 50, 20, 10);
        add_codex_usage(&mut stats, 200, 100, 30);
        // Combined totals
        assert_eq!(stats.tokens_in, 300);
        assert_eq!(stats.tokens_out, 150);
        // Provider-specific
        assert_eq!(stats.claude_tokens_in, 100);
        assert_eq!(stats.codex_tokens_in, 200);
    }

    // ── process_claude_global_file tests ──

    #[test]
    fn process_claude_global_file_incremental_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Write one line
        let line1 = format!(
            r#"{{"type":"assistant","timestamp":"{today}T10:00:00Z","message":{{"usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":10,"cache_creation_input_tokens":5}},"content":[]}}}}"#
        );
        std::fs::write(&path, format!("{line1}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        let pb = std::path::PathBuf::from(&path);
        process_claude_global_file(&pb, &mut stats, &today);
        assert_eq!(stats.tokens_in, 100);
        assert_eq!(stats.tokens_out, 50);
        let offset1 = stats.file_offsets[&pb];

        // Append a second line
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let line2 = format!(
            r#"{{"type":"assistant","timestamp":"{today}T11:00:00Z","message":{{"usage":{{"input_tokens":200,"output_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}},"content":[]}}}}"#
        );
        writeln!(file, "{line2}").unwrap();

        // Second call reads only new bytes
        process_claude_global_file(&pb, &mut stats, &today);
        assert_eq!(stats.tokens_in, 300); // 100 + 200
        assert_eq!(stats.tokens_out, 150); // 50 + 100
        assert!(stats.file_offsets[&pb] > offset1);
    }

    #[test]
    fn process_claude_global_file_skips_non_assistant_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let lines = format!(
            r#"{{"type":"human","timestamp":"{today}T10:00:00Z","message":{{"usage":{{"input_tokens":500,"output_tokens":500}},"content":[]}}}}"#
        );
        std::fs::write(&path, format!("{lines}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_claude_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.tokens_in, 0, "non-assistant lines should be skipped");
    }

    #[test]
    fn process_claude_global_file_skips_other_dates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Line from yesterday — doesn't contain today's date
        let line = r#"{"type":"assistant","timestamp":"1999-01-01T10:00:00Z","message":{"usage":{"input_tokens":500,"output_tokens":500},"content":[]}}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_claude_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.tokens_in, 0, "other dates should be skipped");
    }

    // ── process_codex_global_file tests ──

    #[test]
    fn process_codex_global_file_basic_token_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let line = format!(
            r#"{{"type":"event_msg","timestamp":"{today}T10:00:00Z","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":50,"cached_input_tokens":10,"total_tokens":150}}}}}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.codex_tokens_in, 100);
        assert_eq!(stats.codex_tokens_out, 50);
        assert_eq!(stats.codex_tokens_cache_read, 10);
    }

    #[test]
    fn process_codex_global_file_non_event_msg_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let line = format!(
            r#"{{"type":"other_type","timestamp":"{today}T10:00:00Z","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.codex_tokens_in, 0, "non-event_msg should be skipped");
    }

    #[test]
    fn process_codex_global_file_missing_payload_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Has token_count and total_token_usage in text to pass quick filter, but no payload field
        let line = format!(
            r#"{{"type":"event_msg","timestamp":"{today}T10:00:00Z","data":"token_count total_token_usage"}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.codex_tokens_in, 0);
    }

    #[test]
    fn process_codex_global_file_non_token_count_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // payload type is not "token_count" (but text contains both quick-filter strings)
        let line = format!(
            r#"{{"type":"event_msg","timestamp":"{today}T10:00:00Z","payload":{{"type":"other","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":50,"total_tokens":150}}}}}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.codex_tokens_in, 0, "non-token_count payload should be skipped");
    }

    #[test]
    fn process_codex_global_file_not_today_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // First line with yesterday's timestamp — should be tracked for delta but not added to today's tokens
        let line = format!(
            r#"{{"type":"event_msg","timestamp":"1999-01-01T10:00:00Z","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":50,"cached_input_tokens":0,"total_tokens":150}}}}}}}}"#
        );
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = today.clone();
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(stats.codex_tokens_in, 0, "yesterday's tokens should not count for today");
    }

    // ── update_global_stats date change ──

    #[test]
    fn update_global_stats_inner_resets_on_date_change() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let mut stats = GlobalStats::default();
        stats.date = "1999-01-01".to_string();
        stats.tokens_in = 999;
        stats.tokens_out = 999;
        stats.claude_tokens_in = 500;
        stats.codex_tokens_in = 499;
        stats.codex_file_states.insert(
            std::path::PathBuf::from("/old/file"),
            CodexFileState {
                read_offset: 100,
                last_total_tokens: 50,
                last_input_tokens: 30,
                last_output_tokens: 20,
                last_cached_input_tokens: 0,
            },
        );
        stats.file_offsets.insert(std::path::PathBuf::from("/old/claude"), 200);

        // Call with the real today — should reset everything
        update_global_stats(&mut stats);

        assert_eq!(stats.date, today);
        assert_eq!(stats.tokens_in, 0);
        assert_eq!(stats.tokens_out, 0);
        assert_eq!(stats.claude_tokens_in, 0);
        assert_eq!(stats.codex_tokens_in, 0);
        assert!(stats.codex_file_states.is_empty());
        assert!(stats.file_offsets.is_empty());
    }

    // ── proptest ──────────────────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn extract_assistant_message_text_never_panics(json_str in ".*") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    let _ = extract_assistant_message_text(&v);
                }
            }

            #[test]
            fn extract_with_valid_structure(text in "[a-zA-Z0-9 .!?]{1,100}") {
                let v: serde_json::Value = serde_json::json!({
                    "message": {
                        "content": [{"text": text}]
                    }
                });
                let result = extract_assistant_message_text(&v);
                prop_assert_eq!(result, Some(text));
            }

            #[test]
            fn extract_with_multiple_blocks(
                text1 in "[a-zA-Z0-9]{1,30}",
                text2 in "[a-zA-Z0-9]{1,30}"
            ) {
                let v: serde_json::Value = serde_json::json!({
                    "message": {
                        "content": [{"text": &text1}, {"text": &text2}]
                    }
                });
                let result = extract_assistant_message_text(&v);
                prop_assert_eq!(result, Some(format!("{text1} {text2}")));
            }

            #[test]
            fn escape_project_path_replaces_all_slashes(path in "/[a-zA-Z0-9/]{1,50}") {
                let escaped = escape_project_path(&path);
                prop_assert!(!escaped.contains('/'));
            }

            #[test]
            fn stats_from_valid_jsonl_never_panics(
                tokens_in in 0u64..1_000_000,
                tokens_out in 0u64..1_000_000
            ) {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("test.jsonl");
                let line = serde_json::json!({
                    "type": "assistant",
                    "timestamp": "2025-01-01T00:00:00Z",
                    "message": {
                        "content": [{"text": "hello"}],
                        "usage": {
                            "input_tokens": tokens_in,
                            "output_tokens": tokens_out
                        }
                    }
                });
                std::fs::write(&path, format!("{}\n", line)).unwrap();
                let mut stats = SessionStats::default();
                let _ = update_session_stats_from_path_and_last_message(&path, &mut stats);
                prop_assert_eq!(stats.tokens_in, tokens_in);
                prop_assert_eq!(stats.tokens_out, tokens_out);
                prop_assert_eq!(stats.turns, 1);
            }

            #[test]
            fn stats_incremental_reads_accumulate(n in 1u32..10) {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("test.jsonl");
                let mut content = String::new();
                for i in 0..n {
                    let line = serde_json::json!({
                        "type": "assistant",
                        "timestamp": format!("2025-01-01T00:00:{:02}Z", i),
                        "message": {
                            "content": [{"text": "msg"}],
                            "usage": {
                                "input_tokens": 100,
                                "output_tokens": 50
                            }
                        }
                    });
                    content.push_str(&format!("{}\n", line));
                }
                std::fs::write(&path, &content).unwrap();
                let mut stats = SessionStats::default();
                let _ = update_session_stats_from_path_and_last_message(&path, &mut stats);
                prop_assert_eq!(stats.turns, n);
                prop_assert_eq!(stats.tokens_in, 100 * n as u64);
                prop_assert_eq!(stats.tokens_out, 50 * n as u64);
            }
        }
    }
}
