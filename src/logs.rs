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
        let input = self.tokens_in as f64 * CLAUDE_INPUT_USD_PER_MTOK / 1_000_000.0;
        let output = self.tokens_out as f64 * CLAUDE_OUTPUT_USD_PER_MTOK / 1_000_000.0;
        let cache_read =
            self.tokens_cache_read as f64 * CLAUDE_CACHE_READ_USD_PER_MTOK / 1_000_000.0;
        let cache_write =
            self.tokens_cache_write as f64 * CLAUDE_CACHE_WRITE_USD_PER_MTOK / 1_000_000.0;
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
#[cfg(test)]
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
#[cfg(test)]
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
    if stats.read_offset > 0 && file.seek(SeekFrom::Start(stats.read_offset)).is_err() {
        return None;
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

// Claude Sonnet token pricing (USD per million tokens).
// Update these when Anthropic changes pricing.
const CLAUDE_INPUT_USD_PER_MTOK: f64 = 3.0;
const CLAUDE_OUTPUT_USD_PER_MTOK: f64 = 15.0;
const CLAUDE_CACHE_READ_USD_PER_MTOK: f64 = 0.30;
const CLAUDE_CACHE_WRITE_USD_PER_MTOK: f64 = 3.75;

// Uses OpenAI's published GPT-5 Codex token pricing as an estimate.
// Update these when OpenAI changes pricing.
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
#[derive(Debug, Clone, Default)]
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
    pub gemini_tokens_in: u64,
    pub gemini_tokens_out: u64,
    pub gemini_tokens_cached: u64,
    /// Per-file read offsets for incremental Claude log reading.
    file_offsets: HashMap<PathBuf, u64>,
    /// Per-file incremental state for Codex token_count parsing.
    codex_file_states: HashMap<PathBuf, CodexFileState>,
    /// Per-file sizes for Gemini session change detection.
    gemini_file_sizes: HashMap<PathBuf, u64>,
    /// Per-file token totals for Gemini (to compute deltas on re-parse).
    gemini_file_tokens: HashMap<PathBuf, (u64, u64, u64)>,
    /// Cached file list to avoid recursive scans on every refresh.
    known_claude_files: Vec<PathBuf>,
    /// Cached file list to avoid recursive scans on every refresh.
    known_codex_files: Vec<PathBuf>,
    /// Cached file list for Gemini session JSON files.
    known_gemini_files: Vec<PathBuf>,
    /// Unix timestamp of last recursive file discovery.
    last_file_discovery_ts: i64,
    /// Date string (YYYY-MM-DD) these stats are for; reset when date changes.
    date: String,
}

impl GlobalStats {
    fn has_provider_breakdown(&self) -> bool {
        self.claude_tokens_in > 0
            || self.claude_tokens_out > 0
            || self.claude_tokens_cache_read > 0
            || self.claude_tokens_cache_write > 0
            || self.codex_tokens_in > 0
            || self.codex_tokens_out > 0
            || self.codex_tokens_cache_read > 0
            || self.gemini_tokens_in > 0
            || self.gemini_tokens_out > 0
            || self.gemini_tokens_cached > 0
    }

    pub fn has_usage(&self) -> bool {
        if self.has_provider_breakdown() {
            self.claude_tokens_in > 0
                || self.claude_tokens_out > 0
                || self.claude_tokens_cache_read > 0
                || self.claude_tokens_cache_write > 0
                || self.codex_tokens_in > 0
                || self.codex_tokens_out > 0
                || self.codex_tokens_cache_read > 0
                || self.gemini_tokens_in > 0
                || self.gemini_tokens_out > 0
                || self.gemini_tokens_cached > 0
        } else {
            self.tokens_in > 0
                || self.tokens_out > 0
                || self.tokens_cache_read > 0
                || self.tokens_cache_write > 0
        }
    }

    pub fn claude_display_tokens(&self) -> u64 {
        if self.has_provider_breakdown() {
            self.claude_tokens_in + self.claude_tokens_out
        } else {
            self.tokens_in + self.tokens_out
        }
    }

    pub fn codex_display_tokens(&self) -> u64 {
        if self.has_provider_breakdown() {
            self.codex_tokens_in + self.codex_tokens_out
        } else {
            0
        }
    }

    pub fn gemini_display_tokens(&self) -> u64 {
        if self.has_provider_breakdown() {
            self.gemini_tokens_in + self.gemini_tokens_out
        } else {
            0
        }
    }

    pub fn claude_cost_usd(&self) -> f64 {
        if !self.has_provider_breakdown() {
            let input = self.tokens_in as f64 * CLAUDE_INPUT_USD_PER_MTOK / 1_000_000.0;
            let output = self.tokens_out as f64 * CLAUDE_OUTPUT_USD_PER_MTOK / 1_000_000.0;
            let cache_read =
                self.tokens_cache_read as f64 * CLAUDE_CACHE_READ_USD_PER_MTOK / 1_000_000.0;
            let cache_write =
                self.tokens_cache_write as f64 * CLAUDE_CACHE_WRITE_USD_PER_MTOK / 1_000_000.0;
            return input + output + cache_read + cache_write;
        }

        let claude_input = self.claude_tokens_in as f64 * CLAUDE_INPUT_USD_PER_MTOK / 1_000_000.0;
        let claude_output =
            self.claude_tokens_out as f64 * CLAUDE_OUTPUT_USD_PER_MTOK / 1_000_000.0;
        let claude_cache_read =
            self.claude_tokens_cache_read as f64 * CLAUDE_CACHE_READ_USD_PER_MTOK / 1_000_000.0;
        let claude_cache_write =
            self.claude_tokens_cache_write as f64 * CLAUDE_CACHE_WRITE_USD_PER_MTOK / 1_000_000.0;

        claude_input + claude_output + claude_cache_read + claude_cache_write
    }

    pub fn codex_cost_usd(&self) -> f64 {
        if !self.has_provider_breakdown() {
            return 0.0;
        }

        let codex_uncached_input_tokens = self
            .codex_tokens_in
            .saturating_sub(self.codex_tokens_cache_read);
        let codex_input =
            codex_uncached_input_tokens as f64 * CODEX_INPUT_USD_PER_MTOK / 1_000_000.0;
        let codex_output = self.codex_tokens_out as f64 * CODEX_OUTPUT_USD_PER_MTOK / 1_000_000.0;
        let codex_cache_read =
            self.codex_tokens_cache_read as f64 * CODEX_CACHE_READ_USD_PER_MTOK / 1_000_000.0;

        codex_input + codex_output + codex_cache_read
    }

    pub fn gemini_cost_usd(&self) -> f64 {
        if !self.has_provider_breakdown() {
            return 0.0;
        }

        let gemini_uncached_input = self
            .gemini_tokens_in
            .saturating_sub(self.gemini_tokens_cached);
        let gemini_input = gemini_uncached_input as f64 * GEMINI_INPUT_USD_PER_MTOK / 1_000_000.0;
        let gemini_output =
            self.gemini_tokens_out as f64 * GEMINI_OUTPUT_USD_PER_MTOK / 1_000_000.0;
        let gemini_cache_read =
            self.gemini_tokens_cached as f64 * GEMINI_CACHE_READ_USD_PER_MTOK / 1_000_000.0;

        gemini_input + gemini_output + gemini_cache_read
    }

