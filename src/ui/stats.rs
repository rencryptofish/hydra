use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::app::UiApp;
use crate::logs::{format_cost, format_tokens};
use crate::ui::truncate_chars;

pub fn draw_stats(frame: &mut Frame, app: &UiApp, area: Rect) {
    let inner_width = area.width.saturating_sub(2) as usize;

    let specs = [
        StatsLineSpec {
            label: "Claude",
            short_label: "Cl",
            cost: format_cost(app.snapshot.global_stats.claude_cost_usd()),
            tokens: format_tokens(app.snapshot.global_stats.claude_display_tokens()),
        },
        StatsLineSpec {
            label: "Codex",
            short_label: "Cx",
            cost: format_cost(app.snapshot.global_stats.codex_cost_usd()),
            tokens: format_tokens(app.snapshot.global_stats.codex_display_tokens()),
        },
        StatsLineSpec {
            label: "Gemini",
            short_label: "Ge",
            cost: format_cost(app.snapshot.global_stats.gemini_cost_usd()),
            tokens: format_tokens(app.snapshot.global_stats.gemini_display_tokens()),
        },
    ];

    let lines: Vec<Line> = if let Some(layout) = choose_stats_layout(&specs, inner_width) {
        specs
            .iter()
            .map(|spec| {
                let line = render_stats_line(spec, layout);
                Line::from(Span::styled(line, Style::default()))
            })
            .collect()
    } else {
        specs
            .iter()
            .map(|spec| {
                let line = format_agent_stats_line_compact(
                    spec.label,
                    spec.short_label,
                    &spec.cost,
                    &spec.tokens,
                    inner_width,
                );
                Line::from(Span::styled(line, Style::default()))
            })
            .collect()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Stats ")
        .border_style(Style::default().fg(ratatui::style::Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

#[derive(Clone, Copy)]
enum StatsLabelMode {
    Full,
    Short,
}

#[derive(Clone, Copy)]
struct StatsLayout {
    label_mode: StatsLabelMode,
    show_tokens: bool,
    label_width: usize,
    cost_width: usize,
    tokens_width: usize,
}

struct StatsLineSpec<'a> {
    label: &'a str,
    short_label: &'a str,
    cost: String,
    tokens: String,
}

fn choose_stats_layout(specs: &[StatsLineSpec<'_>], inner_width: usize) -> Option<StatsLayout> {
    let candidates = [
        (StatsLabelMode::Full, true),
        (StatsLabelMode::Short, true),
        (StatsLabelMode::Full, false),
        (StatsLabelMode::Short, false),
    ];

    for (label_mode, show_tokens) in candidates {
        if let Some(layout) = build_stats_layout(specs, inner_width, label_mode, show_tokens) {
            return Some(layout);
        }
    }

    None
}

fn build_stats_layout(
    specs: &[StatsLineSpec<'_>],
    inner_width: usize,
    label_mode: StatsLabelMode,
    show_tokens: bool,
) -> Option<StatsLayout> {
    if inner_width == 0 {
        return None;
    }

    let label_width = specs
        .iter()
        .map(|spec| stats_label(spec, label_mode).chars().count())
        .max()
        .unwrap_or(0);
    let cost_width = specs
        .iter()
        .map(|spec| spec.cost.chars().count())
        .max()
        .unwrap_or(0);
    let tokens_width = if show_tokens {
        specs
            .iter()
            .map(|spec| spec.tokens.chars().count())
            .max()
            .unwrap_or(0)
    } else {
        0
    };

    let required = label_width + 1 + cost_width + if show_tokens { 1 + tokens_width } else { 0 };
    if required > inner_width {
        return None;
    }

    Some(StatsLayout {
        label_mode,
        show_tokens,
        label_width,
        cost_width,
        tokens_width,
    })
}

fn stats_label<'a>(spec: &'a StatsLineSpec<'a>, mode: StatsLabelMode) -> &'a str {
    match mode {
        StatsLabelMode::Full => spec.label,
        StatsLabelMode::Short => spec.short_label,
    }
}

fn render_stats_line(spec: &StatsLineSpec<'_>, layout: StatsLayout) -> String {
    let label = stats_label(spec, layout.label_mode);
    if layout.show_tokens {
        format!(
            "{:<label_w$} {:>cost_w$} {:>tokens_w$}",
            label,
            spec.cost,
            spec.tokens,
            label_w = layout.label_width,
            cost_w = layout.cost_width,
            tokens_w = layout.tokens_width
        )
    } else {
        format!(
            "{:<label_w$} {:>cost_w$}",
            label,
            spec.cost,
            label_w = layout.label_width,
            cost_w = layout.cost_width
        )
    }
}

fn format_agent_stats_line_compact(
    label: &str,
    short_label: &str,
    cost: &str,
    tokens: &str,
    inner_width: usize,
) -> String {
    if inner_width == 0 {
        return String::new();
    }

    let full = format!("{label} {cost} {tokens}");
    if full.chars().count() <= inner_width {
        return full;
    }

    let short_with_tokens = format!("{short_label} {cost} {tokens}");
    if short_with_tokens.chars().count() <= inner_width {
        return short_with_tokens;
    }

    let no_tokens = format!("{label} {cost}");
    if no_tokens.chars().count() <= inner_width {
        return no_tokens;
    }

    let short_no_tokens = format!("{short_label} {cost}");
    if short_no_tokens.chars().count() <= inner_width {
        return short_no_tokens;
    }

    truncate_chars(&short_no_tokens, inner_width)
}
