//! Single-root `tui` adapter for Alan.
//!
//! This wraps the existing UI state verbatim: [`Controller`] owns application
//! state, [`UiState`] owns frontend interaction state, and [`AppView`] renders
//! both. No behavior lives here beyond routing `tui` callbacks back into that
//! code, matching `main.rs::event_loop` before it.
//!
//! `Controller` is not `Sync` (it holds `JoinHandle`s and plain state), so the
//! root keeps it behind a `Mutex`. `render` is `&self` by framework contract.

use std::sync::Mutex;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::Rect;
use tui::context::Context;
use tui::keymap::{InputContext, KeyMapper};
use tui::{ActionStatus, Component, RenderContext, Subscription, SubscriptionEvent};

use crate::core::{Action, Controller, Overlay};
use crate::views::{AppView, UiState};

/// How often streamed agent output is collected while the app is idle.
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Item type of the poll ticker. The value is unused; each item means "poll now".
type PollTick = ();

/// Semantic input for the Alan frontend.
///
/// Context-free inputs (resize, mouse wheel) are semantic variants decoded in
/// [`AlanKeyMapper`]; everything else stays a 1:1 [`AlanAction::Raw`] wrapper
/// until a later slice moves it over.
#[derive(Debug, Clone)]
pub enum AlanAction {
    Resize,
    MouseScrollUp,
    MouseScrollDown,
    Raw(Event),
}

/// Passes every terminal event through as [`AlanAction::Raw`].
///
/// Owns the `KeyMapper` seam so future refinements happen here, at the
/// runtime boundary, and components never depend on raw crossterm mapping.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlanKeyMapper;

impl KeyMapper<AlanAction> for AlanKeyMapper {
    fn map(&self, event: &Event, _context: &InputContext) -> Option<AlanAction> {
        match event {
            Event::Resize(_, _) => Some(AlanAction::Resize),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => Some(AlanAction::MouseScrollUp),
                MouseEventKind::ScrollDown => Some(AlanAction::MouseScrollDown),
                _ => Some(AlanAction::Raw(event.clone())),
            },
            event => Some(AlanAction::Raw(event.clone())),
        }
    }
}

/// Owns the whole Alan frontend as one `tui` component.
pub struct AlanRoot {
    inner: Mutex<Inner>,
    /// Retained so the poll stream keeps running. Dropping it cancels the stream.
    #[allow(dead_code)]
    poll: Option<Subscription>,
}

struct Inner {
    controller: Controller,
    ui: UiState,
    view: AppView,
}

impl AlanRoot {
    pub fn new(controller: Controller) -> Self {
        Self {
            inner: Mutex::new(Inner {
                controller,
                ui: UiState::new(),
                view: AppView::new(),
            }),
            poll: None,
        }
    }
}

