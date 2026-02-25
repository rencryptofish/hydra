use std::collections::BTreeMap;

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::truncate_chars;

#[derive(Clone, Copy, Debug, Default)]
struct DiffAggregate {
    insertions: u32,
    deletions: u32,
    untracked: u32,
}

impl DiffAggregate {
    fn add_file(&mut self, f: &crate::app::DiffFile) {
        self.insertions += f.insertions;
        self.deletions += f.deletions;
        if f.untracked {
            self.untracked += 1;
        }
    }
}

struct DiffTreeFile {
    name: String,
    diff: crate::app::DiffFile,
}

#[derive(Default)]
struct DiffTreeNode {
    dirs: BTreeMap<String, DiffTreeNode>,
    files: Vec<DiffTreeFile>,
    aggregate: DiffAggregate,
}

impl DiffTreeNode {
    fn add_path(&mut self, f: &crate::app::DiffFile) {
        if f.path.ends_with('/') {
            return;
        }

        let mut parts: Vec<&str> = f.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return;
        }

        let filename = match parts.pop() {
            Some(name) if !name.is_empty() => name,
            _ => return,
        };

        self.aggregate.add_file(f);

        let mut node = self;
        for segment in parts {
            node = node.dirs.entry(segment.to_string()).or_default();
            node.aggregate.add_file(f);
        }

        node.files.push(DiffTreeFile {
            name: filename.to_string(),
            diff: f.clone(),
        });
    }

    fn sort_recursive(&mut self) {
        self.files.sort_by(|a, b| a.name.cmp(&b.name));
        for child in self.dirs.values_mut() {
            child.sort_recursive();
        }
    }
}

enum DiffTreeEntry<'a> {
    Dir {
        name: &'a str,
        node: &'a DiffTreeNode,
    },
    File(&'a DiffTreeFile),
}

fn tree_prefix(ancestors_have_next: &[bool], is_last: bool) -> String {
    let mut prefix = String::new();
    for has_next in ancestors_have_next {
        prefix.push_str(if *has_next { "│ " } else { "  " });
    }
    prefix.push_str(if is_last { "└ " } else { "├ " });
    prefix
}

fn format_tree_stat(aggregate: DiffAggregate) -> String {
    if aggregate.insertions == 0 && aggregate.deletions == 0 {
        return match aggregate.untracked {
            0 => String::new(),
            1 => "new".to_string(),
            n => format!("new{n}"),
        };
    }
    format_compact_diff(aggregate.insertions, aggregate.deletions)
}

fn push_tree_line(
    lines: &mut Vec<Line<'static>>,
    width: usize,
    prefix: &str,
    label: String,
    aggregate: DiffAggregate,
    is_untracked_file: bool,
) {
    let green = Style::default().fg(Color::Green);
    let red = Style::default().fg(Color::Red);
    let cyan = Style::default().fg(Color::Cyan);

    let inner_w = width.saturating_sub(1); // leave 1 char margin
    let stat = format_tree_stat(aggregate);
    let stat_len = stat.chars().count();
    let prefix_len = prefix.chars().count();
    let min_gap = usize::from(!stat.is_empty());
    let available = inner_w.saturating_sub(prefix_len + stat_len + min_gap);

    let label_chars = label.chars().count();
    let clipped_label = if available == 0 {
        String::new()
    } else if label_chars > available && available > 1 {
        format!("{}…", truncate_chars(&label, available - 1))
    } else if label_chars > available {
        truncate_chars(&label, available)
    } else {
        label
    };

    let clipped_chars = clipped_label.chars().count();
    let padding = if stat.is_empty() {
        0
    } else {
        inner_w.saturating_sub(prefix_len + clipped_chars + stat_len)
    };

    let mut spans = vec![Span::styled(
        format!("{prefix}{clipped_label}{}", " ".repeat(padding)),
        Style::default(),
    )];

    if !stat.is_empty() {
        if aggregate.insertions == 0 && aggregate.deletions == 0 {
            spans.push(Span::styled(stat, cyan));
        } else {
            if aggregate.insertions > 0 {
                spans.push(Span::styled(format!("+{}", aggregate.insertions), green));
            }
            if aggregate.deletions > 0 {
                spans.push(Span::styled(format!("-{}", aggregate.deletions), red));
            }
            if is_untracked_file && aggregate.untracked > 0 {
                spans.push(Span::styled("new", cyan));
            }
        }
    }

    lines.push(Line::from(spans));
}

