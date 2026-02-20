mod app;
mod event;
mod logs;
mod session;
mod tmux;
mod ui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEvent, KeyEventKind},
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
    let tmux_name = tmux::create_session(project_id, name, &agent, cwd).await?;
    println!("Created session: {tmux_name}");
    Ok(())
}

async fn cmd_kill(project_id: &str, name: &str) -> Result<()> {
    let tmux_name = session::tmux_session_name(project_id, name);
    tmux::kill_session(&tmux_name).await?;
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
    app.refresh_sessions().await;
    app.refresh_preview().await;

    let mut events = EventHandler::new(Duration::from_millis(250));

    // Main loop
    loop {
        terminal.draw(|frame| ui::draw(frame, &app))?;

        if app.should_quit {
            break;
        }

        match events.next().await {
            Some(Event::Key(key)) => {
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key).await;
                    app.refresh_preview().await;
                }
            }
            Some(Event::Mouse(mouse)) => {
                app.handle_mouse(mouse);
                app.refresh_preview().await;
            }
            Some(Event::Tick) => {
                app.refresh_sessions().await;
                app.refresh_preview().await;
                app.refresh_messages().await;
            }
            Some(Event::Resize) => {}
            None => break,
        }
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

async fn handle_key(app: &mut App, key: KeyEvent) {
    match app.mode {
        Mode::Browse => handle_browse_key(app, key.code),
        Mode::Attached => handle_attached_key(app, key).await,
        Mode::NewSessionName => handle_name_input_key(app, key.code),
        Mode::NewSessionAgent => handle_agent_select_key(app, key.code).await,
        Mode::ConfirmDelete => handle_confirm_delete_key(app, key.code).await,
    }
}

fn handle_browse_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Enter => app.attach_selected(),
        KeyCode::Char('n') => app.start_new_session(),
        KeyCode::Char('d') => app.request_delete(),
        _ => {}
    }
}

fn handle_name_input_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Enter => app.submit_session_name(),
        KeyCode::Esc => app.cancel_mode(),
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => {
            // Only allow valid tmux session name chars
            if c.is_alphanumeric() || c == '-' || c == '_' {
                app.input.push(c);
            }
        }
        _ => {}
    }
}

async fn handle_agent_select_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Enter => app.confirm_new_session().await,
        KeyCode::Esc => app.cancel_mode(),
        KeyCode::Char('j') | KeyCode::Down => app.agent_select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.agent_select_prev(),
        _ => {}
    }
}

async fn handle_attached_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Esc {
        app.detach();
        return;
    }

    if let Some(session) = app.sessions.get(app.selected) {
        if let Some(tmux_key) = tmux::keycode_to_tmux(key.code, key.modifiers) {
            let tmux_name = session.tmux_name.clone();
            let _ = tmux::send_keys(&tmux_name, &tmux_key).await;
        }
    }
}

async fn handle_confirm_delete_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('y') => app.confirm_delete().await,
        KeyCode::Esc | KeyCode::Char('n') => app.cancel_mode(),
        _ => {}
    }
}

