//! The create-worktree dialog (DESIGN.md §10), shown in its own OS window
//! ([`crate::ui::chrome`]).
//!
//! Asks for a branch name, a base branch or commit, a directory, whether to
//! create the branch, and whether to open the session afterwards. The form
//! state and its derived path live in [`CreateForm`], which is a plain value
//! with no egui in it, so the rules below are unit-tested.

use std::path::PathBuf;

use egui::Ui;
use grove_core::git::{RefEntry, WorktreeAdd};
use grove_core::model::{Project, suggest_worktree_path};

use crate::ui::{icons, theme};

/// Everything the dialog holds between frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateForm {
    pub project_id: String,
    pub project_name: String,
    pub repository_path: PathBuf,
    pub git_common_dir: PathBuf,
    /// Parent directory the suggested path is built under.
    pub default_parent: PathBuf,
    /// Name of the branch to create. Ignored when `create_branch` is false.
    pub branch: String,
    /// Base branch or commit; also the branch checked out when not creating.
    pub base_ref: String,
    pub path: String,
    pub create_branch: bool,
    pub open_after: bool,
    /// Choices for the base ref, filled in by the worker.
    pub refs: Vec<RefEntry>,
    pub refs_loaded: bool,
    /// Once the user edits the path, Grove stops deriving it from the branch.
    pub path_edited: bool,
}

impl CreateForm {
    pub fn new(project: &Project) -> Self {
        Self {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            repository_path: project.repository_path.clone(),
            git_common_dir: project.git_common_dir.clone(),
            default_parent: project.default_worktree_path.clone(),
            branch: String::new(),
            base_ref: String::new(),
            path: String::new(),
            create_branch: true,
            open_after: true,
            refs: Vec::new(),
            refs_loaded: false,
            path_edited: false,
        }
    }

    /// The name the directory is derived from: the new branch when creating
    /// one, else the branch being checked out.
    pub fn source_name(&self) -> &str {
        let name = if self.create_branch {
            self.branch.trim()
        } else {
            self.base_ref.trim()
        };
        // `origin/feature/x` is checked out as a directory named after the
        // branch, not after the remote.
        match name.split_once('/') {
            Some((remote, rest)) if !self.create_branch && self.is_remote(remote) => rest,
            _ => name,
        }
    }

    fn is_remote(&self, candidate: &str) -> bool {
        self.refs
            .iter()
            .any(|r| r.is_remote && r.name.starts_with(&format!("{candidate}/")))
    }

    /// Re-derive the suggested path, unless the user has taken it over.
    pub fn sync_path(&mut self) {
        if self.path_edited {
            return;
        }
        let source = self.source_name();
        self.path = if source.is_empty() {
            String::new()
        } else {
            suggest_worktree_path(&self.default_parent, source)
                .display()
                .to_string()
        };
    }

    /// Why the form cannot be submitted yet, if it cannot.
    pub fn problem(&self) -> Option<&'static str> {
        if self.create_branch && self.branch.trim().is_empty() {
            return Some("A branch name is required.");
        }
        if !self.create_branch && self.base_ref.trim().is_empty() {
            return Some("Choose the branch or commit to check out.");
        }
        if self.path.trim().is_empty() {
            return Some("A worktree directory is required.");
        }
        None
    }

    pub fn is_valid(&self) -> bool {
        self.problem().is_none()
    }

    /// The git operation this form describes. Grove passes the values through
    /// as arguments; validating refs is git's job, and its refusal is shown
    /// verbatim.
    pub fn to_add(&self) -> Option<WorktreeAdd> {
        if !self.is_valid() {
            return None;
        }
        let base = self.base_ref.trim();
        Some(WorktreeAdd {
            path: PathBuf::from(self.path.trim()),
            new_branch: self
                .create_branch
                .then(|| self.branch.trim().to_string())
                .filter(|b| !b.is_empty()),
            base_ref: (!base.is_empty()).then(|| base.to_string()),
        })
    }
}

/// What the dialog is asking the app to do.
#[derive(Default)]
pub enum Outcome {
    #[default]
    Idle,
    Cancelled,
    Create(Box<WorktreeAdd>),
    /// Ask for the desktop's directory picker; the field stays typeable.
    Browse,
}

/// Default inner size of the create-worktree window: three fields, two
/// checkboxes and a button row, with room for a long path.
pub const SIZE: [f32; 2] = [480.0, 380.0];
/// Floor: the labels and a field that still shows a path.
pub const MIN_SIZE: [f32; 2] = [380.0, 300.0];

/// The window title, which names the project the worktree belongs to.
pub fn title(form: &CreateForm) -> String {
    format!("Create worktree — {}", form.project_name)
}

