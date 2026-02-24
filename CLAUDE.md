# Hydra

TUI-based AI agent tmux session manager. Lets you run multiple Claude/Codex/Gemini agents in parallel, each in its own tmux session, with a sidebar+preview layout.

## Build & Test

```bash
cargo build
cargo test                # unit + snapshot + CLI + proptest tests
cargo bench               # criterion benchmarks (rendering, input, data processing)
cargo insta review        # review snapshot diffs after UI changes
cargo +nightly udeps --all-targets  # detect unused dependencies
make install              # build release binary and install to ~/.cargo/bin/
make check                # CI gate: test + deny + fmt --check + clippy -D warnings
make udeps                # cargo-udeps dependency usage check
make deny                 # cargo-deny license/advisory audit
make coverage             # HTML coverage via cargo llvm-cov
make fuzz                 # prints cargo fuzz run commands for all targets
make mutants              # cargo mutants (mutation testing)
```

## Architecture

Single-binary Rust TUI (ratatui + crossterm + tokio) with a Backend/UI actor model:

- **`src/lib.rs`** — Thin re-export of all modules so `benches/` (external crates) can access them.
- **`src/main.rs`** — CLI parsing (clap), TUI event loop. Creates channels between Backend and UiApp, spawns Backend as a `tokio::spawn` task. The event loop has **no `.await` calls** for key/mouse handling — UI never blocks on I/O.
- **`src/app.rs`** — `UiApp` (UI-side state) + `Mode` enum (Browse, Compose, NewSessionAgent, ConfirmDelete). Also defines shared channel types: `BackendCommand` (UI→Backend), `StateSnapshot` (Backend→UI via `watch`), `PreviewUpdate` (Backend→UI via `mpsc`). Also contains UI sub-structs like `PreviewState` and `ComposeState`.
- **`src/backend.rs`** — `Backend` actor task that owns all I/O state: `Box<dyn SessionManager>`, status detectors, session data, conversation buffers. Runs a `tokio::select!` loop handling: commands from UI, `%output` notifications (event-driven status), session refresh ticks, and message/stats refresh. Also contains `OutputDetector` for `%output`-based status detection.
- **`src/tmux.rs`** — `SessionManager` async trait (`#[async_trait]`) + `TmuxSessionManager` impl (subprocess-per-command fallback). All tmux subprocess calls use `tokio::process::Command` (non-blocking). Also has `keycode_to_tmux()` for crossterm→tmux key mapping.
- **`src/tmux_control.rs`** — `ControlModeSessionManager` impl using a persistent `tmux -C` pipe instead of spawning subprocesses. `TmuxControlConnection` manages the child process, a background reader task, and FIFO command-response correlation via `VecDeque<PendingCommand>`. Parses `%output`, `%pane-exited`, `%session-changed` notifications and broadcasts them via `tokio::sync::broadcast`. The connection is shared (`Arc<TmuxControlConnection>`) between the `ControlModeSessionManager` and the `Backend` for notification subscription. Also has `decode_octal_escapes()` for control mode's byte-level octal encoding and `quote_tmux_arg()` for shell-style argument quoting.
- **`src/session.rs`** — `Session`, `SessionStatus`, `AgentType` types. Pure data, no I/O.
- **`src/ui.rs`** — All ratatui rendering. All draw functions take `&UiApp`. Snapshot-tested with `insta`.
- **`src/logs.rs`** — Multi-provider log readers/parsers (Claude/Codex/Gemini). Resolves provider log paths from tmux pane PIDs/process trees, extracts last assistant messages, parses structured conversation entries, and computes incremental per-session + global usage stats/costs.
- **`src/manifest.rs`** — Session persistence for revival across restarts. `SessionRecord` + `Manifest` types (serde), file I/O with `tokio::fs`. Stores at `~/.hydra/<project_id>/sessions.json`. All functions take `base_dir: &Path` for testability. Includes `SessionRecord::for_new_session()` constructor and `resume_command()`/`create_command()` builders.
- **`src/event.rs`** — Async crossterm event reader (keys, mouse, tick, resize).

## Key Patterns

