# Hydra

TUI-based AI agent tmux session manager. Lets you run multiple Claude/Codex agents in parallel, each in its own tmux session, with a sidebar+preview layout.

## Build & Test

```bash
cargo build
cargo test                # unit + snapshot + CLI + proptest tests
cargo bench               # criterion benchmarks (rendering, input, data processing)
cargo insta review        # review snapshot diffs after UI changes
make install              # build release binary and install to ~/.cargo/bin/
make check                # CI gate: test + deny + fmt --check + clippy -D warnings
make deny                 # cargo-deny license/advisory audit
make coverage             # HTML coverage via cargo llvm-cov
make fuzz                 # prints cargo fuzz run commands for all targets
make mutants              # cargo mutants (mutation testing)
```

## Architecture

Single-binary Rust TUI (ratatui + crossterm + tokio):

- **`src/lib.rs`** — Thin re-export of all modules so `benches/` (external crates) can access them.
- **`src/main.rs`** — CLI parsing (clap), TUI event loop, key dispatch. Passes full `KeyEvent` (not just `KeyCode`) to handlers for modifier support. Imports modules via `use hydra::*`.
- **`src/app.rs`** — `App` state + `Mode` enum (Browse, Attached, NewSessionAgent, ConfirmDelete). Owns `Box<dyn SessionManager>` for testability. `StatusDetector` and `TaskTimers` are extracted structs that encapsulate status-change debouncing and elapsed-time tracking respectively.
- **`src/tmux.rs`** — `SessionManager` async trait (`#[async_trait]`) + `TmuxSessionManager` impl. All tmux subprocess calls use `tokio::process::Command` (non-blocking). Also has `keycode_to_tmux()` for crossterm→tmux key mapping.
- **`src/session.rs`** — `Session`, `SessionStatus`, `AgentType` types. Pure data, no I/O.
- **`src/ui.rs`** — All ratatui rendering. Snapshot-tested with `insta`.
- **`src/logs.rs`** — Claude Code log file reader. Traces tmux pane PID → `lsof` → `.claude/tasks/<uuid>/` → JSONL log file. Extracts last assistant message per session. Also provides `SessionStats` + `update_session_stats()` for incremental JSONL metrics (tokens, cost, tool calls, files touched).
- **`src/manifest.rs`** — Session persistence for revival across restarts. `SessionRecord` + `Manifest` types (serde), file I/O with `tokio::fs`. Stores at `~/.hydra/<project_id>/sessions.json`. All functions take `base_dir: &Path` for testability. Includes `SessionRecord::for_new_session()` constructor and `resume_command()`/`create_command()` builders.
- **`src/event.rs`** — Async crossterm event reader (keys, mouse, tick, resize).

## Key Patterns

- **SessionManager trait**: All tmux interaction goes through `#[async_trait] trait SessionManager: Send + Sync` so tests can use mock/noop impls. `App::new_with_manager()` is the test constructor. Async methods that call the manager must clone fields (e.g. `project_id`) before `.await` to avoid borrow conflicts across await points.
- **Auto-generated session names**: Session names are auto-assigned from the NATO phonetic alphabet (alpha, bravo, charlie, ...). The `generate_name()` function in `session.rs` picks the first unused name, filling gaps. Falls back to `agent-N` if all 26 are taken.
- **Content-change detection**: `StatusDetector` struct in `app.rs` owns all pane-content-comparison state and debounce counters. `normalize_capture()` strips braille spinners (U+2800–U+28FF), Claude Code spinner glyphs (✢✳✶✻✽), ASCII digits, direction arrows (↑↓), ANSI escape sequences, and trailing whitespace. **Hysteresis** debounces both directions: Running→Idle requires 30 consecutive unchanged refreshes; Idle→Running requires 5 consecutive changed refreshes. **Exited debounce**: 3 ticks normally, extended to 15 ticks when `active_subagents > 0` (allows orchestration to settle). **Log-accelerated Idle**: when `session_stats.task_elapsed()` returns None, the idle threshold drops from 30 to 8 refreshes. `StatusDetector::prune()` and `TaskTimers::prune()` use `HashSet<&String>` for O(1) cleanup of stale session data.
- **Sidebar grouping**: Sessions are grouped by status (Idle → Running → Exited) with dim header rows (e.g. `── ● Idle ──`), then sorted alphabetically within each group. The explicit headers make the grouping intentional rather than chaotic. `SessionStatus::sort_order()` defines the group ordering. The `selected` index maps to `app.sessions` (not visual rows); the UI calculates the visual row by counting header items.
- **Status indicator lights**: Each session shows a colored `●` dot in the sidebar:
  - **Green** = Idle (ready for input, pane content unchanged for the idle threshold)
  - **Red** = Running (busy, pane content changed recently)
  - **Yellow** = Exited (agent process ended, pane is dead)
