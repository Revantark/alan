//! Terminal lifecycle with panic-safe cleanup.
//!
//! The guard enables raw mode and the alternate screen on construction and
//! restores the terminal on drop, including during panics and early returns
//! from errors. It can additionally enable mouse capture, bracketed paste,
//! kitty keyboard enhancement flags, and a cursor style (see
//! [`TerminalOptions`]); every enabled feature is undone on drop in reverse
//! order.

use std::io::{self, Stdout, stdout};
use std::sync::Arc;

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::{Terminal, backend};

use crate::error::RuntimeError;

/// Features a [`TerminalGuard`] sets up on the terminal.
///
/// The default mirrors the setup Alan's binary performs: mouse capture and
/// bracketed paste enabled, [`KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`]
/// and [`KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES`] pushed when
/// the terminal supports them, and a steady-bar cursor.
#[derive(Debug, Clone)]
pub struct TerminalOptions {
    /// Capture mouse scroll and click events.
    pub mouse_capture: bool,
    /// Deliver pasted text as `crossterm::event::Event::Paste` events instead
    /// of a stream of keystrokes.
    pub bracketed_paste: bool,
    /// Keyboard enhancement flags to push. Flags are only pushed when the
    /// terminal reports support for the kitty keyboard protocol; `None`
    /// disables keyboard enhancement entirely.
    pub keyboard_enhancement: Option<KeyboardEnhancementFlags>,
    /// Cursor style to set for the guard's lifetime, reset to the terminal's
    /// default on drop. `None` leaves the current style in place.
    pub cursor_style: Option<SetCursorStyle>,
}

impl Default for TerminalOptions {
    /// Defaults matching the terminal configuration Alan's binary performs today.
    fn default() -> Self {
        Self {
            mouse_capture: true,
            bracketed_paste: true,
            keyboard_enhancement: Some(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            ),
            cursor_style: Some(SetCursorStyle::SteadyBar),
        }
    }
}

/// Terminal features currently applied by a guard, tracked so teardown undoes
/// exactly what was enabled.
#[derive(Debug, Default, Clone, Copy)]
struct EnabledFeatures {
    alt_screen: bool,
    mouse_capture: bool,
    bracketed_paste: bool,
    keyboard_enhancement: bool,
    cursor_style: bool,
}

impl EnabledFeatures {
    /// Undo the applied features, most recently applied first.
    ///
    /// Best effort: a failure to restore one feature does not stop the others
    /// from being restored.
    fn restore(self) {
        let mut stdout = io::stdout();
        if self.cursor_style {
            let _ = execute!(stdout, SetCursorStyle::DefaultUserShape);
        }
        if self.keyboard_enhancement {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        if self.bracketed_paste {
            let _ = execute!(stdout, DisableBracketedPaste);
        }
        if self.mouse_capture {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        if self.alt_screen {
            let _ = execute!(stdout, LeaveAlternateScreen);
        }
    }
}

/// Guard restoring terminal state on drop.
///
/// Holds the terminal so callers can draw through the guard; on drop (normal
/// exit, `?` propagation, or panic) it restores every terminal feature the
/// guard enabled.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    enabled: EnabledFeatures,
}

/// Backend type used by the framework's default terminal.
pub type CrosstermBackend<B> = backend::CrosstermBackend<B>;

impl TerminalGuard {
    /// Enable raw mode, enter the alternate screen, and create the terminal
    /// with [`TerminalOptions::default`].
    pub fn new() -> Result<Self, RuntimeError> {
        Self::with_options(TerminalOptions::default())
    }

    /// Enable raw mode, enter the alternate screen, and create the terminal
    /// with the requested `options`.
    ///
    /// Each feature is recorded as it is applied, and on drop — or when setup
    /// fails partway through — the applied features are undone in reverse
    /// order. Keyboard enhancement flags are pushed only after the terminal
    /// reports support for them.
    pub fn with_options(options: TerminalOptions) -> Result<Self, RuntimeError> {
        enable_raw_mode()?;

        let mut enabled = EnabledFeatures::default();
        let setup = (|| {
            let mut stdout = stdout();
            execute!(stdout, EnterAlternateScreen)?;
            enabled.alt_screen = true;

            if options.mouse_capture {
                execute!(stdout, EnableMouseCapture)?;
                enabled.mouse_capture = true;
            }
            if options.bracketed_paste {
                execute!(stdout, EnableBracketedPaste)?;
                enabled.bracketed_paste = true;
            }

            // Pushing enhancement flags on a terminal without kitty protocol
            // support produces garbage input, so only push after the terminal
            // reports support.
            let keyboard_enhancement = options
                .keyboard_enhancement
                .filter(|_| supports_keyboard_enhancement().unwrap_or(false));
            if let Some(flags) = keyboard_enhancement {
                execute!(stdout, PushKeyboardEnhancementFlags(flags))?;
                enabled.keyboard_enhancement = true;
            }

            if let Some(style) = options.cursor_style {
                execute!(stdout, style)?;
                enabled.cursor_style = true;
            }

            Terminal::new(CrosstermBackend::new(stdout))
        })();
        match setup {
            Ok(terminal) => Ok(Self { terminal, enabled }),
            Err(error) => {
                enabled.restore();
                let _ = disable_raw_mode();
                Err(RuntimeError::Terminal(error))
            }
        }
    }

    /// Borrow the terminal for drawing.
    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    /// Whether keyboard enhancement flags were pushed for this guard.
    ///
    /// Applications use this to decide which key events to expect, since
    /// `Release`/`Repeat` key kinds only arrive when the kitty keyboard
    /// protocol is active.
    pub fn keyboard_enhanced(&self) -> bool {
        self.enabled.keyboard_enhancement
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.flush();
        self.enabled.restore();
        let _ = disable_raw_mode();
    }
}

type PanicHook = dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static;

/// Guard restoring the previous panic hook when dropped.
pub struct PanicHookGuard {
    previous: Option<Arc<PanicHook>>,
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let current = std::panic::take_hook();
            drop(current);
            std::panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

/// Install a panic hook that restores the terminal before delegating.
///
/// Restores every terminal feature the framework may have enabled — cursor
/// style, keyboard enhancement flags, bracketed paste, mouse capture, the
/// alternate screen, and raw mode — then delegates to the previous hook.
pub fn install_panic_hook() -> PanicHookGuard {
    let previous: Arc<PanicHook> = std::panic::take_hook().into();
    let delegated = Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        restore_for_panic();
        delegated(info);
    }));
    PanicHookGuard {
        previous: Some(previous),
    }
}

/// Best-effort restore of every terminal feature the framework may have
/// enabled, for use from the panic hook where the guard's exact feature record
/// is not available. Each command is a no-op for features that were never
/// enabled (per the kitty keyboard protocol, popping from an empty enhancement
/// stack is a no-op).
fn restore_for_panic() {
    let _ = execute!(
        io::stdout(),
        SetCursorStyle::DefaultUserShape,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
    );
    let _ = disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_match_alan_setup() {
        let options = TerminalOptions::default();
        assert!(options.mouse_capture);
        assert!(options.bracketed_paste);
        assert_eq!(
            options.keyboard_enhancement,
            Some(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        );
        assert_eq!(options.cursor_style, Some(SetCursorStyle::SteadyBar));
    }
}
