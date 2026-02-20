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
- **`src/app.rs`** — `App` state + `Mode` enum (Browse, Attached, NewSessionAgent, ConfirmDelete). Owns `Box<dyn SessionManager>` for testability.
- **`src/tmux.rs`** — `SessionManager` async trait (`#[async_trait]`) + `TmuxSessionManager` impl. All tmux subprocess calls use `tokio::process::Command` (non-blocking). Also has `keycode_to_tmux()` for crossterm→tmux key mapping.
- **`src/session.rs`** — `Session`, `SessionStatus`, `AgentType` types. Pure data, no I/O.
- **`src/ui.rs`** — All ratatui rendering. Snapshot-tested with `insta`.
- **`src/logs.rs`** — Claude Code log file reader. Traces tmux pane PID → `lsof` → `.claude/tasks/<uuid>/` → JSONL log file. Extracts last assistant message per session. Also provides `SessionStats` + `update_session_stats()` for incremental JSONL metrics (tokens, cost, tool calls, files touched).
- **`src/manifest.rs`** — Session persistence for revival across restarts. `SessionRecord` + `Manifest` types (serde), file I/O with `tokio::fs`. Stores at `~/.hydra/<project_id>/sessions.json`. All functions take `base_dir: &Path` for testability. Includes `SessionRecord::for_new_session()` constructor and `resume_command()`/`create_command()` builders.
- **`src/event.rs`** — Async crossterm event reader (keys, mouse, tick, resize).

## Key Patterns

- **SessionManager trait**: All tmux interaction goes through `#[async_trait] trait SessionManager: Send + Sync` so tests can use mock/noop impls. `App::new_with_manager()` is the test constructor. Async methods that call the manager must clone fields (e.g. `project_id`) before `.await` to avoid borrow conflicts across await points.
- **Auto-generated session names**: Session names are auto-assigned from the NATO phonetic alphabet (alpha, bravo, charlie, ...). The `generate_name()` function in `session.rs` picks the first unused name, filling gaps. Falls back to `agent-N` if all 26 are taken.
- **Content-change detection**: Session status (Running/Idle/Exited) is determined by comparing `capture-pane` output between ticks, not `session_activity` (which only tracks client input, not pane output for detached sessions). Running→Idle is **debounced**: requires 3 consecutive unchanged ticks (~750ms) to prevent flickering. Idle→Running is instant (any content change).
- **Sidebar stability**: Sessions are sorted alphabetically by name only (not by status). Status is conveyed by the colored dot — sorting by status caused constant list reordering as agents alternate between Running and Idle.
- **Status indicator lights**: Each session shows a colored `●` dot in the sidebar:
  - **Green** = Idle (ready for input, pane content unchanged for 3+ ticks)
  - **Red** = Running (busy, pane content changed recently)
  - **Yellow** = Exited (agent process ended, pane is dead)
- **Task elapsed timer**: Tracks per-session `Instant` timestamps in App. Running starts the clock; Idle <5s shows frozen duration (same task); Idle >5s clears it (new task).
- **Embedded attach mode**: `Mode::Attached` forwards keystrokes via `tmux send-keys` instead of `tmux attach`. Preview border switches to thick green (`BorderType::Thick` + bold) for clear visual distinction. Esc returns to Browse.
- **Last message display**: Sidebar shows the last Claude assistant message per session (dimmed second line, truncated to 50 chars). UUIDs are resolved once via PID→lsof and cached in `App.log_uuids`. Messages refresh every 20 ticks (~5s). Only reads last 200KB of JSONL for efficiency.
- **Claude Code JSONL logs**: Located at `~/.claude/projects/<escaped-cwd>/<uuid>.jsonl`. Path escaping replaces `/` with `-` (e.g. `/Users/monkey/hydra` → `-Users-monkey-hydra`). Structure: `{"type": "assistant", "message": {"content": [{"text": "..."}]}}`. The UUID is discovered by parsing `--session-id` from the process command line (`ps -p <pid> -o command=`), falling back to `lsof -p <pane_pid>` for legacy sessions without `--session-id`.
- **remain-on-exit**: Set on session creation so exited agents stay visible with `Exited` status instead of vanishing.
- **Agent type caching**: `TmuxSessionManager` caches `HYDRA_AGENT_TYPE` env var lookups in a `std::sync::Mutex<HashMap>` to avoid repeated `tmux show-environment` calls on every tick. Uses `std::sync::Mutex` (not tokio) since the lock is never held across `.await` points. Cache is also pre-populated on `create_session`.
- **Async I/O**: All tmux subprocess calls use `tokio::process::Command` instead of `std::process::Command` to avoid blocking the event loop. Key press events trigger immediate `refresh_preview()` for responsive navigation (no waiting for next tick).
- **Session stats**: `SessionStats` in `logs.rs` tracks per-session metrics (turns, tokens in/out, cache tokens, edits, bash commands, unique files). Updated incrementally via `update_session_stats()` which reads only new bytes since last offset — fast even on 100MB+ logs. Stats refresh on the same 20-tick cadence as last messages. Rendered in a bordered "Stats" block at the bottom of the sidebar. Cost estimate uses Sonnet pricing ($3/$15 per MTok in/out).
- **Session persistence / revival**: `manifest.rs` saves session metadata to `~/.hydra/<project_id>/sessions.json`. On startup, `revive_sessions()` loads the manifest, compares against live tmux sessions, and recreates missing ones using each agent's resume command (Claude: `--resume <UUID>`, Codex: `resume --last`). Failed revival attempts are tracked per-record (`failed_attempts`); entries are pruned after `MAX_FAILED_ATTEMPTS` (3) consecutive failures. Manifest is updated on session create/delete.
- **Per-file diff tree**: Sidebar shows a "Changes" block with directory-grouped file diffs from `git diff --numstat`. `DiffFile` struct in `app.rs` holds path/insertions/deletions. `build_diff_tree_lines()` in `ui.rs` groups files by directory, shows compact `+N-N` stats with color coding (green/red). Refreshes on each `refresh_sessions()` tick.

