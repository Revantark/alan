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
    /// Explicit paste/attach request (Ctrl+V): attach a clipboard image if
    /// present, otherwise paste clipboard text.
    PasteOrAttachImage,
    ScrollUp,
    ScrollDown,
    MouseScrollUp,
    MouseScrollDown,
    TogglePlanMode,
}

/// An image attached to the next prompt via clipboard paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub name: String,
    pub mime_type: String,
    /// Raw base64-encoded image data (no `data:` prefix).
    pub base64_data: String,
}

/// Semantic commands emitted by frontend state and handled by application core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Interrupt,
    Cancel,
    Submit {
        text: String,
        images: Vec<ImageAttachment>,
    },
    MoveLoginSelection(isize),
    TogglePlanMode,
}
