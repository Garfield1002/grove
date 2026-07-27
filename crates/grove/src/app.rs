//! The eframe application: state held for the UI, channel plumbing to the
//! worker, and the narrow vertical layout from direction 1c.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use grove_core::config::Config;
use grove_core::git::StatusSummary;
use grove_core::model::{Project, SessionPresence};
use grove_core::state::{ProjectRecord, State};
use grove_core::status::{SessionReport, SessionStatus};
use grove_core::workflow::Activation;
use grove_core::{Paths, state};

use crate::status_watch::{Control, StatusWatch, WorktreeLabel};
use crate::ui::chrome::Detached;
use crate::ui::dialogs::create_worktree::CreateForm;
use crate::ui::dialogs::removal::{RemovalForm, Request};
use crate::ui::{self, Action, theme};
use crate::workers::{ErrorReport, Message, PickTarget, Task, Workers};

pub struct GroveApp {
    paths: Paths,
    home: Option<PathBuf>,
    workers: Workers,
    /// The status poller and the `grove notify` listener (Milestone 4).
    watch: StatusWatch,
    messages: Receiver<Message>,
    /// Last polled status per worktree id, kept so a refreshed worktree list
    /// shows its status immediately instead of blank until the next poll.
    statuses: HashMap<String, SessionReport>,

    config: Option<Config>,
    state: State,
    projects: Vec<Project>,

    selected: Option<String>,
    filter: String,
    status: Option<String>,
    errors: Vec<ErrorReport>,

    open_project_path: Option<String>,
    /// A worktree Grove just created, selected as soon as a refresh lists it.
    pending_selection: Option<(String, PathBuf)>,
    /// The three detached windows. The main window is a narrow sliver, so
    /// these render as their own toplevels (`ui::chrome`), one of each.
    create: Detached<CreateForm>,
    removal: Detached<RemovalForm>,
    settings: Detached<ui::settings::Form>,
}

/// Stable viewport ids for the detached windows: one per kind, which is what
/// makes "at most one instance" a fact about the window system too.
const SETTINGS_VIEWPORT: &str = "grove-settings-window";
const CREATE_VIEWPORT: &str = "grove-create-worktree-window";
const REMOVAL_VIEWPORT: &str = "grove-removal-window";

impl GroveApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths) -> Self {
        theme::apply(&cc.egui_ctx);
        let (workers, messages) = Workers::start(paths.clone(), cc.egui_ctx.clone());
        let watch = StatusWatch::start(&paths, workers.message_sender(), cc.egui_ctx.clone());

        // Reading two small TOML files on the UI thread is fine; running a
        // subprocess here would not be, which is why config loading (it may
        // probe PATH and write a file) and all git/tmux work go to the worker.
        let mut errors = Vec::new();
        let loaded = state::load(&paths.state_file()).unwrap_or_else(|e| {
            errors.push(ErrorReport::new("could not read state.toml", &e));
            State::default()
        });

        let projects: Vec<Project> = loaded
            .projects
            .iter()
            .map(|record| Project {
                id: record.id.clone(),
                name: record.name.clone(),
                repository_path: record.repository_path.clone(),
                git_common_dir: record.git_common_dir.clone(),
                default_worktree_path: record.default_worktree_path.clone(),
                is_expanded: record.is_expanded,
                worktrees: Vec::new(),
            })
            .collect();