- **Task elapsed timer**: Tracks per-session `Instant` timestamps in App. Running starts the clock; Idle <5s shows frozen duration (same task); Idle >5s clears it (new task).
- **Embedded attach mode**: `Mode::Attached` forwards keystrokes via `tmux send-keys` instead of `tmux attach`. Preview border switches to thick green (`BorderType::Thick` + bold) for clear visual distinction. Esc returns to Browse and resets selection to index 0 (first idle session).
- **Last message display**: Sidebar shows the last Claude assistant message per session (dimmed second line, truncated to 50 chars). UUIDs are resolved via PID→process tree lookup and cached in `App.log_uuids`. Failed UUID resolutions are retried with cooldown to avoid repeated expensive subprocess walks. Messages refresh every 50 ticks (~5s). Only reads incremental JSONL bytes for efficiency.
- **Claude Code JSONL logs**: Located at `~/.claude/projects/<escaped-cwd>/<uuid>.jsonl`. Path escaping replaces `/` with `-` (e.g. `/home/user/project` → `-home-user-project`). Structure: `{"type": "assistant", "message": {"content": [{"text": "..."}]}}`. The UUID is discovered by parsing `--session-id` from the process command line (`ps -p <pid> -o command=`), falling back to `lsof -p <pane_pid>` for legacy sessions without `--session-id`.
- **remain-on-exit**: Set on session creation so exited agents stay visible with `Exited` status instead of vanishing.
- **Agent type caching**: `TmuxSessionManager` caches `HYDRA_AGENT_TYPE` env var lookups in a `std::sync::Mutex<HashMap>` to avoid repeated `tmux show-environment` calls on every tick. Uses `std::sync::Mutex` (not tokio) since the lock is never held across `.await` points. Cache is also pre-populated on `create_session`.
- **Async I/O**: All tmux subprocess calls use `tokio::process::Command` instead of `std::process::Command` to avoid blocking the event loop. In Attached mode, key presses forward to tmux and force `refresh_preview_live()` for low-latency echo; Browse-mode key navigation uses `refresh_preview()` cache-aware refresh.
- **Session stats**: `SessionStats` in `logs.rs` tracks per-session metrics (turns, tokens in/out, cache tokens, edits, bash commands, unique files). Updated incrementally via `update_session_stats()` which reads only new bytes since last offset — fast even on 100MB+ logs. Stats refresh on the same 50-tick cadence as last messages (~5s at 100ms tick). Rendered in a bordered "Stats" block at the bottom of the sidebar.
- **Global stats**: `GlobalStats` in `logs.rs` aggregates cost/tokens across ALL Claude Code sessions on the machine for today. Scans `~/.claude/projects/` recursively (including nested `subagents/*.jsonl` files) via `update_global_stats()`. Uses incremental file offsets (only reads new bytes after first scan). Date-aware: resets at midnight. Cost uses Sonnet pricing ($3/$15 per MTok in/out). Cold scan of ~1400 files / 1GB takes ~1.8s; subsequent calls are fast. Does NOT include Codex usage (different log format at `~/.codex/sessions/`). The sidebar Stats block shows global cost/tokens + per-session edits.
- **Session persistence / revival**: `manifest.rs` saves session metadata to `~/.hydra/<project_id>/sessions.json`. On startup, `revive_sessions()` loads the manifest, compares against live tmux sessions, and recreates missing ones using each agent's resume command (Claude: `--resume <UUID>`, Codex: `resume --last`). Failed revival attempts are tracked per-record (`failed_attempts`); entries are pruned after `MAX_FAILED_ATTEMPTS` (3) consecutive failures. Manifest is updated on session create/delete.
- **Per-file diff tree**: Sidebar shows a "Changes" block with directory-grouped file diffs from `git diff --numstat`. `DiffFile` struct in `app.rs` holds path/insertions/deletions. `build_diff_tree_lines()` in `ui.rs` groups files by directory, shows compact `+N-N` stats with color coding (green/red). Refreshes on each `refresh_sessions()` tick.