- **Backend/UI actor model**: The Backend runs in `tokio::spawn`, owns all I/O state, and communicates with UiApp via channels: `watch::Sender<StateSnapshot>` (latest-value semantics — UI always gets freshest state), `mpsc::Sender<PreviewUpdate>` (preview data), and `mpsc::Receiver<BackendCommand>` (UI actions). The UI event loop is fully synchronous — `handle_key()` and `handle_mouse()` never `.await`. `UiApp::poll_state()` is called each tick to drain channels.
- **SessionManager trait**: All tmux interaction goes through `#[async_trait] trait SessionManager: Send + Sync` so tests can use mock/noop impls. `UiApp::new_test()` is the test constructor (creates dummy channels). Async methods in the Backend must clone fields (e.g. `project_id`) before `.await` to avoid borrow conflicts across await points.
- **Auto-generated session names**: Session names are auto-assigned from the NATO phonetic alphabet (alpha, bravo, charlie, ...). The `generate_name()` function in `session.rs` picks the first unused name, filling gaps. Falls back to `agent-N` if all 26 are taken.
- **Status detection in Backend**: `SessionRuntime` combines `%output` recency (`OutputDetector`), provider-preferred strategy (`StatusStrategy::JsonlActivity` or `OutputEvent`), and batched pane-dead checks (`batch_pane_status()`). Sessions go `Running` when recent output/log activity exists, otherwise `Idle`.
- **Exited debounce**: Dead panes are debounced before marking `Exited` (3 ticks default, 15 ticks when `active_subagents > 0`) to avoid transient false exits during agent handoffs.
- **Sidebar grouping**: Sessions are grouped by status (Idle → Running → Exited) with dim header rows (e.g. `── ● Idle ──`), then sorted alphabetically within each group. The explicit headers make the grouping intentional rather than chaotic. `SessionStatus::sort_order()` defines the group ordering. The `selected` index maps to `app.sessions` (not visual rows); the UI calculates the visual row by counting header items.
- **Status indicator lights**: Each session shows a colored `●` dot in the sidebar:
  - **Green** = Idle (ready for input, pane content unchanged for the idle threshold)
  - **Red** = Running (busy, pane content changed recently)
  - **Yellow** = Exited (agent process ended, pane is dead)
- **Task elapsed timer**: Tracks per-session `Instant` timestamps in App. Running starts the clock; Idle <5s shows frozen duration (same task); Idle >5s clears it (new task).
- **Compose mode**: `Mode::Compose` is message-oriented (not attached passthrough). User types in a local compose buffer; `Enter` submits, `Esc`/`Ctrl+C` cancels. Submit path sends literal text first, then a delayed `Enter` key (`send_text_enter`) to reduce dropped-enter behavior in Ink/TUI CLIs. Empty compose on Codex sends a bare `Enter` (`SendKeys`) for startup/resume prompts.
- **Conversation preview**: Sessions with parsed provider logs (Claude/Codex/Gemini) render structured conversation entries from `ConversationBuffer` (max 500 entries). `render_conversation()` styles user/assistant/tool events. Fallback is raw `capture-pane` content when no parsed conversation is available.
- **Last message display**: Sidebar shows the latest parsed assistant text per session (dimmed second line, truncated). Log path/session-id resolution is cached per tmux session with retry cooldowns to avoid expensive process-tree/lsof scans every tick. Message+conversation refresh is cadence-gated (~2s).
- **Claude Code JSONL logs**: Located at `~/.claude/projects/<escaped-cwd>/<uuid>.jsonl`. Path escaping replaces `/` with `-` (e.g. `/home/user/project` → `-home-user-project`). Structure: `{"type": "assistant", "message": {"content": [{"text": "..."}]}}`. The UUID is discovered by parsing `--session-id` from the process command line (`ps -p <pid> -o command=`), falling back to `lsof -p <pane_pid>` for legacy sessions without `--session-id`.
- **remain-on-exit**: Set on session creation so exited agents stay visible with `Exited` status instead of vanishing.
- **Agent type caching**: `TmuxSessionManager` caches `HYDRA_AGENT_TYPE` env var lookups in a `std::sync::Mutex<HashMap>` to avoid repeated `tmux show-environment` calls on every tick. Uses `std::sync::Mutex` (not tokio) since the lock is never held across `.await` points. Cache is also pre-populated on `create_session`. Uncached lookups are resolved in parallel via `join_all`.
- **Preview capture pipeline**: `PreviewRuntime` resolves preview in order: parsed conversation entries, cached pane capture, then live `capture_pane`/`capture_pane_scrollback` (budgeted per tick). This keeps UI responsive while still refreshing active sessions.
- **Batch pane status**: `batch_pane_status()` in `tmux.rs` uses a single `tmux list-panes -a -F "#{session_name} #{pane_dead} #{pane_activity}"` call to fetch dead/activity data for all panes in one subprocess call.
- **Nested session isolation**: `create_session()` wraps the agent command with `unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; exec <cmd>` and calls `tmux set-environment -r` to prevent Claude Code env vars from propagating into agent sessions.
- **Async I/O**: All tmux subprocess calls use `tokio::process::Command` instead of `std::process::Command`. The Backend actor runs all I/O in its own `tokio::spawn` task. The UI event loop never blocks — `UiApp::refresh_preview_from_cache()` provides instant feedback from cached preview data, while the Backend sends updates via channels.
- **Session stats**: `SessionStats` in `logs.rs` tracks per-session metrics (turns, tokens in/out, cache tokens, edits, bash commands, unique files). Updated incrementally via `update_session_stats()` which reads only new bytes since last offset — fast even on 100MB+ logs. Stats refresh on the same 40-tick cadence as messages/conversations (~2s). Rendered in a bordered "Stats" block at the bottom of the sidebar.
- **Global stats**: `GlobalStats` in `logs.rs` aggregates daily usage/cost across Claude (`~/.claude/projects`), Codex (`~/.codex/sessions`), and Gemini (`~/.gemini/tmp`) logs. It uses incremental offsets/file-state caches and resets on date rollover. Sidebar stats render per-provider cost/token totals plus per-session edits.
- **Session persistence / revival**: `manifest.rs` saves session metadata to `~/.hydra/<project_id>/sessions.json`. On startup, `revive_sessions()` loads the manifest, compares against live tmux sessions, and recreates missing ones using each agent's resume command (Claude: `--resume <UUID>`, Codex: `resume --last`, Gemini: `--resume`). Failed revival attempts are tracked per-record (`failed_attempts`); entries are pruned after `MAX_FAILED_ATTEMPTS` (3) consecutive failures. Manifest is updated on session create/delete.
- **Per-file diff tree**: Sidebar shows a "Changes" block with directory-grouped file diffs from `git diff --numstat`. `DiffFile` struct in `app.rs` holds path/insertions/deletions. `build_diff_tree_lines()` in `ui.rs` groups files by directory, shows compact `+N-N` stats with color coding (green/red). Refreshes on each `refresh_sessions()` tick.