        workers.send(Task::LoadConfig);
        for project in &projects {
            workers.send(Task::RefreshProject {
                project_id: project.id.clone(),
                repository_path: project.repository_path.clone(),
                git_common_dir: project.git_common_dir.clone(),
            });
        }

        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            paths,
            workers,
            watch,
            messages,
            statuses: HashMap::new(),
            config: None,
            state: loaded,
            projects,
            selected: None,
            filter: String::new(),
            status: None,
            errors,
            open_project_path: None,
            pending_selection: None,
            create: Detached::default(),
            removal: Detached::default(),
            settings: Detached::default(),
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Message::ConfigLoaded { loaded } => {
                    if loaded.created {
                        self.status = Some(format!(
                            "Detected a terminal and wrote {}",
                            self.paths.config_file().display()
                        ));
                    }
                    if let Some(form) = self.settings.get_mut() {
                        form.reloaded(&loaded.config);
                    }
                    self.watch
                        .send(Control::Reconfigure(Box::new(loaded.config.status.clone())));
                    self.config = Some(loaded.config);
                }
                Message::ConfigSaved { path } => {
                    self.status = Some(format!("Saved {}", path.display()));
                    if let Some(form) = self.settings.get_mut() {
                        form.note = Some("Saved. Your comments are untouched.".to_string());
                    }
                }
                Message::TerminalDetected { template } => {
                    if let Some(form) = self.settings.get_mut() {
                        form.terminal_command = template.clone();
                        form.note = None;
                        self.workers.send(Task::ProbeTerminal(template));
                    }
                }
                Message::TerminalProbed {
                    command,
                    program,
                    found,
                } => {
                    if let Some(form) = self.settings.get_mut() {
                        form.probe = Some(ui::settings::Probe {
                            command,
                            program,
                            found,
                        });
                    }
                }
                Message::DirectoryPicked { target, path } => apply_picked(
                    target,
                    path,
                    &mut self.open_project_path,
                    &mut self.create,
                    &mut self.settings,
                ),
                Message::ProjectOpened(project) => self.add_project(*project),
                Message::WorktreesRefreshed {
                    project_id,
                    worktrees,
                } => {
                    if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
                        project.worktrees = worktrees;
                        // Select a worktree Grove has just created, once the
                        // refreshed list actually contains it.
                        if let Some((pending_project, path)) = &self.pending_selection
                            && pending_project == &project_id
                            && let Some(worktree) =
                                project.worktrees.iter().find(|w| &w.path == path)
                        {
                            self.selected = Some(worktree.id.clone());
                            self.pending_selection = None;
                        }
                    }
                    // A fresh list arrives with no statuses on it; re-stamp
                    // the last poll rather than blanking every pill.
                    self.apply_session_statuses();
                    self.describe_worktrees();
                }
                Message::StatusesRefreshed {
                    project_id,
                    statuses,
                } => self.apply_statuses(&project_id, &statuses),
                Message::SessionsRefreshed(presence) => self.apply_presence(&presence),
                Message::AgentStarted { worktree_id, unit } => {
                    self.selected = Some(worktree_id);
                    self.status = Some(match unit {
                        Some(unit) => format!("Started the agent in {unit}"),
                        None => "Started the agent".to_string(),
                    });
                    self.watch.send(Control::PollNow);
                }
                Message::StatusPolled(statuses) => {
                    self.statuses = statuses;
                    self.apply_session_statuses();
                }
                Message::Notified {
                    worktree_id,
                    state,
                    message,
                } => self.apply_notification(&worktree_id, state, message),
                Message::BaseRefsLoaded {
                    project_id,
                    refs,
                    current,
                } => {
                    if let Some(form) = self.create.get_mut()
                        && form.project_id == project_id
                    {
                        form.refs = refs;
                        form.refs_loaded = true;
                        if form.base_ref.trim().is_empty()
                            && let Some(current) = current
                        {
                            form.base_ref = current;
                            form.sync_path();
                        }
                    }
                }
                Message::WorktreeCreated { project_id, path } => {
                    self.status = Some(format!("Created {}", path.display()));
                    self.pending_selection = Some((project_id, path));
                }
                Message::RemovalGathered {
                    project_id,
                    worktree_id,
                    report,
                } => {
                    if let Some(form) = self.removal.get_mut()
                        && form.project_id == project_id
                        && form.worktree_id == worktree_id
                    {
                        form.report = Some(*report);
                    }
                }
                Message::RemovalDone {
                    project_id,
                    operation,
                    detail,
                } => {
                    self.status = Some(detail.clone());
                    if let Some(form) = self.removal.get_mut()
                        && form.project_id == project_id
                    {
                        // The session or the worktree is gone; its latch
                        // would otherwise outlive it until the next poll.
                        self.watch.forget(&form.worktree_id);
                        self.statuses.remove(&form.worktree_id);
                        form.note_done(operation, detail);
                    }
                }
                Message::RemovalFailed {
                    project_id,
                    operation,
                    report,
                } => {
                    let summary = report.summary.clone();
                    self.status = Some(format!(
                        "Did not {} — see the error below.",
                        operation.label()
                    ));
                    self.errors.push(report);
                    if let Some(form) = self.removal.get_mut()
                        && form.project_id == project_id
                    {
                        form.note_refusal(operation, summary);
                    }
                }
                Message::Activated {
                    worktree_id,
                    activation,
                } => {
                    self.selected = Some(worktree_id);
                    self.status = Some(match activation {
                        Activation::SwitchedClient {
                            session,
                            client_tty,
                        } => {
                            format!("Switched {client_tty} to {session}")
                        }
                        Activation::LaunchedTerminal { session, .. } => {
                            format!("Launched a terminal on {session}")
                        }
                    });
                }
                Message::Failed(report) => {
                    // A failed save must release the Save button, or the pane
                    // would sit on "Saving…" for ever.
                    if let Some(form) = self.settings.get_mut() {
                        form.saving = false;
                    }
                    self.errors.push(report);
                }
            }
        }
    }

    fn add_project(&mut self, project: Project) {
        self.state.upsert(ProjectRecord {
            id: project.id.clone(),
            name: project.name.clone(),
            repository_path: project.repository_path.clone(),
            git_common_dir: project.git_common_dir.clone(),
            default_worktree_path: project.default_worktree_path.clone(),
            is_expanded: project.is_expanded,
        });
        match self.projects.iter_mut().find(|p| p.id == project.id) {
            Some(existing) => {
                self.status = Some(format!("{} is already open", project.name));
                *existing = project;
            }
            None => {
                self.status = Some(format!(
                    "Registered {} ({} worktrees)",
                    project.name,
                    project.worktrees.len()
                ));
                self.projects.push(project);
            }
        }
        self.save_state();
    }

    /// Stamp fresh git statuses onto a project's rows. A worktree with no
    /// reading keeps whatever it had: never blanked, never invented.
    fn apply_statuses(&mut self, project_id: &str, statuses: &HashMap<String, StatusSummary>) {
        if let Some(project) = self.projects.iter_mut().find(|p| p.id == project_id) {
            grove_core::workflow::apply_statuses(&mut project.worktrees, statuses);
        }
    }

    fn apply_presence(&mut self, presence: &HashMap<String, SessionPresence>) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_presence(&mut project.worktrees, presence);
        }
        // Presence just changed, so a row that lost its session must lose its
        // status with it rather than waiting for the next poll.
        self.apply_session_statuses();
    }

    /// Stamp the last polled statuses onto every row.
    fn apply_session_statuses(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_status(&mut project.worktrees, &self.statuses);
        }
    }

    /// An explicit `grove notify` report: show its message straight away.
    ///
    /// The status itself is left to the poller, which re-reads tmux a moment
    /// later — except for attention, which is latched in the engine and would
    /// otherwise not show until that poll lands.
    fn apply_notification(
        &mut self,
        worktree_id: &str,
        state: SessionStatus,
        message: Option<String>,
    ) {
        if state == SessionStatus::Attention {
            // Keep any resource figures the last poll produced; only the
            // status is being overridden here.
            let report = self.statuses.entry(worktree_id.to_string()).or_default();
            report.status = SessionStatus::Attention;
        }
        for project in &mut self.projects {
            if let Some(worktree) = project.worktrees.iter_mut().find(|w| w.id == worktree_id) {
                worktree.status_message = message.clone();
                if state == SessionStatus::Attention && worktree.session.exists() {
                    worktree.status = Some(SessionStatus::Attention);
                }
            }
        }
    }

    /// Opening a session is what clears its attention: the in-memory latch
    /// here, and the durable tmux option on the worker.
    ///
    /// Both halves must go, or the next poll would read the option back and
    /// re-raise attention the user has just answered.
    fn clear_attention(&mut self, worktree_id: &str, session: &str) {
        if !self.watch.opened(worktree_id) {
            return;
        }
        self.statuses.remove(worktree_id);
        for project in &mut self.projects {
            if let Some(worktree) = project.worktrees.iter_mut().find(|w| w.id == worktree_id) {
                worktree.status = None;
                worktree.status_message = None;
            }
        }
        self.workers.send(Task::ClearAttention {
            session: session.to_string(),
        });
    }

    /// Tell the poller what to call each worktree in a desktop notification.
    fn describe_worktrees(&self) {
        let labels: HashMap<String, WorktreeLabel> = self
            .projects
            .iter()
            .flat_map(|project| {
                project.worktrees.iter().map(|worktree| {
                    (
                        worktree.id.clone(),
                        WorktreeLabel {
                            project: project.name.clone(),
                            worktree: worktree.label(),
                        },
                    )
                })
            })
            .collect();
        self.watch.send(Control::Describe(labels));
    }

    fn save_state(&self) {
        self.workers
            .send(Task::SaveState(Box::new(self.state.clone())));
    }

    fn refresh_all(&mut self) {
        for project in &self.projects {
            self.workers.send(Task::RefreshProject {
                project_id: project.id.clone(),
                repository_path: project.repository_path.clone(),
                git_common_dir: project.git_common_dir.clone(),
            });
        }
        self.workers.send(Task::RefreshSessions);
        self.status = Some("Refreshing from git and tmux…".to_string());
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::ToggleProject(id) => {
                if let Some(project) = self.projects.iter_mut().find(|p| p.id == id) {
                    project.is_expanded = !project.is_expanded;
                    let expanded = project.is_expanded;
                    if let Some(record) = self.state.projects.iter_mut().find(|p| p.id == id) {
                        record.is_expanded = expanded;
                    }
                    self.save_state();
                }
            }
            Action::RefreshProject(id) => {
                if let Some(project) = self.projects.iter().find(|p| p.id == id) {
                    self.workers.send(Task::RefreshProject {
                        project_id: project.id.clone(),
                        repository_path: project.repository_path.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                    });
                }
            }
            // Removing a project touches Grove's index only: no worktree,
            // branch, repository or tmux session is affected.
            Action::RemoveProject(id) => self.remove_project(&id),
            Action::CreateWorktree(id) => self.open_create_dialog(&id),
            Action::ActivateWorktree {
                project_id,
                worktree_id,
            } => {
                if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
                    && let Some(worktree) = project.worktree(&worktree_id)
                {
                    let session = worktree.session_name();
                    self.workers.send(Task::Activate {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                    });
                    self.clear_attention(&worktree_id, &session);
                    self.selected = Some(worktree_id);
                }
            }
            Action::StartAgent {
                project_id,
                worktree_id,
            } => {
                if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
                    && let Some(worktree) = project.worktree(&worktree_id)
                {
                    self.workers.send(Task::StartAgent {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                    });
                    self.selected = Some(worktree_id);
                }
            }
            Action::SelectWorktree { worktree_id, .. } => self.selected = Some(worktree_id),
            Action::OpenInNewTerminal {
                project_id,
                worktree_id,
            } => {
                if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
                    && let Some(worktree) = project.worktree(&worktree_id)
                {
                    self.selected = Some(worktree_id);
                    self.workers.send(Task::OpenInNewTerminal {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                    });
                }
            }
            Action::RemoveWorktree {
                project_id,
                worktree_id,
            } => self.open_removal_dialog(&project_id, &worktree_id),
        }
    }

    /// Remove a project from Grove's index. Metadata only — this must never
    /// be accompanied by a git or tmux operation (ARCHITECTURE.md §8.1).
    fn remove_project(&mut self, id: &str) {
        self.projects.retain(|p| p.id != id);
        self.state.remove(id);
        self.save_state();
        if self.removal.get().is_some_and(|f| f.project_id == id) {
            self.removal.close();
        }
        self.status = Some("Removed from Grove. The repository is untouched.".to_string());
    }

    /// Open the create-worktree window, or raise the one already open. Asking
    /// again for the same project keeps whatever has been typed.
    fn open_create_dialog(&mut self, project_id: &str) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id) else {
            return;
        };
        let form = CreateForm::new(project);
        let (id, repository_path) = (project.id.clone(), project.repository_path.clone());
        if self
            .create
            .open_or_focus(form, |open| open.project_id == id)
        {
            self.workers.send(Task::LoadBaseRefs {
                project_id: id,
                repository_path,
            });
        }
    }

    fn open_removal_dialog(&mut self, project_id: &str, worktree_id: &str) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id) else {
            return;
        };
        let Some(worktree) = project.worktree(worktree_id) else {
            return;
        };
        self.selected = Some(worktree_id.to_string());
        let gather = Task::GatherRemoval {
            project_id: project.id.clone(),
            worktree: Box::new(worktree.clone()),
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
                if let Some(session) = form.session.clone() {
                    self.workers.send(Task::CloseSession {
                        project_id: form.project_id.clone(),
                        session,
                    });
                }
            }
            Request::RemoveWorktree { force } => self.workers.send(Task::RemoveWorktree {
                project_id: form.project_id.clone(),
                repository_path: form.repository_path.clone(),
                git_common_dir: form.git_common_dir.clone(),
                worktree_path: form.worktree_path.clone(),
                force,
            }),
            Request::DeleteBranch { force } => {
                if let Some(branch) = form.branch.clone() {
                    self.workers.send(Task::DeleteBranch {
                        project_id: form.project_id.clone(),
                        repository_path: form.repository_path.clone(),
                        git_common_dir: form.git_common_dir.clone(),
                        branch,
                        force,
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

    fn create_window(&mut self, ctx: &egui::Context) {
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
                    project_name: form.project_name.clone(),
                    repository_path: form.repository_path.clone(),
                    git_common_dir: form.git_common_dir.clone(),
                    add,
                    open_after: form.open_after,
                });
                self.status = Some("Creating the worktree…".to_string());
                close = true;
            }
        }
        if close {
            self.create.close();
        }
    }

    fn removal_window(&mut self, ctx: &egui::Context) {
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

    fn settings_window(&mut self, ctx: &egui::Context) {
        let id = egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT);
        if self.settings.take_focus_request() {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
        }
        let Some(form) = self.settings.get_mut() else {
            return;
        };
        let (paths, home) = (&self.paths, self.home.as_deref());

        let mut action = None;
        let mut close = false;
        ctx.show_viewport_immediate(
            id,
            ui::chrome::viewport("Settings", ui::settings::SIZE, ui::settings::MIN_SIZE),
            |ctx, class| {
                let dialog = ui::chrome::show(ctx, class, "Settings", |ui| {
                    ui::settings::body(ui, &mut *form, paths, home)
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

    /// The rows the keyboard walks: every visible worktree, in list order.
    fn visible_rows(&self) -> Vec<(String, String)> {
        visible_rows(&self.projects, &self.filter)
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.visible_rows();
        if let Some(next) = next_selection(&rows, self.selected.as_deref(), delta) {
            self.selected = Some(next);
        }
    }

    fn selected_row(&self) -> Option<(String, String)> {
        let selected = self.selected.as_ref()?;
        self.visible_rows()
            .into_iter()
            .find(|(_, worktree_id)| worktree_id == selected)
    }

    /// The project a keyboard shortcut acts on: the selected row's project,
    /// else the only project, else nothing.
    fn context_project(&self) -> Option<String> {
        if let Some((project_id, _)) = self.selected_row() {
            return Some(project_id);
        }
        match self.projects.as_slice() {
            [only] => Some(only.id.clone()),
            _ => None,
        }
    }

    /// Keyboard navigation for the main window (DESIGN.md §16).
    ///
    /// Settings and create-worktree are their own toplevels now, so the list
    /// behind them stays navigable: neither can act on a row, and having the
    /// sliver go deaf because a window is open elsewhere on the desktop would
    /// be baffling. The removal window still deafens it — it is a destructive
    /// confirmation about the selected row, and Delete must not open a second
    /// one behind it. The in-window open-project dialog keeps its guard for
    /// the same reason it always had it: it owns the keyboard.
    fn keyboard(&mut self, ctx: &egui::Context) {
        // The window has no decorations and therefore no close button, so
        // Grove has to offer the shortcut itself. Ctrl+Q quits from any of
        // Grove's windows; Ctrl+W on the main window closes it, which for the
        // only remaining window means quitting too.
        let close = ctx.input(|i| {
            i.modifiers.command && (i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::W))
        });
        if close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if self.removal.is_open() || self.open_project_path.is_some() {
            return;
        }
        let (down, up, enter, remove, new, refresh) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Delete),
                i.modifiers.command && i.key_pressed(egui::Key::N),
                i.modifiers.command && i.key_pressed(egui::Key::R),
            )
        });

        if down {
            self.move_selection(1);
        }
        if up {
            self.move_selection(-1);
        }
        if enter && let Some((project_id, worktree_id)) = self.selected_row() {
            self.apply_action(Action::ActivateWorktree {
                project_id,
                worktree_id,
            });
        }
        if remove && let Some((project_id, worktree_id)) = self.selected_row() {
            self.open_removal_dialog(&project_id, &worktree_id);
        }
        if new && let Some(project_id) = self.context_project() {
            self.open_create_dialog(&project_id);
        }
        if refresh {
            match self.context_project() {
                Some(project_id) => self.apply_action(Action::RefreshProject(project_id)),
                None => self.refresh_all(),
            }
        }
    }

    /// The header bar: the app title, the Restore placeholder, the open-project
    /// action, and the filter field — the mockup's top region.
    ///
    /// The bar doubles as the window's drag handle. The window is undecorated
    /// (see `main`), so without this there would be no way to move it. The
    /// drag region is interacted with *first*, which in egui puts it below the
    /// buttons and the text field: a click on a control is never a drag.
    fn header(&mut self, ui: &mut egui::Ui) {
        let bar = egui::Rect::from_min_size(
            ui.cursor().min,
            egui::vec2(ui.available_width(), theme::ICON_BUTTON),
        );
        ui::chrome::drag_region(ui, bar, "grove-titlebar");

        ui.horizontal(|ui| {
            ui.set_min_height(theme::ICON_BUTTON);
            ui.label(
                egui::RichText::new("Grove")
                    .size(theme::FONT_TITLE)
                    .strong()
                    .color(theme::TEXT_STRONG),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui::icons::button(ui, true, ui::icons::plus)
                    .on_hover_text("Open a project")
                    .clicked()
                {
                    self.open_project_path = Some(String::new());
                }
                ui::icons::chip(ui, "Restore", false, ui::icons::refresh).on_hover_text(
                    "Milestone 3 — restore and reconcile saved state.\n\
                     Ctrl+R re-reads git and tmux in the meantime.",
                );
            });
        });

        ui.add_space(8.0);
        self.filter_field(ui);
    }

    /// The mockup's filter field: a rounded, subtly bordered pill with a
    /// magnifier and a placeholder.
    fn filter_field(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(theme::FIELD)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .corner_radius(egui::CornerRadius::same(theme::CHIP_RADIUS))
            .inner_margin(egui::Margin::symmetric(10, 0))
            .show(ui, |ui| {
                ui.set_height(theme::FIELD_HEIGHT);
                ui.horizontal_centered(|ui| {
                    let (glass, _) =
                        ui.allocate_exact_size(egui::Vec2::splat(13.0), egui::Sense::hover());
                    ui::icons::magnifier(ui.painter(), glass, theme::TEXT_FAINT);
                    ui.add_space(2.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .frame(false)
                            .font(egui::FontId::proportional(theme::FONT_BODY))
                            .hint_text(theme::label(
                                "Filter worktrees…",
                                theme::FONT_BODY,
                                theme::TEXT_FAINT,
                            ))
                            .desired_width(f32::INFINITY),
                    );
                });
            });
    }

    /// The footer: "Open Project" on the left, the settings affordance on the
    /// right, and the most recent status line underneath. Refresh stays on
    /// Ctrl+R and the context menus — a second circular arrow down here just
    /// read as a duplicate of the header's Restore.
    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_min_height(theme::ICON_BUTTON);
            if open_project_entry(ui).clicked() {
                self.open_project_path = Some(String::new());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui::icons::button(ui, true, ui::icons::gear)
                    .on_hover_text("Settings")
                    .clicked()
                {
                    // Settings is its own window: the gear raises the one
                    // already open rather than toggling it shut, so unsaved
                    // edits survive a stray click.
                    if self.settings.is_open() {
                        self.settings.request_focus();
                    } else {
                        let form = ui::settings::Form::new(self.config.as_ref());
                        // The valid/invalid indicator needs a PATH probe,
                        // which is filesystem work and therefore the worker's.
                        self.workers
                            .send(Task::ProbeTerminal(form.terminal_command.clone()));
                        self.settings.open(form);
                    }
                }
            });
        });
        if let Some(status) = &self.status {
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(theme::label(status, theme::FONT_SMALL, theme::TEXT_FAINT))
                    .truncate(),
            );
        }
    }
}

