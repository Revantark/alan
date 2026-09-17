//! The prompt editor as a `tui` component.
//!
//! Owns the [`TextArea`], the attached images, and the prompt gutter. It is a
//! real entity: it can be focused and receives keys directly. Rendering and
//! measurement come from its own state, so a parent can size the footer from
//! it without holding a separate copy of the prompt.

use crate::core::completion::token;
use crate::core::{Completer, CompletionRequest, ImageAttachment, PathsContext, SlashCommand};
use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Paragraph, Widget};
use std::collections::VecDeque;
use strum::IntoEnumIterator;
use tui::context::Context;
use tui::entity::Entity;
use tui::subscription::Subscription;
use tui::{ActionStatus, Component, RenderContext};
use tui_textarea::{CursorMove, CursorRenderMode, TextArea, WrapMode};

use crate::root::{AlanAction, PromptSubmission};
use crate::views::components::popup_v2::{PopupListv2, PopupSelected};
use crate::views::theme;

/// The prompt input area. Owns its text buffer and attachments, and renders
/// itself. It is a `tui` component so it can hold focus and receive keys.
pub struct PromptEditor {
    editor: TextArea<'static>,
    attachments: Vec<ImageAttachment>,
    /// The completion popup entity, created in `init`.
    popup: Option<Entity<PopupListv2>>,
    /// Subscription for popup accept/dismiss events.
    popup_subscription: Option<Subscription>,
    /// The last trigger character that started a completion scan.
    last_trigger: Option<char>,
    /// The request the user dismissed with Esc. Suppressed until the editor
    /// state changes, so the tick cannot reopen a popup the user closed.
    dismissed: Option<CompletionRequest>,
    /// The completer with its backends.
    completer: Completer,
    /// User-typed prompts, oldest first, for Up/Down recall. Seeded from the
    /// restored session in `ChatView::init`, appended on every submit.
    history: VecDeque<String>,
    /// Index into `history` of the entry currently shown in the editor.
    /// `None` means the live draft (what the user is typing) is shown.
    history_index: Option<usize>,
}

impl PromptEditor {
    pub fn new() -> Self {
        Self {
            editor: Self::new_editor(),
            attachments: Vec::new(),
            popup: None,
            popup_subscription: None,
            last_trigger: None,
            dismissed: None,
            completer: Completer::new()
                .with_backend(
                    Box::new(crate::core::CommandCompleterBackend),
                    Box::new(crate::core::CommandsContext {
                        commands: crate::core::SlashCommand::iter().collect(),
                    }),
                )
                .with_backend(
                    Box::new(crate::core::PathCompleterBackend),
                    Box::new(crate::core::PathsContext {
                        paths: Vec::new(),
                        status: crate::core::CompletionStatus::Loading,
                    }),
                ),
            history: VecDeque::new(),
            history_index: None,
        }
    }

    pub fn seed_history(&mut self, prompts: Vec<String>) {
        self.history = prompts.into();
        self.history_index = None;
    }

    /// Rows the prompt needs at `width`, accounting for soft wrapping. Mutable
    /// because `TextArea::measure` caches the result.
    pub fn rows(&self, width: u16) -> u16 {
        self.editor.clone().measure(width.max(1)).preferred_rows
    }

    pub fn attachments(&self) -> &[ImageAttachment] {
        &self.attachments
    }

    /// Rows the attachment section needs, accounting for its header.
    pub fn attachment_height(&self) -> u16 {
        if self.attachments.is_empty() {
            0
        } else {
            3 + self.attachments.len() as u16
        }
    }

