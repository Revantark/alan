use crate::core::Controller;
use ratatui::Frame;
use ratatui::layout::Rect;

use super::UiState;

/// Ratatui-facing view component.
///
/// Components render application state but do not handle terminal events or
/// issue application commands.
pub trait Component {
    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        controller: &Controller,
        state: &mut UiState,
    );
}
