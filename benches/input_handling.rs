use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::sync::Arc;

use hydra::app::{StateSnapshot, UiApp};
use hydra::session::{AgentType, Session, SessionStatus};

// ── Helpers ─────────────────────────────────────────────────────────

fn make_session(name: &str, status: SessionStatus) -> Session {
    Session {
        name: name.to_string(),
        tmux_name: format!("hydra-bench-{name}"),
        agent_type: AgentType::Claude,
        status,
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
                0 => SessionStatus::Idle,
                1 => SessionStatus::Running,
                _ => SessionStatus::Exited,
            };
            make_session(&name, status)
        })
        .collect();

    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(1);
    let (_state_tx, state_rx) = tokio::sync::watch::channel(Arc::new(StateSnapshot::default()));
    let (_preview_tx, preview_rx) = tokio::sync::mpsc::channel(1);
    let mut app = UiApp::new(state_rx, preview_rx, cmd_tx);
    app.sessions = sessions;
    app
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
    use ratatui::layout::Rect;

    let mut group = c.benchmark_group("handle_mouse");

    // Sidebar click
    group.bench_function("sidebar_click", |b| {
        b.iter_batched(
            || {
                let app = make_app_with_n_sessions(10);
                app.sidebar_area.set(Rect::new(0, 0, 20, 24));
                app.preview_area.set(Rect::new(20, 0, 60, 24));
                app
            },
            |mut app| {
                let mouse = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 10,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                };
                app.handle_mouse(black_box(mouse));
                app
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Preview scroll
    group.bench_function("preview_scroll", |b| {
        b.iter_batched(
            || {
                let mut app = make_app_with_n_sessions(3);
                app.sidebar_area.set(Rect::new(0, 0, 20, 24));
                app.preview_area.set(Rect::new(20, 0, 60, 24));
                app.preview.set_text("line\n".repeat(200));
                app
            },
            |mut app| {
                let mouse = MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 40,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                };
                app.handle_mouse(black_box(mouse));
                app
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
                app.sidebar_area.set(Rect::new(0, 0, 20, 24));
                app.preview_area.set(Rect::new(20, 0, 60, 24));
                app.preview.scroll_offset = 10;
                app
            },
            |mut app| {
                let mouse = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 40,
                    row: 12,
                    modifiers: KeyModifiers::NONE,
                };
                app.handle_mouse(black_box(mouse));
                app
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
