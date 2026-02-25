use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::sync::Arc;

use hydra::app::{StateSnapshot, UiApp};
use hydra::session::{AgentType, Session, VisualStatus, ProcessState, AgentState};
use hydra::ui;
use ratatui::layout::Rect;

// ── Helpers ─────────────────────────────────────────────────────────

fn make_session(name: &str, visual_status: VisualStatus) -> Session {
    let (process_state, agent_state) = match visual_status {
        VisualStatus::Idle => (ProcessState::Alive, AgentState::Idle),
        VisualStatus::Running(_s) => (ProcessState::Alive, AgentState::Thinking),
        VisualStatus::Exited => (ProcessState::Exited { exit_code: None, reason: None }, AgentState::Idle),
        VisualStatus::Booting => (ProcessState::Booting, AgentState::Idle),
    };
    Session {
        name: name.to_string(),
        tmux_name: format!("hydra-bench-{name}"),
        agent_type: AgentType::Claude,
        process_state,
        agent_state,
        last_activity_at: std::time::Instant::now(),
        task_elapsed: None,
        _alive: true,
    }
}

const NATO: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
    "uniform", "victor", "whiskey", "xray", "yankee", "zulu",
];

fn make_app_with_n_sessions(n: usize) -> UiApp {
    let sessions: Vec<Session> = (0..n)
        .map(|i| {
            let name = if i < NATO.len() {
                NATO[i].to_string()
            } else {
                format!("agent-{}", i + 1)
            };
            let status = match i % 3 {
                0 => VisualStatus::Idle,
                1 => VisualStatus::Running("Thinking".to_string()),
                _ => VisualStatus::Exited,
            };
            make_session(&name, status)
        })
        .collect();

    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(1);
    let (_state_tx, state_rx) = tokio::sync::watch::channel(Arc::new(StateSnapshot::default()));
    let (_preview_tx, preview_rx) = tokio::sync::mpsc::channel(1);
    let mut app = UiApp::new(state_rx, preview_rx, cmd_tx);
    Arc::make_mut(&mut app.snapshot).sessions = sessions;
    app
}

fn benchmark_layout() -> ui::UiLayout {
    ui::compute_layout(Rect::new(0, 0, 80, 24))
}

// ── Benchmarks ──────────────────────────────────────────────────────

fn bench_handle_browse_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("handle_browse_key");

    for n in [3, 10, 26] {
        group.bench_function(format!("j_nav_{n}_sessions"), |b| {
            b.iter_batched(
                || make_app_with_n_sessions(n),
                |mut app| {
                    app.select_next();
                    app
                },
                criterion::BatchSize::SmallInput,
            );
        });

        group.bench_function(format!("k_nav_{n}_sessions"), |b| {
            b.iter_batched(
                || {
                    let mut app = make_app_with_n_sessions(n);
                    app.selected = n.saturating_sub(1);
                    app
                },
                |mut app| {
                    app.select_prev();
                    app
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_handle_mouse(c: &mut Criterion) {
    let mut group = c.benchmark_group("handle_mouse");

    // Sidebar click
    group.bench_function("sidebar_click", |b| {
        b.iter_batched(
            || {
                let app = make_app_with_n_sessions(10);
                let layout = benchmark_layout();
                (app, layout)
            },
            |(mut app, layout)| {
                let mouse = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 10,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                };
                app.handle_mouse(black_box(mouse), &layout);
                (app, layout)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Preview scroll
    group.bench_function("preview_scroll", |b| {
        b.iter_batched(
            || {
                let mut app = make_app_with_n_sessions(3);
                app.preview.set_text("line\n".repeat(200));
                let layout = benchmark_layout();
                (app, layout)
            },
            |(mut app, layout)| {
                let mouse = MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 40,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                };
                app.handle_mouse(black_box(mouse), &layout);
                (app, layout)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Preview click (in Compose mode)
    group.bench_function("preview_click_compose", |b| {
        b.iter_batched(
            || {
                let mut app = make_app_with_n_sessions(3);
                app.mode = hydra::app::Mode::Compose;
                app.preview.scroll_offset = 10;
                let layout = benchmark_layout();
                (app, layout)
            },
            |(mut app, layout)| {
                let mouse = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 40,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                };
                app.handle_mouse(black_box(mouse), &layout);
                (app, layout)
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_parse_diff_numstat(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_diff_numstat");

    for n in [5, 50, 200] {
        let input: String = (0..n)
            .map(|i| format!("{}\t{}\tsrc/file_{}.rs\n", i * 3 + 1, i + 1, i))
            .collect();

        group.bench_function(format!("{n}_files"), |b| {
            b.iter(|| hydra::app::parse_diff_numstat(black_box(&input)));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_handle_browse_key,
    bench_handle_mouse,
    bench_parse_diff_numstat,
);
criterion_main!(benches);
