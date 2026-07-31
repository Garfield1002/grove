//! The detached windows, and the forms behind them.
//!
//! The main window is a narrow sliver, so open-project, create-worktree,
//! safe-removal and settings each render as their own toplevel rather than as
//! an `egui::Window` inside it. Each is driven with
//! `Context::show_viewport_immediate` — see `ui::chrome` for why immediate and
//! not deferred. The callback runs inline, on this thread, so a window keeps
//! borrowing `GroveApp`'s fields exactly as the in-window bodies did, and the
//! worker plumbing is untouched: errors still land in `self.errors` and are
//! shown by the main window's strip.
//!
//! This is a seam in the file, not in the ownership. Every one of these needs
//! the worker to send to, the status line to write, and the rows to resolve a
//! project against, all at once — unlike `rows` and `selection`, which own
//! state and could be lifted out whole. Grouping the four `Detached` fields
//! into a struct of their own was considered and dropped: `Detached` already
//! keeps "at most one instance of each", and there is no rule *between* the
//! four for a type to hold.

use std::path::{Path, PathBuf};

use crate::ui::chrome::Detached;
use crate::ui::dialogs::create_worktree::CreateForm;
use crate::ui::dialogs::open_project::OpenProjectForm;
use crate::ui::dialogs::removal::{RemovalForm, Request};
use crate::ui::{self};
use crate::workers::{PickTarget, Task};

use super::GroveApp;

/// Stable viewport ids for the detached windows: one per kind, which is what
/// makes "at most one instance" a fact about the window system too.
const SETTINGS_VIEWPORT: &str = "grove-settings-window";
const CREATE_VIEWPORT: &str = "grove-create-worktree-window";
const REMOVAL_VIEWPORT: &str = "grove-removal-window";
const OPEN_PROJECT_VIEWPORT: &str = "grove-open-project-window";

impl GroveApp {
    /// Open the create-worktree window, or raise the one already open. Asking
    /// again for the same project keeps whatever has been typed.
    pub(super) fn open_create_dialog(&mut self, project_id: &str) {
        let Some(project) = self.rows.project(project_id) else {
            return;
        };
        let form = CreateForm::new(project);
        let id = project.id.clone();
        if self
            .create
            .open_or_focus(form, |open| open.project_id == id)
        {
            self.workers.send(Task::LoadBaseRefs { project_id: id });
        }
    }

    pub(super) fn open_removal_dialog(&mut self, project_id: &str, worktree_id: &str) {
        let Some(project) = self.rows.project(project_id) else {
            return;
        };
        let Some(worktree) = project.worktree(worktree_id) else {
            return;
        };
        self.selection.select(worktree_id.to_string());
        let gather = Task::GatherRemoval {
            worktree_id: worktree.id.clone(),
        };
        let form = RemovalForm {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            repository_path: project.repository_path.clone(),
            git_common_dir: project.git_common_dir.clone(),
            worktree_id: worktree.id.clone(),
            worktree_label: worktree.label(),
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            session: worktree.session.exists().then(|| worktree.session_name()),
            report: None,
            armed: None,
            force_worktree_offered: false,
            force_branch_offered: false,
            done: Vec::new(),
            refusals: Vec::new(),
        };
        // Re-opening the window on the same worktree must not throw away a
        // half-finished confirmation, or the risk report already gathered.
        let worktree_id = worktree_id.to_string();
        if self
            .removal
            .open_or_focus(form, |open| open.worktree_id == worktree_id)
        {
            self.workers.send(gather);
        }
    }

