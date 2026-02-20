mod app;
mod event;
mod logs;
mod manifest;
mod session;
mod tmux;
mod ui;

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

use app::{App, Mode};
use event::{Event, EventHandler};
use session::{project_id, AgentType};

const EVENT_TICK_RATE: Duration = Duration::from_millis(100);
const SESSION_REFRESH_INTERVAL_TICKS: u8 = 2;

#[derive(Parser)]
#[command(name = "hydra", about = "AI Agent tmux session manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
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
                    } else if !was_attached {
                        app.refresh_preview().await;
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
                    app.refresh_preview().await;
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