fn render_diff_tree(
    node: &DiffTreeNode,
    lines: &mut Vec<Line<'static>>,
    width: usize,
    ancestors_have_next: &[bool],
) {
    let total_entries = node.dirs.len() + node.files.len();
    if total_entries == 0 {
        return;
    }

    let mut entries: Vec<DiffTreeEntry<'_>> = Vec::with_capacity(total_entries);
    for (name, child) in &node.dirs {
        entries.push(DiffTreeEntry::Dir { name, node: child });
    }
    for file in &node.files {
        entries.push(DiffTreeEntry::File(file));
    }

    for (idx, entry) in entries.iter().enumerate() {
        let is_last = idx + 1 == total_entries;
        let prefix = tree_prefix(ancestors_have_next, is_last);

        match entry {
            DiffTreeEntry::Dir { name, node } => {
                push_tree_line(
                    lines,
                    width,
                    &prefix,
                    format!("{name}/"),
                    node.aggregate,
                    false,
                );

                let mut next_ancestors = ancestors_have_next.to_vec();
                next_ancestors.push(!is_last);
                render_diff_tree(node, lines, width, &next_ancestors);
            }
            DiffTreeEntry::File(file) => {
                let aggregate = DiffAggregate {
                    insertions: file.diff.insertions,
                    deletions: file.diff.deletions,
                    untracked: u32::from(file.diff.untracked),
                };
                push_tree_line(
                    lines,
                    width,
                    &prefix,
                    file.name.clone(),
                    aggregate,
                    file.diff.untracked,
                );
            }
        }
    }
}

/// Build lines for a compact diff tree grouped by directory.
/// Renders nested directories with branch guides and per-directory rollups.
pub fn build_diff_tree_lines(
    diff_files: &[crate::app::DiffFile],
    width: usize,
) -> Vec<Line<'static>> {
    if diff_files.is_empty() {
        return vec![];
    }

    let mut root = DiffTreeNode::default();
    for f in diff_files {
        root.add_path(f);
    }
    root.sort_recursive();

    let mut lines: Vec<Line<'static>> = Vec::new();
    render_diff_tree(&root, &mut lines, width, &[]);
    lines
}

/// Format compact diff: "+45-12", "+45", "-12"
fn format_compact_diff(ins: u32, del: u32) -> String {
    match (ins > 0, del > 0) {
        (true, true) => format!("+{ins}-{del}"),
        (true, false) => format!("+{ins}"),
        (false, true) => format!("-{del}"),
        (false, false) => String::new(),
    }
}

pub(crate) fn draw_diff_tree(
    frame: &mut Frame,
    app: &crate::app::UiApp,
    lines: &[Line],
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Changes ")
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_rows = inner.height as usize;
    let total_lines = lines.len();
    let max_offset = total_lines.saturating_sub(max_rows);
    let capped_offset = (app.diff_scroll_offset as usize).min(max_offset);

    let start = total_lines
        .saturating_sub(max_rows)
        .saturating_sub(capped_offset);
    let end = (start + max_rows).min(total_lines);
    let visible: Vec<Line> = lines[start..end].to_vec();

    let paragraph = Paragraph::new(visible);
    frame.render_widget(paragraph, inner);
}

#[cfg(test)]
mod tests {
    use crate::app::DiffFile;

    // ── format_compact_diff unit tests ──────────────────────────────

    #[test]
    fn format_compact_diff_both() {
        assert_eq!(super::format_compact_diff(10, 5), "+10-5");
    }

    #[test]
    fn format_compact_diff_insert_only() {
        assert_eq!(super::format_compact_diff(7, 0), "+7");
    }

    #[test]
    fn format_compact_diff_delete_only() {
        assert_eq!(super::format_compact_diff(0, 3), "-3");
    }

    #[test]
    fn format_compact_diff_zero() {
        assert_eq!(super::format_compact_diff(0, 0), "");
    }