    fn new_editor() -> TextArea<'static> {
        let mut editor = TextArea::default();
        editor.set_style(Style::default().fg(theme::EDITOR_FG).bg(theme::EDITOR_BG));
        editor.set_cursor_line_style(Style::default());
        editor.set_wrap_mode(WrapMode::WordOrGlyph);
        editor.set_cursor_render_mode(CursorRenderMode::Hidden);
        editor.set_min_rows(1);
        editor.set_max_rows(theme::EDITOR_VISIBLE_LINES);
        editor.set_undo_coalescing(true);
        editor
    }

    /// Submit the current editor text as a prompt, recording it in
    /// history if it is a plain prompt and not a duplicate, then
    /// dispatching [`AlanAction::Submit`] to the parent.
    fn submit_text(&mut self, text: String, cx: &mut Context<'_, Self, AlanAction>) {
        let trimmed = text.trim().to_owned();
        // Slash commands are actions, not prompts: they are not
        // recorded for Up/Down recall.
        if is_plain_prompt(&trimmed) && self.history.back() != Some(&trimmed) {
            self.history.push_back(trimmed);
            if self.history.len() > 200 {
                self.history.pop_front();
            }
        }
        // Submitting exits the recall cycle back to the live draft.
        self.history_index = None;
        cx.dispatch_parent(&AlanAction::Submit(PromptSubmission {
            images: std::mem::take(&mut self.attachments),
            text,
        }));
        self.editor.clear();
    }

    fn handle_event(
        &mut self,
        event: Event,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        let status = match event {
            Event::Key(key) if key.code == KeyCode::Esc && !self.attachments.is_empty() => {
                self.attachments.pop();
                ActionStatus::Handled
            }
            Event::Key(key) if is_multiline_enter(key) => {
                self.editor.insert_newline();
                ActionStatus::Handled
            }
            Event::Key(key) if key.code == KeyCode::Enter => {
                let text = self.editor.lines().join("\n");
                self.submit_text(text, cx);
                ActionStatus::Handled
            }
            Event::Key(key)
                if key.code == KeyCode::Char('v')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if !self.try_clipboard_image() {
                    let text = arboard::Clipboard::new().and_then(|mut c| c.get_text());
                    if let Ok(text) = text
                        && !text.is_empty()
                    {
                        self.editor.insert_str(text);
                    }
                }
                ActionStatus::Handled
            }
            Event::Key(key)
                if key.code == KeyCode::Char('u')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.delete_line_by_head();
                ActionStatus::Handled
            }
            Event::Key(key)
                if key.code == KeyCode::Char('z')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.undo();
                ActionStatus::Handled
            }
            Event::Key(key)
                if key.code == KeyCode::Char('r')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.editor.redo();
                ActionStatus::Handled
            }
            Event::Paste(text) => {
                self.editor.insert_str(text);
                ActionStatus::Handled
            }
            Event::Key(key) if key.code == KeyCode::Up && key.kind == KeyEventKind::Press => {
                let buffer = self.editor.lines().join("\n");
                let cursor_row = self.editor.cursor().0;
                if let Some(recall) =
                    recall_up(&self.history, self.history_index, &buffer, cursor_row)
                {
                    self.history_index = recall.index;
                    self.editor.clear();
                    self.editor.move_cursor(CursorMove::Jump(0, 0));
                    self.editor.insert_str(&recall.text);
                    self.editor.move_cursor(CursorMove::End);
                    refresh_completion(
                        &mut self.completer,
                        self.popup,
                        cx,
                        &mut self.dismissed,
                        &mut self.last_trigger,
                        &mut self.editor,
                    );
                    ActionStatus::Handled
                } else {
                    self.handle_input_and_refresh(event, cx)
                }
            }
            // Down-arrow recall: cycle newer, restoring the live draft when
            // walking past the newest entry. Only fires when the cursor is
            // at the bottom of the buffer (last line, last column).
            Event::Key(key) if key.code == KeyCode::Down && key.kind == KeyEventKind::Press => {
                let (cursor_row, _) = self.editor.cursor();
                let lines = self.editor.lines();
                let at_bottom = cursor_row == lines.len().saturating_sub(1);
                let maybe_recall = if at_bottom {
                    recall_down(&self.history, self.history_index)
                } else {
                    None
                };
                if let Some(recall) = maybe_recall {
                    self.history_index = recall.index;
                    self.editor.clear();
                    self.editor.move_cursor(CursorMove::Jump(0, 0));
                    self.editor.insert_str(&recall.text);
                    self.editor.move_cursor(CursorMove::End);
                    refresh_completion(
                        &mut self.completer,
                        self.popup,
                        cx,
                        &mut self.dismissed,
                        &mut self.last_trigger,
                        &mut self.editor,
                    );
                    ActionStatus::Handled
                } else {
                    self.handle_input_and_refresh(event, cx)
                }
            }
            event => self.handle_input_and_refresh(event, cx),
        };
        self.sync_command_highlight();
        status
    }

    /// Apply the event to the editor. If nothing actually changed, let the
    /// event propagate (`Continue`); otherwise refresh the completion popup.
    fn handle_input_and_refresh(
        &mut self,
        event: Event,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        let o = self.editor.cursor();
        let modified = self.editor.input(event);
        let t = self.editor.cursor();
        if !modified && o == t {
            ActionStatus::Continue
        } else {
            refresh_completion(
                &mut self.completer,
                self.popup,
                cx,
                &mut self.dismissed,
                &mut self.last_trigger,
                &mut self.editor,
            );
            ActionStatus::Handled
        }
    }

    fn sync_command_highlight(&mut self) {
        self.editor.clear_custom_highlight();
        let [line] = self.editor.lines() else {
            return;
        };
        if SlashCommand::parse(line).is_none() {
            return;
        }
        let end = line.find(char::is_whitespace).unwrap_or(line.len());
        self.editor.custom_highlight(
            ((0, 0), (0, end)),
            Style::default().fg(theme::COMMAND_FG),
            1,
        );
    }

    /// Check the system clipboard for an image and add it as an attachment.
    /// Returns true when an image was attached.
    fn try_clipboard_image(&mut self) -> bool {
        let img = match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_image()) {
            Ok(img) => img,
            Err(error) => {
                tracing::debug!(%error, "clipboard: no readable image");
                return false;
            }
        };
        if img.width == 0 || img.height == 0 {
            tracing::debug!("clipboard: ignoring zero-size image");
            return false;
        }
        let (w, h) = (img.width, img.height);
        let raw = img.into_owned_bytes();

        let Some(rgba) = image::RgbaImage::from_raw(w as u32, h as u32, raw.into_owned()) else {
            tracing::debug!(width = w, height = h, "clipboard: invalid image data");
            return false;
        };
        let mut png_buf = std::io::Cursor::new(Vec::new());
        if let Err(error) =
            image::DynamicImage::ImageRgba8(rgba).write_to(&mut png_buf, image::ImageFormat::Png)
        {
            tracing::debug!(%error, "clipboard: PNG encoding failed");
            return false;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(png_buf.get_ref());

        self.attachments.push(ImageAttachment {
            name: format!("image-{}", self.attachments.len() + 1),
            mime_type: "image/png".into(),
            base64_data: data,
        });
        tracing::debug!(
            name = self.attachments.last().unwrap().name,
            "clipboard: image attached"
        );
        true
    }

    pub fn completion_request(&self) -> Option<CompletionRequest> {
        let (row, col) = self.editor.cursor();
        let line = self.editor.lines().get(row)?;
        let cursor = char_offset(line, col.min(line.chars().count()));
        let token = token::at(line, cursor)?;
        Some(CompletionRequest {
            trigger: token.trigger,
            pattern: line[token.range.clone()].to_owned(),
            range: token.range,
            row,
        })
    }

    pub fn insert_completion(&mut self, replacement: &str, range: std::ops::Range<usize>) {
        let separate = self.needs_separator_after(range.end);
        self.replace_range(range, replacement);
        if separate {
            self.editor.insert_str(" ");
        }
    }

    fn needs_separator_after(&self, at: usize) -> bool {
        let (row, _) = self.editor.cursor();
        self.editor
            .lines()
            .get(row)
            .and_then(|line| line.get(at..))
            .is_none_or(|rest| !rest.starts_with(char::is_whitespace))
    }

    fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        let (row, _) = self.editor.cursor();
        let Some(line) = self.editor.lines().get(row) else {
            return;
        };
        let Some(before) = line.get(..range.start) else {
            return;
        };
        let Some(replaced) = line.get(range) else {
            return;
        };
        let start_col = before.chars().count();
        let chars = replaced.chars().count();
        self.editor
            .move_cursor(CursorMove::Jump(row as u16, start_col as u16));
        self.editor.delete_str(chars);
        self.editor.insert_str(text);
    }
}

