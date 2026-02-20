# Hydra Enhancement Plan — JSONL Metrics Integration

## Already Done

- [x] Status indicator lights (Green=Idle, Red=Running, Yellow=Exited)
- [x] Last assistant message in sidebar (via PID→lsof→UUID→JSONL)
- [x] Task elapsed timer per session
- [x] Preview scrollback with scroll support
- [x] Mouse click/scroll in sidebar and preview
- [x] Session slug from JSONL (human-readable names)

---

## Phase 1: Token Usage & Cost Tracking

**Goal**: Show per-session token consumption and estimated cost in the sidebar or a detail view.

### Data source
- JSONL `type: "assistant"` lines → `message.usage` object
- Fields: `input_tokens`, `output_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`

### Implementation
1. **Add to `logs.rs`**: New function `read_session_stats(cwd, uuid) -> Option<SessionStats>` that parses the JSONL tail and accumulates:
   - `total_input_tokens: u64`
   - `total_output_tokens: u64`
   - `total_cache_read_tokens: u64`
   - `total_cache_write_tokens: u64`
   - `turn_count: u32` (count `type: "system", subtype: "turn_duration"` lines)
   - `total_duration_ms: u64` (sum of `durationMs` from turn_duration entries)
   - `model: String` (from last assistant message `message.model`)
2. **Add `SessionStats` struct** to `session.rs`
3. **Add to `Session` struct**: `pub stats: Option<SessionStats>`
4. **Refresh in `app.rs`**: Alongside last-message refresh (every 20 ticks), call `read_session_stats` and populate `session.stats`
5. **Display in `ui.rs`**: Show in sidebar under session name, e.g.:
   ```
   >> ● worker-1 [Claude]          3m 12s
      Last: "Fixed the auth bug..."
      ↕ 12.4k tok  ~$0.08  opus-4-6
   ```
6. **Cost formula** (approximate):
   - Opus input: $15/MTok, output: $75/MTok, cache read: $1.50/MTok, cache write: $18.75/MTok
   - Sonnet input: $3/MTok, output: $15/MTok
   - Detect model from `message.model` field and apply rates

### Tests
- Unit test for `read_session_stats` with a fixture JSONL
- Unit test for cost calculation
- Update UI snapshot tests to include stats display

---

## Phase 2: Tool Usage Breakdown

**Goal**: Show what tools each agent is using — quick signal for what the agent is doing.

### Data source
- JSONL `type: "assistant"` → `message.content[]` where item `type: "tool_use"` → `name` field
- Tool names: Read, Edit, Write, Bash, Grep, Glob, Task, TaskCreate, TaskUpdate, etc.

### Implementation
1. **Add to `SessionStats`**: `tool_counts: HashMap<String, u32>`
2. **Parse in `read_session_stats`**: Count tool_use blocks by `name`
3. **Display**: Compact summary in detail view or on `Tab` keypress:
   ```
   Tools: Edit:12 Read:8 Bash:5 Grep:3
   ```

---

## Phase 3: Files Modified Tracking

**Goal**: Show which files each agent has touched — helps understand scope of work.

### Data source
- JSONL `type: "file-history-snapshot"` → `snapshot.trackedFileBackups` keys

### Implementation
1. **Add to `SessionStats`**: `files_modified: Vec<String>`
2. **Parse in `read_session_stats`**: Collect unique file paths from snapshot entries
3. **Display**: In a detail pane or expandable section:
   ```
   Files (5): src/app.rs, src/ui.rs, src/tmux.rs, ...
   ```

---

## Phase 4: Session Detail View (new Mode)

**Goal**: Press `i` (info) on a session to see a full detail panel.

### Implementation
1. **New mode**: `Mode::SessionDetail`
2. **Key binding**: `i` in Browse mode → `Mode::SessionDetail`, `Esc` to return
3. **Display** (replaces preview pane):
   ```
   ┌ worker-1 [Claude opus-4-6] ─────────────────┐
   │ Status: ● Running (2m 34s)                   │
   │ Turns: 14                                     │
   │ Tokens: 45.2k in / 12.1k out / 89.3k cached  │
   │ Cost: ~$0.42                                   │
   │ Cache hit rate: 78%                            │
   │ Branch: feature/auth-fix                       │
   │                                                │
   │ Tools used:                                    │
   │   Edit: 12  Read: 8  Bash: 5  Grep: 3         │
   │                                                │
   │ Files modified (5):                            │
   │   src/app.rs                                   │
   │   src/ui.rs                                    │
   │   src/tmux.rs                                  │
   │   src/session.rs                               │
   │   tests/app_tests.rs                           │
   │                                                │
   │ Last message:                                  │
   │   "All four fixes are implemented and          │
   │    verified. Here's a summary of..."           │
   │                                                │
   │ Initial prompt:                                │
   │   "fix the auth bug in the login flow"         │
   └────────────────────────────────────────────────┘
   ```
4. **Data needed** (all from JSONL already parsed):
   - Everything from SessionStats
   - Initial prompt (first `type: "user"` where content is a string)
   - Git branch (`gitBranch` field)
   - Full last message (untruncated)

---

## Phase 5: Aggregate Dashboard

**Goal**: Show totals across all sessions — total cost, total tokens, busiest agent.

### Implementation
1. **New mode or header bar**: `Mode::Dashboard` or always-visible summary
2. **Display at top of sidebar**:
   ```
   ┌ Sessions (4) ─ $1.24 total ─ 182k tokens ┐
   ```
3. Or a full dashboard view on `D` keypress showing per-session comparison

---

## Phase 6: Subagent Tracking

**Goal**: Show when an agent spawns Task subagents and their progress.

### Data source
- JSONL `type: "queue-operation"` with `operation: "enqueue"/"remove"`
- Subagent JSONL at `{uuid}/subagents/agent-{id}.jsonl`

### Implementation
1. Track active subagents per session
2. Show count in sidebar: `● worker-1 [Claude] ⑶` (3 subagents active)
3. In detail view, list active subagent descriptions

---

## Priority Order

1. **Phase 1** (Token/cost) — most useful signal, moderate effort
2. **Phase 4** (Detail view) — great UX, depends on Phase 1
3. **Phase 2** (Tool breakdown) — easy add-on to Phase 1 parsing
4. **Phase 3** (Files modified) — easy add-on to Phase 1 parsing
5. **Phase 5** (Dashboard) — nice-to-have aggregate
6. **Phase 6** (Subagents) — advanced, lower priority

## Notes

- All JSONL parsing should be async and non-blocking (use `tokio::task::spawn_blocking` for file I/O)
- Cache parsed stats and only re-parse when file mtime changes
- Consider a `LogReader` trait for testability (like `SessionManager`)
- The 200KB tail read in `logs.rs` may need to increase for stats (token counts span entire file) — consider full-file scan on first load, then incremental from last-read offset