impl Component<AlanAction> for AlanRoot {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.poll = Some(cx.subscribe_stream(poll_ticks(), |event, root, cx| {
            let SubscriptionEvent::Item(()) = event else {
                return;
            };
            let mut inner = root.inner.lock().expect("alan root poisoned");
            let poll = inner.controller.poll();
            inner.ui.on_poll(poll);
            inner.ui.tick();
            if inner.ui.take_dirty() {
                cx.notify();
            }
        }));
    }

    fn handle_action(
        &mut self,
        action: &AlanAction,
        cx: &mut Context<'_, Self, AlanAction>,
    ) -> ActionStatus
    where
        Self: Sized,
    {
        match action {
            AlanAction::Resize | AlanAction::MouseScrollUp | AlanAction::MouseScrollDown => {
                let action = match action {
                    AlanAction::Resize => Action::Resize,
                    AlanAction::MouseScrollUp => Action::MouseScrollUp,
                    AlanAction::MouseScrollDown => Action::MouseScrollDown,
                    AlanAction::Raw(_) => unreachable!("matched above"),
                };
                let mut inner = self.inner.lock().expect("alan root poisoned");
                inner.ui.apply(action, false);
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => {
                let mut inner = self.inner.lock().expect("alan root poisoned");
                let command = if inner.controller.overlay() == Overlay::Login {
                    let selecting = inner.controller.login_selection_active();
                    action_from_event(event).and_then(|action| inner.ui.apply(action, selecting))
                } else {
                    // Disjoint field borrows: `view`/`controller` reads feed `ui` mutation.
                    let Inner {
                        controller,
                        ui,
                        view,
                    } = &mut *inner;
                    ui.handle_event(event.clone(), view.lines(), controller.completion_mut())
                };
                let should_quit = command.is_some_and(|command| inner.controller.handle(command));
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                if should_quit {
                    cx.quit();
                }
                ActionStatus::Handled
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, _cx: &RenderContext<'_, AlanAction>) {
        let mut inner = self.inner.lock().expect("alan root poisoned");
        // Scroll follow is kept in sync by the poll stream (`init`) and by
        // `Chat::render` via `UiState::sync_scroll`; drawing itself adds nothing.
        let _ = area;
        let Inner {
            controller,
            ui,
            view,
        } = &mut *inner;
        view.render(frame, controller, ui);
    }
}

fn poll_ticks() -> impl Stream<Item = PollTick> + Send + 'static {
    futures_util::stream::unfold((), |state| async move {
        tokio::time::sleep(TICK_INTERVAL).await;
        Some(((), state))
    })
}

fn action_from_event(event: &Event) -> Option<Action> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return None;
            }
            Some(match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Action::Interrupt
                }
                KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Action::PasteOrAttachImage
                }
                KeyCode::Tab | KeyCode::BackTab
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.code == KeyCode::BackTab =>
                {
                    Action::TogglePlanMode
                }
                KeyCode::Enter => Action::Submit,
                KeyCode::Esc => Action::ClearInput,
                KeyCode::Backspace => Action::Backspace,
                KeyCode::Char(c) => Action::Insert(c),
                KeyCode::PageUp | KeyCode::Up => Action::ScrollUp,
                KeyCode::PageDown | KeyCode::Down => Action::ScrollDown,
                _ => return None,
            })
        }
        Event::Mouse(mouse) => Some(match mouse.kind {
            MouseEventKind::ScrollUp => Action::MouseScrollUp,
            MouseEventKind::ScrollDown => Action::MouseScrollDown,
            _ => return None,
        }),
        Event::Resize(_, _) => Some(Action::Resize),
        Event::Paste(data) => Some(Action::Paste(data.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapper_passes_other_events_through_unchanged() {
        let mapper = AlanKeyMapper;
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        ));
        let mapped = mapper.map(&event, &InputContext::default());
        assert!(matches!(mapped, Some(AlanAction::Raw(actual)) if actual == event));
    }

    #[test]
    fn mapper_maps_resize_regardless_of_context() {
        let mapper = AlanKeyMapper;
        for context in [
            InputContext::default(),
            InputContext {
                overlay_active: true,
                focus_active: true,
            },
        ] {
            assert!(matches!(
                mapper.map(&Event::Resize(120, 40), &context),
                Some(AlanAction::Resize)
            ));
        }
    }

    #[test]
    fn mapper_maps_mouse_wheel_regardless_of_context() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mapper = AlanKeyMapper;
        for context in [
            InputContext::default(),
            InputContext {
                overlay_active: true,
                focus_active: true,
            },
        ] {
            let up = Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(matches!(
                mapper.map(&up, &context),
                Some(AlanAction::MouseScrollUp)
            ));
            let down = Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(matches!(
                mapper.map(&down, &context),
                Some(AlanAction::MouseScrollDown)
            ));
            // Clicks still need chat-area geometry, so they stay raw.
            let click = Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(matches!(
                mapper.map(&click, &context),
                Some(AlanAction::Raw(_))
            ));
        }
    }

    #[test]
    fn q_is_editor_input_not_quit() {
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ));
        assert_eq!(action_from_event(&event), Some(Action::Insert('q')));
    }

    #[test]
    fn ctrl_c_interrupts_and_resize_invalidates() {
        let interrupt = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(action_from_event(&interrupt), Some(Action::Interrupt));
        assert_eq!(
            action_from_event(&Event::Resize(120, 40)),
            Some(Action::Resize)
        );
    }

    #[test]
    fn arrow_keys_scroll_or_navigate() {
        let up = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        ));
        let down = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        assert_eq!(action_from_event(&up), Some(Action::ScrollUp));
        assert_eq!(action_from_event(&down), Some(Action::ScrollDown));
    }
}