    /// Dispatch one confirmed removal operation. Exactly one, never a bundle.
    fn apply_removal(&mut self, request: Request) {
        let Some(form) = self.removal.get() else {
            return;
        };
        match request {
            Request::RemoveProject => {
                let id = form.project_id.clone();
                self.remove_project(&id);
            }
            Request::CloseSession => {
                if form.session.is_some() {
                    self.workers.send(Task::CloseSession {
                        project_id: form.project_id.clone(),
                        worktree_id: form.worktree_id.clone(),
                        idempotency_key: format!(
                            "gui-close-worktree-session-{}",
                            grove_core::nonce()
                        ),
                    });
                }
            }
            Request::RemoveWorktree { force } => self.workers.send(Task::RemoveWorktree {
                project_id: form.project_id.clone(),
                worktree_id: form.worktree_id.clone(),
                force,
                idempotency_key: format!("gui-remove-worktree-{}", grove_core::nonce()),
            }),
            Request::DeleteBranch { force } => {
                if let Some(branch) = form.branch.clone() {
                    self.workers.send(Task::DeleteBranch {
                        project_id: form.project_id.clone(),
                        branch,
                        force,
                        idempotency_key: format!("gui-delete-branch-{}", grove_core::nonce()),
                    });
                }
            }
        }
    }

    /// Settings never touch a file on the UI thread: writing `config.toml`,
    /// probing PATH and handing a path to the desktop all go to the worker.
    fn apply_settings_action(&mut self, action: ui::settings::Action) {
        let Some(form) = self.settings.get() else {
            return;
        };
        match action {
            ui::settings::Action::Save => {
                let edits = form.edits();
                if edits.is_empty() {
                    return;
                }
                self.status = Some("Saving config.toml…".to_string());
                self.workers.send(Task::SaveConfig(edits));
            }
            ui::settings::Action::DetectTerminal => self.workers.send(Task::DetectTerminal),
            ui::settings::Action::Probe(command) => self.workers.send(Task::ProbeTerminal(command)),
            ui::settings::Action::OpenConfigFile => self
                .workers
                .send(Task::OpenWithDesktop(self.paths.config_file())),
            #[cfg(feature = "agents")]
            ui::settings::Action::ClaudeHooks(op) => self.workers.send(Task::ClaudeHooks(op)),
            ui::settings::Action::BrowseWorktreeParent => {
                let start = pick_start(&form.default_parent, self.home.as_deref());
                self.workers.send(Task::PickDirectory {
                    target: PickTarget::WorktreeParent,
                    start,
                });
            }
        }
    }

    // -------------------------------------------------------- detached windows
    //
    // Each dialog is its own toplevel, driven with
    // `Context::show_viewport_immediate` — see `ui::chrome` for why immediate
    // and not deferred. The callback runs inline, on this thread, so a window
    // keeps borrowing this struct's fields exactly as the in-window
    // `egui::Window` bodies did, and the worker plumbing is untouched: errors
    // still land in `self.errors` and are shown by the main window's strip.

