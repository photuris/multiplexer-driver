//! tmux driver (§7). Workspaces are sessions, tabs are windows, and
//! handles are tmux pane IDs (`%12`).

use crate::{driver::Driver, patterns::Patterns};

/// Drives one tmux server.
#[derive(Debug)]
#[expect(dead_code, reason = "filled in by Task 3")]
pub struct Tmux {
    /// Socket name for `-L`; `None` uses the ambient server.
    session: Option<String>,
    /// Status patterns; `None` means status has no signal.
    patterns: Option<Patterns>,
}

impl Tmux {
    /// Creates a driver for the server named `session` (or the
    /// ambient one).
    pub fn new(session: Option<String>, patterns: Option<Patterns>) -> Self {
        Self { session, patterns }
    }
}

impl Driver for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }
}