    /// Estimated cost in USD using provider-specific pricing.
    /// Claude: Sonnet ($3 in / $15 out / $0.30 cache-read / $3.75 cache-write per MTok).
    /// Codex: GPT-5 Codex estimate ($1.25 in / $10 out / $0.125 cache-read per MTok).
    pub fn cost_usd(&self) -> f64 {
        self.claude_cost_usd() + self.codex_cost_usd() + self.gemini_cost_usd()
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
        stats.gemini_tokens_in = 0;
        stats.gemini_tokens_out = 0;
        stats.gemini_tokens_cached = 0;
        stats.file_offsets.clear();
        stats.codex_file_states.clear();
        stats.gemini_file_sizes.clear();
        stats.gemini_file_tokens.clear();
        stats.known_claude_files.clear();
        stats.known_codex_files.clear();
        stats.known_gemini_files.clear();
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
    let (claude_projects_dir, codex_sessions_dir, gemini_tmp_dir) = match base_dir {
        Some(dir) => (
            dir.to_path_buf(),
            dir.join(".codex").join("sessions"),
            dir.join(".gemini").join("tmp"),
        ),
        None => {
            let home = match std::env::var("HOME") {
                Ok(h) => h,
                Err(_) => return,
            };
            (
                PathBuf::from(&home).join(".claude").join("projects"),
                PathBuf::from(&home).join(".codex").join("sessions"),
                PathBuf::from(&home).join(".gemini").join("tmp"),
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

        let mut gemini_files = Vec::new();
        collect_gemini_session_files(&gemini_tmp_dir, &mut gemini_files);
        stats.known_gemini_files = gemini_files;

        let gemini_file_set: HashSet<PathBuf> = stats.known_gemini_files.iter().cloned().collect();
        stats
            .gemini_file_sizes
            .retain(|p, _| gemini_file_set.contains(p));
        stats
            .gemini_file_tokens
            .retain(|p, _| gemini_file_set.contains(p));

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

    // Process Gemini session files.
    for i in 0..stats.known_gemini_files.len() {
        let path = stats.known_gemini_files[i].clone();
        process_gemini_global_file(&path, stats, today);
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

        if total_tokens > last_total_tokens && is_today {
            let delta_input = total_input_tokens.saturating_sub(last_input_tokens);
            let delta_output = total_output_tokens.saturating_sub(last_output_tokens);
            let delta_cache_read =
                total_cached_input_tokens.saturating_sub(last_cached_input_tokens);
            add_codex_usage(stats, delta_input, delta_output, delta_cache_read);
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

// ── Conversation entries for structured preview ─────────────────────

/// A single entry in a Claude Code conversation, parsed from JSONL logs.
#[derive(Debug, Clone)]
pub enum ConversationEntry {
    UserMessage {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ToolUse {
        tool_name: String,
        details: Option<String>,
    },
    ToolResult {
        filenames: Vec<String>,
        summary: Option<String>,
    },
    QueueOperation {
        operation: String,
        task_id: Option<String>,
    },
    Progress {
        kind: String,
        detail: String,
    },
    SystemEvent {
        subtype: String,
        detail: String,
    },
    FileHistorySnapshot {
        tracked_files: usize,
        files: Vec<String>,
        is_update: bool,
    },
    Unparsed {
        reason: String,
        raw: String,
    },
}

fn summarize_jsonl_line(line: &str, max_chars: usize) -> String {
    let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let mut out: String = compact.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

fn extract_text_parts(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr
            .iter()
            .flat_map(extract_text_parts)
            .collect::<Vec<String>>(),
        serde_json::Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(|t| t.as_str()) {
                return vec![text.to_string()];
            }
            if let Some(content) = map.get("content") {
                return extract_text_parts(content);
            }
            vec![]
        }
        _ => vec![],
    }
}

fn extract_text(value: &serde_json::Value) -> Option<String> {
    let parts: Vec<String> = extract_text_parts(value)
        .into_iter()
        .filter_map(|p| {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn summarize_tool_input(input: &serde_json::Value) -> Option<String> {
    if let Some(text) = extract_text(input) {
        return Some(summarize_jsonl_line(&text, 120));
    }

    let obj = input.as_object()?;
    if obj.is_empty() {
        return None;
    }

    let mut details: Vec<String> = Vec::new();
    let important_fields = [
        ("file_path", "file"),
        ("path", "path"),
        ("old_path", "old"),
        ("new_path", "new"),
        ("command", "cmd"),
        ("query", "query"),
        ("pattern", "pattern"),
        ("url", "url"),
    ];

    for (key, label) in important_fields {
        if let Some(v) = obj.get(key) {
            if let Some(text) = extract_text(v) {
                details.push(format!("{label}={}", summarize_jsonl_line(&text, 80)));
            }
        }
    }

    if details.is_empty() {
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let shown: Vec<&str> = keys.into_iter().take(5).collect();
        details.push(format!("args={}", shown.join(", ")));
    }

    Some(details.join(" | "))
}

fn summarize_tool_use_details(item: &serde_json::Value) -> Option<String> {
    let mut details: Vec<String> = Vec::new();

    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
        details.push(format!("id={id}"));
    }

    if let Some(input) = item.get("input") {
        if let Some(summary) = summarize_tool_input(input) {
            details.push(summary);
        }
    }

    if details.is_empty() {
        None
    } else {
        Some(details.join(" | "))
    }
}

fn extract_tag_value(content: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = content.find(&open)? + open.len();
    let end = content[start..].find(&close)? + start;
    let inner = content[start..end].trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner.to_string())
    }
}

fn summarize_progress_entry(value: &serde_json::Value) -> Option<(String, String)> {
    let data = value.get("data")?;
    let kind = data.get("type").and_then(|t| t.as_str())?.to_string();

    let detail = match kind.as_str() {
        "waiting_for_task" => {
            let desc = data
                .get("taskDescription")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if desc.is_empty() {
                return None;
            }
            let task_type = data
                .get("taskType")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty());
            match task_type {
                Some(task_type) => format!("{desc} ({task_type})"),
                None => desc.to_string(),
            }
        }
        "search_results_received" => {
            let query = data
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let result_count = data.get("resultCount").and_then(|v| v.as_u64());
            match (result_count, query.is_empty()) {
                (Some(count), false) => format!("{count} results for {query}"),
                (Some(count), true) => format!("{count} results"),
                (None, false) => query.to_string(),
                (None, true) => return None,
            }
        }
        "query_update" => data
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        "mcp_progress" => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(status) = data.get("status").and_then(|v| v.as_str()) {
                parts.push(status.to_string());
            }
            if let Some(server) = data.get("serverName").and_then(|v| v.as_str()) {
                parts.push(format!("server={server}"));
            }
            if let Some(tool) = data.get("toolName").and_then(|v| v.as_str()) {
                parts.push(format!("tool={tool}"));
            }
            if let Some(ms) = data.get("elapsedTimeMs").and_then(|v| v.as_f64()) {
                parts.push(format!("{}ms", ms.round() as u64));
            }
            if parts.is_empty() {
                return None;
            }
            parts.join(" | ")
        }
        "bash_progress" => {
            let output = data
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let elapsed = data.get("elapsedTimeSeconds").and_then(|v| v.as_u64());
            let total_lines = data.get("totalLines").and_then(|v| v.as_u64()).unwrap_or(0);
            if output.is_empty() && total_lines == 0 {
                return None;
            }
            let mut parts: Vec<String> = Vec::new();
            if !output.is_empty() {
                parts.push(summarize_jsonl_line(output, 120));
            }
            if let Some(elapsed) = elapsed {
                parts.push(format!("{elapsed}s"));
            }
            if total_lines > 0 {
                parts.push(format!("{total_lines} lines"));
            }
            if parts.is_empty() {
                return None;
            }
            parts.join(" | ")
        }
        // High-volume noise with little user-facing value in the transcript.
        "hook_progress" | "agent_progress" => return None,
        _ => extract_text(data).unwrap_or_default(),
    };

    let detail = detail.trim();
    if detail.is_empty() {
        None
    } else {
        Some((kind, summarize_jsonl_line(detail, 180)))
    }
}

fn summarize_system_entry(value: &serde_json::Value) -> Option<(String, String)> {
    let subtype = value
        .get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let detail = match subtype.as_str() {
        // Very frequent and already represented by task timers in the UI.
        "turn_duration" => return None,
        "compact_boundary" => "Context compacted".to_string(),
        "microcompact_boundary" => "Micro-compaction boundary".to_string(),
        "api_error" => {
            let mut parts: Vec<String> = vec!["API error".to_string()];
            let retry_attempt = value.get("retryAttempt").and_then(|v| v.as_u64());
            let max_retries = value.get("maxRetries").and_then(|v| v.as_u64());
            if let (Some(attempt), Some(max)) = (retry_attempt, max_retries) {
                parts.push(format!("attempt {attempt}/{max}"));
            } else if let Some(attempt) = retry_attempt {
                parts.push(format!("attempt {attempt}"));
            }
            if let Some(ms) = value.get("retryInMs").and_then(|v| v.as_f64()) {
                parts.push(format!("retry in {}ms", ms.round() as u64));
            }
            if let Some(text) = value
                .get("error")
                .and_then(extract_text)
                .or_else(|| value.get("message").and_then(extract_text))
            {
                if !text.trim().is_empty() {
                    parts.push(summarize_jsonl_line(&text, 120));
                }
            }
            parts.join(" | ")
        }
        "local_command" => {
            let content = value
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let command = extract_tag_value(content, "command-name");
            let message = extract_tag_value(content, "command-message");
            let stdout = extract_tag_value(content, "local-command-stdout");
            let stderr = extract_tag_value(content, "local-command-stderr");

            match (stderr, stdout, command, message) {
                (Some(stderr), _, _, _) => {
                    format!("stderr: {}", summarize_jsonl_line(&stderr, 140))
                }
                (_, Some(stdout), _, _) => {
                    format!("stdout: {}", summarize_jsonl_line(&stdout, 140))
                }
                (_, _, Some(cmd), Some(msg)) => {
                    format!("{cmd}: {}", summarize_jsonl_line(&msg, 120))
                }
                (_, _, Some(cmd), None) => cmd,
                (_, _, None, Some(msg)) => summarize_jsonl_line(&msg, 140),
                _ if !content.is_empty() => summarize_jsonl_line(content, 140),
                _ => return None,
            }
        }
        "stop_hook_summary" => {
            let hook_count = value.get("hookCount").and_then(|v| v.as_u64());
            let hook_errors = value
                .get("hookErrors")
                .and_then(|v| v.as_array())
                .map(|arr| arr.len())
                .unwrap_or(0);
            let prevented = value
                .get("preventedContinuation")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let stop_reason = value
                .get("stopReason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let has_output = value
                .get("hasOutput")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if hook_errors == 0 && !prevented && stop_reason.is_empty() && !has_output {
                return None;
            }

            let mut parts: Vec<String> = Vec::new();
            if let Some(count) = hook_count {
                parts.push(format!("hooks={count}"));
            }
            if hook_errors > 0 {
                parts.push(format!("errors={hook_errors}"));
            }
            if prevented {
                parts.push("prevented continuation".to_string());
            }
            if !stop_reason.is_empty() {
                parts.push(format!("reason={}", summarize_jsonl_line(stop_reason, 80)));
            }
            if has_output {
                parts.push("has output".to_string());
            }
            if parts.is_empty() {
                return None;
            }
            parts.join(" | ")
        }
        _ => value
            .get("content")
            .and_then(extract_text)
            .or_else(|| value.get("message").and_then(extract_text))
            .or_else(|| {
                value
                    .get("level")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "event".to_string()),
    };

    let detail = detail.trim();
    if detail.is_empty() {
        None
    } else {
        Some((subtype, summarize_jsonl_line(detail, 180)))
    }
}

fn summarize_file_history_snapshot(
    value: &serde_json::Value,
) -> Option<(usize, Vec<String>, bool)> {
    let is_update = value
        .get("isSnapshotUpdate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let tracked = value
        .get("snapshot")
        .and_then(|s| s.get("trackedFileBackups"))
        .and_then(|v| v.as_object())?;

    let tracked_files = tracked.len();
    if tracked_files == 0 && !is_update {
        return None;
    }

    let mut files: Vec<String> = tracked.keys().cloned().collect();
    files.sort();
    files.truncate(3);
    Some((tracked_files, files, is_update))
}

fn extract_tool_result_parts(value: &serde_json::Value) -> (Vec<String>, Option<String>) {
    let filenames = value
        .get("filenames")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();

    let summary_fields = [
        "content", "output", "message", "text", "error", "stderr", "stdout",
    ];
    let mut summary: Option<String> = None;
    for field in summary_fields {
        if let Some(v) = value.get(field) {
            if let Some(text) = extract_text(v) {
                summary = Some(summarize_jsonl_line(&text, 180));
                break;
            }
        }
    }

    if summary.is_none() {
        if let Some(ok) = value.get("success").and_then(|v| v.as_bool()) {
            summary = Some(if ok {
                "status=success".to_string()
            } else {
                "status=failure".to_string()
            });
        }
    }

    (filenames, summary)
}

/// Parse conversation entries from a Claude JSONL log file.
/// Reads incrementally from `read_offset`; returns new entries + updated offset.
pub fn parse_conversation_entries(
    path: &std::path::Path,
    read_offset: u64,
) -> (Vec<ConversationEntry>, u64) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (vec![], read_offset),
    };
    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return (vec![], read_offset),
    };

    if file_len <= read_offset {
        return (vec![], read_offset);
    }

    if read_offset > 0 && file.seek(SeekFrom::Start(read_offset)).is_err() {
        return (vec![], read_offset);
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return (vec![], read_offset);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut entries = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v,
            Err(_) => {
                entries.push(ConversationEntry::Unparsed {
                    reason: "Malformed JSONL".to_string(),
                    raw: summarize_jsonl_line(line, 220),
                });
                continue;
            }
        };

        let mut parsed = false;
        let mut handled = false;

        // Tool results can appear without a top-level `type`.
        if let Some(tool_result) = value.get("toolUseResult") {
            handled = true;
            let (filenames, summary) = extract_tool_result_parts(tool_result);
            if !filenames.is_empty() || summary.is_some() {
                entries.push(ConversationEntry::ToolResult { filenames, summary });
                parsed = true;
            }
        }

        match value.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                handled = true;
                if let Some(content) = value
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for item in content {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") | Some("thinking") | Some("reasoning") => {
                                if let Some(text) = item.get("text").and_then(extract_text) {
                                    entries.push(ConversationEntry::AssistantText { text });
                                    parsed = true;
                                }
                            }
                            Some("tool_use") => {
                                if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                    entries.push(ConversationEntry::ToolUse {
                                        tool_name: name.to_string(),
                                        details: summarize_tool_use_details(item),
                                    });
                                    parsed = true;
                                }
                            }
                            Some("tool_result") => {
                                let (filenames, summary) = extract_tool_result_parts(item);
                                if !filenames.is_empty() || summary.is_some() {
                                    entries
                                        .push(ConversationEntry::ToolResult { filenames, summary });
                                    parsed = true;
                                }
                            }
                            _ => {
                                // Some logs include text entries without explicit `type`.
                                if let Some(text) = item.get("text").and_then(extract_text) {
                                    entries.push(ConversationEntry::AssistantText { text });
                                    parsed = true;
                                }
                            }
                        }
                    }
                }
            }
            Some("user") => {
                handled = true;
                if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
                    if let Some(text) = extract_text(content) {
                        entries.push(ConversationEntry::UserMessage { text });
                        parsed = true;
                    }
                }
            }
            Some("queue-operation") => {
                handled = true;
                let operation = value
                    .get("operation")
                    .and_then(|o| o.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let task_id = value
                    .get("taskId")
                    .or_else(|| value.get("task_id"))
                    .or_else(|| value.get("id"))
                    .and_then(|id| id.as_str())
                    .map(str::to_string);
                entries.push(ConversationEntry::QueueOperation { operation, task_id });
                parsed = true;
            }
            Some("progress") => {
                handled = true;
                if let Some((kind, detail)) = summarize_progress_entry(&value) {
                    entries.push(ConversationEntry::Progress { kind, detail });
                    parsed = true;
                }
            }
            Some("system") => {
                handled = true;
                if let Some((subtype, detail)) = summarize_system_entry(&value) {
                    entries.push(ConversationEntry::SystemEvent { subtype, detail });
                    parsed = true;
                }
            }
            Some("file-history-snapshot") => {
                handled = true;
                if let Some((tracked_files, files, is_update)) =
                    summarize_file_history_snapshot(&value)
                {
                    entries.push(ConversationEntry::FileHistorySnapshot {
                        tracked_files,
                        files,
                        is_update,
                    });
                    parsed = true;
                }
            }
            Some(_) | None => {}
        }

        if !parsed && !handled {
            let reason = match value.get("type").and_then(|t| t.as_str()) {
                Some(kind) => format!("Unhandled entry type: {kind}"),
                None => "Unhandled entry (missing type)".to_string(),
            };
            entries.push(ConversationEntry::Unparsed {
                reason,
                raw: summarize_jsonl_line(line, 220),
            });
        }
    }

    (entries, file_len)
}