## Testing

- **Unit tests**: `src/session.rs` (pure functions: project_id, name parsing, AgentType) and `src/app.rs` (state machine: mode transitions, navigation, create/delete flows)
- **Snapshot tests**: `src/ui.rs` uses `insta::assert_snapshot!` with `ratatui::backend::TestBackend` (80x24). Run `cargo insta review` after intentional UI changes. Snapshots live in `src/snapshots/`.
- **CLI tests**: `tests/cli_tests.rs` with `assert_cmd` — tests help, ls, arg validation, unknown commands
- **Mock**: `MockSessionManager` in app tests (controllable return values), `NoopSessionManager` in UI tests — both implement full `SessionManager` trait with `#[async_trait]`. Tests calling async App methods (`refresh_sessions`, `refresh_preview`, `confirm_new_session`, `confirm_delete`) use `#[tokio::test]`.
- **Benchmarks**: `benches/rendering.rs`, `benches/input_handling.rs`, `benches/data_processing.rs` use criterion. Each defines its own `NoopSessionManager` and helper factories. Run `cargo bench` or `cargo bench --bench rendering` for a single suite.
- **Property-based tests**: `proptest` used across session.rs (name generation, project_id, parse roundtrips), tmux.rs (keycode mapping never panics), app.rs (normalize_capture), and logs.rs (JSONL parsing, incremental stats composability). Verify determinism, roundtrip correctness, and no-panic guarantees.
- **Fuzz targets**: `fuzz/fuzz_targets/` has targets for `normalize_capture`, `jsonl_parsing`, `extract_message`, `diff_numstat`. Run via `cargo fuzz run <target>`.
- **Test isolation**: All tests that touch the filesystem use `tempfile::tempdir()` for isolated temp directories. Test helpers (`test_app()`, `test_app_with_sessions()`) set `app.manifest_dir` to per-thread temp dirs. Never write to `~/.hydra/` in tests.
- When adding a new `SessionManager` method, update mock impls in BOTH `app.rs` and `ui.rs` test modules

## Conventions

- Tick rate is 50ms (`EVENT_TICK_RATE`), session/preview refresh runs every tick (~50ms)
- tmux session names: `hydra-<8char_sha256_hex>-<user_name>`
- Agent commands: `claude --dangerously-skip-permissions`, `codex -c check_for_update_on_startup=false --yolo`
- Mouse handling lives in `App::handle_mouse()` (moved from `main.rs` to `app.rs`)
- **Preview scrolling**: `preview_scroll_offset: u16` tracks lines scrolled up from bottom (0 = bottom). Scroll wheel over preview adjusts by 3 lines/tick. Offset resets on session selection change. Rendering uses `Paragraph::scroll()` with math: `scroll_y = max_scroll_offset - capped_offset` so offset 0 shows latest output.
- **Scrollback capture**: `capture_pane_scrollback()` uses `tmux capture-pane -p -S -5000` to fetch recent history for preview scrolling. Regular `capture_pane()` (visible pane only) is used for status comparison and live preview — keeps status detection lightweight. Both trim trailing newlines to prevent short output from appearing to float halfway up the preview (tmux pads capture to full pane height).
- **No mouse forwarding to tmux**: Mouse clicks in the preview are NOT forwarded to agent tmux panes — agents don't support mouse input, and forwarding SGR mouse sequences causes garbled text (e.g. `[<0;12;21m`). Left-clicking inside the preview in Attached mode only resets `preview_scroll_offset` to 0. Clicks outside the preview detach. Scroll events are handled locally.
- **Pending action pattern**: `handle_mouse()` is synchronous (to avoid converting 15+ tests to async), so it can't call async trait methods directly. Instead it sets `App.pending_literal_keys: Option<(String, String)>` which the event loop consumes via `flush_pending_keys()`. This pattern keeps mouse tests simple while supporting async I/O.
- **Literal key sending**: `send_keys_literal()` on `SessionManager` sends raw text/escape sequences via `tmux send-keys -l` (literal mode). Has a default no-op impl in the trait so mock impls don't need to override it.

