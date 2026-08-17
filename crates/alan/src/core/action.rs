//! UI-independent actions produced by frontend input adapters.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Interrupt,
    Resize,
    Submit,
    ClearInput,
    Backspace,
    Insert(char),
    Paste(String),
    ScrollUp,
    ScrollDown,
    MouseScrollUp,
    MouseScrollDown,
    TogglePlanMode,
}

/// Semantic commands emitted by frontend state and handled by application core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Interrupt,
    Cancel,
    Submit(String),
    MoveLoginSelection(isize),
    TogglePlanMode,
}
