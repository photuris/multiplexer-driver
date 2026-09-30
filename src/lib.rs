//! Drive terminal multiplexer harnesses (tmux, Herdr) and the coding
//! agents inside them through one vocabulary of workspaces, tabs, and
//! panes. See the design spec in `docs/superpowers/specs/`.

pub mod agents;
pub mod driver;
pub mod model;
pub mod patterns;
pub mod process;
pub mod text;
