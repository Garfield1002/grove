//! Grove's core logic: git worktree discovery, a private tmux server,
//! terminal launching, configuration and state.
//!
//! This crate has no UI dependencies. Parsers take strings, commands are
//! built as `(program, args)` values, and every function that runs a
//! subprocess says so in its documentation — the UI must call those from a
//! worker thread only.

pub mod atomic;
pub mod config;
pub mod config_write;
pub mod error;
pub mod git;
pub mod ids;
pub mod ipc;
pub mod model;
pub mod paths;
pub mod process;
pub mod removal;
pub mod state;
pub mod status;
pub mod terminal;
pub mod tmux;
pub mod workflow;

pub use error::{Error, Result};
pub use model::{Project, SessionPresence, Worktree};
pub use paths::Paths;
pub use status::{SessionSignals, SessionStatus, StatusEngine, StatusPolicy};
pub use tmux::TmuxServer;
