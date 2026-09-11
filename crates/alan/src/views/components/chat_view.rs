//! The chat surface as a single `tui` component.
//!
//! Owns the vertical stack of the chat: the transcript ([`ChatHistory`]),
//! the pending attachment list, the status line, and the prompt editor. A
//! single owner is what lets the attachments sit *above* the status line —
//! the root could not interleave them because it held only the editor while
//! `ChatHistory` owned the status.

use std::sync::Arc;

use crate::root::AlanAction;
use crate::views::components::{ModelPick, ModelsPicker};
use crate::views::theme;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use tui::component::{ActionStatus, Component, RenderContext};
use tui::context::Context;
use tui::entity::Entity;

use super::chat_history::ChatHistory;
use super::editor::PromptEditor;
use super::status::{STATUS_HEIGHT, StatusSnapshot, render_status};

/// Blank rows between the status line and the prompt cursor.
const STATUS_EDITOR_GAP: u16 = 1;

/// Blank rows below the prompt editor.
const EDITOR_BOTTOM_PAD: u16 = 1;

/// Emitted when a submission needs the login overlay opened. The root owns the
/// providers and credentials, so it subscribes and opens the overlay itself.
#[derive(Debug, Clone, Copy)]
pub struct LoginRequested;

/// The chat column: transcript, attachments, status, and prompt editor.
pub struct ChatView {
    /// The transcript component to install on `init`; taken when inserted.
    chat_source: Option<ChatHistory>,
    chat: Option<Entity<ChatHistory>>,
    editor: Option<Entity<PromptEditor>>,
    provider: Arc<dyn providers::Provider>,
    model_subscription: Option<tui::Subscription>,
    model_picker: Option<Entity<ModelsPicker>>,
}

impl ChatView {
    pub fn new(chat: ChatHistory, provider: Arc<dyn providers::Provider>) -> Self {
        Self {
            chat_source: Some(chat),
            chat: None,
            editor: None,
            provider,
            model_subscription: None,
            model_picker: None,
        }
    }

    /// Dispatch `action` to the transcript component, if installed.
    fn dispatch_chat(
        &self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        let Some(chat) = self.chat else {
            return ActionStatus::Continue;
        };
        cx.dispatch(chat, action)
    }
}