    pub(super) fn open_project_window(&mut self, ctx: &egui::Context) {
        use ui::dialogs::open_project::{self as open, Outcome};

        let id = egui::ViewportId::from_hash_of(OPEN_PROJECT_VIEWPORT);
        if self.open_project.take_focus_request() {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        let Some(form) = self.open_project.get_mut() else {
            return;
        };

        let mut outcome = Outcome::Idle;
        let mut close = false;
        ctx.show_viewport_immediate(
            id,
            ui::chrome::viewport(open::TITLE, open::SIZE, open::MIN_SIZE),
            |ctx, class| {
                let dialog =
                    ui::chrome::show(ctx, class, open::TITLE, |ui| open::body(ui, &mut *form));
                outcome = dialog.inner;
                close |= dialog.close;
            },
        );

        match outcome {
            Outcome::Idle => {}
            Outcome::Browse => self.workers.send(Task::PickDirectory {
                target: PickTarget::ProjectPath,
                start: pick_start(&form.path, self.home.as_deref()),
            }),
            Outcome::Cancelled => close = true,
            Outcome::Confirmed(path) => {
                self.status = Some(format!("Opening {path}…"));
                self.workers.send(Task::OpenProject {
                    path: PathBuf::from(path),
                    idempotency_key: format!("gui-project-open-{}", grove_core::nonce()),
                });
                close = true;
            }
        }
        if close {
            self.open_project.close();
        }
    }

    pub(super) fn create_window(&mut self, ctx: &egui::Context) {
        use ui::dialogs::create_worktree::{self as create, Outcome};

        let id = egui::ViewportId::from_hash_of(CREATE_VIEWPORT);
        if self.create.take_focus_request() {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        let Some(form) = self.create.get_mut() else {
            return;
        };

        let title = create::title(form);
        let mut outcome = Outcome::Idle;
        let mut close = false;
        ctx.show_viewport_immediate(
            id,
            ui::chrome::viewport(&title, create::SIZE, create::MIN_SIZE),
            |ctx, class| {
                let dialog =
                    ui::chrome::show(ctx, class, &title, |ui| create::body(ui, &mut *form));
                outcome = dialog.inner;
                close |= dialog.close;
            },
        );

        match outcome {
            Outcome::Idle => {}
            Outcome::Browse => {
                let start = pick_start(&form.path, Some(&form.default_parent));
                self.workers.send(Task::PickDirectory {
                    target: PickTarget::WorktreePath,
                    start,
                });
            }
            Outcome::Cancelled => close = true,
            Outcome::Create(add) => {
                self.workers.send(Task::CreateWorktree {
                    project_id: form.project_id.clone(),
                    add,
                    open_after: form.open_after,
                    idempotency_key: format!("gui-create-worktree-{}", grove_core::nonce()),
                });
                self.status = Some("Creating the worktree…".to_string());
                close = true;
            }
        }
        if close {
            self.create.close();
        }
    }

    pub(super) fn removal_window(&mut self, ctx: &egui::Context) {
        use ui::dialogs::removal;

        let id = egui::ViewportId::from_hash_of(REMOVAL_VIEWPORT);
        if self.removal.take_focus_request() {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        let Some(form) = self.removal.get_mut() else {
            return;
        };

        let title = removal::title(form);
        let mut request = None;
        let mut close = false;
        ctx.show_viewport_immediate(
            id,
            ui::chrome::viewport(&title, removal::SIZE, removal::MIN_SIZE),
            |ctx, class| {
                let dialog =
                    ui::chrome::show(ctx, class, &title, |ui| removal::body(ui, &mut *form));
                request = dialog.inner;
                close |= dialog.close;
            },
        );

        // Closing first would drop the form `apply_removal` reads. The dialog
        // stays open after an operation either way: the four are separate
        // decisions and the results have to stay readable.
        if let Some(request) = request {
            self.apply_removal(request);
        }
        if close {
            self.removal.close();
        }
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        let id = egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT);
        if self.settings.take_focus_request() {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        let Some(form) = self.settings.get_mut() else {
            return;
        };
        let (paths, home) = (&self.paths, self.home.as_deref());
        #[cfg(feature = "agents")]
        let hooks = self.claude_hooks.as_ref();

        let mut action = None;
        let mut close = false;
        ctx.show_viewport_immediate(
            id,
            ui::chrome::viewport("Settings", ui::settings::SIZE, ui::settings::MIN_SIZE),
            |ctx, class| {
                let dialog = ui::chrome::show(ctx, class, "Settings", |ui| {
                    ui::settings::body(
                        ui,
                        &mut *form,
                        paths,
                        home,
                        #[cfg(feature = "agents")]
                        hooks,
                    )
                });
                action = dialog.inner;
                close |= dialog.close;
            },
        );

        if let Some(action) = action {
            self.apply_settings_action(action);
        }
        if close {
            self.settings.close();
        }
    }
}

/// Where the directory picker should open: what the user has typed so far,
/// else a sensible fallback. Purely textual — deciding whether the path
/// exists is the worker's job, not the UI thread's.
pub(super) fn pick_start(typed: &str, fallback: Option<&Path>) -> Option<PathBuf> {
    let typed = typed.trim();
    if typed.is_empty() {
        return fallback.map(Path::to_path_buf);
    }
    Some(PathBuf::from(typed))
}

/// Put a directory the user picked into the field that asked for it.
///
/// The picked path only ever *fills in* a text field: every path stays
/// editable by hand, which is also the whole story when Grove is built
/// without the `native-file-picker` feature.
pub(super) fn apply_picked(
    target: PickTarget,
    path: PathBuf,
    open_project: &mut Detached<OpenProjectForm>,
    create: &mut Detached<CreateForm>,
    settings: &mut Detached<ui::settings::Form>,
) {
    let text = path.display().to_string();
    match target {
        PickTarget::ProjectPath => {
            if let Some(form) = open_project.get_mut() {
                form.path = text;
            }
        }
        PickTarget::WorktreePath => {
            if let Some(form) = create.get_mut() {
                form.path = text;
                // The user chose this directory; stop re-deriving it from the
                // branch name behind their back.
                form.path_edited = true;
            }
        }
        PickTarget::WorktreeParent => {
            if let Some(form) = settings.get_mut() {
                form.default_parent = text;
                form.note = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::WorktreeEntry;
    use grove_core::model::{Project, Worktree};

    fn project(id: &str, name: &str, branches: &[&str]) -> Project {
        let git_common_dir = PathBuf::from(format!("/home/u/{name}/.git"));
        Project {
            id: id.to_string(),
            name: name.to_string(),
            repository_path: PathBuf::from(format!("/home/u/{name}")),
            git_common_dir: git_common_dir.clone(),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
            worktrees: branches
                .iter()
                .map(|branch| {
                    Worktree::from_entry(
                        &WorktreeEntry {
                            path: PathBuf::from(format!("/home/u/wt/{name}-{branch}")),
                            branch: Some((*branch).to_string()),
                            ..WorktreeEntry::default()
                        },
                        id,
                        &git_common_dir,
                        false,
                    )
                })
                .collect(),
            unavailable: None,
        }
    }

    #[test]
    fn the_picker_starts_where_the_user_was_already_pointing() {
        assert_eq!(
            pick_start("  /home/u/wt  ", Some(Path::new("/home/u"))),
            Some(PathBuf::from("/home/u/wt"))
        );
        assert_eq!(
            pick_start("   ", Some(Path::new("/home/u"))),
            Some(PathBuf::from("/home/u")),
            "an empty field falls back"
        );
        assert_eq!(pick_start("", None), None);
    }

    #[test]
    fn a_picked_directory_fills_in_the_field_that_asked_for_it() {
        let mut open_project = Detached::default();
        open_project.open(OpenProjectForm::empty());
        let mut create = Detached::default();
        create.open(CreateForm::new(&project("p1", "acme", &[])));
        let mut settings = Detached::default();
        settings.open(ui::settings::Form::default());

        apply_picked(
            PickTarget::ProjectPath,
            PathBuf::from("/home/u/acme"),
            &mut open_project,
            &mut create,
            &mut settings,
        );
        assert_eq!(
            open_project.get().map(|f| f.path.as_str()),
            Some("/home/u/acme")
        );

        apply_picked(
            PickTarget::WorktreePath,
            PathBuf::from("/home/u/wt/feature"),
            &mut open_project,
            &mut create,
            &mut settings,
        );
        let form = create.get().expect("form");
        assert_eq!(form.path, "/home/u/wt/feature");
        assert!(
            form.path_edited,
            "a chosen directory must not be re-derived from the branch name"
        );

        apply_picked(
            PickTarget::WorktreeParent,
            PathBuf::from("/home/u/trees"),
            &mut open_project,
            &mut create,
            &mut settings,
        );
        assert_eq!(
            settings.get().expect("settings").default_parent,
            "/home/u/trees"
        );
    }

    /// The dialog may have been closed while the portal was open; the answer
    /// then has nowhere to go and must not panic.
    #[test]
    fn a_picked_directory_for_a_closed_dialog_is_dropped() {
        let mut open_project: Detached<OpenProjectForm> = Detached::default();
        let mut create: Detached<CreateForm> = Detached::default();
        let mut settings: Detached<ui::settings::Form> = Detached::default();
        for target in [
            PickTarget::ProjectPath,
            PickTarget::WorktreePath,
            PickTarget::WorktreeParent,
        ] {
            apply_picked(
                target,
                PathBuf::from("/home/u"),
                &mut open_project,
                &mut create,
                &mut settings,
            );
        }
        assert!(!open_project.is_open() && !create.is_open() && !settings.is_open());
    }
}