/// Build the JSONL log file path for a Claude Code session.
pub fn session_jsonl_path(cwd: &str, uuid: &str) -> std::path::PathBuf {
    let escaped = escape_project_path(cwd);
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(&home)
        .join(".claude")
        .join("projects")
        .join(&escaped)
        .join(format!("{uuid}.jsonl"))
}

// ── Codex conversation support ──────────────────────────────────────

/// Parse lsof output to find a `.codex/sessions/` JSONL path.
pub fn parse_codex_rollout_from_lsof(output: &str) -> Option<PathBuf> {
    for line in output.lines() {
        if let Some(idx) = line.find(".codex/sessions/") {
            // Walk backwards from `.codex/` to find the start of the absolute path.
            // lsof separates columns by whitespace, so find the last whitespace before idx.
            let before = &line[..idx];
            let path_start = before
                .rfind(char::is_whitespace)
                .map(|i| i + 1)
                .unwrap_or(0);
            let rest = &line[path_start..];
            // Find the end of the path (whitespace or end of line)
            let path_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let candidate = &rest[..path_end];
            if candidate.ends_with(".jsonl") {
                return Some(PathBuf::from(candidate));
            }
        }
    }
    None
}

/// Resolve the Codex rollout JSONL path for a tmux session.
/// Walks the process tree and checks lsof for open `.codex/sessions/` files.
pub async fn resolve_codex_rollout_path(tmux_name: &str) -> Option<PathBuf> {
    let pid = get_pane_pid(tmux_name).await?;
    let all_pids = collect_descendant_pids(pid).await;

    if all_pids.is_empty() {
        return None;
    }

    let pid_list = all_pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let output = run_cmd_timeout(Command::new("lsof").args(["-p", &pid_list]))
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_codex_rollout_from_lsof(&stdout)
}

/// Parse conversation entries from a Codex JSONL log file.
/// Reads incrementally from `read_offset`; returns new entries + updated offset.
pub fn parse_codex_conversation_entries(
    path: &std::path::Path,
    read_offset: u64,
) -> (Vec<ConversationEntry>, u64) {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (vec![], read_offset),
    };
    let file_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return (vec![], read_offset),
    };

    if file_len <= read_offset {
        return (vec![], read_offset);
    }

    if read_offset > 0 && file.seek(SeekFrom::Start(read_offset)).is_err() {
        return (vec![], read_offset);
    }

    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return (vec![], read_offset);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut entries = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Fast-path string checks before JSON parsing
        if line.contains("\"user_message\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(msg) = v
                    .get("payload")
                    .and_then(|p| p.get("message"))
                    .and_then(|m| m.as_str())
                {
                    if !msg.trim().is_empty() {
                        entries.push(ConversationEntry::UserMessage {
                            text: msg.to_string(),
                        });
                    }
                }
            }
            continue;
        }

        if line.contains("\"agent_message\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(msg) = v
                    .get("payload")
                    .and_then(|p| p.get("message"))
                    .and_then(|m| m.as_str())
                {
                    if !msg.trim().is_empty() {
                        entries.push(ConversationEntry::AssistantText {
                            text: msg.to_string(),
                        });
                    }
                }
            }
            continue;
        }

        if line.contains("\"function_call\"") && !line.contains("\"function_call_output\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(payload) = v.get("payload") {
                    if let Some(name) = payload.get("name").and_then(|n| n.as_str()) {
                        let details = payload
                            .get("arguments")
                            .and_then(extract_text)
                            .map(|s| summarize_jsonl_line(&s, 120));
                        entries.push(ConversationEntry::ToolUse {
                            tool_name: name.to_string(),
                            details,
                        });
                    }
                }
            }
            continue;
        }

        // Skip all other line types (session_meta, turn_context, reasoning,
        // token_count, task_started, task_complete, function_call_output)
    }

    (entries, file_len)
}

// ── Gemini conversation support ──────────────────────────────────────

