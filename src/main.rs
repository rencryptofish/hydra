use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, KeyEventKind, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

use hydra::app::{App, Mode};
use hydra::event::{Event, EventHandler};
use hydra::session::{self, project_id, AgentType};
use hydra::tmux::SessionManager;
use hydra::tmux_control::{ControlModeSessionManager, TmuxControlConnection};
use hydra::{manifest, tmux, ui};

const EVENT_TICK_RATE: Duration = Duration::from_millis(50);
const SESSION_REFRESH_INTERVAL_TICKS: u8 = 4;

const GITHUB_REPO_URL: &str = "https://github.com/rencryptofish/hydra.git";

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

async fn cmd_update() -> Result<()> {
    println!("Updating hydra from latest commit...");
    let status = std::process::Command::new("cargo")
        .args(["install", "--git", GITHUB_REPO_URL, "--package", "hydra", "--locked"])
        .env("CARGO_NET_GIT_FETCH_WITH_CLI", "true")
        .status()
        .context("Failed to run cargo — is cargo on PATH?")?;
    if !status.success() {
        anyhow::bail!("cargo install failed");
    }
    println!("hydra updated successfully.");
    Ok(())
}

async fn run_tui(project_id: String, cwd: String) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Try control mode first, fall back to subprocess-per-command.
    // Drop impl on TmuxControlConnection handles cleanup of the control session.
    let manager: Box<dyn SessionManager> = match TmuxControlConnection::connect().await {
        Ok(conn) => Box::new(ControlModeSessionManager::new(conn)),
        Err(_) => Box::new(tmux::TmuxSessionManager::new()),
    };

    let mut app = App::new_with_manager(project_id, cwd, manager);
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
                if session_refresh_tick.is_multiple_of(SESSION_REFRESH_INTERVAL_TICKS) {
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
    fn test_github_repo_url() {
        assert!(GITHUB_REPO_URL.starts_with("https://"));
        assert!(GITHUB_REPO_URL.ends_with(".git"));
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
