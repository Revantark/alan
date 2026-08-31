mod chat;
mod footer;
mod header;
mod login;
mod popup;
mod settings;

pub use chat::Chat;
pub use footer::Footer;
pub use header::Header;
pub use login::LoginOverlay;
pub use popup::PopupList;
pub use settings::SettingsOverlayView;

use ratatui::layout::{Constraint, Layout, Rect};

/// The middle `percent_x` × `percent_y` of `area`, for the overlays that float
/// above the transcript.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, middle, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);
    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(middle);
    center
}
