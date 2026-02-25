<div align="center">

<img src="assets/hydra-logo.svg" alt="Hydra logo" width="420" />

# hydra

**Multi-headed AI agent session manager**

Run multiple Claude, Codex, and Gemini agents in parallel, each in its own tmux session, managed from a single TUI.

[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![CI](https://github.com/rencryptofish/hydra/actions/workflows/ci.yml/badge.svg)](https://github.com/rencryptofish/hydra/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>

---

## Features

- **Sidebar + Preview layout** — browse all agent sessions in a list, see live output in the preview pane
- **Compose mode** — press Enter to open compose, type a full message, press Enter to send, Esc to cancel (draft preserved). Prompt history with Up/Down arrows. Bracketed paste support for multiline input.
- **Status indicators** — green (idle), red (running), yellow (exited) dots per session, with auto-clearing status messages
- **Task timer** — tracks elapsed time for the current running task per agent
- **Last message preview** — shows the last parsed assistant response per session from provider logs
- **Auto-generated names** — sessions get NATO phonetic alphabet names (alpha, bravo, charlie, ...)
- **Session persistence** — sessions survive laptop shutdown; auto-revived on next launch using agent resume
- **Session stats** — live cost, token, and tool-call metrics per agent from provider logs
- **Diff tree** — sidebar shows per-file git diff stats grouped by directory
- **Multi-agent support** — Claude (`claude --dangerously-skip-permissions`), Codex (`codex --yolo`), Gemini (`gemini --yolo`)
- **Mouse support** — click to select sessions, scroll the preview pane
- **Full scrollback** — keyboard and mouse scrolling through complete session history (PgUp/PgDn, Home/End)
- **Copy mode** — press `c` to release mouse capture for terminal text selection
- **Low resource usage** — idle sessions skip pane captures, batch tmux queries, parallel agent resolution

## Requirements

- [Rust](https://rustup.rs/) (stable)
- [tmux](https://github.com/tmux/tmux) (installed and on PATH)
- At least one of: [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://github.com/openai/codex), [Gemini CLI](https://github.com/google-gemini/gemini-cli)

## Install

```bash
cargo install --locked --git https://github.com/rencryptofish/hydra.git hydra
```

If you don't have Rust installed yet:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal && . "$HOME/.cargo/env" && cargo install --locked --git https://github.com/rencryptofish/hydra.git hydra
```

To update later, run `hydra update`.

## Usage

```bash
hydra                    # launch the TUI
hydra new AGENT NAME     # create a new agent session (claude/codex/gemini)
hydra kill NAME          # kill a session
hydra ls                 # list sessions for the current project
hydra update             # update to the latest version from GitHub
```

### Keybindings

**Browse mode**

| Key | Action |
|-----|--------|
| `j` / `k` | Navigate sessions |
| `PgUp` / `PgDn` | Scroll preview pane |
| `Home` / `End` | Jump to top / bottom of preview |
| `Enter` | Open compose mode |
| `n` | New session |
| `d` | Delete session |
| `c` | Toggle copy mode (release mouse for text selection) |
| `q` | Quit |

**Compose mode**

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` | Insert newline |
| `Up` / `Down` | Browse prompt history |
| `PgUp` / `PgDn` | Scroll preview pane |
| `Esc` | Cancel (draft preserved) |

## Architecture

Single-binary Rust TUI built on [ratatui](https://ratatui.rs) + [crossterm](https://docs.rs/crossterm) + [tokio](https://tokio.rs).

```
src/
├── main.rs          CLI parsing, terminal setup, event loop
├── app.rs           UiApp state, mode machine, key/mouse handlers
├── backend.rs       Backend actor (owns all I/O, communicates via channels)
├── backend/         Runtime sub-components (session, message, preview)
├── agent/           Per-provider log parsing and command builders
├── ui.rs            Root UI rendering (layout, draw dispatch)
├── ui/              Rendering submodules (sidebar, preview, conversation, ...)
├── tmux.rs          SessionManager trait + subprocess tmux manager
├── tmux_control.rs  Persistent `tmux -C` control-mode manager
├── session.rs       Session/AgentType data types
├── manifest.rs      Session persistence (~/.hydra/<project>/sessions.json)
├── logs.rs          Log readers + session/global stats + cost calculations
├── event.rs         Async crossterm event reader
└── system/          Git diff parsing, process tree helpers
```

Key design decisions:
- **Backend/UI actor model** — Backend runs in `tokio::spawn`, owns all I/O, and communicates with the UI via `watch` (state snapshots) and `mpsc` (commands, previews). The UI event loop never blocks on I/O.
- **`SessionManager` trait** — all tmux interaction is behind an async trait for testability (mock/noop impls in tests)
- **Hybrid status detection** — prefers tmux `%output` notifications and falls back to capture-based detection when needed
- **Session revival** — manifest file persists session metadata; on startup, dead sessions are recreated with agent-specific resume commands
- **Async I/O** — all tmux subprocess calls and manifest file I/O use `tokio` to avoid blocking the event loop
- **Nested session isolation** — safely runs from within Claude Code by unsetting `CLAUDECODE` env vars in spawned sessions

## Testing

490+ tests across unit, snapshot, CLI integration, and property-based:

```bash
cargo test                # run all tests
cargo insta review        # review snapshot diffs after UI changes
cargo llvm-cov            # generate coverage report
cargo +nightly udeps --all-targets  # detect unused dependencies
```

### Coverage

| Module | Coverage |
|--------|----------|
| `session.rs` | 94% |
| `ui.rs` | 91% |
| `app.rs` | 83% |
| `logs.rs` | 26% |
| `main.rs` | 22% |
| `tmux.rs` | 4% |
| `event.rs` | 0% |
| **Total** | **65%** |

> The low-coverage modules (`tmux.rs`, `event.rs`, `main.rs`) are I/O-heavy code that shells out to tmux or reads terminal events. Core logic and UI rendering are well covered.

## License

MIT
