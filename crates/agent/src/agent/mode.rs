/// Interaction mode for the agent.
///
/// Stored in [`Agent`](super::Agent) as an `AtomicU8`, so the numeric
/// mapping below is part of the mode's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Normal editing mode.
    #[default]
    Normal,
    /// Planning mode: read-only tools, plan suffix on every message.
    Plan,
    /// Review mode: read-only tools, review guidelines on the first message.
    Review,
}

impl Mode {
    pub(super) fn as_u8(self) -> u8 {
        match self {
            Mode::Normal => 0,
            Mode::Plan => 1,
            Mode::Review => 2,
        }
    }

    pub(super) fn from_u8(value: u8) -> Self {
        match value {
            1 => Mode::Plan,
            2 => Mode::Review,
            _ => Mode::Normal,
        }
    }
}