/// Result of a successful Up/Down recall step: the new cycle index and the
/// text to load into the editor.
struct Recall {
    index: Option<usize>,
    text: String,
}

/// Decide whether Up should trigger recall and, if so, what to load.
///
/// `history` is oldest→newest, `index` is the entry currently shown (`None`
/// means the live draft is shown), `buffer` is the current editor contents,
/// and `cursor_row` is the cursor row. Returns `None` to fall through to
/// normal cursor movement.
fn recall_up(
    history: &VecDeque<String>,
    index: Option<usize>,
    buffer: &str,
    cursor_row: usize,
) -> Option<Recall> {
    let matches_current = index
        .and_then(|i| history.get(i))
        .is_some_and(|entry| entry == buffer);
    let fire = match index {
        None => buffer.trim().is_empty() && cursor_row == 0 && !history.is_empty(),
        Some(i) => i > 0 && matches_current && cursor_row == 0,
    };
    if !fire {
        return None;
    }
    let next = match index {
        None => history.len() - 1, // first Up: load the newest entry
        Some(i) => i - 1,
    };
    Some(Recall {
        index: Some(next),
        text: history[next].clone(),
    })
}

/// Decide whether Down should trigger recall and, if so, what to load.
///
/// Walking past the newest entry exits the cycle (`index = None`) and loads
/// an empty buffer. Returns `None` to fall through to normal cursor movement.
fn recall_down(history: &VecDeque<String>, index: Option<usize>) -> Option<Recall> {
    match index {
        Some(i) if i + 1 < history.len() => Some(Recall {
            index: Some(i + 1),
            text: history[i + 1].clone(),
        }),
        Some(_) => Some(Recall {
            index: None,
            text: String::new(),
        }),
        None => None,
    }
}