/// The dialog's contents. The window around it is [`crate::ui::chrome`]'s.
pub fn body(ui: &mut Ui, form: &mut CreateForm) -> Outcome {
    let mut outcome = Outcome::Idle;
    let width = ui.available_width();
    let mut changed = false;

    changed |= ui
        .checkbox(&mut form.create_branch, "Create a new branch")
        .changed();

    if form.create_branch {
        ui.add_space(8.0);
        ui.label(theme::caption("Branch name"));
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut form.branch)
                    .hint_text("feature/auth")
                    .desired_width(width),
            )
            .changed();
    }

    ui.add_space(10.0);
    ui.label(theme::caption(if form.create_branch {
        "Base branch or commit"
    } else {
        "Branch or commit to check out"
    }));
    changed |= base_ref_field(ui, form, width);

    ui.add_space(10.0);
    ui.label(theme::caption("Worktree directory"));
    ui.horizontal(|ui| {
        let browse = crate::ui::NATIVE_FILE_PICKER;
        let reserved = if browse {
            theme::ICON_BUTTON + 8.0
        } else {
            0.0
        };
        let field = (width - reserved).max(120.0);
        let path_field = ui.add(
            egui::TextEdit::singleline(&mut form.path)
                .hint_text("/home/you/worktrees/feature-auth")
                .desired_width(field),
        );
        if path_field.changed() {
            form.path_edited = true;
        }
        if browse
            && icons::button(ui, true, icons::folder)
                .on_hover_text("Choose a directory")
                .clicked()
        {
            outcome = Outcome::Browse;
        }
    });
    if changed {
        form.sync_path();
    }

    ui.add_space(10.0);
    ui.checkbox(&mut form.open_after, "Open the session after creating");

    ui.add_space(10.0);
    ui.add(
        egui::Label::new(theme::label(
            form.problem().unwrap_or(
                "Grove runs `git worktree add` and shows git's own output if it refuses.",
            ),
            theme::FONT_SMALL,
            theme::TEXT_FAINT,
        ))
        .wrap(),
    );

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let create = egui::Button::new(theme::label(
            "Create",
            theme::FONT_BODY,
            if form.is_valid() {
                theme::TEXT_STRONG
            } else {
                theme::TEXT_FAINT
            },
        ))
        .fill(theme::ACCENT_FILL)
        .stroke(egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)));
        if ui.add_enabled(form.is_valid(), create).clicked()
            && let Some(add) = form.to_add()
        {
            outcome = Outcome::Create(Box::new(add));
        }
        if ui
            .button(theme::label("Cancel", theme::FONT_BODY, theme::TEXT_DIM))
            .clicked()
        {
            outcome = Outcome::Cancelled;
        }
    });

    outcome
}