impl Component<AlanAction> for ChatView {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.chat = Some(
            cx.insert(
                self.chat_source
                    .take()
                    .expect("chat component installed once"),
            ),
        );
        self.editor = Some(cx.insert(PromptEditor::new()));
        cx.focus_entity(self.editor.expect("editor entity"));
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus
    where
        Self: Sized,
    {
        // While a blocking operation (e.g. `/summarize-new`) is in flight, all
        // input is swallowed so it cannot race the in-flight task.
        if let Some(chat) = self.chat
            && cx.read(chat, |c| c.is_loading()).unwrap_or(false)
        {
            return ActionStatus::Handled;
        }
        match action {
            // The editor bubbles submissions up to here. Run them on the
            // transcript, then forward a `/login` request to the root as a
            // typed event — the chat cannot open the overlay itself.
            AlanAction::Submit(_) => {
                let Some(chat) = self.chat else {
                    return ActionStatus::Continue;
                };
                let status = cx.dispatch(chat, action);
                if cx.update(chat, |c| c.take_login_request()).unwrap_or(false) {
                    cx.emit(LoginRequested);
                }
                if cx
                    .update(chat, |c| c.take_models_request())
                    .unwrap_or(false)
                {
                    let provider = Arc::clone(&self.provider);
                    let picker = cx.open_overlay(ModelsPicker::new("Models", Vec::new()));
                    let initial_items = provider
                        .models()
                        .iter()
                        .map(|m| format!("{} — {}", m.id, m.name))
                        .collect();
                    let _ = cx.update(picker, |p| p.set_items(initial_items));
                    let fetch_provider = Arc::clone(&provider);
                    let _ = cx.spawn(
                        async move {
                            fetch_provider
                                .fetch_models()
                                .await
                                .map_err(|e| tui::TaskError(e.into()))?;
                            Ok(())
                        },
                        move |result, _view, cx| {
                            if result.is_ok() {
                                let items = provider
                                    .models()
                                    .iter()
                                    .map(|m| format!("{}", m.name))
                                    .collect();
                                let _ = cx.update(picker, |p| p.set_items(items));
                            }
                        },
                    );
                    let provider = Arc::clone(&self.provider);
                    self.model_subscription = Some(cx.subscribe::<ModelPick, ModelsPicker, _>(
                        picker,
                        move |event, _view, _picker, cx| {
                            if let ModelPick::Chosen(index) = event {
                                let Some(model_id) =
                                    provider.models().get(*index).map(|m| m.id.clone())
                                else {
                                    return;
                                };
                                let Some(agent) = cx.read(chat, |c| c.agent()).flatten() else {
                                    return;
                                };
                                let provider = Arc::clone(&provider);
                                let _ = cx.spawn(
                                    async move {
                                        let model = provider
                                            .bind(&model_id)
                                            .map_err(|e| tui::TaskError(e.into()))?;
                                        let name = model.info().name.clone();
                                        agent
                                            .set_model(model)
                                            .await
                                            .map_err(|e| tui::TaskError(e.into()))?;
                                        Ok(name)
                                    },
                                    move |result, _view, cx| {
                                        let _ = cx.update(chat, |c| match result {
                                            Ok(name) => c.apply_model_switch(name),
                                            Err(e) => c.apply_model_switch_failed(e.to_string()),
                                        });
                                    },
                                );
                            }
                        },
                    ));
                }
                status
            }
            // Mode toggle, quit (cancel-or-exit), and wheel scrolling are all
            // owned by the transcript component.
            AlanAction::ToggleMode
            | AlanAction::Quit
            | AlanAction::MouseScrollUp
            | AlanAction::MouseScrollDown => self.dispatch_chat(action, cx),
            AlanAction::Raw(event) => match event {
                // Mouse traffic is owned by the transcript; it hit-tests its
                // own rect and ignores misses.
                Event::Mouse(_) => self.dispatch_chat(action, cx),
                // PageUp/PageDown scroll the transcript, not the editor.
                Event::Key(key)
                    if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
                        && key.kind == KeyEventKind::Press =>
                {
                    self.dispatch_chat(action, cx)
                }
                // Esc clears an active transcript selection (the editor gets
                // first refusal and pops attachments).
                Event::Key(key)
                    if key.code == KeyCode::Esc
                        && key.kind == KeyEventKind::Press
                        && self
                            .chat
                            .and_then(|chat| cx.read(chat, |c| c.has_active_selection()))
                            .unwrap_or(false) =>
                {
                    self.dispatch_chat(action, cx)
                }
                // Everything else is editor input, already offered to the
                // focused editor before it reached this component.
                _ => ActionStatus::Continue,
            },
            _ => ActionStatus::Continue,
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        let Some(chat) = self.chat else {
            return;
        };
        let Some(editor) = self.editor else {
            return;
        };

        // Size the editor and attachment bands from the editor's own state so
        // they always reflect the current buffer and attached images.
        let editor_width = area.width.saturating_sub(theme::PROMPT_GUTTER);
        let editor_rows = cx.read(editor, |e| e.rows(editor_width)).unwrap_or(1);
        let attachment_height = cx.read(editor, |e| e.attachment_height()).unwrap_or(0);

        let [
            chat_area,
            attachment_area,
            status_area,
            _gap,
            editor_area,
            _bottom_pad,
        ] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(attachment_height),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Length(STATUS_EDITOR_GAP),
            Constraint::Length(editor_rows),
            Constraint::Length(EDITOR_BOTTOM_PAD),
        ])
        .areas(area);

        cx.render_entity(chat, frame, chat_area);
        if let Some(picker) = self.model_picker {
            let picker_height = 7.min(chat_area.height);
            let picker_area = Rect {
                x: chat_area.x,
                y: status_area.y.saturating_sub(picker_height),
                width: chat_area.width,
                height: picker_height,
            };
            cx.render_entity(picker, frame, picker_area);
        }
        render_attachments(frame, attachment_area, editor, cx);
        // Paint the whole footer (status through bottom pad) with the editor
        // background, so the gap and bottom padding don't fall back to the
        // terminal default.
        let footer = Rect {
            x: area.x,
            y: status_area.y,
            width: area.width,
            height: area.bottom().saturating_sub(status_area.y),
        };
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG)),
            footer,
        );
        if let Some(snap) = cx.read(chat, |c| c.snapshot()).flatten() {
            render_status(frame, status_area, &StatusSnapshot::from_snapshot(&snap));
        }
        cx.render_entity(editor, frame, editor_area);
    }
}

/// Paint the pending attachment list into `area`.
fn render_attachments(
    frame: &mut Frame,
    area: Rect,
    editor: Entity<PromptEditor>,
    cx: &RenderContext<'_, AlanAction>,
) {
    if area.height == 0 {
        return;
    }
    let Some(names) = cx.read(editor, |e| {
        e.attachments()
            .iter()
            .map(|attachment| attachment.name.clone())
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    if names.is_empty() {
        return;
    }

    let mut lines: Vec<Line<'static>> = vec![
        Line::from("\n"),
        Line::from(Span::styled(
            "  Attachments  (esc removes last)",
            Style::default().fg(theme::ATTACHMENT_FG).bold(),
        )),
    ];
    for name in names {
        lines.push(Line::from(Span::styled(
            format!("   - {name}"),
            Style::default().fg(theme::ATTACHMENT_FG),
        )));
    }
    let attachments =
        Paragraph::new(Text::from(lines)).style(Style::default().bg(theme::ATTACHMENT_BG));
    frame.render_widget(attachments, area);
}