// Gemini 2.5 Pro pricing (USD per million tokens) — free tier uses $0,
// but Vertex AI / paid tier uses these rates.
const GEMINI_INPUT_USD_PER_MTOK: f64 = 1.25;
const GEMINI_OUTPUT_USD_PER_MTOK: f64 = 10.0;
const GEMINI_CACHE_READ_USD_PER_MTOK: f64 = 0.3125;

/// Parse lsof output to find a `.gemini/tmp/` session JSON path.
pub fn parse_gemini_session_from_lsof(output: &str) -> Option<PathBuf> {
    for line in output.lines() {
        if let Some(idx) = line.find(".gemini/tmp/") {
            let before = &line[..idx];
            let path_start = before
                .rfind(char::is_whitespace)
                .map(|i| i + 1)
                .unwrap_or(0);
            let rest = &line[path_start..];
            let path_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
            let candidate = &rest[..path_end];
            if candidate.ends_with(".json") && candidate.contains("/chats/session-") {
                return Some(PathBuf::from(candidate));
            }
        }
    }
    None
}

/// Find the Gemini chats directory for the given CWD.
/// Reads ~/.gemini/projects.json to map cwd → project name, then looks
/// in ~/.gemini/tmp/<project>/chats/.
fn gemini_chats_dir(cwd: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects_path = PathBuf::from(&home).join(".gemini").join("projects.json");
    let data = std::fs::read_to_string(&projects_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    let projects = v.get("projects")?.as_object()?;
    let project_name = projects.get(cwd)?.as_str()?;
    let chats = PathBuf::from(&home)
        .join(".gemini")
        .join("tmp")
        .join(project_name)
        .join("chats");
    if chats.is_dir() {
        Some(chats)
    } else {
        None
    }
}

/// Find the most recently modified Gemini session file in the chats dir,
/// skipping files that are already claimed by other tmux Gemini sessions.
fn find_latest_gemini_session(
    chats_dir: &std::path::Path,
    claimed_paths: &HashSet<String>,
) -> Option<PathBuf> {
    let entries = std::fs::read_dir(chats_dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let fname = path.file_name()?.to_str()?;
        if !fname.starts_with("session-") {
            continue;
        }
        let path_key = path.to_string_lossy().to_string();
        if claimed_paths.contains(&path_key) {
            continue;
        }
        if let Ok(meta) = path.metadata() {
            if let Ok(modified) = meta.modified() {
                if best.as_ref().is_none_or(|(_, t)| modified > *t) {
                    best = Some((path, modified));
                }
            }
        }
    }
    best.map(|(p, _)| p)
}

/// Resolve the Gemini session JSON file path for a tmux session.
/// First tries lsof to find an open session file, then falls back to
/// the most recently modified session file in the project's chats dir.
pub async fn resolve_gemini_session_path(
    tmux_name: &str,
    cwd: &str,
    claimed_paths: &HashSet<String>,
) -> Option<String> {
    let pid = get_pane_pid(tmux_name).await?;
    let all_pids = collect_descendant_pids(pid).await;

    if !all_pids.is_empty() {
        let pid_list = all_pids
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");

        if let Ok(output) = run_cmd_timeout(Command::new("lsof").args(["-p", &pid_list])).await {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(path) = parse_gemini_session_from_lsof(&stdout) {
                return Some(path.to_string_lossy().to_string());
            }
        }
    }

    // Fallback: find the most recently modified session file
    let chats_dir = gemini_chats_dir(cwd)?;
    let path = find_latest_gemini_session(&chats_dir, claimed_paths)?;
    Some(path.to_string_lossy().to_string())
}

/// Parse a Gemini session JSON file and return conversation entries + stats + last message.
/// Since Gemini uses monolithic JSON (not JSONL), the entire file must be re-parsed.
/// Returns (entries, last_assistant_message, stats_update).
pub fn parse_gemini_session(
    path: &std::path::Path,
) -> (Vec<ConversationEntry>, Option<String>, GeminiStatsUpdate) {
    let (entries, _, last_message, stats) = parse_gemini_session_entries(path, 0);
    (entries, last_message, stats)
}

/// Parse new conversation entries from a Gemini session JSON file.
/// `message_offset` is the previously-seen message index (not byte offset).
/// Returns (new_entries, new_message_offset, last_assistant_message, stats_update).
pub fn parse_gemini_session_entries(
    path: &std::path::Path,
    message_offset: u64,
) -> (
    Vec<ConversationEntry>,
    u64,
    Option<String>,
    GeminiStatsUpdate,
) {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return (vec![], message_offset, None, GeminiStatsUpdate::default()),
    };
    let v: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return (vec![], message_offset, None, GeminiStatsUpdate::default()),
    };
    parse_gemini_session_value(&v, message_offset as usize)
}

/// Stats extracted from a Gemini session file.
#[derive(Debug, Default)]
pub struct GeminiStatsUpdate {
    pub turns: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub edits: u16,
    pub bash_cmds: u16,
    pub files: Vec<String>,
    pub last_user_ts: Option<String>,
    pub last_assistant_ts: Option<String>,
}

fn summarize_gemini_tool_use_details(tool_call: &serde_json::Value) -> Option<String> {
    let mut details: Vec<String> = Vec::new();

    if let Some(id) = tool_call.get("id").and_then(|v| v.as_str()) {
        details.push(format!("id={id}"));
    }

    if let Some(args) = tool_call.get("args") {
        if let Some(summary) = summarize_tool_input(args) {
            details.push(summary);
        }
    }

    if details.is_empty() {
        None
    } else {
        Some(details.join(" | "))
    }
}

fn extract_gemini_tool_paths(args: Option<&serde_json::Value>) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let Some(args) = args else {
        return paths;
    };

    for key in ["file_path", "path", "old_path", "new_path"] {
        if let Some(v) = args.get(key) {
            if let Some(text) = extract_text(v) {
                for raw in text.lines() {
                    let p = raw.trim();
                    if !p.is_empty() && seen.insert(p.to_string()) {
                        paths.push(p.to_string());
                    }
                }
            }
        }
    }

    paths
}

fn extract_gemini_tool_result_parts(
    tool_call: &serde_json::Value,
) -> (Vec<String>, Option<String>) {
    let filenames = extract_gemini_tool_paths(tool_call.get("args"));

    let mut summary = tool_call
        .get("resultDisplay")
        .and_then(extract_text)
        .map(|s| summarize_jsonl_line(&s, 180));

    if summary.is_none() {
        if let Some(results) = tool_call.get("result").and_then(|r| r.as_array()) {
            for result in results {
                if let Some(function_response) = result.get("functionResponse") {
                    if let Some(response) = function_response.get("response") {
                        let (_, s) = extract_tool_result_parts(response);
                        if s.is_some() {
                            summary = s;
                            break;
                        }
                    }
                }
                let (_, s) = extract_tool_result_parts(result);
                if s.is_some() {
                    summary = s;
                    break;
                }
            }
        }
    }

    if summary.is_none() {
        summary = tool_call
            .get("status")
            .and_then(|s| s.as_str())
            .map(|s| format!("status={s}"));
    }

    (filenames, summary)
}

fn parse_gemini_session_value(
    v: &serde_json::Value,
    message_offset: usize,
) -> (
    Vec<ConversationEntry>,
    u64,
    Option<String>,
    GeminiStatsUpdate,
) {
    let mut entries = Vec::new();
    let mut last_message: Option<String> = None;
    let mut stats = GeminiStatsUpdate::default();

    let messages = match v.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return (entries, message_offset as u64, last_message, stats),
    };

    let new_offset = messages.len() as u64;
    let start_idx = if message_offset > messages.len() {
        // Session file can roll over to a new conversation; when the message
        // count shrinks, restart from the beginning instead of dropping entries.
        0
    } else {
        message_offset
    };

    for (idx, msg) in messages.iter().enumerate() {
        let emit_entry = idx >= start_idx;
        let msg_type = match msg.get("type").and_then(|t| t.as_str()) {
            Some(t) => t,
            None => {
                if emit_entry {
                    entries.push(ConversationEntry::Unparsed {
                        reason: "Unhandled Gemini entry (missing type)".to_string(),
                        raw: summarize_jsonl_line(&msg.to_string(), 220),
                    });
                }
                continue;
            }
        };
        let timestamp = msg.get("timestamp").and_then(|t| t.as_str());

        match msg_type {
            "user" => {
                if let Some(ts) = timestamp {
                    stats.last_user_ts = Some(ts.to_string());
                }
                // content is either a string or an array of {text: "..."}
                let text = extract_gemini_message_text(msg);
                if let Some(text) = text {
                    if emit_entry && !text.trim().is_empty() {
                        entries.push(ConversationEntry::UserMessage { text });
                    }
                }
            }
            "gemini" => {
                if let Some(ts) = timestamp {
                    stats.last_assistant_ts = Some(ts.to_string());
                }
                // Extract token usage
                if let Some(tokens) = msg.get("tokens") {
                    stats.turns += 1;
                    stats.tokens_in += tokens.get("input").and_then(|t| t.as_u64()).unwrap_or(0);
                    stats.tokens_out += tokens.get("output").and_then(|t| t.as_u64()).unwrap_or(0);
                    stats.tokens_cached +=
                        tokens.get("cached").and_then(|t| t.as_u64()).unwrap_or(0);
                }

                // Process tool calls
                if let Some(tool_calls) = msg.get("toolCalls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        let name = tc.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                        let paths = extract_gemini_tool_paths(tc.get("args"));
                        // Track edits and bash commands
                        match name {
                            "write_file" | "edit_file" | "replace_in_file" => {
                                stats.edits += 1;
                                for path in &paths {
                                    stats.files.push(path.to_string());
                                }
                            }
                            "run_shell_command" | "shell" => {
                                stats.bash_cmds += 1;
                            }
                            "read_file" => {
                                for path in &paths {
                                    stats.files.push(path.to_string());
                                }
                            }
                            _ => {}
                        }

                        if emit_entry {
                            entries.push(ConversationEntry::ToolUse {
                                tool_name: name.to_string(),
                                details: summarize_gemini_tool_use_details(tc),
                            });

                            let (filenames, summary) = extract_gemini_tool_result_parts(tc);
                            if !filenames.is_empty() || summary.is_some() {
                                entries.push(ConversationEntry::ToolResult { filenames, summary });
                            }
                        }
                    }
                }

                // Extract assistant text content
                let text = extract_gemini_content_text(msg);
                if let Some(ref text) = text {
                    if !text.trim().is_empty() {
                        last_message = Some(text.clone());
                        if emit_entry {
                            entries.push(ConversationEntry::AssistantText { text: text.clone() });
                        }
                    }
                }
            }
            "info" | "warning" | "error" => {
                if emit_entry {
                    let prefix = msg_type.to_uppercase();
                    if let Some(content) = msg.get("content").and_then(extract_text) {
                        let text = format!("[{prefix}] {}", content.trim());
                        if !text.trim().is_empty() {
                            entries.push(ConversationEntry::AssistantText { text });
                        }
                    } else {
                        entries.push(ConversationEntry::Unparsed {
                            reason: format!("Unhandled Gemini {msg_type} entry"),
                            raw: summarize_jsonl_line(&msg.to_string(), 220),
                        });
                    }
                }
            }
            _ => {
                if emit_entry {
                    entries.push(ConversationEntry::Unparsed {
                        reason: format!("Unhandled Gemini message type: {msg_type}"),
                        raw: summarize_jsonl_line(&msg.to_string(), 220),
                    });
                }
            }
        }
    }

    (entries, new_offset, last_message, stats)
}

