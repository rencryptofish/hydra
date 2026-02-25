use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::app::UiApp;
use crate::session::AgentType;

pub(crate) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

pub fn draw_agent_select(frame: &mut Frame, app: &UiApp) {
    let agents = AgentType::all();
    let height = agents.len() as u16 + 2;
    let area = centered_rect(30, height, frame.area());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let marker = if i == app.agent_selection {
                ">> "
            } else {
                "   "
            };
            let label = format!("{marker}{agent}");
            let style = if i == app.agent_selection {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(label, style)))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Select Agent ")
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(list, area);
}

pub fn draw_confirm_delete(frame: &mut Frame, app: &UiApp) {
    let area = centered_rect(40, 5, frame.area());
    frame.render_widget(Clear, area);

    let name = app
        .confirm_delete_target_name()
        .or_else(|| {
            app.snapshot
                .sessions
                .get(app.selected)
                .map(|s| s.name.as_str())
        })
        .unwrap_or("?");

    let text = format!(" Kill session '{name}'? (y/n)");
    let confirm = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Delete ")
            .border_style(Style::default().fg(Color::Red)),
    );
    frame.render_widget(confirm, area);
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    #[test]
    fn centered_rect_normal() {
        let area = Rect::new(0, 0, 80, 24);
        let result = super::centered_rect(40, 10, area);
        assert_eq!(result.width, 40);
        assert_eq!(result.height, 10);
        assert_eq!(result.x, 20); // (80 - 40) / 2
        assert_eq!(result.y, 7); // (24 - 10) / 2
    }

    #[test]
    fn centered_rect_larger_than_area() {
        let area = Rect::new(0, 0, 20, 10);
        let result = super::centered_rect(40, 20, area);
        // Width and height clamped to area
        assert_eq!(result.width, 20);
        assert_eq!(result.height, 10);
    }

    #[test]
    fn centered_rect_with_offset() {
        let area = Rect::new(10, 5, 60, 20);
        let result = super::centered_rect(20, 10, area);
        assert_eq!(result.x, 30); // 10 + (60-20)/2
        assert_eq!(result.y, 10); // 5 + (20-10)/2
    }

    #[test]
    fn centered_rect_zero_size_area() {
        let area = Rect::new(0, 0, 0, 0);
        let result = super::centered_rect(40, 10, area);
        assert_eq!(result.width, 0);
        assert_eq!(result.height, 0);
    }

    #[test]
    fn inset_rect_basic() {
        let area = Rect::new(10, 20, 100, 50);
        let inset = super::super::inset_rect(area, 5);
        assert_eq!(inset.x, 15);
        assert_eq!(inset.y, 25);
        assert_eq!(inset.width, 90);
        assert_eq!(inset.height, 40);
    }

    #[test]
    fn inset_rect_zero_margin() {
        let area = Rect::new(0, 0, 80, 24);
        let inset = super::super::inset_rect(area, 0);
        assert_eq!(inset, area);
    }

    #[test]
    fn inset_rect_margin_exceeds_size() {
        let area = Rect::new(0, 0, 10, 8);
        let inset = super::super::inset_rect(area, 20);
        // saturating_sub means width and height become 0
        assert_eq!(inset.width, 0);
        assert_eq!(inset.height, 0);
        assert_eq!(inset.x, 20); // saturating_add
        assert_eq!(inset.y, 20);
    }
}
