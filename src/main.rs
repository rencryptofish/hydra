use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, KeyEventKind, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use minisign_verify::{PublicKey, Signature};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::env::consts::{ARCH, OS};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use hydra::app::{App, Mode};
use hydra::event::{Event, EventHandler};
use hydra::session::{self, project_id, AgentType};
use hydra::{manifest, tmux, ui};

const EVENT_TICK_RATE: Duration = Duration::from_millis(50);
const SESSION_REFRESH_INTERVAL_TICKS: u8 = 4;

// Minisign Ed25519 public key for verifying release binaries.
// This is the second line (base64 key data) from the .pub file.
// Generate keypair: minisign -G -p hydra.pub -s hydra.key
// Sign binary:      minisign -Sm hydra-darwin-aarch64 -s hydra.key
const UPDATE_PUBLIC_KEY: &str =
    "RWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

const GITHUB_RELEASES_URL: &str =
    "https://api.github.com/repos/rencryptofish/hydra/releases/latest";

#[derive(Parser)]
#[command(name = "hydra", version, about = "AI Agent tmux session manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new agent session
    New {
        /// Agent type (claude, codex)
        agent: String,
        /// Session name
        name: String,
    },
    /// Kill a session
    Kill {
        /// Session name
        name: String,
    },
    /// List sessions for the current project
    Ls,
    /// Update hydra to the latest version from GitHub
    Update,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let cwd = std::env::current_dir()
        .context("Failed to get current directory")?
        .to_string_lossy()
        .to_string();
    let pid = project_id(&cwd);

    match cli.command {
        Some(Commands::New { agent, name }) => cmd_new(&pid, &name, &agent, &cwd).await,
        Some(Commands::Kill { name }) => cmd_kill(&pid, &name).await,
        Some(Commands::Ls) => cmd_ls(&pid).await,
        Some(Commands::Update) => cmd_update().await,
        None => run_tui(pid, cwd).await,
    }
}

async fn cmd_new(project_id: &str, name: &str, agent_str: &str, cwd: &str) -> Result<()> {
    let agent: AgentType = agent_str.parse()?;
    let record = manifest::SessionRecord::for_new_session(name, &agent, cwd);
    let cmd = record.create_command();
    let base_dir = manifest::default_base_dir();

    let tmux_name = tmux::create_session(project_id, name, &agent, cwd, Some(&cmd)).await?;
    manifest::add_session(&base_dir, project_id, record).await?;
    println!("Created session: {tmux_name}");
    Ok(())
}

async fn cmd_kill(project_id: &str, name: &str) -> Result<()> {
    let tmux_name = session::tmux_session_name(project_id, name);
    tmux::kill_session(&tmux_name).await?;
    let base_dir = manifest::default_base_dir();
    let _ = manifest::remove_session(&base_dir, project_id, name).await;
    println!("Killed session: {tmux_name}");
    Ok(())
}

async fn cmd_ls(project_id: &str) -> Result<()> {
    let manager = tmux::TmuxSessionManager::new();
    let sessions = tmux::SessionManager::list_sessions(&manager, project_id).await?;
    if sessions.is_empty() {
        println!("No sessions for this project.");
    } else {
        for s in &sessions {
            println!("{} [{}]", s.name, s.agent_type);
        }
    }
    Ok(())
}

/// Maps the current OS/ARCH to the expected GitHub Release asset name.
fn platform_asset_name() -> Result<String> {
    let os = match OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => anyhow::bail!(
            "Unsupported OS: {other}. Install manually with: cargo install --git https://github.com/rencryptofish/hydra.git"
        ),
    };
    let arch = match ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => anyhow::bail!(
            "Unsupported architecture: {other}. Install manually with: cargo install --git https://github.com/rencryptofish/hydra.git"
        ),
    };
    Ok(format!("hydra-{os}-{arch}"))
}

/// Fetches the latest release metadata from the GitHub API via curl.
fn fetch_release_metadata() -> Result<serde_json::Value> {
    let output = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "30",
            "-H",
            "Accept: application/vnd.github.v3+json",
            GITHUB_RELEASES_URL,
        ])
        .output()
        .context("Failed to run curl — is curl on PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("GitHub API request failed: {stderr}");
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("Failed to parse GitHub API response")?;
    Ok(json)
}

