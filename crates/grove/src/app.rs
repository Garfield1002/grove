//! The eframe application: state held for the UI, channel plumbing to the
//! worker, and the narrow vertical layout from direction 1c.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use grove_core::config::Config;
use grove_core::git::StatusSummary;
use grove_core::model::{Project, SessionPresence};
use grove_core::state::{ProjectRecord, State};
use grove_core::workflow::Activation;
use grove_core::{Paths, state};

use crate::ui::dialogs::create_worktree::CreateForm;
use crate::ui::dialogs::removal::{RemovalForm, Request};
use crate::ui::{self, Action, theme};
use crate::workers::{ErrorReport, Message, Task, Workers};

pub struct GroveApp {
    paths: Paths,
    home: Option<PathBuf>,
    workers: Workers,
    messages: Receiver<Message>,

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
    create: Option<CreateForm>,
    removal: Option<RemovalForm>,
    show_settings: bool,
}

impl GroveApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths) -> Self {
        theme::apply(&cc.egui_ctx);
        let (workers, messages) = Workers::start(paths.clone(), cc.egui_ctx.clone());

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
            messages,
            config: None,
            state: loaded,
            projects,
            selected: None,
            filter: String::new(),
            status: None,
            errors,
            open_project_path: None,
            pending_selection: None,
            create: None,
            removal: None,
            show_settings: false,
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
                    self.config = Some(loaded.config);
                }
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
                }
                Message::StatusesRefreshed {
                    project_id,
                    statuses,
                } => self.apply_statuses(&project_id, &statuses),
                Message::SessionsRefreshed(presence) => self.apply_presence(&presence),
                Message::BaseRefsLoaded {
                    project_id,
                    refs,
                    current,
                } => {
                    if let Some(form) = &mut self.create
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
                    if let Some(form) = &mut self.removal
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
                    if let Some(form) = &mut self.removal
                        && form.project_id == project_id
                    {
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
                    if let Some(form) = &mut self.removal
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
                Message::Failed(report) => self.errors.push(report),
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
                    self.selected = Some(worktree_id);
                    self.workers.send(Task::Activate {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                    });
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
        if self.removal.as_ref().is_some_and(|f| f.project_id == id) {
            self.removal = None;
        }
        self.status = Some("Removed from Grove. The repository is untouched.".to_string());
    }

    fn open_create_dialog(&mut self, project_id: &str) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id) else {
            return;
        };
        let form = CreateForm::new(project);
        self.workers.send(Task::LoadBaseRefs {
            project_id: project.id.clone(),
            repository_path: project.repository_path.clone(),
        });
        self.create = Some(form);
    }

    fn open_removal_dialog(&mut self, project_id: &str, worktree_id: &str) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id) else {
            return;
        };
        let Some(worktree) = project.worktree(worktree_id) else {
            return;
        };
        self.selected = Some(worktree_id.to_string());
        self.workers.send(Task::GatherRemoval {
            project_id: project.id.clone(),
            worktree: Box::new(worktree.clone()),
        });
        self.removal = Some(RemovalForm {
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
        });
    }

    /// Dispatch one confirmed removal operation. Exactly one, never a bundle.
    fn apply_removal(&mut self, request: Request) {
        let Some(form) = &self.removal else { return };
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

    /// The rows the keyboard walks: every visible worktree, in list order.
    fn visible_rows(&self) -> Vec<(String, String)> {
        let needle = self.filter.trim().to_ascii_lowercase();
        let mut rows = Vec::new();
        for project in &self.projects {
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

    fn move_selection(&mut self, delta: isize) {
        let rows = self.visible_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|id| rows.iter().position(|(_, w)| w == id));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            None if delta < 0 => rows.len() - 1,
            None => 0,
        };
        self.selected = Some(rows[next].1.clone());
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

    /// Keyboard navigation (DESIGN.md §16). Ignored while a dialog is open so
    /// Delete cannot fire behind a confirmation.
    fn keyboard(&mut self, ctx: &egui::Context) {
        if self.create.is_some() || self.removal.is_some() || self.open_project_path.is_some() {
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

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Worktrees")
                    .size(14.0)
                    .strong()
                    .color(theme::TEXT_STRONG),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(theme::label("＋", 13.0, theme::TEXT_DIM))
                    .on_hover_text("Open project")
                    .clicked()
                {
                    self.open_project_path = Some(String::new());
                }
                if ui
                    .button(theme::label("Refresh", 11.0, theme::TEXT_DIM))
                    .on_hover_text("Re-read worktrees from git and sessions from tmux")
                    .clicked()
                {
                    self.refresh_all();
                }
            });
        });
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::singleline(&mut self.filter)
                .hint_text("Filter worktrees…")
                .desired_width(f32::INFINITY),
        );
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            if ui
                .button(theme::label("Open Project", 12.0, theme::TEXT_DIM))
                .clicked()
            {
                self.open_project_path = Some(String::new());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(theme::label("⚙", 13.0, theme::TEXT_MUTED))
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.show_settings = !self.show_settings;
                }
            });
        });
        if let Some(status) = &self.status {
            ui.add_space(2.0);
            ui.add(egui::Label::new(theme::label(status, 10.0, theme::TEXT_FAINT)).truncate());
        }
    }
}

impl eframe::App for GroveApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_messages();

        if let Some(path) = &mut self.open_project_path {
            match ui::dialogs::open_project(ctx, path) {
                ui::dialogs::OpenProject::Idle => {}
                ui::dialogs::OpenProject::Cancelled => self.open_project_path = None,
                ui::dialogs::OpenProject::Confirmed(path) => {
                    self.open_project_path = None;
                    self.status = Some(format!("Opening {path}…"));
                    self.workers.send(Task::OpenProject(PathBuf::from(path)));
                }
            }
        }

        if let Some(form) = &mut self.create {
            match ui::dialogs::create_worktree::show(ctx, form) {
                ui::dialogs::create_worktree::Outcome::Idle => {}
                ui::dialogs::create_worktree::Outcome::Cancelled => self.create = None,
                ui::dialogs::create_worktree::Outcome::Create(add) => {
                    self.workers.send(Task::CreateWorktree {
                        project_id: form.project_id.clone(),
                        project_name: form.project_name.clone(),
                        repository_path: form.repository_path.clone(),
                        git_common_dir: form.git_common_dir.clone(),
                        add,
                        open_after: form.open_after,
                    });
                    self.status = Some("Creating the worktree…".to_string());
                    self.create = None;
                }
            }
        }

        if let Some(form) = &mut self.removal {
            let mut open = true;
            let request = ui::dialogs::removal::show(ctx, form, &mut open);
            if !open {
                self.removal = None;
            }
            if let Some(request) = request {
                self.apply_removal(request);
            }
        }

        if self.show_settings {
            let mut open = true;
            ui::settings::show(ctx, &mut open, &self.paths, self.config.as_ref());
            self.show_settings = open;
        }

        egui::TopBottomPanel::top("grove-header")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_SUNKEN)
                    .inner_margin(egui::Margin::symmetric(10, 9)),
            )
            .show(ctx, |ui| self.header(ui));

        egui::TopBottomPanel::bottom("grove-footer")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_FOOTER)
                    .inner_margin(egui::Margin::symmetric(10, 9)),
            )
            .show(ctx, |ui| self.footer(ui));

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
                    .inner_margin(egui::Margin::symmetric(10, 6)),
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
