use criterion::{black_box, criterion_group, criterion_main, Criterion};
use hydra::app::{DiffFile, StateSnapshot, UiApp};
use hydra::logs::GlobalStats;
use hydra::session::{AgentType, Session, VisualStatus, ProcessState, AgentState};
use hydra::ui;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use std::sync::Arc;

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
    let snap = Arc::make_mut(&mut app.snapshot);
    snap.sessions = sessions;

    // Add last messages for each session
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        let name = if i < NATO.len() {
            NATO[i].to_string()
        } else {
            format!("agent-{}", i + 1)
        };
        snap.last_messages
            .insert(name, "Working on implementing the feature...".to_string());
    }

    app
}

fn make_diff_files(n: usize) -> Vec<DiffFile> {
    let dirs = ["src/", "tests/", "benches/", "src/components/", "lib/"];
    (0..n)
        .map(|i| DiffFile {
            path: format!("{}file_{}.rs", dirs[i % dirs.len()], i),
            insertions: (i as u32 * 7 + 3) % 200,
            deletions: (i as u32 * 3 + 1) % 50,
            untracked: i % 10 == 0,
        })
        .collect()
}

fn generate_preview_lines(n: usize) -> String {
    (0..n)
        .map(|i| format!("line {i}: some output text from the agent process"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Benchmarks ──────────────────────────────────────────────────────

fn bench_draw_full_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("draw_full_frame");

    for n in [0, 3, 10, 26] {
        group.bench_function(format!("{n}_sessions"), |b| {
            let mut app = make_app_with_n_sessions(n);
            app.preview
                .set_text("Hello from the agent\nLine 2\nLine 3".to_string());
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();

            b.iter(|| {
                terminal
                    .draw(|frame| {
                        ui::draw(frame, black_box(&app));
                    })
                    .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_draw_sidebar(c: &mut Criterion) {
    let mut group = c.benchmark_group("draw_sidebar");

    for n in [3, 10, 26] {
        group.bench_function(format!("{n}_sessions"), |b| {
            let app = make_app_with_n_sessions(n);
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let area = Rect::new(0, 0, 20, 24);

            b.iter(|| {
                terminal
                    .draw(|frame| {
                        ui::draw_sidebar(frame, black_box(&app), area);
                    })
                    .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_draw_preview(c: &mut Criterion) {
    let mut group = c.benchmark_group("draw_preview");

    for (label, lines) in [("10_lines", 10), ("100_lines", 100), ("5000_lines", 5000)] {
        group.bench_function(label, |b| {
            let mut app = make_app_with_n_sessions(1);
            app.preview.set_text(generate_preview_lines(lines));
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let area = Rect::new(20, 0, 60, 24);

            b.iter(|| {
                terminal
                    .draw(|frame| {
                        ui::draw_preview(frame, black_box(&app), area);
                    })
                    .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_build_diff_tree_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_diff_tree_lines");

    for (n, width) in [(5, 30), (50, 30), (200, 30), (50, 60)] {
        group.bench_function(format!("{n}_files_w{width}"), |b| {
            let files = make_diff_files(n);
            b.iter(|| {
                ui::build_diff_tree_lines(black_box(&files), black_box(width));
            });
        });
    }

    group.finish();
}

fn bench_draw_large_terminal(c: &mut Criterion) {
    let mut group = c.benchmark_group("draw_large_terminal");

    for (w, h) in [(120, 40), (200, 60)] {
        group.bench_function(format!("{w}x{h}"), |b| {
            let mut app = make_app_with_n_sessions(10);
            app.preview.set_text(generate_preview_lines(200));
            Arc::make_mut(&mut app.snapshot).diff_files = make_diff_files(20);
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();

            b.iter(|| {
                terminal
                    .draw(|frame| {
                        ui::draw(frame, black_box(&app));
                    })
                    .unwrap();
            });
        });
    }

    group.finish();
}

fn bench_draw_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("draw_stats");

    group.bench_function("with_stats", |b| {
        let mut app = make_app_with_n_sessions(5);
        let snap = Arc::make_mut(&mut app.snapshot);
        snap.global_stats = GlobalStats::default();
        snap.diff_files = make_diff_files(10);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 20, 20, 3);

        b.iter(|| {
            terminal
                .draw(|frame| {
                    ui::draw_stats(frame, black_box(&app), area);
                })
                .unwrap();
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_draw_full_frame,
    bench_draw_sidebar,
    bench_draw_preview,
    bench_build_diff_tree_lines,
    bench_draw_large_terminal,
    bench_draw_stats,
);
criterion_main!(benches);
