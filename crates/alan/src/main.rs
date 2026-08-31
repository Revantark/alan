mod core;
mod logging;
mod views;

use core::settings;
use core::{Action, Controller, Overlay};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use std::io::stdout;
use std::time::Duration;

use agent::{Agent, SessionManager, default_tools};
use futures_util::StreamExt;
use providers::{FileCredentialStore, OpenRouterProvider, Provider, ProviderRegistry};
use std::path::PathBuf;
use std::sync::Arc;

use crate::logging::init;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _guard = init().unwrap();
    let alan_dir = alan_data_dir()?;
    let cwd = std::env::current_dir()?;

    let settings = settings::SettingsController::new(&alan_dir, &cwd)?;

    let credential_store = Arc::new(FileCredentialStore::new(alan_dir.join("auth.json")));
    let provider = OpenRouterProvider::from_store(credential_store.clone())
        .with_model(&settings.current().model)
        .build()?;
    let registry = ProviderRegistry::new([Arc::new(provider) as Arc<dyn Provider>]);
    let model = settings::bind(&registry, settings.current())?;
    let session_manager = Arc::new(SessionManager::new(alan_dir.join("sessions")));
    let resumed_session = if let Some(session_id) = configured_session_id()? {
        Some(session_manager.get_session(&session_id, &cwd).await?)
    } else {
        None
    };

    let was_resumed = resumed_session.is_some();
    let mut agent_builder = Agent::builder(model)
        .with_default_system_prompt()
        .with_directory(cwd.clone())
        .with_tools(default_tools())
        .session_manager(session_manager);

    if let Some(session) = resumed_session {
        agent_builder = agent_builder.resume_session(session);
    }
    let agent = agent_builder.build()?;

    let mut app = Controller::new(agent, registry, credential_store, settings);

    if was_resumed {
        app.restore_session_history().await;
    }
    let result = event_loop(&mut app).await;
    if let Some(session_id) = app.session_id().await {
        println!("\nSession saved. Resume it with:\n\nALAN_SESSION={session_id} alan");
    }
    result
}

fn alan_data_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine Alan home directory"))?;
    Ok(PathBuf::from(home).join(".alan"))
}

fn configured_session_id() -> anyhow::Result<Option<String>> {
    let Some(id) = std::env::var_os("ALAN_SESSION") else {
        return Ok(None);
    };
    let id = id.to_string_lossy().trim().to_owned();
    if id.is_empty() {
        return Err(anyhow::anyhow!("ALAN_SESSION must not be empty"));
    }
    Ok(Some(id))
}

async fn event_loop(app: &mut Controller) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let keyboard_enhancement =
        crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    if keyboard_enhancement {
        execute!(
            stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            ),
        )?;
    }
    let result = async {
        terminal.clear()?;
        let mut ui = views::UiState::new();
        let mut view = views::AppView::new();
        execute!(stdout(), SetCursorStyle::SteadyBar)?;
        let mut events = EventStream::new();
        let mut render_tick = tokio::time::interval(Duration::from_millis(16));
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let poll = app.poll();
            ui.on_poll(poll);
            ui.tick();

            if ui.take_dirty() {
                terminal.draw(|frame| view.render(frame, app, &mut ui))?;
            }

            tokio::select! {
                maybe_event = events.next() => {
                    let Some(result) = maybe_event else {
                        break;
                    };
                    let event = result?;
                    let command = match app.overlay() {
                        Overlay::None => ui.handle_event(event, view.lines(), app.completion_mut()),
                        overlay => {
                            let mode = app.input_mode(overlay);
                            action_from_event(&event).and_then(|a| ui.apply(a, mode))
                        }
                    };
                    let should_quit = command.is_some_and(|command| app.handle(command));
                    if let Some(seed) = app.take_input_seed() {
                        ui.seed_input(seed);
                    }
                    if ui.take_dirty() {
                        terminal.draw(|frame| view.render(frame, app, &mut ui))?;
                    }
                    if should_quit {
                        break;
                    }
                }
                _ = render_tick.tick() => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if keyboard_enhancement {
        execute!(stdout(), PopKeyboardEnhancementFlags)?;
    }
    execute!(stdout(), DisableMouseCapture, DisableBracketedPaste)?;
    ratatui::restore();
    result
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
                // Plain Tab is free here: this map is only consulted with an
                // overlay open, and completion's Tab lives on the other path.
                KeyCode::Tab | KeyCode::BackTab => Action::Cycle,
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

    /// The settings overlay offers "Tab to switch", and terminals report
    /// Shift+Tab as either `BackTab` or a shifted `Tab`.
    #[test]
    fn every_tab_spelling_cycles() {
        for key in [
            crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            crossterm::event::KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
        ] {
            assert_eq!(
                action_from_event(&Event::Key(key)),
                Some(Action::Cycle),
                "{key:?}"
            );
        }
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
