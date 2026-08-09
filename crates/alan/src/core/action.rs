//! UI-independent actions produced by frontend input adapters.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Submit,
    ClearInput,
    Backspace,
    Insert(char),
    ScrollUp,
    ScrollDown,
}
