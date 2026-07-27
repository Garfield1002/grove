//! tmux integration, always against Grove's private server.

pub mod client;
pub mod server;
pub mod session;

pub use client::{ClientInfo, list_clients, primary_client, switch_client};
pub use server::TmuxServer;
pub use session::{
    PaneInfo, SessionInfo, SessionMetadata, SessionSpec, WindowInfo, associate_session,
    ensure_session, has_session, kill_session, list_panes, list_sessions, select_window,
    windows_of,
};