## Recent Learnings (2026-02-20)

- Keep the UI event tick responsive (100ms) with a 2-tick (~200ms) session refresh cadence, and use forced live preview capture in Attached mode to keep typing echo real-time.
- Cache preview line count in app state and update it only when preview text changes; avoid `lines().count()` during every draw.
- Parse session JSONL once per refresh path: update stats and extract the last assistant text in the same incremental pass.
- Use a retry cooldown for unresolved UUID lookups (6 cycles ~= 30s at 5s refresh cadence) to avoid repeatedly traversing process trees for sessions that do not expose Claude UUIDs.
- Replace repeated `iter().find()` lookups in session refresh loops with a prebuilt `HashMap` to avoid O(n^2) behavior as session counts grow.
- `raw.githubusercontent.com` returns 404 for private repos — can't use `curl | bash` install pattern. Use `cargo install --git ssh://...` instead.
- `cargo install --git ssh://...` uses libgit2 by default, which doesn't read the SSH agent. Set `CARGO_NET_GIT_FETCH_WITH_CLI=true` to force cargo to use the system `git` CLI (which does).
- `hydra update` now uses signed binary releases: downloads binary + `.minisig` from GitHub Releases, verifies Ed25519 signature in memory via `minisign-verify` crate before writing to disk. `UPDATE_PUBLIC_KEY` in `main.rs` is a placeholder — must be replaced with the real key before cutting releases.
- `ansi-to-tui` v7 is compatible with ratatui 0.29; v8 depends on `ratatui-core` (a separate crate for ratatui 0.30+) and causes type mismatches.
- Preview pane uses ANSI color rendering: `tmux capture-pane -e` emits escape sequences, parsed once via `ansi_to_tui::IntoText` into cached `Text<'static>`. Raw string kept for `normalize_capture()` status detection.
- **Benchmark baselines (criterion, 2026-02-20)**: Full frame draw ~50–60µs at 80×24 (<1% of 100ms tick); key/mouse dispatch <200ns; `normalize_capture` 6–130µs scaling with input size; JSONL incremental parse ~5× faster than full; preview rendering is the most expensive path (~6ms at 5000 lines). All well within the tick budget.
- To expose internal functions to criterion benchmarks, `src/lib.rs` re-exports modules and hot-path helpers (`normalize_capture`, `parse_diff_numstat`, `draw_sidebar`/`draw_preview`/`draw_stats`, `extract_assistant_message_text`, `update_session_stats_from_path_and_last_message`, `apply_tmux_modifiers`) are `pub`.
- Extract complex state machine logic (status debouncing, timer tracking) into dedicated structs (`StatusDetector`, `TaskTimers`) with `pub(crate)` fields — keeps `App` manageable and makes state transitions independently testable.
- `normalize_capture()` must strip agent-specific spinner glyphs and progress characters (digits, arrows) in addition to braille — otherwise timer updates and wait-screen animations cause false Running→Idle→Running flicker.
- Exited-session debounce needs to be longer when subagents are active (`DEAD_TICK_SUBAGENT_THRESHOLD = 15`) — orchestrating agents briefly lose their pane during subagent handoffs.
- `proptest` char ranges (`'a'..='z'`) don't implement `Strategy` — use `proptest::char::range()` instead.
- Makefile `check` target composes `test + deny + fmt --check + clippy` for CI. `make coverage` uses `cargo llvm-cov html`, `make mutants` uses `cargo mutants`.

## Common Changes

- **Add agent type**: Add variant to `AgentType` in `session.rs`, implement `command()`, `Display`, `FromStr`, update `all()`, update tests
- **Add UI mode**: Add variant to `Mode` in `app.rs`, add key handler in `main.rs`, add draw function in `ui.rs`, add snapshot test
- **Add SessionManager method**: Update trait in `tmux.rs`, implement on `TmuxSessionManager`, update mocks in `app.rs` and `ui.rs` test modules. If the method has a sensible default (e.g. no-op), provide a default impl in the trait to avoid updating every mock.
- **Change status colors**: Edit `status_color()` in `ui.rs` — maps `SessionStatus` → `ratatui::Color`