/// Where the directory picker should open: what the user has typed so far,
/// else a sensible fallback. Purely textual — deciding whether the path
/// exists is the worker's job, not the UI thread's.
fn pick_start(typed: &str, fallback: Option<&Path>) -> Option<PathBuf> {
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
fn apply_picked(
    target: PickTarget,
    path: PathBuf,
    open_project: &mut Option<String>,
    create: &mut Detached<CreateForm>,
    settings: &mut Detached<ui::settings::Form>,
) {
    let text = path.display().to_string();
    match target {
        PickTarget::ProjectPath => {
            if let Some(field) = open_project {
                *field = text;
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

/// The footer's "Open Project" entry: a folder icon and a label that hover
/// together, as one target.
fn open_project_entry(ui: &mut egui::Ui) -> egui::Response {
    let font = egui::FontId::proportional(theme::FONT_BODY);
    let galley = ui
        .painter()
        .layout_no_wrap("Open Project".to_owned(), font, theme::TEXT_DIM);
    let width = 13.0 + 7.0 + galley.size().x;
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, theme::ICON_BUTTON), egui::Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);

    if ui.is_rect_visible(rect) {
        let tint = if response.hovered() {
            theme::TEXT_STRONG
        } else {
            theme::TEXT_DIM
        };
        let painter = ui.painter();
        let icon = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 6.5, rect.center().y),
            egui::Vec2::splat(13.0),
        );
        ui::icons::folder(painter, icon, tint);
        painter.galley(
            egui::pos2(icon.right() + 7.0, rect.center().y - galley.size().y / 2.0),
            galley,
            tint,
        );
    }
    response
}

impl eframe::App for GroveApp {
    /// Every viewport is cleared to the theme's window body. eframe's default
    /// is a translucent grey, which a freshly mapped dialog would show for the
    /// frame before its panels paint — a pale flash on an otherwise dark app.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();

        // First thing in the frame: the window is undecorated, so its resize
        // edges are Grove's to provide, and registering them here puts every
        // other widget on top of them (`ui::window_edge`).
        ui::window_edge::show(ctx);

        if let Some(path) = &mut self.open_project_path {
            match ui::dialogs::open_project(ctx, path) {
                ui::dialogs::OpenProject::Idle => {}
                ui::dialogs::OpenProject::Browse => self.workers.send(Task::PickDirectory {
                    target: PickTarget::ProjectPath,
                    start: pick_start(path, self.home.as_deref()),
                }),
                ui::dialogs::OpenProject::Cancelled => self.open_project_path = None,
                ui::dialogs::OpenProject::Confirmed(path) => {
                    self.open_project_path = None;
                    self.status = Some(format!("Opening {path}…"));
                    self.workers.send(Task::OpenProject(PathBuf::from(path)));
                }
            }
        }

        self.create_window(ctx);
        self.removal_window(ctx);
        self.settings_window(ctx);

        let header = egui::TopBottomPanel::top("grove-header")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_SUNKEN)
                    .inner_margin(egui::Margin::symmetric(theme::PANEL_MARGIN_X, 11)),
            )
            .show(ctx, |ui| self.header(ui));
        hairline(
            ctx,
            header.response.rect.left_bottom(),
            ctx.screen_rect().width(),
        );

        let footer = egui::TopBottomPanel::bottom("grove-footer")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_FOOTER)
                    .inner_margin(egui::Margin::symmetric(theme::PANEL_MARGIN_X, 10)),
            )
            .show(ctx, |ui| self.footer(ui));
        hairline(
            ctx,
            footer.response.rect.left_top(),
            ctx.screen_rect().width(),
        );

        if !self.errors.is_empty() {
            let mut dismissed = false;
            egui::TopBottomPanel::bottom("grove-errors")
                .frame(egui::Frame::new())
                .show(ctx, |ui| {
                    dismissed = ui::dialogs::errors(ui, &self.errors);
                });
            if dismissed {
                self.errors.clear();
            }
        }

        let mut action = None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(theme::LIST_MARGIN_X, 6)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    action = ui::project_list::show(
                        ui,
                        &self.projects,
                        self.selected.as_deref(),
                        &self.filter,
                        self.home.as_deref(),
                    );
                });
            });
        if let Some(action) = action {
            self.apply_action(action);
        }
        self.keyboard(ctx);
    }
}

