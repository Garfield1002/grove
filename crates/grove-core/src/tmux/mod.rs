//! tmux integration, always against Grove's private server.

pub mod client;
pub mod server;
pub mod session;

pub use client::{ClientInfo, list_clients, primary_client, switch_client};
pub use server::TmuxServer;
pub use session::{SessionInfo, ensure_session, has_session, list_sessions};
