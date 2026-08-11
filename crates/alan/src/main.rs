mod core;
mod views;

use core::{Action, Controller};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use std::io::stdout;
use std::time::Duration;

use agent::Agent;
use futures_util::StreamExt;
use providers::{FileCredentialStore, OpenRouterProvider, Provider, ProviderRegistry};
use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model_id = std::env::var("ALAN_MODEL").unwrap_or_else(|_| "openai/gpt-4o-mini".into());
    let credential_store = Arc::new(FileCredentialStore::new(auth_path()?));
    let provider = OpenRouterProvider::from_store(credential_store.clone())
        .with_model(&model_id)
        .build()?;
    let model = provider.bind(&model_id)?;
    let registry = ProviderRegistry::new([Arc::new(provider) as Arc<dyn Provider>]);
    let agent = Agent::builder(model).build();

    let mut app = Controller::with_runtime(agent, registry, credential_store);
    event_loop(&mut app).await
}

fn auth_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine Alan home directory"))?;
    Ok(PathBuf::from(home).join(".alan").join("auth.json"))
}

async fn event_loop(app: &mut Controller) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = async {
        terminal.clear()?;
        let mut ui = views::UiState::new();
        let mut events = EventStream::new();
        let mut render_tick = tokio::time::interval(Duration::from_millis(16));
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let poll = app.poll();
            ui.on_poll(poll);
            ui.tick();

            if ui.take_dirty() {
                terminal.draw(|frame| views::draw(frame, app, &mut ui))?;
            }

            tokio::select! {
                maybe_event = events.next() => {
                    let Some(result) = maybe_event else {
                        break;
                    };
                    let event = result?;
                    if let Some(action) = action_from_event(&event) {
                        let login_selection_active = app.login_selection_active();
                        if let Some(command) = ui.apply(action, login_selection_active)
                            && app.handle(command)
                        {
                            break;
                        }
                    }
                }
                _ = render_tick.tick() => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    execute!(stdout(), DisableMouseCapture)?;
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
