//! Raw input mapping.
//!
//! The framework receives native terminal events; a user-provided
//! [`KeyMapper`] converts them into semantic actions. Components must not
//! depend on raw crossterm events — crossterm types appear only at this
//! frontend/runtime boundary.

use crossterm::event::Event;

/// Context describing the input state at mapping time.
#[derive(Debug, Default, Clone, Copy)]
pub struct InputContext {
    /// Whether an overlay is capturing input.
    pub overlay_active: bool,
    /// Whether any component currently holds focus.
    pub focus_active: bool,
}

/// Converts native terminal events into semantic actions.
///
/// Return `None` to ignore an event entirely (no action is dispatched).
pub trait KeyMapper<A>: Send + Sync + 'static {
    fn map(&self, event: &Event, context: &InputContext) -> Option<A>;
}

/// A [`KeyMapper`] that passes crossterm events through unchanged.
///
/// Useful for applications whose action type *is* the crossterm event, or in
/// tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughMapper;

impl KeyMapper<Event> for PassthroughMapper {
    fn map(&self, event: &Event, _context: &InputContext) -> Option<Event> {
        Some(event.clone())
    }
}

/// A [`KeyMapper`] that maps every event to no action.
///
/// The runtime's default so an application without a mapper simply receives
/// no input. Most applications should set a real mapper via
/// [`RuntimeBuilder::key_mapper`](crate::RuntimeBuilder::key_mapper).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMapper;

impl<A> KeyMapper<A> for NoopMapper {
    fn map(&self, _event: &Event, _context: &InputContext) -> Option<A> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    /// Application action for the tests below.
    #[derive(Debug, PartialEq, Eq)]
    enum TestAction {
        Quit,
        Insert(char),
    }

    struct TestMapper;

    impl KeyMapper<TestAction> for TestMapper {
        fn map(&self, event: &Event, _context: &InputContext) -> Option<TestAction> {
            let Event::Key(key) = event else {
                return None;
            };
            if key.kind != KeyEventKind::Press {
                return None;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), KeyModifiers::CONTROL) => Some(TestAction::Quit),
                (KeyCode::Char(c), KeyModifiers::NONE) => Some(TestAction::Insert(c)),
                _ => None,
            }
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn maps_semantic_actions() {
        let mapper = TestMapper;
        assert_eq!(
            mapper.map(
                &key(KeyCode::Char('q'), KeyModifiers::CONTROL),
                &InputContext::default()
            ),
            Some(TestAction::Quit)
        );
        assert_eq!(
            mapper.map(
                &key(KeyCode::Char('x'), KeyModifiers::NONE),
                &InputContext::default()
            ),
            Some(TestAction::Insert('x'))
        );
    }

    #[test]
    fn ignores_unmapped_and_released_keys() {
        let mapper = TestMapper;
        assert_eq!(
            mapper.map(
                &key(KeyCode::F(1), KeyModifiers::NONE),
                &InputContext::default()
            ),
            None
        );
        let released = Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(mapper.map(&released, &InputContext::default()), None);
    }

    #[test]
    fn passthrough_forwards_events() {
        let mapper = PassthroughMapper;
        let event = key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(mapper.map(&event, &InputContext::default()), Some(event));
    }
}
