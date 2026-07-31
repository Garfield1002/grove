//! The eframe application: state held for the UI, channel plumbing to the
//! worker, and the narrow vertical layout from direction 1c.
//!
//! What `GroveApp` still owns is what has to be here: the channels, the frame,
//! and the decisions that need several of the parts at once — which worker
//! message means what, which action a click or a keystroke is, and when a
//! launch may bring its agents back. The parts that own state of their own
//! have been lifted out:
//!
//! - [`rows`] — the project list and everything stamped onto it.
//! - [`selection`] — what is selected, and what the keyboard does to it.
//! - [`service_events`] — whether an event frame is worth applying at all.
//! - [`dialogs`] — the four detached windows and their forms. A seam in the
//!   file rather than in the ownership; that module says why.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use grove_core::Paths;
use grove_core::claude::HookChange;
use grove_core::config::Config;
use grove_core::ipc::Notification;
use grove_core::protocol::Event;
use grove_core::reconcile::{OrphanSession, Reconciliation};
use grove_core::state::State;
use grove_core::workflow::Activation;

use crate::status_watch::{Control, StatusWatch};
use crate::ui::chrome::Detached;
use crate::ui::dialogs::create_worktree::CreateForm;
use crate::ui::dialogs::open_project::OpenProjectForm;
use crate::ui::dialogs::removal::RemovalForm;
use crate::ui::{self, Action, theme};
use crate::workers::{ErrorReport, Message, Task, Workers};

mod dialogs;
mod rows;
mod selection;
mod service_events;

use dialogs::apply_picked;
use rows::Rows;
use selection::{Selection, pressed_digit};
use service_events::{ServiceEventAction, ServiceUpdate, classify_service_event};

pub struct GroveApp {
    paths: Paths,
    home: Option<PathBuf>,
    workers: Workers,
    /// The status poller and the `grove notify` listener (Milestone 4).
    watch: StatusWatch,
    messages: Receiver<Message>,
    /// The rows and everything stamped onto them. One owner for the project
    /// list, the state snapshot, and the three caches derived onto it.
    rows: Rows,
    /// Grove's hooks in Claude Code's settings, as the last check found them.
    /// `None` until one has run.
    claude_hooks: Option<HookChange>,
    /// Highest service event revision applied on this connection history.
    /// Replayed or reordered frames are ignored; a gap is healed by polling.
    last_service_revision: u64,

    config: Option<Config>,

    /// What the list has selected, and the filter that decides what it can
    /// walk.
    selection: Selection,
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

    /// A worktree Grove just created, selected as soon as a refresh lists it.
    pending_selection: Option<(String, PathBuf)>,
    /// The number `grove toggle <n>` started this process for, opened as soon
    /// as the first reconciliation says what that number points at.
    pending_toggle: Option<u8>,
    /// Whether Git/tmux reconciliation has produced the rows a numbered
    /// toggle resolves against. Service-delivered toggles can arrive during
    /// GUI startup and must wait just like a toggle that launched the process.
    reconciled_once: bool,
    /// Whether this launch has already asked to bring its agents back. One
    /// pass per process: reconciliation also runs on refresh, on adopting an
    /// orphan and on closing one, and none of those is a restart.
    agents_resumed: bool,
    /// The detached windows. The main window is a narrow sliver, so these
    /// render as their own toplevels (`ui::chrome`), one of each.
    open_project: Detached<OpenProjectForm>,
    create: Detached<CreateForm>,
    removal: Detached<RemovalForm>,
    settings: Detached<ui::settings::Form>,
}