/// Whether `text` is a plain prompt worth recording for Up/Down recall.
///
/// Slash commands are actions, not prompts, so they are excluded. This is
/// the same predicate used at ingestion time (the editor's Enter branch and
/// the seed from the restored session), so the recall deque stays clean and
/// the cycle index arithmetic never has to skip entries.
pub(crate) fn is_plain_prompt(text: &str) -> bool {
    !text.is_empty() && SlashCommand::parse(text).is_none()
}

impl Default for PromptEditor {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a character-column index (as reported by `TextArea::cursor`)
/// into a byte offset within `line`, clamped to the line length.
fn char_offset(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

fn refresh_completion(
    completer: &mut Completer,
    popup: Option<Entity<PopupListv2>>,
    cx: &mut Context<'_, PromptEditor, AlanAction>,
    dismissed: &mut Option<CompletionRequest>,
    last_trigger: &mut Option<char>,
    editor: &mut TextArea<'static>,
) {
    let Some(popup) = popup else {
        return;
    };

    let request = match popup_completion_request(editor) {
        // No token under the cursor: close the popup and forget any
        // dismissal, so re-typing the trigger later can reopen it.
        None => {
            *dismissed = None;
            close_popup(cx, popup);
            return;
        }
        Some(req) => req,
    };

    // The user dismissed this exact request with Esc; keep the popup closed
    // until the editor state changes, so the 16ms tick cannot reopen it.
    if dismissed.as_ref() == Some(&request) {
        close_popup(cx, popup);
        return;
    }
    *dismissed = None;

    // A trigger change means a backend switched active. The path backend's data
    // is a filesystem scan, so it gets a loading context and a spawned task.
    // The command backend's data is immutable config — nothing to refresh.
    if *last_trigger != Some(request.trigger) {
        *last_trigger = Some(request.trigger);
        if request.trigger == '@' {
            spawn_path_scan(cx, completer, popup, request);
            return;
        }
    }

    let Some(result) = completer.complete(request) else {
        close_popup(cx, popup);
        return;
    };
    apply_completion_result(cx, popup, result);
}

/// Close the popup and reset its items.
fn close_popup(cx: &mut Context<'_, PromptEditor, AlanAction>, popup: Entity<PopupListv2>) {
    cx.update(popup, |popup| popup.set(false, None, Vec::new()));
}

/// Update the popup from a `CompletionResult`, skipping the update when the
/// items already match (avoids redundant renders).
fn apply_completion_result(
    cx: &mut Context<'_, PromptEditor, AlanAction>,
    popup: Entity<PopupListv2>,
    result: crate::core::completion::CompletionResult,
) {
    let message = match &result.status {
        crate::core::CompletionStatus::Loading => Some("Loading…".to_owned()),
        crate::core::CompletionStatus::Ready if result.items.is_empty() => {
            Some("No matches".to_owned())
        }
        crate::core::CompletionStatus::Ready => None,
    };
    let items: Vec<String> = result
        .items
        .iter()
        .map(|item| item.display.clone())
        .collect();
    let unchanged = cx
        .read(popup, |popup| {
            popup.matches(true, message.as_deref(), &items)
        })
        .unwrap_or(false);
    if unchanged {
        return;
    }
    cx.update(popup, |popup| popup.set(true, message, items));
    cx.focus_entity(popup);
    cx.notify();
}

/// Spawn a filesystem scan for the `@` (path) backend. When the scan completes,
/// refreshes the context and re-runs completion, updating the popup.
fn spawn_path_scan(
    cx: &mut Context<'_, PromptEditor, AlanAction>,
    completer: &mut Completer,
    popup: Entity<PopupListv2>,
    request: CompletionRequest,
) {
    let ctx = PathsContext {
        paths: Vec::new(),
        status: crate::core::CompletionStatus::Loading,
    };
    completer.set_context('@', Box::new(ctx));
    let root_path = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let trigger = request.trigger;
    let request_clone = request.clone();
    cx.spawn(scan_task(root_path), move |result, root, cx| {
        let paths: Vec<String> = result.unwrap_or_default();
        let ctx = PathsContext {
            paths,
            status: crate::core::CompletionStatus::Ready,
        };
        root.completer.set_context(trigger, Box::new(ctx));

        let Some(result) = root.completer.complete(request_clone) else {
            close_popup(cx, popup);
            return;
        };
        apply_completion_result(cx, popup, result);
    });
}

fn popup_completion_request(editor: &TextArea<'static>) -> Option<CompletionRequest> {
    let (row, col) = editor.cursor();
    let line = editor.lines().get(row)?;
    let cursor = char_offset(line, col.min(line.chars().count()));
    let token = token::at(line, cursor)?;
    Some(CompletionRequest {
        trigger: token.trigger,
        pattern: line[token.range.clone()].to_owned(),
        range: token.range,
        row,
    })
}

/// A spawned scan: walks the workspace and returns its relative paths.
/// Runs on the blocking thread pool, so it never blocks the UI loop.
async fn scan_task(root: std::path::PathBuf) -> Result<Vec<String>, tui::TaskError> {
    let result =
        tokio::task::spawn_blocking(move || crate::core::completion::scan::scan_dir(&root))
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("scan panicked")));
    result.map_err(|error| tui::TaskError(Box::new(error)))
}

