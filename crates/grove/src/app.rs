//! The eframe application: state held for the UI, channel plumbing to the
//! worker, and the narrow vertical layout from direction 1c.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use grove_core::claude::HookChange;
use grove_core::config::Config;
use grove_core::git::StatusSummary;
use grove_core::ipc::Notification;
use grove_core::model::{Project, SessionPresence};
use grove_core::notice::Notices;
use grove_core::reconcile::{OrphanSession, ProjectRef, Reconciliation};
use grove_core::state::{AgentRecord, ProjectRecord, SessionRecord, State};
use grove_core::status::{SessionReport, SessionStatus};
use grove_core::tmux::WindowInfo;
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
    /// Last known windows per tmux session name, kept for the same reason: a
    /// refreshed worktree list keeps its child rows instead of blinking empty.
    windows: HashMap<String, Vec<WindowInfo>>,
    /// What the last `grove notify` said, per worktree and per window. Held
    /// here rather than on the rows because a refresh rebuilds those, and a
    /// message has nothing to be re-derived from.
    notices: Notices,
    /// Grove's hooks in Claude Code's settings, as the last check found them.
    /// `None` until one has run.
    claude_hooks: Option<HookChange>,

    config: Option<Config>,
    state: State,
    projects: Vec<Project>,

    selected: Option<String>,
    /// The window row the user last opened, as (worktree id, window index).
    /// Cleared when the selection moves to another worktree, so only one row
    /// in the tree is ever drawn as selected.
    selected_window: Option<(String, u32)>,
    filter: String,
    status: Option<String>,
    errors: Vec<ErrorReport>,

    /// Sessions the last reconciliation found with no worktree behind them
    /// (DESIGN.md §11). Listed, never acted on without the user.
    orphans: Vec<OrphanSession>,
    /// How many further orphans the user has silenced, so the list can offer
    /// to report them again.
    ignored_orphans: usize,
    /// The orphan whose "close session" is armed. Closing a session is a
    /// confirmed operation of its own, so the first click only arms it.
    orphan_armed: Option<String>,
    /// Whether the footer's quit-and-kill-server control is armed. Killing
    /// the server ends every session at once, so the first click only arms.
    shutdown_armed: bool,
    /// Set when the worker confirms the tmux server is down; the next frame
    /// closes the window. Quitting waits for that confirmation so a failed
    /// kill leaves Grove open with the error on screen.
    quit_after_kill: bool,

    open_project_path: Option<String>,
    /// A worktree Grove just created, selected as soon as a refresh lists it.
    pending_selection: Option<(String, PathBuf)>,
    /// The number `grove toggle <n>` started this process for, opened as soon
    /// as the first reconciliation says what that number points at.
    pending_toggle: Option<u8>,
    /// Whether this launch has already asked to bring its agents back. One
    /// pass per process: reconciliation also runs on refresh, on adopting an
    /// orphan and on closing one, and none of those is a restart.
    agents_resumed: bool,
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
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths, pending_toggle: Option<u8>) -> Self {
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
                unavailable: None,
            })
            .collect();

        workers.send(Task::LoadConfig);
        // Startup reconciliation (ARCHITECTURE.md §7): one pass over git and
        // tmux rather than a refresh per project, so sessions are reattached,
        // stopped ones are named as stopped and orphans are found before the
        // user touches anything.
        workers.send(Task::Reconcile {
            projects: project_refs(&projects),
            recorded: loaded.recorded_session_ids(),
            ignored: loaded.ignored_sessions.clone(),
        });

        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            paths,
            workers,
            watch,
            messages,
            statuses: HashMap::new(),
            windows: HashMap::new(),
            notices: Notices::default(),
            claude_hooks: None,
            config: None,
            state: loaded,
            projects,
            selected: None,
            selected_window: None,
            filter: String::new(),
            status: None,
            errors,
            orphans: Vec::new(),
            ignored_orphans: 0,
            orphan_armed: None,
            shutdown_armed: false,
            quit_after_kill: false,
            open_project_path: None,
            pending_selection: None,
            pending_toggle,
            agents_resumed: false,
            create: Detached::default(),
            removal: Detached::default(),
            settings: Detached::default(),
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
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
                        // git answered, so the project is there after all.
                        project.unavailable = None;
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
                    // the last poll rather than blanking every pill, and the
                    // session index, or a refresh would turn every *stopped*
                    // row back into "no session".
                    self.mark_stopped_sessions();
                    self.apply_session_statuses();
                    self.apply_session_windows();
                    self.describe_worktrees();
                }
                Message::StatusesRefreshed {
                    project_id,
                    statuses,
                } => self.apply_statuses(&project_id, &statuses),
                Message::SessionsRefreshed { presence, windows } => {
                    self.windows = windows;
                    self.apply_presence(&presence);
                }
                Message::Reconciled(result) => self.apply_reconciliation(*result),
                Message::SessionOpened { activation } => {
                    self.status = Some(describe(&activation));
                }
                Message::Associated {
                    worktree_id,
                    session,
                } => {
                    self.status = Some(format!("{session} is now this worktree's session."));
                    self.selected = Some(worktree_id);
                    self.orphan_armed = None;
                    self.reconcile();
                }
                Message::ServerKilled => {
                    self.quit_after_kill = true;
                }
                Message::OrphanClosed { session } => {
                    self.status = Some(format!(
                        "Closed {session}. No worktree or branch was touched."
                    ));
                    self.orphan_armed = None;
                    self.reconcile();
                }
                // Said either way. "Every agent was still running" is the
                // common answer after a quick restart, and it is a different
                // thing from Grove having done nothing.
                Message::AgentsResumed { worktree_ids } => {
                    self.status = Some(match worktree_ids.len() {
                        0 => "No conversation needed resuming.".to_string(),
                        1 => "Resumed 1 agent conversation.".to_string(),
                        n => format!("Resumed {n} agent conversations."),
                    });
                    if let Some(first) = worktree_ids.first() {
                        self.selected = Some(first.clone());
                    }
                    self.watch.send(Control::PollNow);
                }
                Message::AgentStarted { worktree_id, unit } => {
                    self.selected = Some(worktree_id);
                    self.status = Some(match unit {
                        Some(unit) => format!("Started the agent in {unit}"),
                        None => "Started the agent".to_string(),
                    });
                    self.watch.send(Control::PollNow);
                }
                Message::GitStatusDue => self.refresh_git_statuses(),
                Message::StatusPolled(statuses) => {
                    self.statuses = statuses;
                    self.apply_session_statuses();
                }
                Message::WindowsPolled(windows) => {
                    self.windows = windows;
                    self.apply_session_windows();
                }
                Message::Toggled { slot } => self.apply_toggle(ctx, slot),
                Message::Notified(notification) => self.apply_notification(&notification),
                Message::ClaudeHooks { op, change } => self.apply_hook_change(op, *change),
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
                        // A session the *user* closed is not a stopped
                        // session: forget the mapping, or the row would go on
                        // offering to bring back what was just dismissed.
                        if operation == crate::workers::RemovalOp::CloseSession {
                            self.state.forget_session(&form.worktree_id);
                            let state = self.state.clone();
                            self.workers.send(Task::SaveState(Box::new(state)));
                        }
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
                    self.status = Some(describe(&activation));
                    // The worktree now has a session; recording it is what
                    // makes a later disappearance read as *stopped* rather
                    // than as "there was never one" (DESIGN.md §11).
                    self.record_session(&worktree_id, activation.session());
                    self.selected = Some(worktree_id);
                }
                Message::WindowOpened {
                    worktree_id,
                    window,
                } => {
                    self.selected = Some(worktree_id);
                    self.status = Some(format!(
                        "Opened window {} in {}",
                        window.window, window.session
                    ));
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

    /// Re-derive *stopped* from the session index for every row without a live
    /// session. Reconciliation does this too; doing it here as well keeps an
    /// ordinary refresh from downgrading "session stopped" to "no session".
    fn mark_stopped_sessions(&mut self) {
        for project in &mut self.projects {
            for worktree in &mut project.worktrees {
                worktree.session_stopped =
                    !worktree.session.exists() && self.state.session(&worktree.id).is_some();
            }
        }
    }

    fn apply_presence(&mut self, presence: &HashMap<String, SessionPresence>) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_presence(&mut project.worktrees, presence);
        }
        self.mark_stopped_sessions();
        // Presence just changed, so a row that lost its session must lose its
        // status — and its windows — with it rather than waiting for the next
        // poll.
        self.apply_session_statuses();
        self.apply_session_windows();
    }

    /// Re-read every project's working-tree status, on the poller's cadence.
    ///
    /// Queued on the worker, never run here: this is one `git status` per
    /// worktree.
    fn refresh_git_statuses(&self) {
        for project in &self.projects {
            if project.worktrees.is_empty() {
                continue;
            }
            self.workers.send(Task::RefreshStatuses {
                project_id: project.id.clone(),
                worktrees: project.worktrees.clone(),
            });
        }
    }

    /// Stamp the last polled statuses onto every row.
    fn apply_session_statuses(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_status(&mut project.worktrees, &self.statuses);
        }
    }

    /// Stamp the last known tmux windows onto every row, so each worktree
    /// carries the child rows the tree draws under it.
    fn apply_session_windows(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_session_windows(&mut project.worktrees, &self.windows);
        }
        // The notes hang off the windows, so they are restamped with them: a
        // note for a window that has closed must not outlive its row.
        self.apply_window_notes();
    }

    /// Stamp what each window last reported onto the rows that draw it.
    fn apply_window_notes(&mut self) {
        for project in &mut self.projects {
            grove_core::workflow::apply_window_notes(&mut project.worktrees, &self.notices);
        }
    }

    /// One `grove toggle` from the CLI (`crate::toggle`).
    ///
    /// Without a number the window is the subject, and on Wayland the only
    /// honest half of "hide" is to close: a client cannot un-hide itself
    /// there. Closing leaves every tmux session running, and the next
    /// `grove toggle` starts Grove again.
    fn apply_toggle(&mut self, ctx: &egui::Context, slot: Option<u8>) {
        let Some(slot) = slot else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };
        // Raising the window is a request, not a guarantee — a Wayland
        // compositor may well refuse it. Opening the session is the part that
        // has to happen, so it is not conditional on this.
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.activate_slot(slot);
    }

    /// Select the worktree carrying `slot` and open its session, as pressing
    /// Enter on its row does. A number pointing at nothing is reported and
    /// nothing else: it is a stale label, never a reason to act on another row.
    fn activate_slot(&mut self, slot: u8) {
        match slot_target(&self.projects, &self.state, slot) {
            Some((project_id, worktree_id)) => self.apply_action(Action::ActivateWorktree {
                project_id,
                worktree_id,
            }),
            None => {
                self.status = Some(format!("No worktree in Grove's list is numbered {slot}."));
            }
        }
    }

    /// Put a number on the selected worktree, or take it off again.
    ///
    /// Pressing the digit a worktree already carries clears it, so one
    /// keystroke both assigns and unassigns.
    fn set_slot(&mut self, worktree_id: &str, slot: u8) {
        if self.state.slot(worktree_id) == Some(slot) {
            self.state.clear_slot(worktree_id);
            self.status = Some(format!("Took {slot} off this worktree."));
        } else if self.state.assign_slot(slot, worktree_id) {
            self.status = Some(format!("`grove toggle {slot}` now opens this worktree."));
        } else {
            return;
        }
        self.save_state();
    }

    /// An explicit `grove notify` report: show its message straight away.
    ///
    /// The status itself is left to the poller, which re-reads tmux a moment
    /// later — except for attention, which is latched in the engine and would
    /// otherwise not show until that poll lands.
    fn apply_notification(&mut self, notification: &Notification) {
        let worktree_id = notification.worktree_id.as_str();
        let state = notification.state;
        self.notices.record(notification);
        if state == SessionStatus::Attention {
            // Keep any resource figures the last poll produced; only the
            // status is being overridden here.
            let report = self.statuses.entry(worktree_id.to_string()).or_default();
            report.status = SessionStatus::Attention;
        }
        for project in &mut self.projects {
            if let Some(worktree) = project.worktrees.iter_mut().find(|w| w.id == worktree_id) {
                worktree.status_message = notification.message.clone();
                if state == SessionStatus::Attention && worktree.session.exists() {
                    worktree.status = Some(SessionStatus::Attention);
                }
            }
        }
        self.apply_window_notes();
        self.record_agent(notification);
    }

    /// Remember the conversation an agent reported, so Grove can offer to
    /// resume it or open its transcript later.
    ///
    /// `state.toml` is only written when something actually changed: agents
    /// report several times a turn and the id is the same every time.
    fn record_agent(&mut self, notification: &Notification) {
        if !notification.has_agent_record() {
            return;
        }
        let changed = self.state.record_agent(AgentRecord {
            worktree_id: notification.worktree_id.clone(),
            session_id: notification.agent_session.clone().unwrap_or_default(),
            transcript_path: notification.transcript.clone().unwrap_or_default(),
        });
        if changed {
            self.save_state();
        }
    }

    /// What Claude Code's settings say after a check, an install or a removal.
    fn apply_hook_change(&mut self, op: crate::workers::HookOp, change: HookChange) {
        use crate::workers::HookOp;
        // A check is how the Settings pane finds out where things stand; only
        // an install or a removal is worth a line in the status bar.
        match op {
            HookOp::Check => {}
            HookOp::Install if change.changed => {
                self.status = Some(format!(
                    "Installed Grove's hooks in {}. Restart Claude Code for them to take effect.",
                    change.path.display()
                ));
            }
            HookOp::Install => {
                self.status = Some("Grove's hooks were already installed.".to_string());
            }
            HookOp::Uninstall if change.changed => {
                self.status = Some(format!(
                    "Removed Grove's hooks from {}.",
                    change.path.display()
                ));
            }
            HookOp::Uninstall => {
                self.status = Some("Grove had no hooks installed.".to_string());
            }
        }
        self.claude_hooks = Some(change);
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
        // The messages explained a state the user has now gone and looked at,
        // per window as well as for the worktree.
        self.notices.clear(worktree_id);
        for project in &mut self.projects {
            if let Some(worktree) = project.worktrees.iter_mut().find(|w| w.id == worktree_id) {
                worktree.status = None;
                worktree.status_message = None;
                worktree.window_notes.clear();
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

    /// Ask the worker for a reconciliation pass (ARCHITECTURE.md §7). This is
    /// what the header's Restore control, Ctrl+R with no row selected, and
    /// startup all do.
    fn reconcile(&mut self) {
        self.workers.send(Task::Reconcile {
            projects: project_refs(&self.projects),
            recorded: self.state.recorded_session_ids(),
            ignored: self.state.ignored_sessions.clone(),
        });
        self.status = Some("Reconciling with git and tmux…".to_string());
    }

    /// Apply one reconciliation pass to the UI and the index.
    ///
    /// It marks and it records; it removes nothing. An unavailable project
    /// keeps its record *and* its last known rows — a project on an unplugged
    /// drive must not look as though its worktrees were deleted.
    fn apply_reconciliation(&mut self, result: Reconciliation) {
        let summary = result.summary();
        for status in result.projects {
            let Some(project) = self.projects.iter_mut().find(|p| p.id == status.id) else {
                continue;
            };
            project.unavailable = status.unavailable;
            if project.unavailable.is_none() {
                project.worktrees = status.worktrees;
            }
        }
        self.orphans = result.orphans;
        self.ignored_orphans = result.ignored;
        if self
            .orphan_armed
            .as_ref()
            .is_some_and(|name| !self.orphans.iter().any(|o| &o.name == name))
        {
            self.orphan_armed = None;
        }
        self.record_live_sessions();
        self.forget_stale_notices();
        self.apply_session_statuses();
        self.apply_session_windows();
        self.describe_worktrees();
        self.status = Some(summary);
        // A `grove toggle <n>` that had to start this process waited for this:
        // until reconciliation has run there are no rows for a number to name.
        // It is taken either way — one pass is the whole grace period, and a
        // number that named nothing must not fire at some later refresh.
        if let Some(slot) = self.pending_toggle.take() {
            self.activate_slot(slot);
        }
        self.resume_agents_once();
    }

    /// Ask the worker to bring back the conversations this launch lost.
    ///
    /// After the first reconciliation, because that is the point where the
    /// rows are what git and tmux actually say — resuming into a worktree
    /// Grove had not yet checked would be acting on last session's index.
    fn resume_agents_once(&mut self) {
        if self.agents_resumed {
            return;
        }
        // Config first: until it has loaded, "resume on startup" has no
        // answer, and treating that as "no" would spend the one pass this
        // launch gets on a question nobody asked yet.
        let Some(config) = self.config.as_ref() else {
            return;
        };
        let enabled = config.agents.resume_on_startup;
        // Nothing to do is still done: a config that says no, or a state file
        // with no conversations in it, must not leave this armed for a later
        // refresh to fire.
        self.agents_resumed = true;
        if !enabled || self.state.agents.is_empty() {
            return;
        }
        self.workers.send(Task::ResumeAgents {
            projects: self.projects.clone(),
            records: self.state.agents.clone(),
        });
    }

    /// Drop reports for worktrees reconciliation no longer lists, so a Grove
    /// left open for weeks cannot accumulate them.
    ///
    /// Bookkeeping only, and deliberately not tied to `state.toml`: forgetting
    /// what an agent said about a row that is gone removes nothing anywhere.
    fn forget_stale_notices(&mut self) {
        if self.notices.is_empty() {
            return;
        }
        let live: std::collections::HashSet<&str> = self
            .projects
            .iter()
            .flat_map(|project| project.worktrees.iter().map(|w| w.id.as_str()))
            .collect();
        self.notices.retain_ids(|id| live.contains(id));
    }

    /// Note every worktree that currently has a session, so a session that
    /// later disappears is reported as *stopped*. Records for sessions that
    /// have gone are deliberately kept: that is the whole signal.
    fn record_live_sessions(&mut self) {
        let live: Vec<SessionRecord> = self
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .filter(|worktree| worktree.session.exists())
                    .map(|worktree| SessionRecord {
                        worktree_id: worktree.id.clone(),
                        project_id: project.id.clone(),
                        worktree_path: worktree.path.clone(),
                        session_name: worktree.session_name(),
                        last_activity_at: grove_core::workflow::now_epoch(),
                    })
            })
            .collect();
        let before = self.state.sessions.clone();
        for record in live {
            self.state.record_session(record);
        }
        // Every pass restamps the activity time, so compare the mappings
        // themselves: an unchanged reconciliation must not rewrite state.toml.
        let changed = before.len() != self.state.sessions.len()
            || before.iter().zip(&self.state.sessions).any(|(a, b)| {
                a.worktree_id != b.worktree_id
                    || a.project_id != b.project_id
                    || a.session_name != b.session_name
                    || a.worktree_path != b.worktree_path
            });
        if changed {
            self.save_state();
        }
    }

    /// Record one worktree's session mapping, by worktree id.
    fn record_session(&mut self, worktree_id: &str, session: &str) {
        let found = self.projects.iter().find_map(|project| {
            project
                .worktree(worktree_id)
                .map(|worktree| (project.id.clone(), worktree.path.clone()))
        });
        let Some((project_id, worktree_path)) = found else {
            return;
        };
        self.state.record_session(SessionRecord {
            worktree_id: worktree_id.to_string(),
            project_id,
            worktree_path,
            session_name: session.to_string(),
            last_activity_at: grove_core::workflow::now_epoch(),
        });
        self.save_state();
    }

    /// Is there a `resume_command` to offer at all? There is one by default —
    /// Claude Code's, since Claude Code is what reports the ids — so this is
    /// false only for a user who blanked the key.
    fn can_resume_agents(&self) -> bool {
        self.config
            .as_ref()
            .is_some_and(|config| config.agents.resume_command().is_some())
    }

    /// Start the configured agent in a worktree, or resume the conversation
    /// `resume` names.
    fn start_agent(&mut self, project_id: &str, worktree_id: &str, resume: Option<String>) {
        if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
            && let Some(worktree) = project.worktree(worktree_id)
        {
            self.workers.send(Task::StartAgent {
                project_name: project.name.clone(),
                git_common_dir: project.git_common_dir.clone(),
                worktree: Box::new(worktree.clone()),
                resume,
            });
            self.selected = Some(worktree_id.to_string());
        }
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
            } => self.start_agent(&project_id, &worktree_id, None),
            // Resuming needs the conversation the agent last reported here.
            // Without one there is nothing to resume, and starting a fresh
            // conversation instead would look identical and lose the user's
            // place — so this says so rather than doing something else.
            Action::ResumeAgent {
                project_id,
                worktree_id,
            } => match self.state.agent(&worktree_id) {
                Some(record) if !record.session_id.is_empty() => {
                    let resume = record.session_id.clone();
                    self.start_agent(&project_id, &worktree_id, Some(resume));
                }
                _ => {
                    self.status =
                        Some("No agent conversation has been reported for this worktree.".into());
                }
            },
            Action::OpenAgentTranscript { worktree_id } => match self.state.agent(&worktree_id) {
                Some(record) if record.has_transcript() => {
                    self.workers
                        .send(Task::OpenWithDesktop(record.transcript_path.clone()));
                }
                _ => {
                    self.status = Some("No transcript has been reported for this worktree.".into());
                }
            },
            Action::SelectWorktree { worktree_id, .. } => self.selected = Some(worktree_id),
            Action::SetWorktreeSlot { worktree_id, slot } => match slot {
                Some(slot) => self.set_slot(&worktree_id, slot),
                None => {
                    if self.state.clear_slot(&worktree_id) {
                        self.status = Some("Took the number off this worktree.".to_string());
                        self.save_state();
                    }
                }
            },
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
            Action::OpenNewWindow {
                project_id,
                worktree_id,
            } => {
                if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
                    && let Some(worktree) = project.worktree(&worktree_id)
                {
                    self.selected = Some(worktree_id);
                    self.workers.send(Task::OpenNewWindow {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                    });
                }
            }
            // Opening a window is opening the session, so it clears attention
            // exactly as opening the worktree row does.
            Action::ActivateWindow {
                project_id,
                worktree_id,
                window_index,
            } => {
                if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
                    && let Some(worktree) = project.worktree(&worktree_id)
                {
                    let session = worktree.session_name();
                    self.workers.send(Task::ActivateWindow {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                        window_index,
                    });
                    self.clear_attention(&worktree_id, &session);
                    self.selected_window = Some((worktree_id.clone(), window_index));
                    self.selected = Some(worktree_id);
                }
            }
            Action::RemoveWorktree {
                project_id,
                worktree_id,
            } => self.open_removal_dialog(&project_id, &worktree_id),
            // "Locate project" is the open-project prompt, pre-filled with
            // where the project used to be. Registering it again updates the
            // existing record when it is the same repository, because the
            // project id is derived from the git-common-dir.
            Action::LocateProject(id) => {
                if let Some(project) = self.projects.iter().find(|p| p.id == id) {
                    self.open_project_path = Some(project.repository_path.display().to_string());
                    self.status = Some(format!(
                        "Point Grove at {} where it lives now.",
                        project.name
                    ));
                }
            }
        }
        // Exactly one row in the tree is selected: a window's highlight only
        // survives while its own worktree is the selected one.
        if self
            .selected_window
            .as_ref()
            .is_some_and(|(id, _)| Some(id.as_str()) != self.selected.as_deref())
        {
            self.selected_window = None;
        }
    }

    /// One choice about one orphaned session (DESIGN.md §11). Exactly one, and
    /// closing is armed before it happens.
    fn apply_orphan_action(&mut self, action: ui::orphans::OrphanAction) {
        use ui::orphans::OrphanAction;
        match action {
            OrphanAction::Open(session) => {
                let cwd = self
                    .orphans
                    .iter()
                    .find(|o| o.name == session)
                    .and_then(|o| o.worktree_path.clone())
                    .unwrap_or_else(|| PathBuf::from("."));
                self.workers.send(Task::OpenSession { session, cwd });
            }
            OrphanAction::Associate {
                session,
                project_id,
                worktree_id,
            } => {
                if let Some(project) = self.projects.iter().find(|p| p.id == project_id)
                    && let Some(worktree) = project.worktree(&worktree_id)
                {
                    self.workers.send(Task::AssociateSession {
                        project_name: project.name.clone(),
                        git_common_dir: project.git_common_dir.clone(),
                        worktree: Box::new(worktree.clone()),
                        session,
                    });
                }
            }
            // The first click arms; only the second one closes anything.
            OrphanAction::Close(session) => {
                if self.orphan_armed.as_deref() == Some(session.as_str()) {
                    self.workers.send(Task::CloseOrphan { session });
                } else {
                    self.status = Some(format!(
                        "Choose “Confirm: close {session}” to end that session."
                    ));
                    self.orphan_armed = Some(session);
                }
            }
            OrphanAction::Ignore(session) => {
                self.state.ignore_session(&session);
                self.save_state();
                self.status = Some(format!(
                    "Ignoring {session}. It is still running; use Restore to list it again."
                ));
                self.reconcile();
            }
            OrphanAction::ShowIgnored => {
                self.state.clear_ignored_sessions();
                self.save_state();
                self.reconcile();
            }
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
        let hooks = self.claude_hooks.as_ref();

        let mut action = None;
        let mut close = false;
        ctx.show_viewport_immediate(
            id,
            ui::chrome::viewport("Settings", ui::settings::SIZE, ui::settings::MIN_SIZE),
            |ctx, class| {
                let dialog = ui::chrome::show(ctx, class, "Settings", |ui| {
                    ui::settings::body(ui, &mut *form, paths, home, hooks)
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
        // Ctrl+R is the Restore chip: a full reconciliation against git and
        // tmux. A single project's Refresh stays in its menu, where it is
        // scoped on purpose.
        if refresh {
            self.reconcile();
        }

        // Alt+<digit> puts that number on the selected row, or takes it off
        // again. Alt because the plain digits belong to the filter field and
        // Ctrl+<digit> is what a terminal emulator tends to eat.
        if let Some(digit) = ctx.input(pressed_digit)
            && let Some((_, worktree_id)) = self.selected_row()
        {
            self.set_slot(&worktree_id, digit);
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
                if ui::icons::chip(ui, "Restore", true, ui::icons::refresh)
                    .on_hover_text(
                        "Rebuild Grove's view from git and tmux.\n\
                         Reattaches live sessions, marks missing worktrees and\n\
                         stopped sessions, and lists orphaned sessions.\n\
                         Nothing is ever deleted. (Ctrl+R)",
                    )
                    .clicked()
                {
                    self.reconcile();
                }
            });
        });

        ui.add_space(8.0);
        self.filter_field(ui);
    }

    /// The footer's quit-and-kill-server control, next to the gear.
    ///
    /// Plain quitting (Ctrl+Q, closing the window) leaves the tmux server and
    /// every session running — FR-7, sessions outlive the GUI. This is the
    /// one deliberate exception: end everything, then quit. Ending every
    /// session at once is destructive, so the first click only arms the
    /// button and the second one acts; any click elsewhere disarms it.
    fn shutdown_button(&mut self, ui: &mut egui::Ui) {
        let armed = self.shutdown_armed;
        let response = ui::icons::button(ui, true, |painter, rect, tint| {
            ui::icons::power(painter, rect, if armed { theme::DANGER } else { tint });
        })
        .on_hover_text(if armed {
            "Confirm: kill the tmux server and quit Grove."
        } else {
            "Quit Grove and kill its tmux server.\n\
             Every session — and whatever runs inside — ends.\n\
             A second click confirms. To quit and leave the\n\
             sessions running, just close the window (Ctrl+Q)."
        });
        if response.clicked() {
            if armed {
                self.shutdown_armed = false;
                self.status = Some("Killing the tmux server…".to_string());
                self.workers.send(Task::KillServer);
            } else {
                self.shutdown_armed = true;
                self.status =
                    Some("Click the power button again to end every session and quit.".to_string());
            }
        } else if armed && response.clicked_elsewhere() {
            self.shutdown_armed = false;
        }
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
                        // Same reasoning for Claude Code's settings: reading
                        // that file is the worker's job, and the pane should
                        // open already knowing what it says.
                        self.workers
                            .send(Task::ClaudeHooks(crate::workers::HookOp::Check));
                        self.settings.open(form);
                    }
                }
                self.shutdown_button(ui);
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

/// What Grove knows about each project before git is consulted, for a
/// reconciliation pass.
fn project_refs(projects: &[Project]) -> Vec<ProjectRef> {
    projects
        .iter()
        .map(|project| ProjectRef {
            id: project.id.clone(),
            name: project.name.clone(),
            repository_path: project.repository_path.clone(),
            git_common_dir: project.git_common_dir.clone(),
        })
        .collect()
}

/// The status line for an activation, whichever route it took.
fn describe(activation: &Activation) -> String {
    match activation {
        Activation::SwitchedClient {
            session,
            client_tty,
        } => format!("Switched {client_tty} to {session}"),
        Activation::LaunchedTerminal { session, .. } => {
            format!("Launched a terminal on {session}")
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
        // The poller polls slowly while Grove paints nothing, which is most of
        // its life. A frame here is the window being looked at again; if it is
        // the first one after a gap, the tree is stale, so poll at once rather
        // than showing the user a snapshot from up to half a minute ago.
        if self.watch.painted() {
            self.watch.send(Control::PollNow);
        }
        self.drain_messages(ctx);

        // The worker has confirmed the tmux server is down (the footer's
        // quit-and-kill control): nothing is left to outlive the GUI, so quit.
        if self.quit_after_kill {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

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
        let mut orphan_action = None;
        let central = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(theme::LIST_MARGIN_X, 6)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .show(ui, |ui| {
                        action = ui::project_list::show(
                            ui,
                            &self.projects,
                            ui::project_list::Tree {
                                selected: self.selected.as_deref(),
                                selected_window: self
                                    .selected_window
                                    .as_ref()
                                    .map(|(id, index)| (id.as_str(), *index)),
                                filter: &self.filter,
                                home: self.home.as_deref(),
                                slots: &self.state.slots,
                                agents: &self.state.agents,
                                can_resume: self.can_resume_agents(),
                            },
                        );
                        orphan_action = ui::orphans::show(
                            ui,
                            &self.orphans,
                            self.ignored_orphans,
                            self.orphan_armed.as_deref(),
                            &self.projects,
                        );
                        ui.min_rect().bottom()
                    })
                    .inner
            });
        // Decoration for a short list only, in the background layer so it can
        // neither cover a row nor take a click. Painted against the panel's
        // *outer* rect, so the art bleeds to the window edge instead of
        // stopping at the frame's inner margin.
        let panel = central.response.rect;
        if let Some(free) = ui::backdrop::free_space(panel, central.inner) {
            ui::backdrop::show(ctx, panel, free);
        }
        if let Some(action) = action {
            self.apply_action(action);
        }
        if let Some(action) = orphan_action {
            self.apply_orphan_action(action);
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

/// The (project id, worktree id) a number points at, if it still points at a
/// row Grove is listing.
///
/// A number that names nothing resolves to `None` and is left alone: it is a
/// stale label, and the worktree it named may simply be on a project that is
/// currently unavailable.
fn slot_target(projects: &[Project], state: &State, slot: u8) -> Option<(String, String)> {
    let worktree_id = state.slot_worktree(slot)?;
    projects
        .iter()
        .find(|project| project.worktree(worktree_id).is_some())
        .map(|project| (project.id.clone(), worktree_id.to_string()))
}

/// The 1..=9 digit pressed with Alt this frame, if any.
///
/// Zero is not among them: the numbers are 1–9 (`state::MAX_SLOT`), and Alt+0
/// meaning nothing is better than it quietly meaning something else.
fn pressed_digit(input: &egui::InputState) -> Option<u8> {
    if !input.modifiers.alt {
        return None;
    }
    const DIGITS: [(egui::Key, u8); 9] = [
        (egui::Key::Num1, 1),
        (egui::Key::Num2, 2),
        (egui::Key::Num3, 3),
        (egui::Key::Num4, 4),
        (egui::Key::Num5, 5),
        (egui::Key::Num6, 6),
        (egui::Key::Num7, 7),
        (egui::Key::Num8, 8),
        (egui::Key::Num9, 9),
    ];
    DIGITS
        .iter()
        .find(|(key, _)| input.key_pressed(*key))
        .map(|(_, digit)| *digit)
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
            unavailable: None,
        }
    }

    fn ids(rows: &[(String, String)]) -> Vec<String> {
        rows.iter().map(|(_, w)| w.clone()).collect()
    }

    #[test]
    fn a_number_resolves_to_its_row() {
        let projects = vec![
            project("p1", "acme", &["main", "feature"]),
            project("p2", "design", &["main"]),
        ];
        let target = projects[1].worktrees[0].id.clone();
        let mut state = State::default();
        state.assign_slot(3, &target);
        assert_eq!(
            slot_target(&projects, &state, 3),
            Some(("p2".to_string(), target))
        );
    }

    /// A number Grove cannot resolve must select nothing at all — never the
    /// nearest row, and never a row from another project.
    #[test]
    fn an_unassigned_or_stale_number_resolves_to_nothing() {
        let projects = vec![project("p1", "acme", &["main"])];
        let mut state = State::default();
        assert_eq!(slot_target(&projects, &state, 3), None, "never assigned");

        state.assign_slot(3, "deadbe");
        assert_eq!(
            slot_target(&projects, &state, 3),
            None,
            "points at a worktree Grove is not listing"
        );

        // A collapsed project still holds its rows: the number is about the
        // worktree, not about what the list happens to be showing.
        let mut collapsed = projects.clone();
        collapsed[0].is_expanded = false;
        state.assign_slot(3, &collapsed[0].worktrees[0].id);
        assert!(slot_target(&collapsed, &state, 3).is_some());
    }

    #[test]
    fn alt_and_a_digit_is_what_assigns_a_number() {
        assert_eq!(digit_press(egui::Key::Num3, egui::Modifiers::ALT), Some(3));
        assert_eq!(digit_press(egui::Key::Num9, egui::Modifiers::ALT), Some(9));
        // Without Alt the digits belong to the filter field.
        assert_eq!(digit_press(egui::Key::Num3, egui::Modifiers::NONE), None);
        assert_eq!(digit_press(egui::Key::Num3, egui::Modifiers::COMMAND), None);
        // Zero is not a number a worktree can carry.
        assert_eq!(digit_press(egui::Key::Num0, egui::Modifiers::ALT), None);
    }

    /// Run one headless frame carrying a single key press, and ask what
    /// `pressed_digit` made of it.
    fn digit_press(key: egui::Key, modifiers: egui::Modifiers) -> Option<u8> {
        let ctx = egui::Context::default();
        let mut digit = None;
        let _ = ctx.run(
            egui::RawInput {
                // `InputState::modifiers` comes from here, not from the events.
                modifiers,
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            },
            |ctx| digit = ctx.input(pressed_digit),
        );
        digit
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

    // ------------------------------------------------------- reconciliation

    #[test]
    fn project_refs_carry_the_repository_identity_reconciliation_needs() {
        let projects = vec![project("p1", "acme", &["main"])];
        let refs = project_refs(&projects);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "p1");
        assert_eq!(refs[0].name, "acme");
        assert_eq!(refs[0].repository_path, PathBuf::from("/home/u/acme"));
        assert_eq!(
            refs[0].git_common_dir,
            PathBuf::from("/home/u/acme/.git"),
            "matching is by repository identity, not by name"
        );
        assert!(project_refs(&[]).is_empty());
    }

    #[test]
    fn an_activation_is_described_by_the_route_it_took() {
        assert_eq!(
            describe(&Activation::SwitchedClient {
                session: "wt-a1b2c3".into(),
                client_tty: "/dev/pts/3".into(),
            }),
            "Switched /dev/pts/3 to wt-a1b2c3"
        );
        assert_eq!(
            describe(&Activation::LaunchedTerminal {
                session: "wt-a1b2c3".into(),
                command: "foot tmux".into(),
            }),
            "Launched a terminal on wt-a1b2c3"
        );
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