    // ── build_diff_tree_lines unit tests ─────────────────────────────

    #[test]
    fn diff_tree_empty() {
        let lines = super::build_diff_tree_lines(&[], 40);
        assert!(lines.is_empty());
    }

    #[test]
    fn diff_tree_root_level_file() {
        let files = vec![DiffFile {
            path: "README.md".into(),
            insertions: 3,
            deletions: 0,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_with_directory() {
        let files = vec![
            DiffFile {
                path: "src/app.rs".into(),
                insertions: 10,
                deletions: 2,
                untracked: false,
            },
            DiffFile {
                path: "src/ui.rs".into(),
                insertions: 5,
                deletions: 0,
                untracked: false,
            },
        ];
        let lines = super::build_diff_tree_lines(&files, 40);
        // 1 directory header + 2 files
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn diff_tree_multiple_directories() {
        let files = vec![
            DiffFile {
                path: "src/app.rs".into(),
                insertions: 1,
                deletions: 0,
                untracked: false,
            },
            DiffFile {
                path: "tests/cli.rs".into(),
                insertions: 2,
                deletions: 1,
                untracked: false,
            },
        ];
        let lines = super::build_diff_tree_lines(&files, 40);
        // 2 directory headers + 2 files
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn diff_tree_deletion_only_file() {
        let files = vec![DiffFile {
            path: "old.rs".into(),
            insertions: 0,
            deletions: 15,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_narrow_width_truncates_name() {
        let files = vec![DiffFile {
            path: "src/very_long_filename_that_exceeds.rs".into(),
            insertions: 100,
            deletions: 50,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 10);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_narrow_width_truncates_directory() {
        let files = vec![DiffFile {
            path: "very/deeply/nested/directory/structure/file.rs".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // inner_w = 11, dir "very/deeply/nested/directory/structure/" is 40 chars > 11
        let lines = super::build_diff_tree_lines(&files, 12);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_zero_changes_file() {
        let files = vec![DiffFile {
            path: "unchanged.rs".into(),
            insertions: 0,
            deletions: 0,
            untracked: false,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_untracked_file() {
        let files = vec![DiffFile {
            path: "new_file.rs".into(),
            insertions: 0,
            deletions: 0,
            untracked: true,
        }];
        let lines = super::build_diff_tree_lines(&files, 40);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn diff_tree_mixed_tracked_and_untracked() {
        let files = vec![
            DiffFile {
                path: "src/app.rs".into(),
                insertions: 10,
                deletions: 2,
                untracked: false,
            },
            DiffFile {
                path: "src/new.rs".into(),
                insertions: 0,
                deletions: 0,
                untracked: true,
            },
        ];
        let lines = super::build_diff_tree_lines(&files, 40);
        // 1 directory header + 2 files
        assert_eq!(lines.len(), 3);
    }

    // ── diff tree resilience tests ──────────────────────────────────

    #[test]
    fn diff_tree_utf8_filename() {
        // Non-ASCII filename that would panic with byte slicing
        let files = vec![DiffFile {
            path: "src/café.rs".into(),
            insertions: 5,
            deletions: 2,
            untracked: false,
        }];
        // Width narrow enough to force truncation
        let lines = super::build_diff_tree_lines(&files, 10);
        assert!(
            !lines.is_empty(),
            "should produce lines for UTF-8 filenames"
        );
    }

    #[test]
    fn diff_tree_width_zero() {
        let files = vec![DiffFile {
            path: "src/app.rs".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // width=0 should not panic
        let lines = super::build_diff_tree_lines(&files, 0);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_width_one() {
        let files = vec![DiffFile {
            path: "src/app.rs".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // width=1 should not panic
        let lines = super::build_diff_tree_lines(&files, 1);
        assert!(!lines.is_empty());
    }

    #[test]
    fn diff_tree_trailing_slash_path() {
        let files = vec![DiffFile {
            path: "src/".into(),
            insertions: 1,
            deletions: 0,
            untracked: false,
        }];
        // Trailing slash produces empty basename — should be skipped
        let lines = super::build_diff_tree_lines(&files, 40);
        assert!(lines.is_empty(), "trailing-slash path should be skipped");
    }
}
