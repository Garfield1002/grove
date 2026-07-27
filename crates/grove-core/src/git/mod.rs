//! git integration: command construction and output parsing.

pub mod commands;
pub mod parser;
pub mod status;

pub use commands::{
    ProjectDiscovery, RefEntry, WorktreeAdd, branch_delete, current_branch, discover_project,
    git_common_dir, list_refs, worktree_add, worktree_list, worktree_remove,
};
pub use parser::{WorktreeEntry, parse_worktree_list};
pub use status::{Operation, StatusSummary, parse_status, status_summary};
