use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use tokio::process::Command;

/// Get the pane PID for a tmux session.
pub async fn get_pane_pid(tmux_name: &str) -> Option<u32> {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", tmux_name, "-F", "#{pane_pid}"])
        .output()
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

/// Use lsof to find the Claude tasks UUID from a process PID.
async fn resolve_uuid_from_pid(pid: u32) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
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

/// Resolve the Claude session UUID for a tmux session via PID tracing.
pub async fn resolve_session_uuid(tmux_name: &str) -> Option<String> {
    let pid = get_pane_pid(tmux_name).await?;
    resolve_uuid_from_pid(pid).await
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
}
