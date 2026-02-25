use std::collections::VecDeque;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::logs::ConversationEntry;

fn push_component_title(lines: &mut Vec<Line<'static>>, title: &str, style: Style) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(title.to_string(), style)));
}

fn push_component_body(lines: &mut Vec<Line<'static>>, text: &str, style: Style) {
    for line in text.lines() {
        lines.push(Line::from(Span::styled(format!("  {line}"), style)));
    }
}

fn push_tool_result_component(
    lines: &mut Vec<Line<'static>>,
    filenames: &[String],
    summary: Option<&str>,
    style: Style,
) {
    push_component_title(lines, "TOOL RESULT", style);
    let preview_count = filenames.len().min(4);
    for file in filenames.iter().take(preview_count) {
        lines.push(Line::from(Span::styled(format!("  - {file}"), style)));
    }
    if filenames.len() > preview_count {
        lines.push(Line::from(Span::styled(
            format!("  ... +{} more", filenames.len() - preview_count),
            style.add_modifier(Modifier::DIM),
        )));
    }
    if let Some(summary) = summary {
        for line in summary.lines().take(3) {
            lines.push(Line::from(Span::styled(format!("  > {line}"), style)));
        }
    }
}

fn push_unparsed_component(
    lines: &mut Vec<Line<'static>>,
    reason: &str,
    raw: &str,
    reason_style: Style,
    raw_style: Style,
) {
    lines.push(Line::from(vec![
        Span::styled(format!("  [{reason}] "), reason_style),
        Span::styled(raw.to_string(), raw_style),
    ]));
}

/// Render conversation entries into styled `Text` for the preview pane.
pub fn render_conversation(entries: &VecDeque<ConversationEntry>) -> ratatui::text::Text<'static> {
    if entries.is_empty() {
        return ratatui::text::Text::from(Line::from(Span::styled(
            "Waiting for agent output...",
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    let user_title = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let assistant_title = Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD);
    let tool_title = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let queue_title = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let progress_title = Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD);
    let system_title = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let snapshot_title = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let body = Style::default();
    let dim = Style::default().add_modifier(Modifier::DIM);
    let warn = Style::default().fg(Color::Magenta);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut unparsed_lines: Vec<Line<'static>> = Vec::new();

    for entry in entries {
        match entry {
            ConversationEntry::UserMessage { text } => {
                push_component_title(&mut lines, "USER", user_title);
                push_component_body(&mut lines, text, body);
            }
            ConversationEntry::AssistantText { text } => {
                push_component_title(&mut lines, "ASSISTANT", assistant_title);
                push_component_body(&mut lines, text, body);
            }
            ConversationEntry::ToolUse { tool_name, details } => {
                push_component_title(&mut lines, "TOOL", tool_title);
                lines.push(Line::from(Span::styled(format!("  {tool_name}"), dim)));
                if let Some(details) = details {
                    lines.push(Line::from(Span::styled(format!("  {details}"), dim)));
                }
            }
            ConversationEntry::ToolResult { filenames, summary } => {
                push_tool_result_component(&mut lines, filenames, summary.as_deref(), dim);
            }
            ConversationEntry::QueueOperation { operation, task_id } => {
                push_component_title(&mut lines, "SUBAGENT", queue_title);
                let text = match task_id {
                    Some(task_id) => format!("  {operation} ({task_id})"),
                    None => format!("  {operation}"),
                };
                lines.push(Line::from(Span::styled(text, dim)));
            }
            ConversationEntry::Progress { kind, detail } => {
                push_component_title(&mut lines, &format!("PROGRESS ({kind})"), progress_title);
                lines.push(Line::from(Span::styled(format!("  {detail}"), dim)));
            }
            ConversationEntry::SystemEvent { subtype, detail } => {
                push_component_title(&mut lines, &format!("SYSTEM ({subtype})"), system_title);
                lines.push(Line::from(Span::styled(format!("  {detail}"), dim)));
            }
            ConversationEntry::FileHistorySnapshot {
                tracked_files,
                files,
                is_update,
            } => {
                push_component_title(&mut lines, "FILE SNAPSHOT", snapshot_title);
                let kind = if *is_update { "update" } else { "new" };
                lines.push(Line::from(Span::styled(
                    format!("  {kind}: {tracked_files} tracked file(s)"),
                    dim,
                )));
                for file in files {
                    lines.push(Line::from(Span::styled(format!("  - {file}"), dim)));
                }
                if *tracked_files > files.len() {
                    lines.push(Line::from(Span::styled(
                        format!("  ... +{} more", tracked_files - files.len()),
                        dim,
                    )));
                }
            }
            ConversationEntry::Unparsed { reason, raw } => {
                push_unparsed_component(&mut unparsed_lines, reason, raw, warn, dim);
            }
        }
    }

    if !unparsed_lines.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "UNPARSED JSONL",
            warn.add_modifier(Modifier::BOLD),
        )));
        lines.extend(unparsed_lines);
    }

    ratatui::text::Text::from(lines)
}