/// The base-ref field: a free-text entry (any commit-ish is valid to git)
/// with the known branches offered from a menu.
fn base_ref_field(ui: &mut Ui, form: &mut CreateForm, row: f32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = (row - theme::ICON_BUTTON - 8.0).max(80.0);
        changed = ui
            .add(
                egui::TextEdit::singleline(&mut form.base_ref)
                    .hint_text("main")
                    .desired_width(width),
            )
            .changed();
        // A painted caret: `▾` (U+25BE) exists only in Hack, which is not in
        // egui's proportional font chain, so it rendered as a tofu box.
        let open = icons::button(ui, true, icons::caret_down)
            .on_hover_text("Local and remote-tracking branches");
        egui::Popup::menu(&open).show(|ui| {
            ui.set_min_width(200.0);
            if form.refs.is_empty() {
                ui.label(theme::label(
                    if form.refs_loaded {
                        "no branches"
                    } else {
                        "loading…"
                    },
                    theme::FONT_SMALL,
                    theme::TEXT_FAINT,
                ));
            }
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    for entry in &form.refs {
                        if ui
                            .button(theme::mono(
                                &entry.name,
                                theme::FONT_BODY,
                                if entry.is_remote {
                                    theme::TEXT_MUTED
                                } else {
                                    theme::TEXT_DIM
                                },
                            ))
                            .clicked()
                        {
                            form.base_ref = entry.name.clone();
                            changed = true;
                            ui.close();
                        }
                    }
                });
        });
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn project() -> Project {
        Project {
            id: "p1".into(),
            name: "acme-web".into(),
            repository_path: PathBuf::from("/home/u/acme-web"),
            git_common_dir: PathBuf::from("/home/u/acme-web/.git"),
            default_worktree_path: PathBuf::from("/home/u/worktrees"),
            is_expanded: true,
            worktrees: Vec::new(),
            unavailable: None,
        }
    }

    fn form() -> CreateForm {
        CreateForm::new(&project())
    }

    #[test]
    fn a_new_form_creates_a_branch_and_opens_the_session() {
        let form = form();
        assert!(form.create_branch);
        assert!(form.open_after);
        assert!(!form.is_valid(), "nothing has been typed yet");
        assert_eq!(form.problem(), Some("A branch name is required."));
    }

    #[test]
    fn the_path_follows_the_branch_name_until_the_user_edits_it() {
        let mut form = form();
        form.branch = "feature/auth".into();
        form.sync_path();
        assert_eq!(form.path, "/home/u/worktrees/feature-auth");

        form.branch = "fix/parser".into();
        form.sync_path();
        assert_eq!(form.path, "/home/u/worktrees/fix-parser");

        form.path_edited = true;
        form.path = "/elsewhere/mine".into();
        form.branch = "feature/other".into();
        form.sync_path();
        assert_eq!(form.path, "/elsewhere/mine", "the user's path wins");
    }

    #[test]
    fn clearing_the_branch_clears_the_suggested_path() {
        let mut form = form();
        form.branch = "x".into();
        form.sync_path();
        assert!(!form.path.is_empty());
        form.branch = "  ".into();
        form.sync_path();
        assert!(form.path.is_empty());
    }

    #[test]
    fn checking_out_an_existing_branch_names_the_directory_after_it() {
        let mut form = form();
        form.create_branch = false;
        form.base_ref = "release-1.4".into();
        form.sync_path();
        assert_eq!(form.path, "/home/u/worktrees/release-1.4");
    }

    #[test]
    fn a_remote_prefix_is_dropped_from_the_suggested_directory() {
        let mut form = form();
        form.refs = vec![RefEntry {
            name: "origin/feature/auth".into(),
            is_remote: true,
        }];
        form.create_branch = false;
        form.base_ref = "origin/feature/auth".into();
        form.sync_path();
        assert_eq!(form.path, "/home/u/worktrees/feature-auth");
    }

    #[test]
    fn a_branch_that_merely_contains_a_slash_keeps_its_first_segment() {
        let mut form = form();
        form.create_branch = false;
        form.base_ref = "feature/auth".into();
        form.sync_path();
        assert_eq!(
            form.path, "/home/u/worktrees/feature-auth",
            "`feature` is not a known remote"
        );
    }

    #[test]
    fn a_new_branch_becomes_dash_b_and_the_base_ref_follows_the_path() {
        let mut form = form();
        form.branch = "feature/auth".into();
        form.base_ref = "origin/main".into();
        form.sync_path();
        let add = form.to_add().expect("valid");
        assert_eq!(add.new_branch.as_deref(), Some("feature/auth"));
        assert_eq!(add.base_ref.as_deref(), Some("origin/main"));
        assert_eq!(add.path, Path::new("/home/u/worktrees/feature-auth"));
    }

    #[test]
    fn checking_out_an_existing_branch_passes_no_dash_b() {
        let mut form = form();
        form.create_branch = false;
        form.base_ref = "release-1.4".into();
        form.sync_path();
        let add = form.to_add().expect("valid");
        assert_eq!(add.new_branch, None);
        assert_eq!(add.base_ref.as_deref(), Some("release-1.4"));
    }

    #[test]
    fn an_empty_base_ref_lets_git_default_to_head() {
        let mut form = form();
        form.branch = "x".into();
        form.sync_path();
        let add = form.to_add().expect("valid");
        assert_eq!(add.base_ref, None);
    }

    #[test]
    fn values_are_trimmed_but_never_otherwise_rewritten() {
        let mut form = form();
        form.branch = "  feature/auth  ".into();
        form.base_ref = " origin/main ".into();
        form.path = "  /home/u/wt/a b  ".into();
        form.path_edited = true;
        let add = form.to_add().expect("valid");
        assert_eq!(add.new_branch.as_deref(), Some("feature/auth"));
        assert_eq!(add.base_ref.as_deref(), Some("origin/main"));
        assert_eq!(add.path, Path::new("/home/u/wt/a b"));
    }

    #[test]
    fn an_invalid_form_produces_no_operation_at_all() {
        let mut form = form();
        assert!(form.to_add().is_none());

        form.create_branch = false;
        assert_eq!(
            form.problem(),
            Some("Choose the branch or commit to check out.")
        );
        assert!(form.to_add().is_none());

        form.base_ref = "main".into();
        form.path.clear();
        form.path_edited = true;
        assert_eq!(form.problem(), Some("A worktree directory is required."));
        assert!(form.to_add().is_none());
    }
}