## Testing

- **Unit tests**: `src/session.rs` (pure functions: project_id, name parsing, AgentType) and `src/app.rs` (state machine: mode transitions, navigation, create/delete flows)
- **Snapshot tests**: `src/ui.rs` uses `insta::assert_snapshot!` with `ratatui::backend::TestBackend` (80x24). Run `cargo insta review` after intentional UI changes. Snapshots live in `src/snapshots/`.
- **CLI tests**: `tests/cli_tests.rs` with `assert_cmd` — tests help, ls, arg validation, unknown commands
- **Mock**: `MockSessionManager` in app tests (controllable return values), `NoopSessionManager` in UI tests — both implement full `SessionManager` trait with `#[async_trait]`. Tests calling async App methods (`refresh_sessions`, `refresh_preview`, `confirm_new_session`, `confirm_delete`) use `#[tokio::test]`.
- **Test isolation**: All tests that touch the filesystem use `tempfile::tempdir()` for isolated temp directories. Test helpers (`test_app()`, `test_app_with_sessions()`) set `app.manifest_dir` to per-thread temp dirs. Never write to `~/.hydra/` in tests.
- When adding a new `SessionManager` method, update mock impls in BOTH `app.rs` and `ui.rs` test modules

## Conventions

- Tick rate is 250ms (`EventHandler::new(Duration::from_millis(250))`)
- tmux session names: `hydra-<8char_sha256_hex>-<user_name>`
- Agent commands: `claude --dangerously-skip-permissions`, `codex -c check_for_update_on_startup=false --full-auto`
- Mouse handling lives in `App::handle_mouse()` (moved from `main.rs` to `app.rs`)
- **Preview scrolling**: `preview_scroll_offset: u16` tracks lines scrolled up from bottom (0 = bottom). Scroll wheel over preview adjusts by 3 lines/tick. Offset resets on session selection change. Rendering uses `Paragraph::scroll()` with math: `scroll_y = max_scroll_offset - capped_offset` so offset 0 shows latest output.
- **Scrollback capture**: `capture_pane_scrollback()` uses `tmux capture-pane -p -S -` to get full history (used for preview display). Regular `capture_pane()` (visible pane only) is used for status comparison — keeps status detection lightweight.
- **No mouse forwarding to tmux**: SGR mouse sequences and arrow-key forwarding for scroll were removed — agents don't support mouse input, and forwarding caused garbled text. Mouse clicks/scroll in the preview are handled locally (scroll viewport, attach/detach).

## Common Changes

- **Add agent type**: Add variant to `AgentType` in `session.rs`, implement `command()`, `Display`, `FromStr`, update `all()`, update tests
- **Add UI mode**: Add variant to `Mode` in `app.rs`, add key handler in `main.rs`, add draw function in `ui.rs`, add snapshot test
- **Add SessionManager method**: Update trait in `tmux.rs`, implement on `TmuxSessionManager`, update mocks in `app.rs` and `ui.rs` test modules
- **Change status colors**: Edit `status_color()` in `ui.rs` — maps `SessionStatus` → `ratatui::Color`
