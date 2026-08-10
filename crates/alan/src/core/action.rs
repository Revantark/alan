//! UI-independent actions produced by frontend input adapters.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Interrupt,
    Resize,
    Submit,
    ClearInput,
    Backspace,
    Insert(char),
    ScrollUp,
    ScrollDown,
    MouseScrollUp,
    MouseScrollDown,
}
