use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyEventKind, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::Duration;

use std::sync::Arc;

use hydra::app::{Mode, StateSnapshot, UiApp};
use hydra::backend::Backend;
use hydra::event::{Event, EventHandler};
use hydra::session::{self, project_id, AgentType};
use hydra::tmux::SessionManager;
use hydra::tmux_control::{ControlModeSessionManager, TmuxControlConnection};
use hydra::{manifest, tmux, ui};

const EVENT_TICK_RATE: Duration = Duration::from_millis(50);

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
        /// Agent type (claude, codex, gemini)
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
        .args(["install", "--git", GITHUB_REPO_URL, "hydra", "--locked"])
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let term_backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(term_backend)?;

    // Try control mode first, fall back to subprocess-per-command.
    // Drop impl on TmuxControlConnection handles cleanup of the control session.
    let (manager, control_conn): (Box<dyn SessionManager>, Option<Arc<TmuxControlConnection>>) =
        match TmuxControlConnection::connect().await {
            Ok(conn) => {
                let arc = Arc::new(conn);
                (
                    Box::new(ControlModeSessionManager::new(Arc::clone(&arc))),
                    Some(arc),
                )
            }
            Err(_) => (Box::new(tmux::TmuxSessionManager::new()), None),
        };

    // Set up channels between Backend and UiApp
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(64);
    let (state_tx, state_rx) = tokio::sync::watch::channel(Arc::new(StateSnapshot::default()));
    let (preview_tx, preview_rx) = tokio::sync::mpsc::channel(16);

    let manifest_dir = manifest::default_base_dir();
    let backend = Backend::new(
        manager,
        project_id,
        cwd,
        manifest_dir,
        state_tx,
        preview_tx,
        control_conn,
    );

    // Spawn the backend actor task
    tokio::spawn(backend.run(cmd_rx));

    let mut app = UiApp::new(state_rx, preview_rx, cmd_tx);
    let mut events = EventHandler::new(EVENT_TICK_RATE);
    let mut prev_mouse_captured = true;

    // Draw initial frame before entering event loop
    terminal.draw(|frame| ui::draw(frame, &app))?;

    // Main loop: no .await calls — UI never blocks on I/O.
    loop {
        if app.should_quit {
            break;
        }

        match events.next().await {
            Some(Event::Key(key)) => {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key);
                    // In Compose mode, no preview refresh needed — user is typing.
                    // In Browse mode, refresh preview from cache for instant feedback.
                    if app.mode != Mode::Compose {
                        app.refresh_preview_from_cache();
                    }
                }
            }
            Some(Event::Paste(text)) => {
                app.handle_paste(text);
            }
            Some(Event::Mouse(mouse)) => {
                if !matches!(mouse.kind, MouseEventKind::Moved) {
                    let size = terminal.size()?;
                    let frame_area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
                    let layout = ui::compute_layout(frame_area);
                    app.handle_mouse(mouse, &layout);
                }
            }
            Some(Event::Tick) => {
                // Poll for backend state updates (non-blocking)
                app.poll_state();
            }
            Some(Event::Resize) => {
                app.needs_redraw = true;
            }
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

        // Only redraw when state has actually changed
        if app.needs_redraw {
            terminal.draw(|frame| ui::draw(frame, &app))?;
            app.needs_redraw = false;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
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