## Testing

- **Unit tests**: `src/session.rs` (pure functions: project_id, name parsing, AgentType) and `src/app.rs` (state machine: mode transitions, navigation, create/delete flows)
- **Snapshot tests**: `src/ui.rs` uses `insta::assert_snapshot!` with `ratatui::backend::TestBackend` (80x24). Run `cargo insta review` after intentional UI changes. Snapshots live in `src/snapshots/`.
- **CLI tests**: `tests/cli_tests.rs` with `assert_cmd` — tests help, ls, arg validation, unknown commands
- **Mock**: `MockSessionManager` in app tests, `UiApp::new_test()` for UI snapshot tests (creates dummy channels, no SessionManager needed). Tests calling async methods (`refresh_sessions`, `refresh_preview`, `confirm_new_session`, `confirm_delete`) use `#[tokio::test]`. UiApp tests are synchronous since handle_key/handle_mouse don't await.
- **Benchmarks**: `benches/rendering.rs`, `benches/input_handling.rs`, `benches/data_processing.rs` use criterion. Bench helpers create `UiApp` with dummy channels via `UiApp::new()`. Run `cargo bench` or `cargo bench --bench rendering` for a single suite.
- **Property-based tests**: `proptest` used across session.rs (name generation, project_id, parse roundtrips), tmux.rs (keycode mapping never panics), app.rs (normalize_capture), and logs.rs (JSONL parsing, incremental stats composability). Verify determinism, roundtrip correctness, and no-panic guarantees.
- **Fuzz targets**: `fuzz/fuzz_targets/` has targets for `normalize_capture`, `jsonl_parsing`, `extract_message`, `diff_numstat`. Run via `cargo fuzz run <target>`.
- **Test isolation**: All tests that touch the filesystem use `tempfile::tempdir()` for isolated temp directories. Test helpers (`UiApp::new_test()` for UI tests) never write to `~/.hydra/`.
- When adding a new `SessionManager` method, update mock impls in BOTH `app.rs` and `ui.rs` test modules

## Conventions