impl Component<AlanAction> for PromptEditor {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.popup = Some(cx.insert(PopupListv2::default()));
        cx.focus_entity(self.popup.expect("popup entity"));
        // The v2 popup owns its own selection and emits on accept/dismiss.
        // Root subscribes (persistent, re-arming) and applies the accepted
        // item by re-deriving the request from the editor — the popup only
        // carries display strings, never a replacement or byte range.
        if let Some(popup) = self.popup {
            self.popup_subscription = Some(cx.subscribe::<PopupSelected, PopupListv2, _>(
                popup,
                |event, editor, popup, cx| {
                    match event {
                        PopupSelected::Dismiss => {
                            editor.dismissed = editor.completion_request();
                        }
                        PopupSelected::Accept { index } => {
                            let Some(request) = editor.completion_request() else {
                                return;
                            };
                            let Some(result) = editor.completer.complete(request) else {
                                return;
                            };
                            let Some(item) = result.items.get(*index) else {
                                return;
                            };
                            let command_text = format!("/{}", item.replacement);
                            if let Some((command, _args)) =
                                SlashCommand::parse_with_args(&command_text)
                                && !command.takes_args()
                            {
                                editor.dismissed = None;
                                cx.update(popup, |popup| popup.set(false, None, Vec::new()));
                                cx.focus_entity(cx.entity());
                                editor.submit_text(command_text, cx);
                                cx.notify();
                                return;
                            }
                            editor.insert_completion(&item.replacement, result.range);
                            editor.dismissed = None;
                        }
                    }
                    cx.update(popup, |popup| popup.set(false, None, Vec::new()));
                    cx.focus_entity(cx.entity());
                    cx.notify();
                },
            ));
        }
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus {
        let status = match action {
            AlanAction::Raw(event) => self.handle_event(event.clone(), cx),
            AlanAction::Paste(text) => self.handle_event(Event::Paste(text.clone()), cx),
            _ => ActionStatus::Continue,
        };
        if status == ActionStatus::Handled {
            cx.notify();
        }
        status
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        let background = Paragraph::new("").style(Style::default().bg(theme::EDITOR_BG));
        frame.render_widget(background, area);

        let [prompt_area, input_area] =
            Layout::horizontal([Constraint::Length(theme::PROMPT_GUTTER), Constraint::Min(1)])
                .areas(area);
        frame.render_widget(
            Paragraph::new("  › ")
                .style(Style::default().fg(theme::PROMPT_FG).bg(theme::EDITOR_BG)),
            prompt_area,
        );
        self.editor.render(input_area, frame.buffer_mut());
        // The widget paints no cursor of its own (`CursorRenderMode::Hidden`),
        // so the terminal owns it. Place it where the buffer thinks it is.
        if let Some(position) = self.editor.rendered_cursor_position() {
            frame.set_cursor_position(position);
        }

        if let Some(area) = PopupListv2::area_above(area, frame.area(), 5)
            && let Some(popup) = self.popup
        {
            cx.render_entity(popup, frame, area);
        }
    }
}