#[cfg(test)]
mod tests {
    use crate::logs::ConversationEntry;
    use std::collections::VecDeque;

    #[test]
    fn conversation_empty() {
        let entries = VecDeque::new();
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_basic() {
        let mut entries = VecDeque::new();
        entries.push_back(ConversationEntry::UserMessage {
            text: "Fix the bug".to_string(),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "I'll fix that for you.".to_string(),
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Edit".to_string(),
            details: Some("id=t1 | file=src/main.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/main.rs".to_string()],
            summary: Some("updated file successfully".to_string()),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "Done! The bug is fixed.".to_string(),
        });
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_tool_heavy() {
        let mut entries = VecDeque::new();
        entries.push_back(ConversationEntry::UserMessage {
            text: "Refactor the module".to_string(),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "Let me read the files first.".to_string(),
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Read".to_string(),
            details: Some("path=src/app.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/app.rs".to_string()],
            summary: None,
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Read".to_string(),
            details: Some("path=src/ui.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/ui.rs".to_string()],
            summary: None,
        });
        entries.push_back(ConversationEntry::ToolUse {
            tool_name: "Edit".to_string(),
            details: Some("id=t3 | file=src/app.rs".to_string()),
        });
        entries.push_back(ConversationEntry::ToolResult {
            filenames: vec!["src/app.rs".to_string(), "src/ui.rs".to_string()],
            summary: Some("2 files modified".to_string()),
        });
        entries.push_back(ConversationEntry::AssistantText {
            text: "Refactoring complete.".to_string(),
        });
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_with_unparsed_logs() {
        let mut entries = VecDeque::new();
        entries.push_back(ConversationEntry::UserMessage {
            text: "show me diagnostics".to_string(),
        });
        entries.push_back(ConversationEntry::Unparsed {
            reason: "Malformed JSONL".to_string(),
            raw: "{\"type\":\"assistant\" BROKEN".to_string(),
        });
        entries.push_back(ConversationEntry::QueueOperation {
            operation: "enqueue".to_string(),
            task_id: Some("task-1".to_string()),
        });
        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn conversation_with_progress_system_and_snapshot() {
        let mut entries = VecDeque::new();
        entries.push_back(ConversationEntry::Progress {
            kind: "waiting_for_task".to_string(),
            detail: "Run integration suite (local_bash)".to_string(),
        });
        entries.push_back(ConversationEntry::SystemEvent {
            subtype: "api_error".to_string(),
            detail: "API error | attempt 2/10 | retry in 536ms".to_string(),
        });
        entries.push_back(ConversationEntry::FileHistorySnapshot {
            tracked_files: 4,
            files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            is_update: true,
        });

        let text = super::render_conversation(&entries);
        let rendered: String = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("PROGRESS (waiting_for_task)"));
        assert!(rendered.contains("SYSTEM (api_error)"));
        assert!(rendered.contains("FILE SNAPSHOT"));
        assert!(rendered.contains("update: 4 tracked file(s)"));
        assert!(rendered.contains("... +2 more"));
    }
}