/// Extract user message text from Gemini's content field.
/// Content can be a string or an array of {text: "..."} objects.
fn extract_gemini_message_text(msg: &serde_json::Value) -> Option<String> {
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    None
}

/// Extract gemini response text content (content field on gemini messages).
fn extract_gemini_content_text(msg: &serde_json::Value) -> Option<String> {
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    // content can also be array format
    if let Some(arr) = content.as_array() {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    None
}

/// Apply a GeminiStatsUpdate to a SessionStats struct.
/// Since Gemini uses monolithic JSON, we replace stats rather than incrementing.
pub fn apply_gemini_stats(stats: &mut SessionStats, update: &GeminiStatsUpdate) {
    stats.turns = update.turns;
    stats.tokens_in = update.tokens_in;
    stats.tokens_out = update.tokens_out;
    stats.tokens_cache_read = update.tokens_cached;
    stats.tokens_cache_write = 0; // Gemini doesn't distinguish cache write
    stats.edits = update.edits;
    stats.bash_cmds = update.bash_cmds;
    stats.last_user_ts = update.last_user_ts.clone();
    stats.last_assistant_ts = update.last_assistant_ts.clone();
    stats.active_subagents = 0;
    stats.files.clear();
    stats.recent_files.clear();
    for f in &update.files {
        stats.touch_file(f.clone());
    }
}

/// Collect all Gemini session JSON files under `<tmp_dir>/*/chats/`.
fn collect_gemini_session_files(tmp_dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(tmp_dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let chats_dir = entry.path().join("chats");
        if !chats_dir.is_dir() {
            continue;
        }
        if let Ok(chat_entries) = std::fs::read_dir(&chats_dir) {
            for chat_entry in chat_entries.flatten() {
                let path = chat_entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    if let Some(fname) = path.file_name().and_then(|f| f.to_str()) {
                        if fname.starts_with("session-") {
                            out.push(path);
                        }
                    }
                }
            }
        }
    }
}

fn add_gemini_usage(
    stats: &mut GlobalStats,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
) {
    stats.tokens_in += input_tokens;
    stats.tokens_out += output_tokens;
    stats.tokens_cache_read += cached_tokens;

    stats.gemini_tokens_in += input_tokens;
    stats.gemini_tokens_out += output_tokens;
    stats.gemini_tokens_cached += cached_tokens;
}

/// Process a single Gemini session JSON file for global stats.
/// Since Gemini rewrites the entire file, we re-parse fully but track
/// the file size to skip unchanged files.
fn process_gemini_global_file(path: &PathBuf, stats: &mut GlobalStats, today: &str) {
    let file_len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };

    let offset = stats.gemini_file_sizes.get(path).copied().unwrap_or(0);
    if file_len == offset {
        return; // File hasn't changed
    }

    // Re-parse the entire file (monolithic JSON)
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let v: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return,
    };

    let messages = match v.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return,
    };

    // Sum tokens from all gemini messages that have today's date
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cached = 0u64;

    for msg in messages {
        if msg.get("type").and_then(|t| t.as_str()) != Some("gemini") {
            continue;
        }
        // Check if the message is from today
        let is_today = msg
            .get("timestamp")
            .and_then(|t| t.as_str())
            .is_some_and(|ts| ts.starts_with(today));
        if !is_today {
            continue;
        }
        if let Some(tokens) = msg.get("tokens") {
            total_input += tokens.get("input").and_then(|t| t.as_u64()).unwrap_or(0);
            total_output += tokens.get("output").and_then(|t| t.as_u64()).unwrap_or(0);
            total_cached += tokens.get("cached").and_then(|t| t.as_u64()).unwrap_or(0);
        }
    }

    // Subtract previous contribution from this file, then add new
    if let Some(&(prev_in, prev_out, prev_cached)) = stats.gemini_file_tokens.get(path) {
        stats.tokens_in -= prev_in;
        stats.tokens_out -= prev_out;
        stats.tokens_cache_read -= prev_cached;
        stats.gemini_tokens_in -= prev_in;
        stats.gemini_tokens_out -= prev_out;
        stats.gemini_tokens_cached -= prev_cached;
    }

    add_gemini_usage(stats, total_input, total_output, total_cached);
    stats
        .gemini_file_tokens
        .insert(path.clone(), (total_input, total_output, total_cached));
    stats.gemini_file_sizes.insert(path.clone(), file_len);
}

