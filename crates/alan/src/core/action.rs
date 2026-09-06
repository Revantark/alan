//! Frontend semantic actions feeding `UiState::apply` / `handle_event`.
//!
//! Most variants are only constructed on the `Event::Key` bridge
//! (`tui_root::action_from_event`, tests) or in unit tests directly —
//! production input flows through `handle_event` — so dead-code warnings
//! here would be noise. Keep every variant while the `Raw(Event)` migration
//! is in flight; prune once the mapper is total.

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
    Submit {
        text: String,
        images: Vec<ImageAttachment>,
    },
    /// Request the root to open the login overlay entity. Only `AlanRoot`
    /// interprets it: reaching `Controller::handle` as `OpenLogin` is a stale
    /// no-op, and `/login` is produced by `Controller::submit`.
    OpenLogin,
}
