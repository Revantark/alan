use ratatui::style::Color;

pub const PROMPT_FG: Color = Color::Cyan;
pub const USER_FG: Color = Color::White;
pub const USER_BG: Color = Color::Rgb(42, 48, 58);
pub const TOOL_FG: Color = Color::Rgb(190, 198, 208);
pub const TOOL_BG: Color = Color::Rgb(34, 40, 49);
pub const TOOL_DONE_FG: Color = Color::Rgb(184, 224, 194);
pub const TOOL_DONE_BG: Color = Color::Rgb(43, 67, 52);
pub const TOOL_ERROR_FG: Color = Color::Rgb(240, 180, 184);
pub const TOOL_ERROR_BG: Color = Color::Rgb(70, 43, 47);
pub const RESPONSE_FG: Color = Color::White;
pub const MUTED_FG: Color = Color::DarkGray;
pub const EDITOR_BG: Color = Color::Rgb(28, 32, 39);
pub const EDITOR_FG: Color = Color::White;
pub const CHAT_PADDING: usize = 3;
pub const EDITOR_VISIBLE_LINES: u16 = 7;
pub const SELECTION_BG: Color = Color::Rgb(58, 76, 107);
pub const SELECTION_FG: Color = Color::White;
