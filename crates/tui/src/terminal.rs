//! Terminal lifecycle with panic-safe cleanup.
//!
//! The guard enables raw mode and the alternate screen on construction and
//! restores the terminal on drop, including during panics and early returns
//! from errors.

use std::io::{self, Stdout, stdout};
use std::sync::Arc;

use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend};

use crate::error::RuntimeError;

/// Guard restoring terminal state on drop.
///
/// Holds the terminal so callers can draw through the guard; on drop (normal
/// exit, `?` propagation, or panic) it leaves the alternate screen and
/// disables raw mode.
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

/// Backend type used by the framework's default terminal.
pub type CrosstermBackend<B> = backend::CrosstermBackend<B>;

impl TerminalGuard {
    /// Enable raw mode, enter the alternate screen, and create the terminal.
    pub fn new() -> Result<Self, RuntimeError> {
        enable_raw_mode()?;
        let setup = (|| {
            let mut stdout = stdout();
            execute!(stdout, EnterAlternateScreen)?;
            let backend = CrosstermBackend::new(stdout);
            Terminal::new(backend)
        })();
        match setup {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(RuntimeError::Terminal(error))
            }
        }
    }

    /// Borrow the terminal for drawing.
    pub fn terminal(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.flush();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
pub fn install_panic_hook() -> PanicHookGuard {
    let previous: Arc<PanicHook> = std::panic::take_hook().into();
    let delegated = Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        delegated(info);
    }));
    PanicHookGuard {
        previous: Some(previous),
    }
}
