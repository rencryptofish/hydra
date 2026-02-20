# Hydra

TUI-based AI agent tmux session manager. Lets you run multiple Claude/Codex agents in parallel, each in its own tmux session, with a sidebar+preview layout.

## Build & Test

```bash
cargo build
cargo test                # unit + snapshot + CLI integration tests
cargo insta review        # review snapshot diffs after UI changes
```

## Architecture

Single-binary Rust TUI (ratatui + crossterm + tokio):

- **`src/main.rs`** — CLI parsing (clap), TUI event loop, key dispatch. Passes full `KeyEvent` (not just `KeyCode`) to handlers for modifier support.
- **`src/app.rs`** — `App` state + `Mode` enum (Browse, Attached, NewSessionName, NewSessionAgent, ConfirmDelete). Owns `Box<dyn SessionManager>` for testability.
- **`src/tmux.rs`** — `SessionManager` trait + `TmuxSessionManager` impl. All tmux subprocess calls live here. Also has `keycode_to_tmux()` for crossterm→tmux key mapping.
- **`src/session.rs`** — `Session`, `SessionStatus`, `AgentType` types. Pure data, no I/O.
- **`src/ui.rs`** — All ratatui rendering. Snapshot-tested with `insta`.
- **`src/logs.rs`** — Claude Code log file reader. Traces tmux pane PID → `lsof` → `.claude/tasks/<uuid>/` → JSONL log file. Extracts last assistant message per session.
- **`src/event.rs`** — Async crossterm event reader (keys, mouse, tick, resize).

## Key Patterns

- **SessionManager trait**: All tmux interaction goes through `trait SessionManager` so tests can use mock/noop impls. `App::new_with_manager()` is the test constructor.
- **Content-change detection**: Session status (Running/Idle/Exited) is determined by comparing `capture-pane` output between ticks, not `session_activity` (which only tracks client input, not pane output for detached sessions).
- **Status indicator lights**: Each session shows a colored `●` dot in the sidebar:
  - **Green** = Idle (ready for input, pane content unchanged between ticks)
  - **Red** = Running (busy, pane content changed since last tick)
  - **Yellow** = Exited (agent process ended, pane is dead)
  - Note: On first tick after launch, all non-exited sessions flash Red (no previous capture to compare). Stabilizes by second tick.
- **Task elapsed timer**: Tracks per-session `Instant` timestamps in App. Running starts the clock; Idle <5s shows frozen duration (same task); Idle >5s clears it (new task).
- **Embedded attach mode**: `Mode::Attached` forwards keystrokes via `tmux send-keys` instead of `tmux attach`. Preview border turns green, help bar updates. Esc returns to Browse.
- **Last message display**: Sidebar shows the last Claude assistant message per session (dimmed second line, truncated to 50 chars). UUIDs are resolved once via PID→lsof and cached in `App.log_uuids`. Messages refresh every 20 ticks (~5s). Only reads last 200KB of JSONL for efficiency.
- **Claude Code JSONL logs**: Located at `~/.claude/projects/<escaped-cwd>/<uuid>.jsonl`. Path escaping replaces `/` with `-` (e.g. `/Users/monkey/hydra` → `-Users-monkey-hydra`). Structure: `{"type": "assistant", "message": {"content": [{"text": "..."}]}}`. The UUID is discovered via `lsof -p <pane_pid>` looking for `.claude/tasks/<uuid>/` open file descriptors.
- **remain-on-exit**: Set on session creation so exited agents stay visible with `Exited` status instead of vanishing.
- **Agent type caching**: `TmuxSessionManager` caches `HYDRA_AGENT_TYPE` env var lookups in a `Mutex<HashMap>` to avoid repeated `tmux show-environment` calls on every tick.

## Testing

- **Unit tests**: `src/session.rs` (pure functions: project_id, name parsing, AgentType) and `src/app.rs` (state machine: mode transitions, navigation, create/delete flows)
- **Snapshot tests**: `src/ui.rs` uses `insta::assert_snapshot!` with `ratatui::backend::TestBackend` (80x24). Run `cargo insta review` after intentional UI changes. Snapshots live in `src/snapshots/`.
- **CLI tests**: `tests/cli_tests.rs` with `assert_cmd` — tests help, ls, arg validation, unknown commands
- **Mock**: `MockSessionManager` in app tests (controllable return values), `NoopSessionManager` in UI tests — both implement full `SessionManager` trait
- When adding a new `SessionManager` method, update mock impls in BOTH `app.rs` and `ui.rs` test modules

## Conventions

- Tick rate is 250ms (`EventHandler::new(Duration::from_millis(250))`)
- tmux session names: `hydra-<8char_sha256_hex>-<user_name>`
- Agent commands: `claude --dangerously-skip-permissions`, `codex --yolo`
- Mouse handling lives in `App::handle_mouse()` (moved from `main.rs` to `app.rs`)
- **Preview scrolling**: `preview_scroll_offset: u16` tracks lines scrolled up from bottom (0 = bottom). Scroll wheel over preview adjusts by 3 lines/tick. Offset resets on session selection change. Rendering uses `Paragraph::scroll()` with math: `scroll_y = max_scroll_offset - capped_offset` so offset 0 shows latest output.
- **Scrollback capture**: `capture_pane_scrollback()` uses `tmux capture-pane -p -S -` to get full history (used for preview display). Regular `capture_pane()` (visible pane only) is used for status comparison — keeps status detection lightweight.
- **No mouse forwarding to tmux**: SGR mouse sequences and arrow-key forwarding for scroll were removed — agents don't support mouse input, and forwarding caused garbled text. Mouse clicks/scroll in the preview are handled locally (scroll viewport, attach/detach).

## Common Changes

- **Add agent type**: Add variant to `AgentType` in `session.rs`, implement `command()`, `Display`, `FromStr`, update `all()`, update tests
- **Add UI mode**: Add variant to `Mode` in `app.rs`, add key handler in `main.rs`, add draw function in `ui.rs`, add snapshot test
- **Add SessionManager method**: Update trait in `tmux.rs`, implement on `TmuxSessionManager`, update mocks in `app.rs` and `ui.rs` test modules
- **Change status colors**: Edit `status_color()` in `ui.rs` — maps `SessionStatus` → `ratatui::Color`
