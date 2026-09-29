//! Global actions for the Alan frontend.

use crate::root::AlanAction;
use alan_tui::keymap::{InputContext, KeyMapper};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

#[derive(Debug, Default, Clone, Copy)]
pub struct AlanKeyMapper;

impl KeyMapper<AlanAction> for AlanKeyMapper {
    fn map(&self, event: &Event, context: &InputContext) -> Option<AlanAction> {
        match event {
            Event::Resize(..) => Some(AlanAction::Resize),
            Event::Paste(data) => Some(AlanAction::Paste(data.clone())),
            // Only real keypresses (and auto-repeat) count; release events
            // would fire an action twice.
            Event::Key(key) if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                Some(AlanAction::Raw(Event::Key(*key)))
            }

            // Shift+Tab (reported as `BackTab` by some terminals) cycles modes.
            Event::Key(key) => Some(match (key.code, key.modifiers) {
                (KeyCode::Char('p'), m)
                    if m.contains(KeyModifiers::CONTROL) && !context.overlay_active =>
                {
                    AlanAction::ToggleProfiles
                }
                (KeyCode::BackTab, _) => AlanAction::ToggleMode,
                (KeyCode::Tab, m) if m.contains(KeyModifiers::SHIFT) => AlanAction::ToggleMode,
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => AlanAction::Quit,
                _ => AlanAction::Raw(Event::Key(*key)),
            }),
            event => Some(AlanAction::Raw(event.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyEventState;

    fn context(overlay_active: bool) -> InputContext {
        InputContext {
            overlay_active,
            focus_active: true,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn map(event: Event, overlay_active: bool) -> Option<AlanAction> {
        AlanKeyMapper.map(&event, &context(overlay_active))
    }

    #[test]
    fn plain_p_is_not_a_global_action() {
        let event = Event::Key(key(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(matches!(map(event, false), Some(AlanAction::Raw(_))));
    }

    #[test]
    fn ctrl_p_opens_the_profile_picker() {
        let event = Event::Key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(matches!(
            map(event, false),
            Some(AlanAction::ToggleProfiles)
        ));
    }

    #[test]
    fn overlays_swallow_the_profile_action() {
        let event = Event::Key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(matches!(map(event, true), Some(AlanAction::Raw(_))));
    }

    #[test]
    fn key_release_does_not_fire_a_global_action() {
        let mut key = key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        key.kind = KeyEventKind::Release;
        assert!(matches!(
            map(Event::Key(key), false),
            Some(AlanAction::Raw(_))
        ));
    }

    #[test]
    fn shift_tab_toggles_mode() {
        let event = Event::Key(key(KeyCode::Tab, KeyModifiers::SHIFT));
        assert!(matches!(map(event, false), Some(AlanAction::ToggleMode)));
    }

    #[test]
    fn backtab_toggles_mode() {
        let event = Event::Key(key(KeyCode::BackTab, KeyModifiers::NONE));
        assert!(matches!(map(event, false), Some(AlanAction::ToggleMode)));
    }

    #[test]
    fn ctrl_c_quits() {
        let event = Event::Key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(map(event, false), Some(AlanAction::Quit)));
    }

    #[test]
    fn resize_and_paste_are_semantic() {
        assert!(matches!(
            map(Event::Resize(1, 1), false),
            Some(AlanAction::Resize)
        ));
        assert!(matches!(
            map(Event::Paste("hi".into()), false),
            Some(AlanAction::Paste(text)) if text == "hi"
        ));
    }
}
