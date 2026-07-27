//! git integration: command construction and output parsing.

pub mod commands;
pub mod parser;

pub use commands::{ProjectDiscovery, discover_project, git_common_dir, worktree_list};
pub use parser::{WorktreeEntry, parse_worktree_list};
