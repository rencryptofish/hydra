use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::{Mode, UiApp};

pub fn draw_help_bar(frame: &mut Frame, app: &UiApp, area: Rect) {
    let help_text = match app.mode {
        Mode::Browse if !app.mouse_captured => "SELECT TEXT TO COPY  |  c: exit copy mode",
        Mode::Browse => {
            "j/k: nav  PgUp/Dn: scroll  Enter: compose  n: new  d: del  c: copy  q: quit"
        }
        Mode::Compose => {
            "Enter: send  Shift+Enter: newline  Up/Dn: history  Esc: cancel (draft kept)"
        }
        Mode::NewSessionAgent => "j/k: select agent  Enter: confirm  Esc: cancel",
        Mode::ConfirmDelete => "y: confirm delete  Esc: cancel",
    };

    let status = if let Some(msg) = &app.status_message {
        format!(" {msg} | {help_text}")
    } else {
        format!(" {help_text}")
    };

    let bar = Paragraph::new(Line::from(Span::styled(
        status,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    frame.render_widget(bar, area);
}