fn is_multiline_enter(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\n' | '\r'))
        || (matches!(key.code, KeyCode::Char('j' | 'm'))
            && key.modifiers.contains(KeyModifiers::CONTROL))
        || (key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL)
                || key.modifiers.contains(KeyModifiers::ALT)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_commands_are_excluded_from_history() {
        assert!(!is_plain_prompt("/help"));
        assert!(!is_plain_prompt("/models"));
        assert!(!is_plain_prompt("/effort high"));
        assert!(!is_plain_prompt("/summarize-new"));
        // `SlashCommand::parse` does not trim, so leading whitespace means the
        // line is not a command and is recorded as a plain prompt.
        assert!(is_plain_prompt("  /login"));
        assert!(is_plain_prompt("  /models"));
        assert!(!is_plain_prompt("/models  "));
        assert!(is_plain_prompt("hello world"));
        assert!(is_plain_prompt("/notacommand"));
        assert!(!is_plain_prompt(""));
    }

    fn hist(items: &[&str]) -> VecDeque<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // History ["hello", "hi"] (hi is newest). Two Ups -> "hello", two Downs ->
    // empty. This is the exact scenario the user reported.
    #[test]
    fn up_down_cycle_to_empty() {
        let history = hist(&["hello", "hi"]);

        // Empty buffer, cursor on row 0, first Up -> load newest.
        let r = recall_up(&history, None, "", 0).unwrap();
        assert_eq!(r.index, Some(1));
        assert_eq!(r.text, "hi");

        // Buffer matches the shown entry ("hi"), second Up -> older.
        let r = recall_up(&history, Some(1), "hi", 0).unwrap();
        assert_eq!(r.index, Some(0));
        assert_eq!(r.text, "hello");

        // Down from "hello" -> newer.
        let r = recall_down(&history, Some(0)).unwrap();
        assert_eq!(r.index, Some(1));
        assert_eq!(r.text, "hi");

        // Down from the newest -> exit the cycle and load an empty buffer.
        let r = recall_down(&history, Some(1)).unwrap();
        assert_eq!(r.index, None);
        assert_eq!(r.text, "");
    }

    #[test]
    fn up_does_not_fire_on_nonempty_buffer() {
        // Per spec, Up only fires when the editor is empty. A non-empty draft
        // is left untouched and Up falls through to cursor movement.
        let history = hist(&["hello", "hi"]);
        assert!(recall_up(&history, None, "draft", 0).is_none());
    }

    #[test]
    fn up_does_not_fire_when_buffer_edited() {
        let history = hist(&["hello", "hi"]);
        // Buffer differs from the shown entry ("hi") -> no recall, falls
        // through to cursor movement.
        assert!(recall_up(&history, Some(1), "hi edited", 0).is_none());
    }

    #[test]
    fn up_does_not_fire_when_cursor_not_on_first_row() {
        let history = hist(&["hello", "hi"]);
        // Empty buffer but cursor on row 1 (e.g. a stray newline) -> no recall.
        assert!(recall_up(&history, None, "", 1).is_none());
    }

    #[test]
    fn up_does_not_fire_on_empty_history() {
        let history = hist(&[]);
        assert!(recall_up(&history, None, "", 0).is_none());
    }

    #[test]
    fn up_does_not_fire_at_oldest_entry() {
        let history = hist(&["hello", "hi"]);
        // At index 0 (oldest) -> no older entry to walk to.
        assert!(recall_up(&history, Some(0), "hello", 0).is_none());
    }

    #[test]
    fn down_is_noop_when_not_recalling() {
        let history = hist(&["hello", "hi"]);
        assert!(recall_down(&history, None).is_none());
    }
}