/// Read the last assistant message from a Claude JSONL log file.
/// Reads only the tail of the file for efficiency on large logs.
#[cfg(test)]
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/test-project", uuid);
        assert_eq!(msg, Some("Here is the answer.".to_string()));

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/parts-project", uuid);
        assert_eq!(msg, Some("Part one. Part two.".to_string()));

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/empty-project", uuid);
        assert_eq!(msg, None);

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/noassist-project", uuid);
        assert_eq!(msg, None);

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/ws-project", uuid);
        assert_eq!(msg, Some("hello world foo".to_string()));

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/invalid-project", uuid);
        assert_eq!(msg, Some("valid line".to_string()));

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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

        let _guard = HomeGuard::set(&tmp_dir);

        let msg = read_last_assistant_message("/tmp/emptycontent-project", uuid);
        assert_eq!(msg, None);

        drop(_guard);
        let _ = std::fs::remove_dir_all(tmp_dir.join(".claude"));
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
    fn global_stats_cost_calculation_with_gemini_breakdown() {
        let stats = GlobalStats {
            gemini_tokens_in: 1_000_000,
            gemini_tokens_out: 100_000,
            gemini_tokens_cached: 200_000,
            ..Default::default()
        };
        let cost = stats.cost_usd();
        assert!(
            (cost - 2.0625).abs() < 0.01,
            "expected ~$2.06, got ${cost:.2}"
        );
        assert!((stats.gemini_cost_usd() - cost).abs() < f64::EPSILON);
    }

    #[test]
    fn global_stats_gemini_cost_saturates_when_cache_exceeds_input() {
        let stats = GlobalStats {
            gemini_tokens_in: 100,
            gemini_tokens_out: 0,
            gemini_tokens_cached: 200,
            ..Default::default()
        };
        let expected = 200.0 * GEMINI_CACHE_READ_USD_PER_MTOK / 1_000_000.0;
        assert!((stats.gemini_cost_usd() - expected).abs() < f64::EPSILON);
        assert!((stats.cost_usd() - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn global_stats_helper_methods_fallback_to_aggregate_fields() {
        let stats = GlobalStats {
            tokens_in: 1_500,
            tokens_out: 500,
            tokens_cache_read: 100,
            tokens_cache_write: 50,
            ..Default::default()
        };

        assert!(stats.has_usage());
        assert_eq!(stats.claude_display_tokens(), 2_000);
        assert_eq!(stats.codex_display_tokens(), 0);
        assert_eq!(stats.gemini_display_tokens(), 0);
        assert!((stats.codex_cost_usd() - 0.0).abs() < f64::EPSILON);
        assert!((stats.gemini_cost_usd() - 0.0).abs() < f64::EPSILON);
        assert!((stats.claude_cost_usd() - stats.cost_usd()).abs() < f64::EPSILON);
    }

    #[test]
    fn global_stats_helper_methods_use_provider_breakdown_when_present() {
        let stats = GlobalStats {
            tokens_in: 99_999,
            tokens_out: 88_888,
            claude_tokens_in: 1_000,
            claude_tokens_out: 100,
            claude_tokens_cache_read: 25,
            claude_tokens_cache_write: 10,
            codex_tokens_in: 2_000,
            codex_tokens_out: 200,
            codex_tokens_cache_read: 400,
            gemini_tokens_in: 3_000,
            gemini_tokens_out: 300,
            gemini_tokens_cached: 600,
            ..Default::default()
        };

        assert!(stats.has_usage());
        assert_eq!(stats.claude_display_tokens(), 1_100);
        assert_eq!(stats.codex_display_tokens(), 2_200);
        assert_eq!(stats.gemini_display_tokens(), 3_300);
        let combined = stats.claude_cost_usd() + stats.codex_cost_usd() + stats.gemini_cost_usd();
        assert!((stats.cost_usd() - combined).abs() < f64::EPSILON);
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
        update_global_stats_inner(&mut stats, &today, Some(tmp.path()));

        assert_eq!(stats.tokens_in, 100, "should only count today's entries");
        assert_eq!(stats.tokens_out, 50);
    }

    #[test]
    fn update_global_stats_resets_on_date_change() {
        let mut stats = crate::logs::GlobalStats {
            tokens_in: 5000,
            tokens_out: 1000,
            date: "2020-01-01".to_string(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: "1999-01-01".to_string(),
            tokens_in: 500,
            tokens_out: 200,
            tokens_cache_read: 100,
            tokens_cache_write: 50,
            ..Default::default()
        };

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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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
        let lines = [r#"{"type":"queue-operation","operation":"remove","taskId":"x"}"#];
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
        assert_eq!(
            extract_assistant_message_text(&v),
            Some("Hello World".to_string())
        );
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
        let pb = std::path::PathBuf::from(&path);
        process_claude_global_file(&pb, &mut stats, &today);
        assert_eq!(stats.tokens_in, 100);
        assert_eq!(stats.tokens_out, 50);
        let offset1 = stats.file_offsets[&pb];

        // Append a second line
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
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

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(
            stats.codex_tokens_in, 0,
            "non-token_count payload should be skipped"
        );
    }

    #[test]
    fn process_codex_global_file_not_today_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // First line with yesterday's timestamp — should be tracked for delta but not added to today's tokens
        let line = r#"{"type":"event_msg","timestamp":"1999-01-01T10:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":50,"cached_input_tokens":0,"total_tokens":150}}}}"#.to_string();
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut stats = crate::logs::GlobalStats {
            date: today.clone(),
            ..Default::default()
        };
        process_codex_global_file(&std::path::PathBuf::from(&path), &mut stats, &today);
        assert_eq!(
            stats.codex_tokens_in, 0,
            "yesterday's tokens should not count for today"
        );
    }

    // ── update_global_stats date change ──

    #[test]
    fn update_global_stats_inner_resets_on_date_change() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(dir.path());
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let mut stats = crate::logs::GlobalStats {
            date: "1999-01-01".to_string(),
            tokens_in: 999,
            tokens_out: 999,
            claude_tokens_in: 500,
            codex_tokens_in: 499,
            ..Default::default()
        };
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
        stats
            .file_offsets
            .insert(std::path::PathBuf::from("/old/claude"), 200);

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

    // ── parse_conversation_entries tests ─────────────────────────────

    #[test]
    fn conversation_entries_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        let (entries, offset) = parse_conversation_entries(&path, 0);
        assert!(entries.is_empty());
        assert_eq!(offset, 0);
    }

    #[test]
    fn conversation_entries_user_and_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "user",
                "timestamp": "2025-01-01T00:00:00Z",
                "message": {"role": "user", "content": "do something"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2025-01-01T00:00:01Z",
                "message": {
                    "content": [{"type": "text", "text": "I'll help you"}],
                    "usage": {"input_tokens": 100, "output_tokens": 50}
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();
        let (entries, offset) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[0], ConversationEntry::UserMessage { text } if text == "do something")
        );
        assert!(
            matches!(&entries[1], ConversationEntry::AssistantText { text } if text == "I'll help you")
        );
        assert_eq!(offset, content.len() as u64);
    }

    #[test]
    fn conversation_entries_user_content_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_array.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "message": {
                    "content": [
                        {"text": "line one"},
                        {"text": "line two"}
                    ]
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();
        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::UserMessage { text } if text == "line one\nline two"
        ));
    }

    #[test]
    fn conversation_entries_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2025-01-01T00:00:01Z",
                "message": {
                    "content": [
                        {"type": "text", "text": "Let me edit that file"},
                        {"type": "tool_use", "name": "Edit", "id": "123", "input": {}}
                    ],
                    "usage": {"input_tokens": 100, "output_tokens": 50}
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();
        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(&entries[0], ConversationEntry::AssistantText { text } if text == "Let me edit that file")
        );
        assert!(
            matches!(&entries[1], ConversationEntry::ToolUse { tool_name, details } if tool_name == "Edit" && details.is_some())
        );
    }

    #[test]
    fn conversation_entries_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "toolUseResult": {
                    "filenames": ["src/app.rs", "src/ui.rs"]
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();
        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], ConversationEntry::ToolResult { filenames, summary } if filenames.len() == 2 && summary.is_none())
        );
    }

    #[test]
    fn conversation_entries_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let line1 = format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "timestamp": "2025-01-01T00:00:00Z",
                "message": {"role": "user", "content": "first"}
            })
        );
        std::fs::write(&path, &line1).unwrap();
        let (entries, offset) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);

        // Append more content
        let line2 = format!(
            "{}",
            serde_json::json!({
                "type": "user",
                "timestamp": "2025-01-01T00:00:01Z",
                "message": {"role": "user", "content": "second"}
            })
        );
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", line2).unwrap();
        drop(file);

        let (entries2, offset2) = parse_conversation_entries(&path, offset);
        assert_eq!(entries2.len(), 1);
        assert!(
            matches!(&entries2[0], ConversationEntry::UserMessage { text } if text == "second")
        );
        assert!(offset2 > offset);
    }

    #[test]
    fn conversation_entries_malformed_line_captured_as_unparsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.jsonl");
        std::fs::write(&path, "{\"type\":\"assistant\" BROKEN\n").unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::Unparsed { reason, .. } if reason == "Malformed JSONL"
        ));
    }

    #[test]
    fn conversation_entries_queue_operation_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "queue-operation",
                "operation": "enqueue",
                "id": "q1"
            }),
        );
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::QueueOperation { operation, task_id }
                if operation == "enqueue" && task_id.as_deref() == Some("q1")
        ));
    }

    #[test]
    fn conversation_entries_progress_waiting_for_task_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("progress.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "progress",
                "data": {
                    "type": "waiting_for_task",
                    "taskDescription": "Run integration suite",
                    "taskType": "local_bash"
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::Progress { kind, detail }
                if kind == "waiting_for_task" && detail.contains("Run integration suite")
        ));
    }

    #[test]
    fn conversation_entries_system_api_error_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("system_api_error.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "system",
                "subtype": "api_error",
                "retryAttempt": 2,
                "maxRetries": 10,
                "retryInMs": 536.45
            }),
        );
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::SystemEvent { subtype, detail }
                if subtype == "api_error"
                    && detail.contains("attempt 2/10")
                    && detail.contains("retry in 536ms")
        ));
    }

    #[test]
    fn conversation_entries_file_history_snapshot_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "file-history-snapshot",
                "isSnapshotUpdate": true,
                "snapshot": {
                    "trackedFileBackups": {
                        "src/a.rs": {"version": 1},
                        "src/b.rs": {"version": 1}
                    }
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::FileHistorySnapshot {
                tracked_files,
                files,
                is_update
            } if *tracked_files == 2 && *is_update && files.len() == 2
        ));
    }

    #[test]
    fn conversation_entries_file_history_snapshot_empty_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot_empty.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "file-history-snapshot",
                "isSnapshotUpdate": false,
                "snapshot": {
                    "trackedFileBackups": {}
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn conversation_entries_unknown_type_captured_as_unparsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "mystery-event",
                "message": "maintenance"
            }),
        );
        std::fs::write(&path, &content).unwrap();

        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::Unparsed { reason, .. }
                if reason == "Unhandled entry type: mystery-event"
        ));
    }

    #[test]
    fn conversation_entries_tool_result_content_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool_result_summary.jsonl");
        let content = format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "toolUseResult": {
                    "content": "command completed with warnings"
                }
            }),
        );
        std::fs::write(&path, &content).unwrap();
        let (entries, _) = parse_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::ToolResult { filenames, summary }
                if filenames.is_empty()
                    && summary.as_deref() == Some("command completed with warnings")
        ));
    }

    #[test]
    fn conversation_entries_nonexistent_file() {
        let (entries, offset) =
            parse_conversation_entries(std::path::Path::new("/nonexistent/file.jsonl"), 0);
        assert!(entries.is_empty());
        assert_eq!(offset, 0);
    }

    // ── parse_codex_rollout_from_lsof tests ─────────────────────────

    #[test]
    fn parse_codex_rollout_finds_jsonl_path() {
        let output = "codex  12345  user  3r   REG  1,20  456  /Users/test/.codex/sessions/2026/02/24/rollout-1234567890-abcd1234.jsonl\n\
                       codex  12345  user  cwd  DIR  1,20  640  /Users/test";
        let result = parse_codex_rollout_from_lsof(output);
        assert_eq!(
            result,
            Some(PathBuf::from(
                "/Users/test/.codex/sessions/2026/02/24/rollout-1234567890-abcd1234.jsonl"
            ))
        );
    }

    #[test]
    fn parse_codex_rollout_no_match() {
        let output = "codex  12345  user  cwd  DIR  1,20  640  /Users/test\n\
                       codex  12345  user  txt  REG  1,20  123  /usr/bin/codex";
        assert_eq!(parse_codex_rollout_from_lsof(output), None);
    }

    #[test]
    fn parse_codex_rollout_ignores_non_jsonl() {
        let output = "codex  12345  user  3r   REG  1,20  456  /Users/test/.codex/sessions/2026/02/24/some-file.txt";
        assert_eq!(parse_codex_rollout_from_lsof(output), None);
    }

    #[test]
    fn parse_codex_rollout_empty() {
        assert_eq!(parse_codex_rollout_from_lsof(""), None);
    }

    // ── parse_codex_conversation_entries tests ──────────────────────

    #[test]
    fn codex_conversation_user_message() {
        let path = write_tmp_jsonl(
            "codex_user",
            &[r#"{"type":"event_msg","payload":{"type":"user_message","message":"fix the bug"}}"#],
        );
        let (entries, offset) = parse_codex_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], ConversationEntry::UserMessage { text } if text == "fix the bug")
        );
        assert!(offset > 0);
    }

    #[test]
    fn codex_conversation_agent_message() {
        let path = write_tmp_jsonl(
            "codex_agent",
            &[r#"{"type":"event_msg","payload":{"type":"agent_message","message":"I fixed it."}}"#],
        );
        let (entries, _) = parse_codex_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], ConversationEntry::AssistantText { text } if text == "I fixed it.")
        );
    }

    #[test]
    fn codex_conversation_function_call() {
        let path = write_tmp_jsonl(
            "codex_tool",
            &[
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}}"#,
            ],
        );
        let (entries, _) = parse_codex_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], ConversationEntry::ToolUse { tool_name, details } if tool_name == "exec_command" && details.is_some())
        );
    }

    #[test]
    fn codex_conversation_skips_function_call_output() {
        let path = write_tmp_jsonl(
            "codex_skip_output",
            &[
                r#"{"type":"response_item","payload":{"type":"function_call_output","output":"some output"}}"#,
            ],
        );
        let (entries, _) = parse_codex_conversation_entries(&path, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn codex_conversation_skips_token_count() {
        let path = write_tmp_jsonl(
            "codex_skip_tokens",
            &[
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100}}}}"#,
            ],
        );
        let (entries, _) = parse_codex_conversation_entries(&path, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn codex_conversation_mixed() {
        let path = write_tmp_jsonl(
            "codex_mixed",
            &[
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"hello"}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hi there"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call_output","output":"ok"}}"#,
            ],
        );
        let (entries, _) = parse_codex_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 3);
        assert!(matches!(&entries[0], ConversationEntry::UserMessage { text } if text == "hello"));
        assert!(
            matches!(&entries[1], ConversationEntry::AssistantText { text } if text == "hi there")
        );
        assert!(
            matches!(&entries[2], ConversationEntry::ToolUse { tool_name, details } if tool_name == "exec_command" && details.is_some())
        );
    }

    #[test]
    fn codex_conversation_incremental() {
        let path = write_tmp_jsonl(
            "codex_incr",
            &[
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"first"}}"#,
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"reply"}}"#,
            ],
        );
        let (entries, offset) = parse_codex_conversation_entries(&path, 0);
        assert_eq!(entries.len(), 2);
        assert!(offset > 0);

        // No new data → empty
        let (entries2, offset2) = parse_codex_conversation_entries(&path, offset);
        assert!(entries2.is_empty());
        assert_eq!(offset2, offset);
    }

    #[test]
    fn codex_conversation_nonexistent_file() {
        let (entries, offset) =
            parse_codex_conversation_entries(std::path::Path::new("/nonexistent/codex.jsonl"), 0);
        assert!(entries.is_empty());
        assert_eq!(offset, 0);
    }

    // ── parse_gemini_session_entries tests ─────────────────────────

    #[test]
    fn gemini_session_entries_parse_tool_use_and_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let content = serde_json::json!({
            "sessionId": "test-session",
            "messages": [
                {
                    "type": "user",
                    "timestamp": "2026-02-24T16:25:37.510Z",
                    "content": [{"text": "read this file"}]
                },
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T16:25:44.454Z",
                    "content": "Done.",
                    "toolCalls": [{
                        "id": "read_file_1",
                        "name": "read_file",
                        "args": {"file_path": "src/session.rs"},
                        "status": "success",
                        "result": [{
                            "functionResponse": {
                                "response": {"output": "file contents..."}
                            }
                        }]
                    }],
                    "tokens": {"input": 10, "output": 5, "cached": 2}
                }
            ]
        });
        std::fs::write(&path, content.to_string()).unwrap();

        let (entries, offset, last_msg, stats) = parse_gemini_session_entries(&path, 0);
        assert_eq!(offset, 2);
        assert_eq!(last_msg.as_deref(), Some("Done."));
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.tokens_in, 10);
        assert_eq!(stats.tokens_out, 5);
        assert_eq!(stats.tokens_cached, 2);
        assert_eq!(entries.len(), 4);
        assert!(
            matches!(&entries[0], ConversationEntry::UserMessage { text } if text == "read this file")
        );
        assert!(matches!(
            &entries[1],
            ConversationEntry::ToolUse { tool_name, details }
                if tool_name == "read_file" && details.is_some()
        ));
        assert!(matches!(
            &entries[2],
            ConversationEntry::ToolResult { filenames, summary }
                if filenames == &vec!["src/session.rs".to_string()] && summary.is_some()
        ));
        assert!(
            matches!(&entries[3], ConversationEntry::AssistantText { text } if text == "Done.")
        );
    }

    #[test]
    fn gemini_session_entries_incremental_and_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let first = serde_json::json!({
            "messages": [
                {"type": "user", "content": [{"text": "one"}]},
                {"type": "gemini", "content": "reply one", "tokens": {"input": 1, "output": 1, "cached": 0}}
            ]
        });
        std::fs::write(&path, first.to_string()).unwrap();
        let (_, offset1, _, _) = parse_gemini_session_entries(&path, 0);
        assert_eq!(offset1, 2);

        let second = serde_json::json!({
            "messages": [
                {"type": "user", "content": [{"text": "one"}]},
                {"type": "gemini", "content": "reply one", "tokens": {"input": 1, "output": 1, "cached": 0}},
                {"type": "user", "content": [{"text": "two"}]}
            ]
        });
        std::fs::write(&path, second.to_string()).unwrap();
        let (new_entries, offset2, _, _) = parse_gemini_session_entries(&path, offset1);
        assert_eq!(offset2, 3);
        assert_eq!(new_entries.len(), 1);
        assert!(
            matches!(&new_entries[0], ConversationEntry::UserMessage { text } if text == "two")
        );

        // Session rollover: file shrinks, parser should restart from beginning.
        let rollover = serde_json::json!({
            "messages": [
                {"type": "user", "content": [{"text": "fresh start"}]}
            ]
        });
        std::fs::write(&path, rollover.to_string()).unwrap();
        let (rolled_entries, offset3, _, _) = parse_gemini_session_entries(&path, offset2);
        assert_eq!(offset3, 1);
        assert_eq!(rolled_entries.len(), 1);
        assert!(matches!(
            &rolled_entries[0],
            ConversationEntry::UserMessage { text } if text == "fresh start"
        ));
    }

    #[test]
    fn gemini_session_entries_unknown_type_unparsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let content = serde_json::json!({
            "messages": [
                {"type": "mystery", "content": "??"}
            ]
        });
        std::fs::write(&path, content.to_string()).unwrap();

        let (entries, _, _, _) = parse_gemini_session_entries(&path, 0);
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            &entries[0],
            ConversationEntry::Unparsed { reason, .. }
                if reason == "Unhandled Gemini message type: mystery"
        ));
    }

    #[test]
    fn apply_gemini_stats_replaces_file_tracking() {
        let mut stats = SessionStats::default();
        stats.touch_file("old.rs".to_string());
        stats.active_subagents = 5;

        let update = GeminiStatsUpdate {
            turns: 2,
            tokens_in: 20,
            tokens_out: 10,
            tokens_cached: 3,
            edits: 1,
            bash_cmds: 2,
            files: vec!["new_a.rs".to_string(), "new_b.rs".to_string()],
            last_user_ts: Some("2026-02-24T16:00:00Z".to_string()),
            last_assistant_ts: Some("2026-02-24T16:01:00Z".to_string()),
        };
        apply_gemini_stats(&mut stats, &update);

        assert_eq!(stats.turns, 2);
        assert_eq!(stats.tokens_in, 20);
        assert_eq!(stats.tokens_out, 10);
        assert_eq!(stats.tokens_cache_read, 3);
        assert_eq!(stats.edits, 1);
        assert_eq!(stats.bash_cmds, 2);
        assert_eq!(stats.active_subagents, 0);
        assert_eq!(stats.files.len(), 2);
        assert!(stats.files.contains("new_a.rs"));
        assert!(stats.files.contains("new_b.rs"));
        assert!(!stats.files.contains("old.rs"));
    }

    // ── Gemini parsing tests ────────────────────────────────────────

    #[test]
    fn parse_gemini_session_basic() {
        let json = r#"{
            "sessionId": "abc-123",
            "messages": [
                {
                    "type": "user",
                    "timestamp": "2026-02-24T10:00:00Z",
                    "content": [{"text": "Hello"}]
                },
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T10:00:05Z",
                    "content": "Hi there!",
                    "tokens": {"input": 100, "output": 50, "cached": 30, "thoughts": 10, "total": 160}
                }
            ]
        }"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let (entries, _, last_msg, stats) = parse_gemini_session_value(&v, 0);

        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[0], ConversationEntry::UserMessage { text } if text == "Hello"));
        assert!(
            matches!(&entries[1], ConversationEntry::AssistantText { text } if text == "Hi there!")
        );
        assert_eq!(last_msg, Some("Hi there!".to_string()));
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.tokens_in, 100);
        assert_eq!(stats.tokens_out, 50);
        assert_eq!(stats.tokens_cached, 30);
    }

    #[test]
    fn parse_gemini_session_with_tool_calls() {
        let json = r#"{
            "sessionId": "abc-123",
            "messages": [
                {
                    "type": "user",
                    "timestamp": "2026-02-24T10:00:00Z",
                    "content": [{"text": "Read my file"}]
                },
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T10:00:05Z",
                    "content": "",
                    "toolCalls": [
                        {
                            "name": "read_file",
                            "args": {"file_path": "src/main.rs"},
                            "status": "success"
                        },
                        {
                            "name": "write_file",
                            "args": {"file_path": "src/new.rs", "content": "fn main() {}"},
                            "status": "success"
                        }
                    ],
                    "tokens": {"input": 200, "output": 80, "cached": 0, "thoughts": 0, "total": 280}
                }
            ]
        }"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let (entries, _, _, stats) = parse_gemini_session_value(&v, 0);

        // user + (tool_use + tool_result) x 2 (no assistant text since content is empty)
        assert_eq!(entries.len(), 5);
        assert!(
            matches!(&entries[1], ConversationEntry::ToolUse { tool_name, .. } if tool_name == "read_file")
        );
        assert!(matches!(&entries[2], ConversationEntry::ToolResult { .. }));
        assert!(
            matches!(&entries[3], ConversationEntry::ToolUse { tool_name, .. } if tool_name == "write_file")
        );
        assert!(matches!(&entries[4], ConversationEntry::ToolResult { .. }));
        assert_eq!(stats.edits, 1); // write_file counts as edit
        assert_eq!(stats.files.len(), 2);
        assert!(stats.files.contains(&"src/main.rs".to_string()));
        assert!(stats.files.contains(&"src/new.rs".to_string()));
    }

    #[test]
    fn parse_gemini_session_empty_messages() {
        let json = r#"{"sessionId": "abc", "messages": []}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let (entries, _, last_msg, stats) = parse_gemini_session_value(&v, 0);
        assert!(entries.is_empty());
        assert!(last_msg.is_none());
        assert_eq!(stats.turns, 0);
    }

    #[test]
    fn parse_gemini_session_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not valid json").unwrap();
        let (entries, last_msg, _) = parse_gemini_session(&path);
        assert!(entries.is_empty());
        assert!(last_msg.is_none());
    }

    #[test]
    fn parse_gemini_lsof_finds_session_json() {
        let output = "node    12345 user   25r    REG  1,18  50000 /Users/test/.gemini/tmp/hydra/chats/session-2026-02-24T16-25-abc123.json\n";
        let result = parse_gemini_session_from_lsof(output);
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(path
            .to_str()
            .unwrap()
            .contains("session-2026-02-24T16-25-abc123.json"));
    }

    #[test]
    fn parse_gemini_lsof_ignores_non_session() {
        let output = "node    12345 user   25r    REG  1,18  50000 /Users/test/.gemini/tmp/hydra/logs.json\n";
        let result = parse_gemini_session_from_lsof(output);
        assert!(result.is_none());
    }

    #[test]
    fn find_latest_gemini_session_skips_claimed_paths() {
        let dir = tempfile::tempdir().unwrap();
        let chats_dir = dir.path().join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let first = chats_dir.join("session-2026-02-24T16-20-a.json");
        let second = chats_dir.join("session-2026-02-24T16-21-b.json");
        std::fs::write(&first, "{}").unwrap();
        std::fs::write(&second, "{}").unwrap();

        let mut claimed = HashSet::new();
        claimed.insert(second.to_string_lossy().to_string());

        let resolved = find_latest_gemini_session(&chats_dir, &claimed).unwrap();
        assert_eq!(resolved, first);
    }

    #[test]
    fn find_latest_gemini_session_returns_none_when_all_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let chats_dir = dir.path().join("chats");
        std::fs::create_dir_all(&chats_dir).unwrap();

        let first = chats_dir.join("session-2026-02-24T16-20-a.json");
        let second = chats_dir.join("session-2026-02-24T16-21-b.json");
        std::fs::write(&first, "{}").unwrap();
        std::fs::write(&second, "{}").unwrap();

        let mut claimed = HashSet::new();
        claimed.insert(first.to_string_lossy().to_string());
        claimed.insert(second.to_string_lossy().to_string());

        let resolved = find_latest_gemini_session(&chats_dir, &claimed);
        assert!(resolved.is_none());
    }

    #[test]
    fn apply_gemini_stats_replaces_values() {
        let mut stats = SessionStats::default();
        stats.turns = 5;
        stats.tokens_in = 1000;

        let update = GeminiStatsUpdate {
            turns: 10,
            tokens_in: 2000,
            tokens_out: 500,
            tokens_cached: 100,
            edits: 3,
            bash_cmds: 1,
            files: vec!["a.rs".to_string()],
            last_user_ts: Some("2026-02-24T10:00:00Z".to_string()),
            last_assistant_ts: Some("2026-02-24T10:00:05Z".to_string()),
        };

        apply_gemini_stats(&mut stats, &update);
        assert_eq!(stats.turns, 10);
        assert_eq!(stats.tokens_in, 2000);
        assert_eq!(stats.tokens_out, 500);
        assert_eq!(stats.tokens_cache_read, 100);
        assert_eq!(stats.edits, 3);
        assert_eq!(stats.bash_cmds, 1);
        assert!(stats.files.contains("a.rs"));
    }

    #[test]
    fn gemini_global_stats_from_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("myproject").join("chats");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_json = r#"{
            "sessionId": "abc-123",
            "messages": [
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T10:00:05Z",
                    "content": "Hello!",
                    "tokens": {"input": 500, "output": 100, "cached": 50}
                }
            ]
        }"#;
        std::fs::write(
            project_dir.join("session-2026-02-24T10-00-abc12345.json"),
            session_json,
        )
        .unwrap();

        let mut stats = GlobalStats::default();
        stats.date = "2026-02-24".to_string();
        // Manually discover files
        let mut gemini_files = Vec::new();
        collect_gemini_session_files(dir.path(), &mut gemini_files);
        stats.known_gemini_files = gemini_files;

        for i in 0..stats.known_gemini_files.len() {
            let path = stats.known_gemini_files[i].clone();
            process_gemini_global_file(&path, &mut stats, "2026-02-24");
        }

        assert_eq!(stats.gemini_tokens_in, 500);
        assert_eq!(stats.gemini_tokens_out, 100);
        assert_eq!(stats.gemini_tokens_cached, 50);
        assert_eq!(stats.tokens_in, 500);
        assert_eq!(stats.tokens_out, 100);
    }

    #[test]
    fn gemini_global_stats_reparse_replaces_prior_file_totals() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("myproject").join("chats");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_path = project_dir.join("session-2026-02-24T10-00-abc12345.json");

        let first = r#"{
            "sessionId": "abc-123",
            "messages": [
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T10:00:05Z",
                    "tokens": {"input": 100, "output": 20, "cached": 10}
                }
            ]
        }"#;
        std::fs::write(&session_path, first).unwrap();

        let mut stats = GlobalStats::default();
        stats.date = "2026-02-24".to_string();

        process_gemini_global_file(&session_path, &mut stats, "2026-02-24");
        assert_eq!(stats.gemini_tokens_in, 100);
        assert_eq!(stats.gemini_tokens_out, 20);
        assert_eq!(stats.gemini_tokens_cached, 10);
        assert_eq!(stats.tokens_in, 100);
        assert_eq!(stats.tokens_out, 20);
        assert_eq!(stats.tokens_cache_read, 10);

        // Gemini rewrites files; totals should be replaced, not incremented.
        let rewritten = r#"{
            "sessionId": "abc-123",
            "messages": [
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T11:00:05Z",
                    "tokens": {"input": 300, "output": 50, "cached": 40}
                },
                {
                    "type": "gemini",
                    "timestamp": "2026-02-24T11:01:05Z",
                    "tokens": {"input": 200, "output": 25, "cached": 10}
                }
            ]
        }"#;
        std::fs::write(&session_path, rewritten).unwrap();
        process_gemini_global_file(&session_path, &mut stats, "2026-02-24");

        assert_eq!(stats.gemini_tokens_in, 500);
        assert_eq!(stats.gemini_tokens_out, 75);
        assert_eq!(stats.gemini_tokens_cached, 50);
        assert_eq!(stats.tokens_in, 500);
        assert_eq!(stats.tokens_out, 75);
        assert_eq!(stats.tokens_cache_read, 50);

        // If rewritten file no longer contains today's messages, contribution
        // from this file should be removed.
        let old_only = r#"{
            "sessionId": "abc-123",
            "messages": [
                {
                    "type": "gemini",
                    "timestamp": "2026-02-23T11:00:05Z",
                    "tokens": {"input": 999, "output": 999, "cached": 999}
                }
            ]
        }"#;
        std::fs::write(&session_path, old_only).unwrap();
        process_gemini_global_file(&session_path, &mut stats, "2026-02-24");

        assert_eq!(stats.gemini_tokens_in, 0);
        assert_eq!(stats.gemini_tokens_out, 0);
        assert_eq!(stats.gemini_tokens_cached, 0);
        assert_eq!(stats.tokens_in, 0);
        assert_eq!(stats.tokens_out, 0);
        assert_eq!(stats.tokens_cache_read, 0);
    }
}