impl GroveApp {
    pub fn new(cc: &eframe::CreationContext<'_>, paths: Paths, pending_toggle: Option<u8>) -> Self {
        theme::apply(&cc.egui_ctx);
        if let Err(error) = crate::service::ensure_running(&paths) {
            // The GUI and tmux sessions remain usable. Agent attention still
            // has its durable tmux marker, but live reports and toggles will
            // not be relayed until a service can start.
            eprintln!("grove: could not start the local service: {error}");
        }
        let (workers, messages) = Workers::start(paths.clone(), cc.egui_ctx.clone());
        let watch = StatusWatch::start(&paths, workers.message_sender(), cc.egui_ctx.clone());

        // The daemon is the only reader and writer of state.toml in
        // production. The GUI starts empty and receives an authoritative
        // state snapshot before it asks the daemon to reconcile Git and tmux.
        workers.send(Task::LoadState);
        workers.send(Task::LoadConfig);

        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            paths,
            workers,
            watch,
            messages,
            rows: Rows::default(),
            claude_hooks: None,
            last_service_revision: 0,
            config: None,
            selection: Selection::default(),
            status: None,
            errors: Vec::new(),
            orphans: Vec::new(),
            ignored_orphans: 0,
            orphan_armed: None,
            shutdown_armed: false,
            quit_after_kill: false,
            pending_selection: None,
            pending_toggle,
            reconciled_once: false,
            agents_resumed: false,
            open_project: Detached::default(),
            create: Detached::default(),
            removal: Detached::default(),
            settings: Detached::default(),
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Message::StateLoaded(state) => {
                    self.apply_daemon_state(*state, true);
                    self.reconcile();
                }
                Message::StateUpdated {
                    state,
                    status,
                    reconcile,
                } => {
                    self.apply_daemon_state(*state, false);
                    self.status = Some(status);
                    if reconcile {
                        self.reconcile();
                    }
                }
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
                    &mut self.open_project,
                    &mut self.create,
                    &mut self.settings,
                ),
                Message::ProjectOpened(project) => {
                    self.status = Some(self.rows.add_project(*project));
                }
                Message::WorktreesRefreshed {
                    project_id,
                    worktrees,
                } => {
                    self.rows.refresh_worktrees(&project_id, worktrees);
                    // Select a worktree Grove has just created, once the
                    // refreshed list actually contains it.
                    if let Some((pending_project, path)) = &self.pending_selection
                        && pending_project == &project_id
                        && let Some(project) = self.rows.project(&project_id)
                        && let Some(worktree) = project.worktrees.iter().find(|w| &w.path == path)
                    {
                        self.selection.select(worktree.id.clone());
                        self.pending_selection = None;
                    }
                    self.describe_worktrees();
                }
                Message::StatusesRefreshed {
                    project_id,
                    statuses,
                } => self.rows.apply_git_statuses(&project_id, &statuses),
                Message::SessionsRefreshed { presence, windows } => {
                    self.rows.set_windows(windows);
                    self.rows.apply_presence(&presence);
                }
                Message::Reconciled { result, state } => {
                    self.apply_reconciliation(*result, *state);
                }
                Message::SessionOpened { activation } => {
                    self.status = Some(describe(&activation));
                }
                Message::Associated {
                    worktree_id,
                    session,
                } => {
                    self.status = Some(format!("{session} is now this worktree's session."));
                    self.selection.select(worktree_id);
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
                        self.selection.select(first.clone());
                    }
                    self.watch.send(Control::PollNow);
                }
                Message::AgentStarted { worktree_id, unit } => {
                    self.selection.select(worktree_id);
                    self.status = Some(match unit {
                        Some(unit) => format!("Started the agent in {unit}"),
                        None => "Started the agent".to_string(),
                    });
                    self.watch.send(Control::PollNow);
                }
                Message::GitStatusDue => self.refresh_git_statuses(),
                Message::StatusPolled(statuses) => self.rows.set_statuses(statuses),
                Message::WindowsPolled(windows) => self.rows.set_windows(windows),
                Message::Toggled { slot } => {
                    if let Some(slot) = slot
                        && !self.reconciled_once
                    {
                        self.pending_toggle = Some(slot);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    } else {
                        self.apply_toggle(ctx, slot);
                    }
                }
                Message::Notified(notification) => self.apply_notification(&notification),
                Message::ServiceEvent(event) => self.apply_service_event(*event),
                Message::ServiceEventsStarted { revision } => {
                    self.last_service_revision = revision;
                }
                Message::ServiceEventsUnavailable => self.watch.send(Control::PollNow),
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
                        self.rows.forget(&form.worktree_id);
                        // A session the *user* closed is not a stopped
                        // session: forget the mapping, or the row would go on
                        // offering to bring back what was just dismissed.
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
                    self.selection.select(worktree_id);
                }
                Message::WindowOpened {
                    worktree_id,
                    window,
                } => {
                    self.selection.select(worktree_id);
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

    /// Re-read every project's working-tree status, on the poller's cadence.
    ///
    /// Queued on the worker, never run here: this is one `git status` per
    /// worktree.
    fn refresh_git_statuses(&self) {
        for project in self.rows.projects() {
            if project.worktrees.is_empty() {
                continue;
            }
            self.workers.send(Task::RefreshStatuses {
                project_id: project.id.clone(),
            });
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
        match self.rows.slot_target(slot) {
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
        if self.rows.state().slot(worktree_id) == Some(slot) {
            self.workers.send(Task::ClearSlot {
                worktree_id: worktree_id.to_string(),
            });
        } else if (1..=grove_core::state::MAX_SLOT).contains(&slot) {
            self.workers.send(Task::AssignSlot {
                number: slot,
                worktree_id: worktree_id.to_string(),
            });
        }
    }

    /// An explicit `grove notify` report: show its message straight away.
    ///
    /// The status itself is left to the poller, which re-reads tmux a moment
    /// later — except for attention, which is latched in the engine and would
    /// otherwise not show until that poll lands.
    fn apply_notification(&mut self, notification: &Notification) {
        self.watch.notified(notification);
        self.rows.apply_notification(notification);
    }

    fn apply_service_event(&mut self, event: Event) {
        let (revision, update, gap) =
            match classify_service_event(self.last_service_revision, event) {
                ServiceEventAction::Ignore => return,
                ServiceEventAction::Recover(error) => {
                    eprintln!("grove: invalid service event: {error}");
                    self.watch.send(Control::PollNow);
                    return;
                }
                ServiceEventAction::Apply {
                    revision,
                    update,
                    gap,
                } => (revision, update, gap),
            };
        if gap {
            // The bounded queue deliberately drops a slow subscriber. If a
            // future transport replays across that gap, polling still makes
            // git and tmux authoritative rather than trusting a partial log.
            self.watch.send(Control::PollNow);
        }
        self.last_service_revision = revision;
        match update {
            ServiceUpdate::State(state) => self.apply_daemon_state(state, false),
            ServiceUpdate::Reconciliation {
                reconciliation,
                state,
            } => self.apply_reconciliation(reconciliation, state),
            ServiceUpdate::Notification(notification) => self.apply_notification(&notification),
            ServiceUpdate::ControlCompleted => {
                self.workers.send(Task::RefreshSessions);
                self.watch.send(Control::PollNow);
            }
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
    fn clear_attention(&mut self, worktree_id: &str) {
        if !self.watch.opened(worktree_id) {
            return;
        }
        self.rows.clear_attention(worktree_id);
        self.workers.send(Task::ClearAttention {
            worktree_id: worktree_id.to_string(),
            idempotency_key: format!("gui-clear-attention-{}", grove_core::agent::nonce()),
        });
    }

    /// Tell the poller what to call each worktree in a desktop notification.
    fn describe_worktrees(&self) {
        self.watch.send(Control::Describe(self.rows.labels()));
    }

    /// Replace the GUI's read-only cache with the state returned by the
    /// daemon, and close a removal window whose project it no longer lists.
    fn apply_daemon_state(&mut self, state: State, bootstrap: bool) {
        self.rows.apply_daemon_state(state, bootstrap);
        if self
            .removal
            .get()
            .is_some_and(|form| self.rows.state().find(&form.project_id).is_none())
        {
            self.removal.close();
        }
    }

    /// Ask the worker for a reconciliation pass (ARCHITECTURE.md §7). This is
    /// what the header's Restore control, Ctrl+R with no row selected, and
    /// startup all do.
    fn reconcile(&mut self) {
        self.workers.send(Task::Reconcile {
            projects: self.rows.project_refs(),
        });
        self.status = Some("Reconciling with git and tmux…".to_string());
    }

    /// Apply one reconciliation pass to the UI and the index.
    ///
    /// It marks and it records; it removes nothing. An unavailable project
    /// keeps its record *and* its last known rows — a project on an unplugged
    /// drive must not look as though its worktrees were deleted.
    fn apply_reconciliation(&mut self, result: Reconciliation, state: State) {
        self.reconciled_once = true;
        let summary = result.summary();
        self.rows.apply_reconciliation(result.projects, state);
        self.orphans = result.orphans;
        self.ignored_orphans = result.ignored;
        if self
            .orphan_armed
            .as_ref()
            .is_some_and(|name| !self.orphans.iter().any(|o| &o.name == name))
        {
            self.orphan_armed = None;
        }
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
        let Some(_config) = self.config.as_ref() else {
            return;
        };
        // The service reads the authoritative config and state. The GUI only
        // supplies a per-launch idempotency key so a lost response cannot
        // start a conversation twice.
        self.agents_resumed = true;
        self.workers.send(Task::ResumeAgents {
            idempotency_key: format!("gui-launch-{}", grove_core::agent::nonce()),
        });
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
        if let Some(worktree) = self.rows.worktree(project_id, worktree_id) {
            self.workers.send(Task::StartAgent {
                worktree_id: worktree.id.clone(),
                resume,
                idempotency_key: format!("gui-agent-start-{}", grove_core::agent::nonce()),
            });
            self.selection.select(worktree_id.to_string());
        }
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::ToggleProject(id) => {
                if let Some(project) = self.rows.project(&id) {
                    self.workers.send(Task::SetProjectExpanded {
                        project_id: id,
                        expanded: !project.is_expanded,
                    });
                }
            }
            Action::RefreshProject(id) => {
                if let Some(project) = self.rows.project(&id) {
                    self.workers.send(Task::RefreshProject {
                        project_id: project.id.clone(),
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
                if let Some(worktree) = self.rows.worktree(&project_id, &worktree_id) {
                    self.workers.send(Task::Activate {
                        worktree_id: worktree.id.clone(),
                        idempotency_key: format!("gui-session-open-{}", grove_core::agent::nonce()),
                    });
                    self.clear_attention(&worktree_id);
                    self.selection.select(worktree_id);
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
            } => match self.rows.state().agent(&worktree_id) {
                Some(record) if !record.session_id.is_empty() => {
                    let resume = record.session_id.clone();
                    self.start_agent(&project_id, &worktree_id, Some(resume));
                }
                _ => {
                    self.status =
                        Some("No agent conversation has been reported for this worktree.".into());
                }
            },
            Action::OpenAgentTranscript { worktree_id } => {
                match self.rows.state().agent(&worktree_id) {
                    Some(record) if record.has_transcript() => {
                        self.workers
                            .send(Task::OpenWithDesktop(record.transcript_path.clone()));
                    }
                    _ => {
                        self.status =
                            Some("No transcript has been reported for this worktree.".into());
                    }
                }
            }
            Action::SelectWorktree { worktree_id, .. } => self.selection.select(worktree_id),
            Action::SetWorktreeSlot { worktree_id, slot } => match slot {
                Some(slot) => self.set_slot(&worktree_id, slot),
                None => {
                    if self.rows.state().slot(&worktree_id).is_some() {
                        self.workers.send(Task::ClearSlot { worktree_id });
                    }
                }
            },
            Action::OpenInNewTerminal {
                project_id,
                worktree_id,
            } => {
                if let Some(worktree) = self.rows.worktree(&project_id, &worktree_id) {
                    let target = worktree.id.clone();
                    self.selection.select(worktree_id);
                    self.workers.send(Task::OpenInNewTerminal {
                        worktree_id: target,
                        idempotency_key: format!(
                            "gui-additional-terminal-{}",
                            grove_core::agent::nonce()
                        ),
                    });
                }
            }
            Action::OpenNewWindow {
                project_id,
                worktree_id,
            } => {
                if let Some(worktree) = self.rows.worktree(&project_id, &worktree_id) {
                    let target = worktree.id.clone();
                    self.selection.select(worktree_id);
                    self.workers.send(Task::OpenNewWindow {
                        worktree_id: target,
                        idempotency_key: format!(
                            "gui-session-window-create-{}",
                            grove_core::agent::nonce()
                        ),
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
                if let Some(worktree) = self.rows.worktree(&project_id, &worktree_id) {
                    let target = worktree.id.clone();
                    self.workers.send(Task::ActivateWindow {
                        worktree_id: target,
                        window_index,
                        idempotency_key: format!(
                            "gui-session-window-open-{}",
                            grove_core::agent::nonce()
                        ),
                    });
                    self.clear_attention(&worktree_id);
                    self.selection.select_window(worktree_id, window_index);
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
                if let Some(project) = self.rows.project(&id) {
                    let at = project.repository_path.display().to_string();
                    let name = project.name.clone();
                    self.open_project.open(OpenProjectForm::at(at));
                    self.status = Some(format!("Point Grove at {name} where it lives now."));
                }
            }
        }
    }

    /// One choice about one orphaned session (DESIGN.md §11). Exactly one, and
    /// closing is armed before it happens.
    fn apply_orphan_action(&mut self, action: ui::orphans::OrphanAction) {
        use ui::orphans::OrphanAction;
        match action {
            OrphanAction::Open(session) => {
                self.workers.send(Task::OpenSession {
                    session,
                    idempotency_key: format!(
                        "gui-open-orphan-session-{}",
                        grove_core::agent::nonce()
                    ),
                });
            }
            OrphanAction::Associate {
                session,
                project_id,
                worktree_id,
            } => {
                if let Some(worktree) = self.rows.worktree(&project_id, &worktree_id) {
                    self.workers.send(Task::AssociateSession {
                        worktree_id: worktree.id.clone(),
                        session,
                        idempotency_key: format!(
                            "gui-associate-session-{}",
                            grove_core::agent::nonce()
                        ),
                    });
                }
            }
            // The first click arms; only the second one closes anything.
            OrphanAction::Close(session) => {
                if self.orphan_armed.as_deref() == Some(session.as_str()) {
                    self.workers.send(Task::CloseOrphan {
                        session,
                        idempotency_key: format!("gui-close-orphan-{}", grove_core::agent::nonce()),
                    });
                } else {
                    self.status = Some(format!(
                        "Choose “Confirm: close {session}” to end that session."
                    ));
                    self.orphan_armed = Some(session);
                }
            }
            OrphanAction::Ignore(session) => {
                self.workers.send(Task::IgnoreSession {
                    session: session.clone(),
                });
            }
            OrphanAction::ShowIgnored => {
                self.workers.send(Task::ClearIgnoredSessions);
            }
        }
    }

    /// Remove a project from Grove's index. Metadata only — this must never
    /// be accompanied by a git or tmux operation (ARCHITECTURE.md §8.1).
    fn remove_project(&mut self, id: &str) {
        self.workers.send(Task::RemoveProject {
            project_id: id.to_string(),
        });
    }

    fn selected_row(&self) -> Option<(String, String)> {
        self.selection.row(self.rows.projects())
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
        // The title bar has a close button, but the shortcut is worth keeping
        // anyway: Ctrl+Q quits from any of Grove's windows, and Ctrl+W on the
        // main window closes it, which for the only remaining window means
        // quitting too.
        let close = ctx.input(|i| {
            i.modifiers.command && (i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::W))
        });
        if close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        if self.removal.is_open() {
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
            self.selection.move_by(self.rows.projects(), 1);
        }
        if up {
            self.selection.move_by(self.rows.projects(), -1);
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
        if new && let Some(project_id) = self.selection.context_project(self.rows.projects()) {
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

    /// The header bar: the filter field, and nothing else.
    ///
    /// The window's title, its close button and its drag handle are the
    /// compositor's now (`main` asks for decorations), so the app name here
    /// would only repeat the title bar above it. Restore stays on Ctrl+R and
    /// opening a project on the footer's entry, each of which already had a
    /// home outside this bar.
    fn header(&mut self, ui: &mut egui::Ui) {
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
                self.workers.send(Task::KillServer {
                    idempotency_key: format!("gui-stop-server-{}", grove_core::agent::nonce()),
                });
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
            .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
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
                        egui::TextEdit::singleline(&mut self.selection.filter)
                            .frame(false)
                            .font(egui::FontId::proportional(theme::FONT_BODY))
                            .hint_text(theme::hint("Filter worktrees…"))
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
                // Like the gear: a second click raises the window already
                // open rather than discarding what has been typed into it.
                if self.open_project.is_open() {
                    self.open_project.request_focus();
                } else {
                    self.open_project.open(OpenProjectForm::empty());
                }
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

        self.open_project_window(ctx);
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
                            self.rows.projects(),
                            ui::project_list::Tree {
                                selected: self.selection.worktree(),
                                selected_window: self.selection.window(),
                                filter: &self.selection.filter,
                                home: self.home.as_deref(),
                                slots: &self.rows.state().slots,
                                agents: &self.rows.state().agents,
                                can_resume: self.can_resume_agents(),
                            },
                        );
                        orphan_action = ui::orphans::show(
                            ui,
                            &self.orphans,
                            self.ignored_orphans,
                            self.orphan_armed.as_deref(),
                            self.rows.projects(),
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
        egui::Stroke::new(1.0_f32, theme::HAIRLINE),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