/// Extracts the binary URL, .minisig URL, and tag name from release JSON.
fn find_asset_urls(
    release: &serde_json::Value,
    asset_name: &str,
) -> Result<(String, String, String)> {
    let tag = release["tag_name"]
        .as_str()
        .context("Release JSON missing 'tag_name'")?
        .to_string();

    let assets = release["assets"]
        .as_array()
        .context("Release JSON missing 'assets' array")?;

    let mut binary_url: Option<String> = None;
    let mut sig_url: Option<String> = None;
    let mut available: Vec<String> = Vec::new();

    let sig_name = format!("{asset_name}.minisig");

    for asset in assets {
        if let Some(name) = asset["name"].as_str() {
            available.push(name.to_string());
            if let Some(url) = asset["browser_download_url"].as_str() {
                if name == asset_name {
                    binary_url = Some(url.to_string());
                } else if name == sig_name {
                    sig_url = Some(url.to_string());
                }
            }
        }
    }

    let binary_url = binary_url.with_context(|| {
        format!(
            "Binary '{asset_name}' not found in release {tag}. Available assets: {}",
            available.join(", ")
        )
    })?;

    let sig_url = sig_url.with_context(|| {
        format!(
            "Signature '{sig_name}' not found in release {tag} — release may not be signed. Available assets: {}",
            available.join(", ")
        )
    })?;

    Ok((binary_url, sig_url, tag))
}

/// Downloads a URL to a Vec<u8> via curl.
fn download_bytes(url: &str, label: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "120",
            url,
        ])
        .output()
        .with_context(|| format!("Failed to download {label} — is curl on PATH?"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to download {label}: {stderr}");
    }

    if output.stdout.is_empty() {
        anyhow::bail!("Downloaded {label} is empty");
    }

    Ok(output.stdout)
}

/// Verifies binary bytes against a minisig signature using the embedded public key.
fn verify_signature(binary: &[u8], sig_content: &[u8]) -> Result<()> {
    let pk = PublicKey::decode(UPDATE_PUBLIC_KEY)
        .map_err(|e| anyhow::anyhow!("Invalid embedded public key: {e}"))?;

    let sig_str = std::str::from_utf8(sig_content).context("Signature file is not valid UTF-8")?;
    let sig = Signature::decode(sig_str)
        .map_err(|e| anyhow::anyhow!("Failed to parse signature: {e}"))?;

    pk.verify(binary, &sig, false).map_err(|e| {
        anyhow::anyhow!(
            "\n\x1b[1;31m!!! SIGNATURE VERIFICATION FAILED !!!\x1b[0m\n\
             \n\
             The downloaded binary does NOT match the expected signature.\n\
             This could indicate a compromised release or man-in-the-middle attack.\n\
             The binary has NOT been written to disk.\n\
             \n\
             Error: {e}"
        )
    })
}

/// Atomically replaces the current binary with the new one.
fn replace_binary(new_binary: &[u8]) -> Result<()> {
    let current = std::env::current_exe().context("Failed to determine current executable path")?;
    let current = current
        .canonicalize()
        .context("Failed to canonicalize executable path")?;
    replace_binary_at(new_binary, &current)
}

/// Atomically replaces the binary at `target` with the new bytes.
/// Writes to a temp file in the same directory, sets permissions, then renames.
fn replace_binary_at(new_binary: &[u8], target: &std::path::Path) -> Result<()> {
    let dir = target
        .parent()
        .context("Target path has no parent directory")?;

    let tmp_name = format!(".hydra-update-{}.tmp", std::process::id());
    let tmp_path = dir.join(&tmp_name);

    // Write to temp file
    if let Err(e) = std::fs::write(&tmp_path, new_binary) {
        let _ = std::fs::remove_file(&tmp_path);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::bail!(
                "Permission denied writing to {}. You may need to run with sudo.",
                dir.display()
            );
        }
        return Err(e).context("Failed to write temporary binary");
    }

    // Set executable permissions (unix only)
    #[cfg(unix)]
    {
        if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
        {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e).context("Failed to set executable permissions");
        }
    }

    // Atomic rename
    if let Err(e) = std::fs::rename(&tmp_path, target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).with_context(|| {
            format!(
                "Failed to replace binary at {}. You may need to run with sudo.",
                target.display()
            )
        });
    }

    Ok(())
}

async fn cmd_update() -> Result<()> {
    println!("Checking for updates...");

    let asset_name = platform_asset_name()?;
    println!("Platform: {asset_name}");

    let release = fetch_release_metadata()?;
    let (binary_url, sig_url, tag) = find_asset_urls(&release, &asset_name)?;
    println!("Latest release: {tag}");

    println!("Downloading binary...");
    let binary = download_bytes(&binary_url, "binary")?;
    println!("Downloaded {} bytes", binary.len());

    println!("Downloading signature...");
    let sig = download_bytes(&sig_url, "signature")?;

    println!("Verifying signature...");
    verify_signature(&binary, &sig)?;
    println!("Signature verified OK");

    println!("Installing...");
    replace_binary(&binary)?;

    println!("hydra updated to {tag} successfully.");
    Ok(())
}