/// The mockup's `rgba(255,255,255,.06)` divider between two regions.
///
/// It goes in its own `Background`-order layer: registered after the panels,
/// so it paints over their fills, but still under any window, so a dialog is
/// never crossed by a stray line.
fn hairline(ctx: &egui::Context, at: egui::Pos2, width: f32) {
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("grove-hairlines"),
    ))
    .hline(
        at.x..=(at.x + width),
        at.y - 0.5,
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
}

/// The worktree rows the user can see, as (project id, worktree id) pairs, in
/// list order. Collapsed projects and filtered-out rows are not walkable.
fn visible_rows(projects: &[Project], filter: &str) -> Vec<(String, String)> {
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows = Vec::new();
    for project in projects {
        if !project.is_expanded {
            continue;
        }
        for worktree in &project.worktrees {
            if ui::project_list::matches_filter(project, worktree, &needle) {
                rows.push((project.id.clone(), worktree.id.clone()));
            }
        }
    }
    rows
}

/// The worktree id Up/Down should move to. `None` when there is nothing to
/// select. The ends do not wrap: a held arrow key stops at the list edge.
fn next_selection(
    rows: &[(String, String)],
    selected: Option<&str>,
    delta: isize,
) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let current = selected.and_then(|id| rows.iter().position(|(_, w)| w == id));
    let next = match current {
        Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        // Nothing selected yet: Down starts at the top, Up at the bottom.
        None if delta < 0 => rows.len() - 1,
        None => 0,
    };
    Some(rows[next].1.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::WorktreeEntry;
    use grove_core::model::Worktree;

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
        }
    }

    fn ids(rows: &[(String, String)]) -> Vec<String> {
        rows.iter().map(|(_, w)| w.clone()).collect()
    }

    #[test]
    fn visible_rows_follow_the_list_order_across_projects() {
        let projects = vec![
            project("p1", "acme", &["main", "feature"]),
            project("p2", "design", &["main"]),
        ];
        let rows = visible_rows(&projects, "");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "p1");
        assert_eq!(rows[2].0, "p2");
    }

    #[test]
    fn a_collapsed_project_is_not_walkable() {
        let mut projects = vec![
            project("p1", "acme", &["main", "feature"]),
            project("p2", "design", &["main"]),
        ];
        projects[0].is_expanded = false;
        let rows = visible_rows(&projects, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "p2");
    }

    #[test]
    fn the_filter_narrows_what_the_keyboard_walks() {
        let projects = vec![project("p1", "acme", &["main", "feature/auth"])];
        let rows = visible_rows(&projects, "auth");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1, projects[0].worktrees[1].id,
            "only the matching row is selectable"
        );
    }

    #[test]
    fn selection_moves_one_row_at_a_time_and_stops_at_the_ends() {
        let projects = vec![project("p1", "acme", &["a", "b", "c"])];
        let rows = visible_rows(&projects, "");
        let all = ids(&rows);

        assert_eq!(
            next_selection(&rows, Some(&all[0]), 1).as_ref(),
            Some(&all[1])
        );
        assert_eq!(
            next_selection(&rows, Some(&all[1]), -1).as_ref(),
            Some(&all[0])
        );
        assert_eq!(
            next_selection(&rows, Some(&all[0]), -1).as_ref(),
            Some(&all[0]),
            "the top does not wrap to the bottom"
        );
        assert_eq!(
            next_selection(&rows, Some(&all[2]), 1).as_ref(),
            Some(&all[2]),
            "the bottom does not wrap to the top"
        );
    }

    #[test]
    fn with_nothing_selected_down_starts_at_the_top_and_up_at_the_bottom() {
        let projects = vec![project("p1", "acme", &["a", "b", "c"])];
        let rows = visible_rows(&projects, "");
        let all = ids(&rows);
        assert_eq!(next_selection(&rows, None, 1).as_ref(), Some(&all[0]));
        assert_eq!(next_selection(&rows, None, -1).as_ref(), Some(&all[2]));
    }

    #[test]
    fn a_selection_that_is_no_longer_visible_restarts_from_the_edge() {
        let projects = vec![project("p1", "acme", &["a", "b"])];
        let rows = visible_rows(&projects, "");
        let all = ids(&rows);
        assert_eq!(
            next_selection(&rows, Some("gone"), 1).as_ref(),
            Some(&all[0])
        );
    }

    #[test]
    fn an_empty_list_selects_nothing() {
        assert_eq!(next_selection(&[], None, 1), None);
        assert_eq!(next_selection(&[], Some("a1b2c3"), -1), None);
        assert!(visible_rows(&[], "").is_empty());
    }

    // ------------------------------------------------ the directory picker

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
        let mut open_project = Some(String::new());
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
        assert_eq!(open_project.as_deref(), Some("/home/u/acme"));

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
        let mut open_project = None;
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
        assert!(open_project.is_none() && !create.is_open() && !settings.is_open());
    }
}
