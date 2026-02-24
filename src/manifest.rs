use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::session::AgentType;

/// Maximum failed revival attempts before pruning a manifest entry.
pub const MAX_FAILED_ATTEMPTS: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionRecord {
    pub name: String,
    pub agent_type: String,
    pub agent_session_id: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub failed_attempts: u32,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Manifest {
    pub sessions: HashMap<String, SessionRecord>,
}

/// Default base directory for manifests: `~/.hydra/`
pub fn default_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hydra")
}

/// Return the manifest file path: `<base_dir>/<project_id>/sessions.json`
pub fn manifest_path(base_dir: &Path, project_id: &str) -> PathBuf {
    base_dir.join(project_id).join("sessions.json")
}

/// Load manifest from disk. Returns empty Manifest on missing or corrupt file.
pub async fn load_manifest(base_dir: &Path, project_id: &str) -> Manifest {
    let path = manifest_path(base_dir, project_id);
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => Manifest::default(),
    }
}

/// Save manifest to disk, creating directories as needed.
/// Uses write-to-temp-then-rename for atomic writes on POSIX,
/// preventing corruption from crashes or concurrent instances.
pub async fn save_manifest(base_dir: &Path, project_id: &str, manifest: &Manifest) -> Result<()> {
    let path = manifest_path(base_dir, project_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_string_pretty(manifest)?;
    // Use a unique temp filename to avoid collisions between concurrent writes
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp_name = format!(
        "sessions.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    let tmp_path = path.with_file_name(tmp_name);
    tokio::fs::write(&tmp_path, json).await?;
    tokio::fs::rename(&tmp_path, &path).await?;
    Ok(())
}

/// Add a session record to the manifest (load-modify-save).
pub async fn add_session(base_dir: &Path, project_id: &str, record: SessionRecord) -> Result<()> {
    let mut manifest = load_manifest(base_dir, project_id).await;
    manifest.sessions.insert(record.name.clone(), record);
    save_manifest(base_dir, project_id, &manifest).await
}

/// Remove a session record from the manifest by name (load-modify-save).
pub async fn remove_session(base_dir: &Path, project_id: &str, name: &str) -> Result<()> {
    let mut manifest = load_manifest(base_dir, project_id).await;
    manifest.sessions.remove(name);
    save_manifest(base_dir, project_id, &manifest).await
}

impl SessionRecord {
    /// Create a new SessionRecord for a fresh session, generating a UUID for Claude.
    pub fn for_new_session(name: &str, agent: &AgentType, cwd: &str) -> Self {
        let agent_session_id = if *agent == AgentType::Claude {
            Some(uuid::Uuid::new_v4().to_string())
        } else {
            None
        };
        Self {
            name: name.to_string(),
            agent_type: agent.to_string().to_lowercase(),
            agent_session_id,
            cwd: cwd.to_string(),
            failed_attempts: 0,
        }
    }

    /// Build the command string to resume this agent session.
    pub fn resume_command(&self) -> String {
        match self.agent_type.as_str() {
            "claude" => {
                if let Some(ref uuid) = self.agent_session_id {
                    format!("claude --dangerously-skip-permissions --resume {uuid}")
                } else {
                    "claude --dangerously-skip-permissions".to_string()
                }
            }
            "codex" => {
                "codex -c check_for_update_on_startup=false --yolo resume --last".to_string()
            }
            "gemini" => "gemini --yolo --resume".to_string(),
            _ => self.agent_type.clone(),
        }
    }

    /// Build the command string for initial session creation.
    /// For Claude, includes `--session-id` so we can resume later.
    pub fn create_command(&self) -> String {
        match self.agent_type.as_str() {
            "claude" => {
                if let Some(ref uuid) = self.agent_session_id {
                    format!("claude --dangerously-skip-permissions --session-id {uuid}")
                } else {
                    "claude --dangerously-skip-permissions".to_string()
                }
            }
            "codex" => "codex -c check_for_update_on_startup=false --yolo".to_string(),
            "gemini" => "gemini --yolo".to_string(),
            _ => self.agent_type.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_command_claude_with_uuid() {
        let record = SessionRecord {
            name: "alpha".to_string(),
            agent_type: "claude".to_string(),
            agent_session_id: Some("abc-123".to_string()),
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(
            record.resume_command(),
            "claude --dangerously-skip-permissions --resume abc-123"
        );
    }

    #[test]
    fn resume_command_claude_without_uuid() {
        let record = SessionRecord {
            name: "alpha".to_string(),
            agent_type: "claude".to_string(),
            agent_session_id: None,
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(
            record.resume_command(),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn resume_command_codex() {
        let record = SessionRecord {
            name: "bravo".to_string(),
            agent_type: "codex".to_string(),
            agent_session_id: None,
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(
            record.resume_command(),
            "codex -c check_for_update_on_startup=false --yolo resume --last"
        );
    }

    #[test]
    fn create_command_claude_with_uuid() {
        let record = SessionRecord {
            name: "alpha".to_string(),
            agent_type: "claude".to_string(),
            agent_session_id: Some("abc-123".to_string()),
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(
            record.create_command(),
            "claude --dangerously-skip-permissions --session-id abc-123"
        );
    }

    #[test]
    fn create_command_claude_without_uuid() {
        let record = SessionRecord {
            name: "alpha".to_string(),
            agent_type: "claude".to_string(),
            agent_session_id: None,
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(
            record.create_command(),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn create_command_codex() {
        let record = SessionRecord {
            name: "bravo".to_string(),
            agent_type: "codex".to_string(),
            agent_session_id: None,
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(
            record.create_command(),
            "codex -c check_for_update_on_startup=false --yolo"
        );
    }

    #[test]
    fn resume_command_custom_agent_returns_agent_type() {
        let record = SessionRecord {
            name: "s1".to_string(),
            agent_type: "aider".to_string(),
            agent_session_id: None,
            cwd: "/tmp".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(record.resume_command(), "aider");
    }

    #[test]
    fn create_command_custom_agent_returns_agent_type() {
        let record = SessionRecord {
            name: "s1".to_string(),
            agent_type: "aider".to_string(),
            agent_session_id: None,
            cwd: "/tmp".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(record.create_command(), "aider");
    }

    #[tokio::test]
    async fn roundtrip_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let pid = "test1234";

        let mut manifest = Manifest::default();
        manifest.sessions.insert(
            "alpha".to_string(),
            SessionRecord {
                name: "alpha".to_string(),
                agent_type: "claude".to_string(),
                agent_session_id: Some("uuid-1".to_string()),
                cwd: "/tmp/test".to_string(),
                failed_attempts: 0,
            },
        );
        manifest.sessions.insert(
            "bravo".to_string(),
            SessionRecord {
                name: "bravo".to_string(),
                agent_type: "codex".to_string(),
                agent_session_id: None,
                cwd: "/tmp/test".to_string(),
                failed_attempts: 0,
            },
        );

        save_manifest(base, pid, &manifest).await.unwrap();
        let loaded = load_manifest(base, pid).await;

        assert_eq!(loaded.sessions.len(), 2);
        assert!(loaded.sessions.contains_key("alpha"));
        assert!(loaded.sessions.contains_key("bravo"));
        assert_eq!(
            loaded.sessions["alpha"].agent_session_id,
            Some("uuid-1".to_string())
        );
        assert_eq!(loaded.sessions["bravo"].agent_session_id, None);
    }

    #[tokio::test]
    async fn load_manifest_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = load_manifest(dir.path(), "nonexistent").await;
        assert!(manifest.sessions.is_empty());
    }

    #[tokio::test]
    async fn corrupt_json_returns_empty_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let pid = "corrupt_test";
        let path = manifest_path(base, pid);
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "not valid json {{{").await.unwrap();

        let manifest = load_manifest(base, pid).await;
        assert!(manifest.sessions.is_empty());
    }

    #[tokio::test]
    async fn add_and_remove_session() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let pid = "test_add_remove";

        let record = SessionRecord {
            name: "alpha".to_string(),
            agent_type: "claude".to_string(),
            agent_session_id: Some("uuid-1".to_string()),
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        add_session(base, pid, record).await.unwrap();

        let manifest = load_manifest(base, pid).await;
        assert_eq!(manifest.sessions.len(), 1);
        assert!(manifest.sessions.contains_key("alpha"));

        remove_session(base, pid, "alpha").await.unwrap();
        let manifest = load_manifest(base, pid).await;
        assert!(manifest.sessions.is_empty());
    }

    #[test]
    fn manifest_path_contains_project_id() {
        let base = Path::new("/home/user/.hydra");
        let path = manifest_path(base, "abcd1234");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains("abcd1234"));
        assert!(path_str.ends_with("sessions.json"));
    }

    #[test]
    fn for_new_session_claude_has_uuid() {
        let record = SessionRecord::for_new_session("alpha", &AgentType::Claude, "/tmp");
        assert_eq!(record.agent_type, "claude");
        assert!(record.agent_session_id.is_some());
        assert_eq!(record.failed_attempts, 0);
    }

    #[test]
    fn for_new_session_codex_no_uuid() {
        let record = SessionRecord::for_new_session("bravo", &AgentType::Codex, "/tmp");
        assert_eq!(record.agent_type, "codex");
        assert!(record.agent_session_id.is_none());
    }

    #[test]
    fn for_new_session_gemini_no_uuid() {
        let record = SessionRecord::for_new_session("charlie", &AgentType::Gemini, "/tmp");
        assert_eq!(record.agent_type, "gemini");
        assert!(record.agent_session_id.is_none());
    }

    #[test]
    fn resume_command_gemini() {
        let record = SessionRecord {
            name: "charlie".to_string(),
            agent_type: "gemini".to_string(),
            agent_session_id: None,
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(record.resume_command(), "gemini --yolo --resume");
    }

    #[test]
    fn create_command_gemini() {
        let record = SessionRecord {
            name: "charlie".to_string(),
            agent_type: "gemini".to_string(),
            agent_session_id: None,
            cwd: "/tmp/test".to_string(),
            failed_attempts: 0,
        };
        assert_eq!(record.create_command(), "gemini --yolo");
    }

    #[test]
    fn failed_attempts_defaults_to_zero_on_deserialize() {
        let json = r#"{"name":"a","agent_type":"claude","agent_session_id":null,"cwd":"/tmp"}"#;
        let record: SessionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.failed_attempts, 0);
    }

    #[test]
    fn default_base_dir_contains_hydra() {
        let dir = default_base_dir();
        assert!(
            dir.to_string_lossy().ends_with(".hydra"),
            "default_base_dir should end with .hydra, got: {}",
            dir.display()
        );
    }

    #[tokio::test]
    async fn atomic_write_no_temp_file_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let pid = "atomic_test";

        let mut manifest = Manifest::default();
        manifest.sessions.insert(
            "alpha".to_string(),
            SessionRecord {
                name: "alpha".to_string(),
                agent_type: "claude".to_string(),
                agent_session_id: None,
                cwd: "/tmp".to_string(),
                failed_attempts: 0,
            },
        );

        save_manifest(base, pid, &manifest).await.unwrap();

        // The final file should exist and be valid JSON
        let path = manifest_path(base, pid);
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let loaded: Manifest = serde_json::from_str(&contents).unwrap();
        assert_eq!(loaded.sessions.len(), 1);

        // The temp file should not exist after a successful write
        let tmp_path = path.with_extension("json.tmp");
        assert!(
            !tmp_path.exists(),
            "temp file should be renamed away, not left behind"
        );
    }

    #[tokio::test]
    async fn concurrent_saves_dont_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let pid = "concurrent_test";

        // Pre-create the directory to avoid concurrent create_dir_all races
        let manifest_dir = base.join(pid);
        tokio::fs::create_dir_all(&manifest_dir).await.unwrap();

        // Run several saves concurrently
        let mut handles = Vec::new();
        for i in 0..10 {
            let base = base.clone();
            let pid = pid.to_string();
            handles.push(tokio::spawn(async move {
                let mut manifest = Manifest::default();
                manifest.sessions.insert(
                    format!("session-{i}"),
                    SessionRecord {
                        name: format!("session-{i}"),
                        agent_type: "claude".to_string(),
                        agent_session_id: None,
                        cwd: "/tmp".to_string(),
                        failed_attempts: 0,
                    },
                );
                save_manifest(&base, &pid, &manifest).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // The file should contain valid JSON (one of the concurrent writes wins)
        let path = manifest_path(&base, pid);
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let loaded: Manifest = serde_json::from_str(&contents).unwrap();
        assert!(
            !loaded.sessions.is_empty(),
            "manifest should contain at least one session from concurrent writes"
        );
    }
}
