mod core;
mod views;

use core::{Action, Controller};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use agent::Agent;
use providers::{OpenRouterProvider, Provider};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build the agent before entering the terminal so startup errors print plainly.
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY not set"))?;
    let model_id = std::env::var("ALAN_MODEL").unwrap_or_else(|_| "openai/gpt-4o-mini".into());

    let provider = OpenRouterProvider::builder(api_key)
        .with_model(&model_id)
        .build()?;
    let model = provider.bind(&model_id)?;
    let agent = Agent::builder(model).build();

    let mut app = Controller::new(agent);
    event_loop(&mut app)
}

fn event_loop(app: &mut Controller) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = (|| {
        terminal.clear()?;

        let mut ui = views::UiState::new();

        loop {
            // Keep transcript pinned to newest content. Old code used u16::MAX
            // directly as Paragraph scroll offset, which hid every response.
            ui.on_poll(app.poll());
            terminal.draw(|frame| views::draw(frame, app, &ui))?;

            if event::poll(std::time::Duration::from_millis(100))? {
                let event = event::read()?;
                if let Some(action) = action_from_event(&event) {
                    if ui.apply(action, app) {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

fn action_from_event(event: &Event) -> Option<Action> {
    let Event::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }

    Some(match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Enter => Action::Submit,
        KeyCode::Esc => Action::ClearInput,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Char(c) => Action::Insert(c),
        KeyCode::PageUp => Action::ScrollUp,
        KeyCode::PageDown => Action::ScrollDown,
        _ => return None,
    })
}
