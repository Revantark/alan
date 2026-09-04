//! Single-root `tui` adapter for Alan.
//!
//! This wraps the existing UI state verbatim: [`Controller`] owns application
//! state, [`UiState`] owns frontend interaction state, and [`AppView`] renders
//! both. No behavior lives here beyond routing `tui` callbacks back into that
//! code, matching `main.rs::event_loop` before it.
//!
//! `Controller` is not `Sync` (it holds `JoinHandle`s and plain state), so the
//! root keeps it behind a `Mutex`. `render` is `&self` by framework contract.

use providers::{CredentialStore, ProviderRegistry};
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crossterm::event::{Event, MouseEventKind};
use futures_util::Stream;
use ratatui::Frame;
use ratatui::layout::Rect;
use tui::context::Context;
use tui::entity::Entity;
use tui::keymap::{InputContext, KeyMapper};
use tui::{ActionStatus, Component, RenderContext, Subscription, SubscriptionEvent};

use crate::core::{Action, CommandOutcome, Controller};
use crate::login_overlay::{LoginDone, LoginOverlay};
use crate::views::Header;
use crate::views::{AppView, UiState};

/// How often streamed agent output is collected while the app is idle.
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Item type of the poll ticker. The value is unused; each item means "poll now".
type PollTick = ();

/// Semantic input for the Alan frontend.
///
/// Context-free inputs (resize, mouse wheel, bracketed paste) are semantic
/// variants decoded in [`AlanKeyMapper`]; everything else stays a 1:1
/// [`AlanAction::Raw`] wrapper until a later slice moves it over.
#[derive(Debug, Clone)]
pub enum AlanAction {
    Resize,
    MouseScrollUp,
    MouseScrollDown,
    Paste(String),
    Raw(Event),
}

/// Passes terminal events through as [`AlanAction::Raw`], except for the
/// context-free inputs decoded above.
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
            Event::Paste(data) => Some(AlanAction::Paste(data.clone())),
            event => Some(AlanAction::Raw(event.clone())),
        }
    }
}

/// Owns the whole Alan frontend as one `tui` component, plus the
/// dependencies needed to open feature overlays (today: login).
pub struct AlanRoot {
    inner: Mutex<Inner>,
    providers: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    /// Retained so the poll stream keeps running. Dropping it cancels the stream.
    poll: Option<Subscription>,
    header: Option<Entity<Header>>,
}

struct Inner {
    controller: Controller,
    ui: UiState,
    view: AppView,
}

impl AlanRoot {
    pub fn new(
        controller: Controller,
        providers: Arc<ProviderRegistry>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                controller,
                ui: UiState::new(),
                view: AppView::new(),
            }),
            providers,
            credentials,
            poll: None,
            header: None,
        }
    }

    fn open_login(&mut self, cx: &mut Context<'_, Self, AlanAction>) {
        let overlay = cx.open_overlay(LoginOverlay::new(
            Arc::clone(&self.providers),
            Arc::clone(&self.credentials),
        ));
        cx.subscribe_once::<LoginDone, _, _>(overlay, |done, root, _, cx| match done {
            LoginDone::Succeeded { provider } => {
                root.login_done(provider.clone(), cx);
            }
            LoginDone::Dismissed => {}
        });
    }

    fn login_done(
        &mut self,
        provider: providers::ProviderId,
        cx: &mut Context<'_, Self, AlanAction>,
    ) {
        let mut inner = self.inner.lock().expect("alan root poisoned");
        inner
            .controller
            .push_info(format!("Logged in to {}", provider.0));
        // The transcript revision bumped but `UiState` doesn't know; the
        // 16ms tick stream would catch it within a frame, but notify now.
        cx.notify();
    }
}

impl Component<AlanAction> for AlanRoot {
    fn init(&mut self, cx: &mut Context<'_, Self, AlanAction>)
    where
        Self: Sized,
    {
        self.header = Some(cx.insert(Header));
        self.poll = Some(cx.subscribe_stream(poll_ticks(), |event, root, cx| {
            let SubscriptionEvent::Item(()) = event else {
                return;
            };
            let mut inner = root.inner.lock().expect("alan root poisoned");
            let poll = inner.controller.poll();
            inner.ui.on_poll(poll);
            inner.ui.tick();
            inner.ui.flush_wheel();
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
                    AlanAction::Paste(_) | AlanAction::Raw(_) => unreachable!("matched above"),
                };
                let mut inner = self.inner.lock().expect("alan root poisoned");
                inner.ui.apply(action);
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                ActionStatus::Handled
            }
            AlanAction::Paste(text) => {
                let mut inner = self.inner.lock().expect("alan root poisoned");
                // Disjoint field borrows: `view`/`controller` reads feed `ui` mutation.
                let Inner {
                    controller,
                    ui,
                    view,
                } = &mut *inner;
                let command = ui.handle_event(
                    Event::Paste(text.clone()),
                    view.lines(),
                    controller.completion_mut(),
                );
                let outcome: Option<CommandOutcome> =
                    command.map(|command| inner.controller.handle(command));
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                if let Some(outcome) = outcome {
                    if outcome.quit {
                        cx.quit();
                    }
                    drop(inner);
                    if outcome.open_login {
                        self.open_login(cx);
                    }
                }
                ActionStatus::Handled
            }
            AlanAction::Raw(event) => {
                let mut inner = self.inner.lock().expect("alan root poisoned");
                let Inner {
                    controller,
                    ui,
                    view,
                } = &mut *inner;
                let command =
                    ui.handle_event(event.clone(), view.lines(), controller.completion_mut());
                let outcome: Option<CommandOutcome> =
                    command.map(|command| inner.controller.handle(command));
                if inner.ui.take_dirty() {
                    cx.notify();
                }
                if let Some(outcome) = outcome {
                    if outcome.quit {
                        cx.quit();
                    }
                    drop(inner);
                    if outcome.open_login {
                        self.open_login(cx);
                    }
                }
                ActionStatus::Handled
            }
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, cx: &RenderContext<'_, AlanAction>) {
        if let Some(header) = self.header {
            let [header_area, body_area] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
            cx.render_entity(header, frame, header_area);
            let mut inner = self.inner.lock().expect("alan root poisoned");
            let Inner {
                controller,
                ui,
                view,
            } = &mut *inner;
            view.render(frame, body_area, controller, ui);
        } else {
            let mut inner = self.inner.lock().expect("alan root poisoned");
            let Inner {
                controller,
                ui,
                view,
            } = &mut *inner;
            view.render(frame, area, controller, ui);
        }
    }
}

fn poll_ticks() -> impl Stream<Item = PollTick> + Send + 'static {
    futures_util::stream::unfold((), |state| async move {
        tokio::time::sleep(TICK_INTERVAL).await;
        Some(((), state))
    })
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;

    /// Map the login path (`UiState::apply`) onto pre-completion keys. Kept while
    /// `AlanAction::Raw` still carries `Event::Key`; each arm is a 1:1 legacy
    /// bridge, not new behavior. Used by the unit tests below; production routes
    /// keys through `UiState::handle_event` instead.
    #[cfg(test)]
    fn action_from_event(event: &Event) -> Option<Action> {
        match event {
            Event::Key(key) => {
                use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

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
    fn mapper_maps_paste_regardless_of_context() {
        let mapper = AlanKeyMapper;
        for context in [
            InputContext::default(),
            InputContext {
                overlay_active: true,
                focus_active: true,
            },
        ] {
            let event = Event::Paste("hello\nworld".into());
            assert!(matches!(
                mapper.map(&event, &context),
                Some(AlanAction::Paste(actual)) if actual == "hello\nworld"
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
