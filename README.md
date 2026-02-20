<div align="center">

```
            ___              ___
           / ◉ \    .__.    / ◉ \
           \___/   / ◉  \   \___/
             \     \____/     /
              \       |      /
               \      |     /
                '._   |  _.'
                   '._|_'
                     |=|
                     |=|
                   __| |__
                  [HYDRA™]
```

# hydra

**Multi-headed AI agent session manager**

Run multiple Claude and Codex agents in parallel, each in its own tmux session, managed from a single TUI.

[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Tests](https://img.shields.io/badge/tests-253_passing-brightgreen.svg)](#testing)
[![Coverage](https://img.shields.io/badge/coverage-65%25-yellow.svg)](#testing)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>

---

## Features

- **Sidebar + Preview layout** — browse all agent sessions in a list, see live output in the preview pane
- **Embedded attach** — jump into any session with Enter, type directly to the agent, Esc to detach
- **Status indicators** — green (idle), red (running), yellow (exited) dots per session
- **Task timer** — tracks elapsed time for the current running task per agent
- **Last message preview** — shows the last assistant response in the sidebar (reads Claude Code JSONL logs)
- **Auto-generated names** — sessions get NATO phonetic alphabet names (alpha, bravo, charlie, ...)
- **Session persistence** — sessions survive laptop shutdown; auto-revived on next launch using agent resume
- **Session stats** — live cost, token, and tool-call metrics per agent from JSONL logs
- **Diff tree** — sidebar shows per-file git diff stats grouped by directory
- **Multi-agent support** — Claude (`claude --dangerously-skip-permissions`) and Codex (`codex --yolo`)
- **Mouse support** — click to select sessions, scroll the preview pane
- **Full scrollback** — scroll up through complete session history in the preview

## Requirements

- [Rust](https://rustup.rs/) (stable)
- [tmux](https://github.com/tmux/tmux) (installed and on PATH)
- At least one of: [Claude Code](https://docs.anthropic.com/en/docs/claude-code), [Codex](https://github.com/openai/codex)

## Install

```bash
git clone https://github.com/yourusername/hydra.git
cd hydra
cargo build --release
cp target/release/hydra ~/.local/bin/  # or anywhere on your PATH
```

## Usage

```bash
hydra              # launch the TUI
hydra new NAME     # create a new agent session
hydra ls           # list sessions
hydra kill NAME    # kill a session
```

### Keybindings

| Key | Action |
|-----|--------|
| `n` | New session |
| `d` | Delete session |
| `Enter` | Attach to session |
| `Esc` | Detach / cancel |
| `j` / `k` | Navigate sessions |
| `Scroll` | Scroll preview pane |
| `q` | Quit |

## Architecture

Single-binary Rust TUI built on [ratatui](https://ratatui.rs) + [crossterm](https://docs.rs/crossterm) + [tokio](https://tokio.rs).

```
src/
├── main.rs       CLI parsing, event loop, key dispatch
├── app.rs        App state, mode machine (Browse/Attached/NewSession/ConfirmDelete)
├── ui.rs         All ratatui rendering (snapshot tested)
├── tmux.rs       SessionManager trait + TmuxSessionManager impl
├── session.rs    Session/AgentType data types
├── manifest.rs   Session persistence (~/.hydra/<project>/sessions.json)
├── logs.rs       Claude Code JSONL log reader + session stats
└── event.rs      Async crossterm event reader
```

Key design decisions:
- **`SessionManager` trait** — all tmux interaction is behind an async trait for testability (mock/noop impls in tests)
- **Content-change detection** — session status is determined by diffing `capture-pane` output between ticks, not `session_activity`
- **Session revival** — manifest file persists session metadata; on startup, dead sessions are recreated with agent-specific resume commands
- **Async I/O** — all tmux subprocess calls and manifest file I/O use `tokio` to avoid blocking the event loop

## Testing

253 tests across unit, snapshot, and CLI integration:

```bash
cargo test                # run all tests
cargo insta review        # review snapshot diffs after UI changes
cargo llvm-cov            # generate coverage report
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
