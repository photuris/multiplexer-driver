//! Herdr driver (§8). Handles are Herdr pane IDs (`w1:p2`).

use crate::driver::Driver;

/// Drives one Herdr server.
#[derive(Debug)]
#[expect(dead_code, reason = "filled in by Task 5")]
pub struct Herdr {
    /// Session name; `None` uses the ambient `HERDR_SOCKET_PATH`.
    session: Option<String>,
}

impl Herdr {
    /// Creates a driver for the Herdr session `session` (or the
    /// ambient one).
    pub fn new(session: Option<String>) -> Self {
        Self { session }
    }
}

impl Driver for Herdr {
    fn name(&self) -> &'static str {
        "herdr"
    }
}
