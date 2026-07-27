//! The egui layer. Deliberately thin: it renders `grove-core` values and
//! turns clicks into [`Action`]s, and it never runs a subprocess.

pub mod dialogs;
pub mod icons;
pub mod project_list;
pub mod settings;
pub mod theme;
pub mod window_edge;
pub mod worktree_row;

/// Something the user asked for in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ToggleProject(String),
    RefreshProject(String),
    /// Open the create-worktree dialog for a project.
    CreateWorktree(String),
    /// Open the removal dialog for a project, with no worktree selected.
    RemoveProject(String),
    ActivateWorktree {
        project_id: String,
        worktree_id: String,
    },
    /// Select a row without opening anything.
    SelectWorktree {
        project_id: String,
        worktree_id: String,
    },
    /// Attach an additional terminal client without retargeting the primary.
    OpenInNewTerminal {
        project_id: String,
        worktree_id: String,
    },
    /// Open the safe-removal dialog for a worktree.
    RemoveWorktree {
        project_id: String,
        worktree_id: String,
    },
}