async fn run_tui(project_id: String, cwd: String) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(project_id, cwd);
    app.revive_sessions().await;
    app.refresh_sessions().await;
    app.refresh_preview().await;

    let mut events = EventHandler::new(EVENT_TICK_RATE);
    let mut prev_mouse_captured = true;
    let mut session_refresh_tick = 0u8;
    // Track when a keystroke last triggered a preview capture, so the tick
    // handler can skip redundant refresh_preview in Attached mode.
    let mut last_key_refresh = std::time::Instant::now() - Duration::from_secs(1);

    // Draw initial frame before entering event loop
    terminal.draw(|frame| ui::draw(frame, &app))?;

    // Main loop: process events first, then draw — eliminates 0-250ms
    // input→display latency from the old draw-then-wait pattern.
    loop {
        if app.should_quit {
            break;
        }

        match events.next().await {
            Some(Event::Key(key)) => {
                if key.kind == KeyEventKind::Press {
                    let was_attached = app.mode == Mode::Attached;
                    app.handle_key(key).await;
                    if was_attached && app.mode == Mode::Attached {
                        // Force a live pane capture on typed input so attached-mode
                        // echo feels immediate rather than waiting for the tick.
                        app.refresh_preview_live().await;
                        last_key_refresh = std::time::Instant::now();
                    } else if !was_attached {
                        app.refresh_preview_from_cache();
                    }
                }
            }
            Some(Event::Mouse(mouse)) => {
                let should_refresh_preview = !matches!(mouse.kind, MouseEventKind::Moved);
                app.handle_mouse(mouse);
                app.flush_pending_keys().await;
                if should_refresh_preview {
                    app.refresh_preview().await;
                }
            }
            Some(Event::Tick) => {
                session_refresh_tick = session_refresh_tick.wrapping_add(1);
                if session_refresh_tick % SESSION_REFRESH_INTERVAL_TICKS == 0 {
                    app.refresh_sessions().await;
                    // In Attached mode, skip preview refresh if a keystroke just
                    // triggered one — avoids redundant tmux capture subprocess.
                    let key_just_refreshed = app.mode == Mode::Attached
                        && last_key_refresh.elapsed() < Duration::from_millis(200);
                    if !key_just_refreshed {
                        app.refresh_preview().await;
                    }
                }
                app.refresh_messages();
            }
            Some(Event::Resize) => {}
            None => break,
        }

        // Toggle mouse capture when the flag changes
        if app.mouse_captured != prev_mouse_captured {
            if app.mouse_captured {
                execute!(terminal.backend_mut(), EnableMouseCapture)?;
            } else {
                execute!(terminal.backend_mut(), DisableMouseCapture)?;
            }
            prev_mouse_captured = app.mouse_captured;
        }

        // Draw after event handling — user sees result immediately
        terminal.draw(|frame| ui::draw(frame, &app))?;
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn test_platform_asset_name() {
        let name = platform_asset_name().unwrap();
        assert!(name.starts_with("hydra-"), "expected hydra- prefix: {name}");
        let valid = [
            "hydra-darwin-aarch64",
            "hydra-darwin-x86_64",
            "hydra-linux-aarch64",
            "hydra-linux-x86_64",
        ];
        assert!(valid.contains(&name.as_str()), "unexpected platform: {name}");
    }

    #[test]
    fn test_find_asset_urls_success() {
        let json = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [
                {
                    "name": "hydra-darwin-aarch64",
                    "browser_download_url": "https://example.com/hydra-darwin-aarch64"
                },
                {
                    "name": "hydra-darwin-aarch64.minisig",
                    "browser_download_url": "https://example.com/hydra-darwin-aarch64.minisig"
                },
                {
                    "name": "hydra-linux-x86_64",
                    "browser_download_url": "https://example.com/hydra-linux-x86_64"
                }
            ]
        });

        let (bin_url, sig_url, tag) =
            find_asset_urls(&json, "hydra-darwin-aarch64").unwrap();
        assert_eq!(bin_url, "https://example.com/hydra-darwin-aarch64");
        assert_eq!(sig_url, "https://example.com/hydra-darwin-aarch64.minisig");
        assert_eq!(tag, "v0.2.0");
    }

    #[test]
    fn test_find_asset_urls_missing_asset() {
        let json = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [
                {
                    "name": "hydra-linux-x86_64",
                    "browser_download_url": "https://example.com/hydra-linux-x86_64"
                }
            ]
        });

        let err = find_asset_urls(&json, "hydra-darwin-aarch64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hydra-darwin-aarch64"), "should mention missing asset: {msg}");
        assert!(msg.contains("hydra-linux-x86_64"), "should list available assets: {msg}");
    }

    #[test]
    fn test_find_asset_urls_missing_signature() {
        let json = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [
                {
                    "name": "hydra-darwin-aarch64",
                    "browser_download_url": "https://example.com/hydra-darwin-aarch64"
                }
            ]
        });

        let err = find_asset_urls(&json, "hydra-darwin-aarch64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("minisig"), "should mention missing signature: {msg}");
        assert!(msg.contains("may not be signed"), "should note unsigned: {msg}");
    }

    #[test]
    fn test_verify_signature_bad_key() {
        let binary = b"fake binary content";
        // A syntactically valid but wrong minisig — will fail verification
        let bad_sig = b"untrusted comment: signature\nRWAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==\ntrusted comment: bad\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==\n";

        let result = verify_signature(binary, bad_sig);
        assert!(result.is_err(), "should reject invalid signature");
    }

    // ── find_asset_urls edge cases ────────────────────────────────────

    #[test]
    fn test_find_asset_urls_missing_tag_name() {
        let json = serde_json::json!({
            "assets": [{"name": "hydra-darwin-aarch64", "browser_download_url": "https://example.com/bin"}]
        });
        let err = find_asset_urls(&json, "hydra-darwin-aarch64").unwrap_err();
        assert!(err.to_string().contains("tag_name"));
    }

    #[test]
    fn test_find_asset_urls_missing_assets_array() {
        let json = serde_json::json!({
            "tag_name": "v1.0.0"
        });
        let err = find_asset_urls(&json, "hydra-darwin-aarch64").unwrap_err();
        assert!(err.to_string().contains("assets"));
    }

    #[test]
    fn test_find_asset_urls_asset_without_download_url() {
        let json = serde_json::json!({
            "tag_name": "v1.0.0",
            "assets": [
                {"name": "hydra-darwin-aarch64"},
                {"name": "hydra-darwin-aarch64.minisig"}
            ]
        });
        let err = find_asset_urls(&json, "hydra-darwin-aarch64").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_find_asset_urls_empty_assets() {
        let json = serde_json::json!({
            "tag_name": "v1.0.0",
            "assets": []
        });
        let err = find_asset_urls(&json, "hydra-darwin-aarch64").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ── verify_signature edge cases ──────────────────────────────────

    #[test]
    fn test_verify_signature_non_utf8_sig() {
        let binary = b"fake binary";
        let bad_sig: &[u8] = &[0xFF, 0xFE, 0xFD]; // invalid UTF-8
        let result = verify_signature(binary, bad_sig);
        assert!(result.is_err());
        // May fail on UTF-8 or on public key decode (placeholder key)
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("UTF-8") || msg.contains("public key"), "unexpected error: {msg}");
    }

    #[test]
    fn test_verify_signature_empty_sig() {
        let binary = b"fake binary";
        let result = verify_signature(binary, b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_signature_garbage_text() {
        let binary = b"fake binary";
        let result = verify_signature(binary, b"this is not a minisig signature at all");
        assert!(result.is_err());
    }

    // ── replace_binary_at tests ──────────────────────────────────────

    #[test]
    fn test_replace_binary_at_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hydra-test-bin");
        std::fs::write(&target, b"old content").unwrap();

        let new_content = b"new binary content here";
        replace_binary_at(new_content, &target).unwrap();

        let written = std::fs::read(&target).unwrap();
        assert_eq!(written, new_content);
    }

    #[cfg(unix)]
    #[test]
    fn test_replace_binary_at_sets_executable_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hydra-test-bin-perms");
        std::fs::write(&target, b"old").unwrap();

        replace_binary_at(b"new", &target).unwrap();

        let meta = std::fs::metadata(&target).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "should have 0o755 permissions");
    }

    #[test]
    fn test_replace_binary_at_nonexistent_parent_dir() {
        let target = std::path::Path::new("/nonexistent/dir/hydra-bin");
        let result = replace_binary_at(b"content", target);
        assert!(result.is_err());
    }

    // ── CLI parsing tests ────────────────────────────────────────────

    #[test]
    fn test_cli_parsing_new_command() {
        let cli = Cli::parse_from(["hydra", "new", "claude", "alpha"]);
        match cli.command {
            Some(Commands::New { agent, name }) => {
                assert_eq!(agent, "claude");
                assert_eq!(name, "alpha");
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn test_cli_parsing_kill_command() {
        let cli = Cli::parse_from(["hydra", "kill", "alpha"]);
        match cli.command {
            Some(Commands::Kill { name }) => assert_eq!(name, "alpha"),
            other => panic!("expected Kill, got {other:?}"),
        }
    }

    #[test]
    fn test_cli_parsing_ls_command() {
        let cli = Cli::parse_from(["hydra", "ls"]);
        assert!(matches!(cli.command, Some(Commands::Ls)));
    }

    #[test]
    fn test_cli_parsing_update_command() {
        let cli = Cli::parse_from(["hydra", "update"]);
        assert!(matches!(cli.command, Some(Commands::Update)));
    }

    #[test]
    fn test_cli_parsing_no_command() {
        let cli = Cli::parse_from(["hydra"]);
        assert!(cli.command.is_none());
    }
}