- Tick rate is 50ms (`EVENT_TICK_RATE`). Backend session refresh runs on a 500ms interval; message/stats work is polled every 50ms and cadence-gated (~2s) by `BackgroundRefreshState::tick()`.
- tmux session names: `hydra-<8char_sha256_hex>-<user_name>`
- Agent commands: `claude --dangerously-skip-permissions`, `codex -c check_for_update_on_startup=false --yolo`, `gemini --yolo`
- Mouse handling lives in `UiApp::handle_mouse()` in `app.rs`
- **Preview scrolling**: `preview_scroll_offset: u16` tracks lines scrolled up from bottom (0 = bottom). Scroll wheel over preview adjusts by 3 lines/tick. Offset resets on session selection change. Rendering uses `Paragraph::scroll()` with math: `scroll_y = max_scroll_offset - capped_offset` so offset 0 shows latest output.
- **Scrollback capture**: `capture_pane_scrollback()` uses `tmux capture-pane -p -S -5000` to fetch recent history for preview scrolling. Regular `capture_pane()` (visible pane) is used for live pane previews when conversation logs are unavailable.
- **No mouse forwarding to tmux**: Mouse clicks in the preview are NOT forwarded to agent tmux panes — agents don't support mouse input, and forwarding SGR mouse sequences causes garbled text (e.g. `[<0;12;21m`). Left-clicking inside the preview in Compose mode only resets `preview_scroll_offset` to 0. Clicks outside the preview exit compose. Scroll events are handled locally.
- **Pending action pattern**: `handle_mouse()` is synchronous so it can't call async I/O directly. Instead it sends `BackendCommand` via `cmd_tx.try_send()` for actions needing I/O (compose send, key forwarding). This pattern keeps mouse tests simple while supporting async I/O through the backend actor.
- **Literal key sending**: `send_keys_literal()` on `SessionManager` sends raw text/escape sequences via `tmux send-keys -l` (literal mode). Has a default no-op impl in the trait so mock impls don't need to override it.

## Recent Learnings (2026-02-24)

