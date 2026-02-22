use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossterm::event::{KeyCode, KeyModifiers};
use std::io::Write;

use hydra::logs::{
    extract_assistant_message_text, format_cost, format_tokens,
    update_session_stats_from_path_and_last_message, SessionStats,
};
use hydra::tmux::{apply_tmux_modifiers, keycode_to_tmux};

// ── Helpers ─────────────────────────────────────────────────────────

fn make_jsonl_line_assistant(i: usize) -> String {
    format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Response number {i} with some content to parse"}}],"usage":{{"input_tokens":{},"output_tokens":{},"cache_creation_input_tokens":50,"cache_read_input_tokens":100}}}}}}"#,
        1000 + i * 10,
        200 + i * 5,
    )
}

fn make_jsonl_line_user(i: usize) -> String {
    format!(
        r#"{{"type":"human","message":{{"content":[{{"type":"text","text":"User message {i}"}}]}}}}"#,
    )
}

fn make_jsonl_line_tool(i: usize) -> String {
    format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Write","input":{{"file_path":"src/file_{i}.rs","content":"fn main() {{}}"}}}}],"usage":{{"input_tokens":500,"output_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":200}}}}}}"#,
    )
}

fn create_jsonl_file(n: usize) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    for i in 0..n {
        match i % 3 {
            0 => writeln!(file, "{}", make_jsonl_line_user(i)).unwrap(),
            1 => writeln!(file, "{}", make_jsonl_line_assistant(i)).unwrap(),
            _ => writeln!(file, "{}", make_jsonl_line_tool(i)).unwrap(),
        }
    }
    file.flush().unwrap();
    file
}

// ── Benchmarks ──────────────────────────────────────────────────────

fn bench_jsonl_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonl_parsing");

    for n in [10, 100, 1000] {
        // Full parse (offset = 0)
        group.bench_function(format!("full_{n}_lines"), |b| {
            let file = create_jsonl_file(n);
            b.iter(|| {
                let mut stats = SessionStats::default();
                update_session_stats_from_path_and_last_message(black_box(file.path()), &mut stats);
            });
        });

        // Incremental parse (offset at ~80%)
        group.bench_function(format!("incremental_{n}_lines"), |b| {
            let file = create_jsonl_file(n);
            // Do a first pass to get the offset
            let mut base_stats = SessionStats::default();
            update_session_stats_from_path_and_last_message(file.path(), &mut base_stats);
            // Set offset to ~80% of file
            let file_len = std::fs::metadata(file.path()).unwrap().len();
            let offset_80 = (file_len as f64 * 0.8) as u64;

            b.iter(|| {
                let mut stats = hydra::logs::SessionStats {
                    read_offset: offset_80,
                    ..Default::default()
                };
                update_session_stats_from_path_and_last_message(black_box(file.path()), &mut stats);
            });
        });
    }

    group.finish();
}

fn bench_extract_assistant_message(c: &mut Criterion) {
    let mut group = c.benchmark_group("extract_assistant_message");

    // Simple single-text content
    let simple: serde_json::Value = serde_json::from_str(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello, I can help with that."}]}}"#,
    )
    .unwrap();
    group.bench_function("simple", |b| {
        b.iter(|| extract_assistant_message_text(black_box(&simple)));
    });

    // Multi-content (text + tool_use)
    let multi: serde_json::Value = serde_json::from_str(
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look at that file."},{"type":"tool_use","name":"Read","input":{"path":"src/main.rs"}},{"type":"text","text":"I see the issue."}]}}"#,
    )
    .unwrap();
    group.bench_function("multi_content", |b| {
        b.iter(|| extract_assistant_message_text(black_box(&multi)));
    });

    // Large text value (~2KB)
    let large_text = "A".repeat(2000);
    let large: serde_json::Value = serde_json::from_str(&format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{large_text}"}}]}}}}"#,
    ))
    .unwrap();
    group.bench_function("large_2kb_text", |b| {
        b.iter(|| extract_assistant_message_text(black_box(&large)));
    });

    // Non-assistant message (should return None quickly)
    let non_assistant: serde_json::Value = serde_json::from_str(
        r#"{"type":"human","message":{"content":[{"type":"text","text":"User input here"}]}}"#,
    )
    .unwrap();
    group.bench_function("non_assistant_none", |b| {
        b.iter(|| extract_assistant_message_text(black_box(&non_assistant)));
    });

    group.finish();
}

fn bench_format_functions(c: &mut Criterion) {
    let mut group = c.benchmark_group("format_functions");

    // format_tokens
    for (label, val) in [
        ("small_500", 500u64),
        ("medium_50k", 50_000),
        ("large_2m", 2_000_000),
    ] {
        group.bench_function(format!("tokens_{label}"), |b| {
            b.iter(|| format_tokens(black_box(val)));
        });
    }

    // format_cost
    for (label, val) in [
        ("zero", 0.0f64),
        ("small_0_50", 0.50),
        ("medium_5_00", 5.00),
        ("large_25", 25.0),
    ] {
        group.bench_function(format!("cost_{label}"), |b| {
            b.iter(|| format_cost(black_box(val)));
        });
    }

    group.finish();
}

fn bench_keycode_mapping(c: &mut Criterion) {
    let mut group = c.benchmark_group("keycode_mapping");

    // Simple character key
    group.bench_function("char_key", |b| {
        b.iter(|| keycode_to_tmux(black_box(KeyCode::Char('a')), black_box(KeyModifiers::NONE)));
    });

    // Character with Ctrl modifier
    group.bench_function("ctrl_char", |b| {
        b.iter(|| {
            keycode_to_tmux(
                black_box(KeyCode::Char('c')),
                black_box(KeyModifiers::CONTROL),
            )
        });
    });

    // Special key (Enter)
    group.bench_function("enter_key", |b| {
        b.iter(|| keycode_to_tmux(black_box(KeyCode::Enter), black_box(KeyModifiers::NONE)));
    });

    // Function key with modifiers
    group.bench_function("f5_ctrl_shift", |b| {
        b.iter(|| {
            keycode_to_tmux(
                black_box(KeyCode::F(5)),
                black_box(KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            )
        });
    });

    // apply_tmux_modifiers directly
    group.bench_function("apply_modifiers_all", |b| {
        b.iter(|| {
            apply_tmux_modifiers(
                black_box("Up"),
                black_box(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT),
            )
        });
    });

    // Unmapped key (should return None)
    group.bench_function("unmapped_key", |b| {
        b.iter(|| keycode_to_tmux(black_box(KeyCode::CapsLock), black_box(KeyModifiers::NONE)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_jsonl_parsing,
    bench_extract_assistant_message,
    bench_format_functions,
    bench_keycode_mapping,
);
criterion_main!(benches);
