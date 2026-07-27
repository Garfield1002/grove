//! The egui layer. Deliberately thin: it renders `grove-core` values and
//! turns clicks into [`Action`]s, and it never runs a subprocess.

pub mod dialogs;
pub mod project_list;
pub mod settings;
pub mod theme;
pub mod worktree_row;

/// Something the user asked for in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ToggleProject(String),
    RefreshProject(String),
    RemoveProject(String),
    ActivateWorktree {
        project_id: String,
        worktree_id: String,
    },
}
