//! Selection helpers re-exported from the shared `tui` crate.
//!
//! This module used to hold its own copy of the selection logic; the canonical
//! implementation now lives in `alan_tui::selection` and is re-exported here so
//! existing `crate::views::selection` imports keep working.
//!
//! Currently unused internally; kept so external callers and tests can still
//! address selection types via this path.
#[allow(unused_imports)]
pub use alan_tui::selection::*;