- **Backend/UI actor split**: Place shared channel types (`BackendCommand`, `StateSnapshot`, `PreviewUpdate`) in `app.rs`, not `backend.rs` — avoids circular deps and keeps `pub mod backend;` in `lib.rs` alive (only `main.rs` imports `Backend`). Make `UiApp` expose the same public field names as the old `App` so draw functions only need type annotation changes (`&App` → `&UiApp`), not field access changes.
- **Event-driven status via `%output` notifications**: `OutputDetector` tracks last-output timestamps per session and marks sessions running quickly on incoming output events; `SessionRuntime` reconciles this with provider log activity on refresh ticks.
- **`ControlModeSessionManager::new()` takes `Arc<TmuxControlConnection>`**: The connection must be shared between the session manager and the Backend (for notification subscription). Create the `Arc` in `main.rs::run_tui()` before constructing either.
- Keep the UI event tick responsive (50ms). Conversation preview from JSONL is synchronous (no subprocess needed).
- **Claude conversation parsing now handles meta log types**: `parse_conversation_entries()` recognizes `type=progress`, `type=system`, and `type=file-history-snapshot` as first-class `ConversationEntry` variants instead of sending them to `UNPARSED JSONL`.
- **Progress noise filtering**: Render useful progress types (`waiting_for_task`, `search_results_received`, `query_update`, `mcp_progress`, and non-empty `bash_progress`) and suppress high-volume low-signal progress (`hook_progress`, `agent_progress`).
- **System event filtering**: Suppress `system.turn_duration` spam, summarize actionable system events (`api_error`, `local_command`, `compact_boundary`, `microcompact_boundary`), and only render `stop_hook_summary` when it contains meaningful signal (errors/prevented continuation/stop reason/output).
- **File snapshot display policy**: Skip empty baseline `file-history-snapshot` entries, render updates/non-empty snapshots as tracked-file counts plus a short sample of file paths in `render_conversation()`.
- Cache preview line count in app state and update it only when preview text changes; avoid `lines().count()` during every draw.
- Parse session JSONL once per refresh path: update stats and extract the last assistant text in the same incremental pass.
- **Gemini conversation parsing is index-based**: Gemini chat logs are monolithic JSON files (`~/.gemini/tmp/<project>/chats/session-*.json`), so incremental refresh must track message index offsets (`messages.len()`), not byte offsets.
- **Gemini tool calls should emit both use and result entries**: Each `toolCalls[]` item includes invocation + result payload, so parse into both `ToolUse` and `ToolResult` for a complete structured timeline in the TUI.
- **Gemini session rollover handling**: If stored message offset is greater than current `messages.len()` (file rewritten/new session), restart parse at index 0 to avoid silently dropping entries.
- **Gemini stats replacement must clear stale file state**: `apply_gemini_stats()` should clear `files`/`recent_files` and reset `active_subagents` before applying the new snapshot to prevent carry-over from previous parses.
- **Global stats discovery must be base-dir aware in tests**: `update_global_stats_inner(base_dir=...)` should discover Gemini files under `<base_dir>/.gemini/tmp` to keep tests hermetic and avoid pulling host `HOME` data.
- Use a retry cooldown for unresolved UUID lookups (6 cycles ~= 30s at 5s refresh cadence) to avoid repeatedly traversing process trees for sessions that do not expose Claude UUIDs.
- Replace repeated `iter().find()` lookups in session refresh loops with a prebuilt `HashMap` to avoid O(n^2) behavior as session counts grow.
- `raw.githubusercontent.com` returns 404 for private repos — can't use `curl | bash` install pattern. Use `cargo install --git ssh://...` instead.
- `cargo install --git ssh://...` uses libgit2 by default, which doesn't read the SSH agent. Set `CARGO_NET_GIT_FETCH_WITH_CLI=true` to force cargo to use the system `git` CLI (which does).
- `hydra update` uses `cargo install --git` with `CARGO_NET_GIT_FETCH_WITH_CLI=true`.
- `ansi-to-tui` v7 is compatible with ratatui 0.29; v8 depends on `ratatui-core` (a separate crate for ratatui 0.30+) and causes type mismatches.
- Preview pane uses ANSI color rendering: `tmux capture-pane -e` emits escape sequences, parsed once via `ansi_to_tui::IntoText` into cached `Text<'static>` for display.
- **Benchmark baselines (criterion, 2026-02-20)**: Full frame draw ~50–60µs at 80×24 (<1% of 100ms tick); key/mouse dispatch <200ns; `normalize_capture` 6–130µs scaling with input size; JSONL incremental parse ~5× faster than full; preview rendering is the most expensive path (~6ms at 5000 lines). All well within the tick budget.
- To expose internal functions to criterion benchmarks, `src/lib.rs` re-exports modules and hot-path helpers (`normalize_capture`, `parse_diff_numstat`, `draw_sidebar`/`draw_preview`/`draw_stats`, `extract_assistant_message_text`, `update_session_stats_from_path_and_last_message`, `apply_tmux_modifiers`) are `pub`.
- Extract complex runtime logic (status debouncing, timer tracking, preview scheduling, message refresh cadence) into dedicated backend structs (`SessionRuntime`, `MessageRuntime`, `PreviewRuntime`) to keep the actor loop small and testable.
- Exited-session debounce needs to be longer when subagents are active (`DEAD_TICK_SUBAGENT_THRESHOLD = 15`) — orchestrating agents briefly lose their pane during subagent handoffs.
- `proptest` char ranges (`'a'..='z'`) don't implement `Strategy` — use `proptest::char::range()` instead.
- Makefile `check` target composes `test + deny + fmt --check + clippy` for CI. `make coverage` uses `cargo llvm-cov html`, `make mutants` uses `cargo mutants`.
- **Sub-struct ownership**: `Backend` owns runtime subcomponents (`SessionRuntime`, `MessageRuntime`, `PreviewRuntime` and their helper state). `UiApp` owns UI-only state (`PreviewState`, `ComposeState`).
- **Performance**: expensive tmux operations are batched (`batch_pane_status`), preview captures are budgeted per tick, and cached state (agent type, previews, resolved log paths) avoids repeated subprocess/process-tree work.
- **Background task cadence**: Messages/conversations/stats refresh every ~2s via `BackgroundRefreshState::tick()` (internally gated at 40-tick cadence).
- **Nested session env var propagation**: When Hydra is launched from within Claude Code, `CLAUDECODE=1` and `CLAUDE_CODE_ENTRYPOINT` propagate into spawned tmux sessions, causing Claude to refuse to start ("nested sessions share runtime resources"). Fixed by wrapping the agent command with `unset CLAUDECODE CLAUDE_CODE_ENTRYPOINT; exec <cmd>` and also calling `tmux set-environment -r` to remove the vars from the session environment table.
- **Integration tests for tmux**: Tests that create real tmux sessions need timing care — `sh -c 'sleep 0.3 && exit 0'` gives enough time for `remain-on-exit` to be set before the command exits. Using bare `true` or instant-exit commands causes race conditions where the session is destroyed before options can be applied.
- **tmux control mode (`tmux -C`)**: `tmux -C new-session -d` is wrong — the `-d` flag causes the control client to immediately exit (`%exit`) since it has no session to attach to. Drop `-d` so the client stays attached to the control session.
- **tmux control mode command IDs**: The `%begin/%end/%error` headers contain server-assigned command IDs (e.g. 476322), NOT sequential from 0. Cannot use `HashMap<u64, PendingCommand>` keyed by a client counter. Must use FIFO ordering (`VecDeque`) since commands and responses are sequential through the pipe.
- **tmux control mode octal escapes**: `decode_octal_escapes()` must decode into a `Vec<u8>` byte buffer, then `String::from_utf8_lossy()`. Each `\NNN` is one raw byte, not one Unicode codepoint — multi-byte UTF-8 characters appear as multiple consecutive escapes (e.g. `\342\227\217` → `●`). Decoding each octal as `char::from_u32()` produces garbled `â` characters.
- **tmux control mode argument quoting**: Control mode commands are parsed like shell command lines — spaces delimit arguments. `send-keys -t sess -l hello world` splits into two key arguments. Use single-quote wrapping and escape `'` as `'\''` so `$VARS` and `#{formats}` are not expanded by tmux before reaching the pane.
- **tmux control mode FIFO ordering**: stdin writes and pending-deque pushes must be under the same lock (`SenderState` bundles both) to guarantee deque order matches pipe order. Fire-and-forget commands must also go through this lock (made async) — spawning a background task to write later breaks ordering.
- **`cargo install --git` with fuzz targets**: When the repo contains `fuzz/Cargo.toml` (e.g. `hydra-fuzz`), `cargo install --git <URL>` fails with "multiple packages with binaries found". Must pass the crate name as a positional argument: `cargo install --git <URL> hydra --locked`. The `--package` flag is NOT valid with `--git` — use the positional form instead.
- **`tokio::select!` with optional receivers**: Use `std::future::pending().await` in the `None` branch to make an optional channel arm inert without busy-looping. Pattern: `notif = async { match rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } }`.
- **`tokio::sync::watch` for state snapshots**: Latest-value semantics — UI always gets the freshest `StateSnapshot`, never blocks the Backend. Use `watch::Receiver::has_changed()` in the UI tick to detect updates without copying stale data.
- **`tokio::sync::broadcast` for fan-out notifications**: Multiple consumers (Backend, future monitoring) can subscribe to the same tmux `%output` notification stream. Lagged receivers get `RecvError::Lagged` — handle gracefully.
- **Rust `if let` with tuple destructuring moves values**: `if let (Some(x), Some(y)) = (a, b)` moves `a` and `b` into the tuple — fallback `else if let Some(x) = a` fails with "use of moved value". Use `match (a, b, c, d) { ... }` with exhaustive arms instead.
- **Clippy `new_without_default`**: Any public `fn new() -> Self` with no arguments must have a corresponding `Default` impl. Use `#[derive(Default)]` when the struct's fields all implement `Default`.
- **Compose submit reliability**: Some Ink/TUI CLIs can miss Enter if literal text and Enter arrive in the same burst; `send_text_enter` sends literal text then waits briefly before sending `Enter`.
- **Preview fallback behavior**: even with control mode enabled, preview rendering can fall back to live `capture_pane()` when no parsed conversation or cache is available.

## Common Changes

- **Add agent type**: Add variant to `AgentType` in `session.rs`, implement `command()`, `Display`, `FromStr`, update `all()`, update tests. Add resume/create commands in `manifest.rs`. Wire provider behavior in `src/agent/*` (`create_command`, `resolve_log_path`, `update_from_log`, preferred status strategy). Update CLI help in `main.rs` and snapshot tests via `cargo insta accept`.
- **Add UI mode**: Add variant to `Mode` in `app.rs`, add key handler in `UiApp::handle_key()`, add draw function in `ui.rs`, add snapshot test. If the mode requires I/O, add a `BackendCommand` variant and handle it in `Backend::handle_command()`.
- **Add SessionManager method**: Update trait in `tmux.rs`, implement on `TmuxSessionManager`, update mocks in `app.rs` and `ui.rs` test modules. If the method has a sensible default (e.g. no-op), provide a default impl in the trait to avoid updating every mock.
- **Change status colors**: Edit `status_color()` in `ui.rs` — maps `SessionStatus` → `ratatui::Color`
