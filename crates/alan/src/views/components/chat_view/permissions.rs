//! Tool-permission prompt band shown while an `authorize` call waits.
use crate::core::permissions::{Answer, PermissionRequest};
use crate::root::AlanAction;
use crate::views::theme;
use alan_tui::ActionStatus;
use alan_tui::Component;
use alan_tui::RenderContext;
use alan_tui::context::Context;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Padding;
use ratatui::widgets::Paragraph;

/// Height of the permission band (blank, question, command, keys).
pub const PERMISSION_HEIGHT: u16 = 4;

/// Focused while a tool-authorization request is pending, so `1`/`0` answer
/// the request instead of typing into the prompt editor.
pub struct PermissionPrompt {
    request: Option<PermissionRequest>,
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self { request: None }
    }

    /// Show a pending request; the band renders and takes focus.
    pub fn show(&mut self, request: PermissionRequest) {
        self.request = Some(request);
    }
}

impl Component<AlanAction> for PermissionPrompt {
    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        let Some(_) = self.request else {
            return ActionStatus::Continue;
        };

        let AlanAction::Raw(Event::Key(key)) = action else {
            return ActionStatus::Handled;
        };

        if key.kind != KeyEventKind::Press {
            return ActionStatus::Handled;
        }

        let decision = match key.code {
            KeyCode::Char('1') => Answer::Allowed,
            KeyCode::Char('2') => Answer::AllowedSession,
            KeyCode::Char('3') => Answer::AllowedAlways,
            KeyCode::Char('8') => Answer::Denied,
            KeyCode::Char('9') => Answer::DeniedSession,
            KeyCode::Char('0') => Answer::Stop,
            _ => return ActionStatus::Handled,
        };

        self.request = None;
        cx.dispatch_parent(&AlanAction::PermissionAnswered(decision));
        cx.drop_focus();

        ActionStatus::Handled
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, '_, AlanAction>) {
        let Some(request) = &self.request else {
            return;
        };

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Permission required:",
                Style::default().fg(theme::PROMPT_FG).bold(),
            )),
            Line::from(Span::styled(
                format!("command: {}", request.name),
                Style::default().fg(theme::EDITOR_FG),
            )),
            Line::from(Span::styled(
                "Allow[1]  AllowSession[2]  AllowAlways[3]  Deny[8]  DenySession[9]  Stop[0]",
                Style::default().fg(theme::PROMPT_FG),
            )),
        ];

        let prompt = Paragraph::new(lines)
            .style(Style::default().bg(theme::EDITOR_BG))
            .block(Block::default().padding(Padding::horizontal(4)));

        frame.render_widget(prompt, area);
    }
}
