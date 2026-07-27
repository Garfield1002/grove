//! The eframe application: state held for the UI, channel plumbing to the
//! worker, and the narrow vertical layout from direction 1c.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use grove_core::config::Config;
use grove_core::model::{Project, SessionPresence};
use grove_core::state::{ProjectRecord, State};
use grove_core::workflow::Activation;
use grove_core::{Paths, state};

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
                    }
                }
                Message::SessionsRefreshed(presence) => self.apply_presence(&presence),
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
            Action::RemoveProject(id) => {
                self.projects.retain(|p| p.id != id);
                self.state.remove(&id);
                self.save_state();
                self.status = Some("Removed from Grove. The repository is untouched.".to_string());
            }
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
                        worktree: Box::new(worktree.clone()),
                    });
                }
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
    }
}
