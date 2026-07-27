//! Orchestration of the Milestone 1 flows.
//!
//! These functions run subprocesses and must only be called from a worker
//! thread. They live in the core crate so the UI layer stays a thin renderer
//! and so the sequencing is testable without a display.

use std::collections::HashMap;
use std::path::Path;

use crate::agent;
use crate::cgroup;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::git::{self, StatusSummary, WorktreeAdd};
use crate::ids;
use crate::model::{
    Project, SessionPresence, WindowNote, Worktree, default_worktree_parent, worktrees_from_entries,
};
use crate::notice::Notices;
use crate::removal::{RemovalInputs, Unpushed};
use crate::status::{SessionReport, SessionSignals};
use crate::terminal::{self, TemplateVars};
use crate::tmux::{self, SessionSpec, TmuxServer};

/// Register the project containing `path`, with its worktrees and current
/// session presence.
pub fn open_project(server: &TmuxServer, config: &Config, path: &Path) -> Result<Project> {
    let discovery = git::discover_project(path)?;
    let id = ids::project_id(&discovery.git_common_dir);
    let worktrees = worktrees_from_entries(&discovery.worktrees, &id, &discovery.git_common_dir);
    let default_worktree_path =
        default_worktree_parent(config.default_worktree_parent(), &discovery.repository_path);
    let mut project = Project {
        id,
        name: discovery.name,
        repository_path: discovery.repository_path,
        git_common_dir: discovery.git_common_dir,
        default_worktree_path,
        is_expanded: true,
        worktrees,
        unavailable: None,
    };
    apply_session_presence(&mut project.worktrees, &session_presence(server)?);
    Ok(project)
}

/// Read the working-tree status of every worktree, keyed by worktree id
/// (DESIGN.md §18).
///
/// Bare and missing worktrees have no working tree, so they are skipped
/// rather than reported as errors, and a single failing worktree does not
/// hide the others: this runs on the worker to keep sublabels fresh, not as
/// part of any operation the user is waiting on.
///
/// Runs subprocesses: worker thread only.
pub fn worktree_statuses(worktrees: &[Worktree]) -> HashMap<String, StatusSummary> {
    let mut statuses = HashMap::new();
    for worktree in worktrees {
        if worktree.is_bare || !worktree.path.is_dir() {
            continue;
        }
        if let Ok(status) = git::status_summary(&worktree.path) {
            statuses.insert(worktree.id.clone(), status);
        }
    }
    statuses
}

/// Stamp statuses onto a worktree list, leaving worktrees with no reading
/// untouched.
pub fn apply_statuses(worktrees: &mut [Worktree], statuses: &HashMap<String, StatusSummary>) {
    for worktree in worktrees {
        if let Some(status) = statuses.get(&worktree.id) {
            worktree.git_status = Some(status.clone());
        }
    }
}

/// Create a worktree, then report where it landed (DESIGN.md §10).
///
/// Git's own stderr survives a failure untouched, which is what the create
/// dialog shows.
///
/// Runs a subprocess: worker thread only.
pub fn create_worktree(repository_path: &Path, add: &WorktreeAdd) -> Result<std::path::PathBuf> {
    git::worktree_add(repository_path, add)
}

/// Gather everything the safe-removal dialog must display *before* offering
/// any destructive operation (DESIGN.md §13).
///
/// Best effort by design: a worktree whose directory has vanished, or whose
/// branch tracks nothing, still produces a report — with the unknowns named
/// as unknown. Nothing here removes, kills or deletes anything.
///
/// Runs subprocesses: worker thread only.
pub fn removal_inputs(server: &TmuxServer, worktree: &Worktree) -> Result<RemovalInputs> {
    let status = git::status_summary(&worktree.path).ok();

    let unpushed = match &status {
        Some(status) => match &status.upstream {
            Some(upstream) => match git::status::unpushed_count(&worktree.path, upstream) {
                Ok(count) => Unpushed::Count(count),
                Err(e) => Unpushed::Unknown(e.to_string()),
            },
            None if status.detached => Unpushed::Unknown("HEAD is detached".to_string()),
            None => Unpushed::NoUpstream,
        },
        None => Unpushed::Unknown("the worktree status could not be read".to_string()),
    };

    let session_name = worktree.session_name();
    let session = tmux::has_session(server, &session_name)?.then_some(session_name.clone());
    let panes = match &session {
        Some(session) => tmux::list_panes(server, session)?,
        None => Vec::new(),
    };

    Ok(RemovalInputs {
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        is_main: worktree.is_main,
        is_locked: worktree.is_locked,
        lock_reason: worktree.lock_reason.clone(),
        status,
        unpushed,
        session,
        panes,
    })
}

/// Re-read a project's worktrees from git and its sessions from tmux.
pub fn refresh_project(
    server: &TmuxServer,
    repository_path: &Path,
    project_id: &str,
    git_common_dir: &Path,
) -> Result<Vec<Worktree>> {
    let entries = git::worktree_list(repository_path)?;
    let mut worktrees = worktrees_from_entries(&entries, project_id, git_common_dir);
    for worktree in &mut worktrees {
        // A worktree git still lists whose directory has gone is *unavailable*
        // — the same mark reconciliation makes, so a refresh after a git
        // operation cannot quietly drop it (DESIGN.md §11).
        worktree.is_missing = !worktree.is_bare && !worktree.path.is_dir();
    }
    apply_session_presence(&mut worktrees, &session_presence(server)?);
    Ok(worktrees)
}

/// Session presence on the private server, keyed by tmux session name.
pub fn session_presence(server: &TmuxServer) -> Result<HashMap<String, SessionPresence>> {
    Ok(tmux::list_sessions(server)?
        .into_iter()
        .map(|session| {
            let presence = if session.attached > 0 {
                SessionPresence::Attached
            } else {
                SessionPresence::Detached
            };
            (session.name, presence)
        })
        .collect())
}

/// Stamp session presence onto a worktree list.
pub fn apply_session_presence(
    worktrees: &mut [Worktree],
    presence: &HashMap<String, SessionPresence>,
) {
    for worktree in worktrees {
        worktree.session = presence
            .get(&worktree.session_name())
            .copied()
            .unwrap_or(SessionPresence::None);
    }
}

/// Every Grove session's windows, keyed by tmux session name.
///
/// Derived from one `list-panes -a`, so this is a single subprocess for the
/// whole server however many worktrees are open.
///
/// Runs subprocesses: worker thread only.
pub fn session_windows(server: &TmuxServer) -> Result<HashMap<String, Vec<tmux::WindowInfo>>> {
    Ok(group_windows(tmux::windows_of(
        &tmux::session::list_all_panes(server)?,
    )))
}

/// Group a flat window listing by session name.
pub fn group_windows(windows: Vec<tmux::WindowInfo>) -> HashMap<String, Vec<tmux::WindowInfo>> {
    let mut by_session: HashMap<String, Vec<tmux::WindowInfo>> = HashMap::new();
    for window in windows {
        by_session
            .entry(window.session.clone())
            .or_default()
            .push(window);
    }
    by_session
}

/// Stamp each worktree's tmux windows onto a worktree list.
///
/// A worktree whose session is not in the map loses its windows: the session
/// has gone, and leaving stale child rows in the tree would offer the user
/// windows that are not there any more.
pub fn apply_session_windows(
    worktrees: &mut [Worktree],
    windows: &HashMap<String, Vec<tmux::WindowInfo>>,
) {
    for worktree in worktrees {
        worktree.windows = windows
            .get(&worktree.session_name())
            .cloned()
            .unwrap_or_default();
    }
}

/// Stamp what each window reported about itself onto a worktree list.
///
/// Notes are dropped for a worktree whose session is gone, exactly as statuses
/// are: a sentence explaining what an agent was waiting for is not something to
/// keep showing beside a session that no longer exists. Notes naming a window
/// tmux no longer lists go too, so a closed window cannot leave the rest of the
/// tree looking like it reports per window when nothing does any more.
pub fn apply_window_notes(worktrees: &mut [Worktree], notices: &Notices) {
    for worktree in worktrees {
        if !worktree.session.exists() {
            worktree.window_notes.clear();
            continue;
        }
        let known: Vec<u32> = worktree.windows.iter().map(|window| window.index).collect();
        worktree.window_notes = notices
            .windows(&worktree.id)
            // Before the first poll a worktree has no window list at all; that
            // is "not known yet", not "no windows", so nothing is filtered.
            .filter(|(index, _)| known.is_empty() || known.contains(index))
            .map(|(index, notice)| WindowNote {
                index,
                status: notice.state,
                message: notice.message.clone(),
            })
            .collect();
    }
}

/// Which agent command to start in a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStart<'a> {
    /// The configured `[agents] command`: a new conversation.
    Fresh,
    /// The configured `[agents] resume_command`, carrying the conversation id
    /// the agent last reported through `grove notify`.
    Resume(&'a str),
}

impl<'a> AgentStart<'a> {
    /// The template this start uses, and the id to substitute into it.
    ///
    /// Resuming is refused rather than quietly downgraded to a fresh start:
    /// the user asked to continue a conversation, and silently beginning a new
    /// one would look identical and lose their place.
    fn template<'c>(self, config: &'c Config, project_name: &str) -> Result<(&'c str, &'a str)> {
        match self {
            AgentStart::Fresh => config
                .agents
                .command_for(project_name)
                .map(|template| (template, ""))
                .ok_or(Error::NoAgentCommand),
            AgentStart::Resume("") => Err(Error::NoAgentSession),
            AgentStart::Resume(id) => config
                .agents
                .resume_command()
                .map(|template| (template, id))
                .ok_or(Error::NoResumeCommand),
        }
    }
}

/// Start the configured agent in a worktree's session (DESIGN.md §7).
///
/// The session is ensured first: starting an agent is also a reasonable way to
/// open a worktree, and a `new-window` against a session that does not exist
/// would simply fail. The agent gets its own window, so closing it leaves the
/// shell — and the session — alone.
///
/// Runs subprocesses: worker thread only.
pub fn start_agent(
    server: &TmuxServer,
    config: &Config,
    runtime_dir: &Path,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
    start: AgentStart,
) -> Result<agent::AgentLaunch> {
    let (template, agent_session) = start.template(config, project_name)?;
    if !worktree.path.is_dir() {
        return Err(Error::WorktreeMissing(worktree.path.clone()));
    }
    let spec = session_spec(project_name, git_common_dir, worktree);
    let (session, _) = tmux::ensure_session(server, &spec)?;

    let vars = TemplateVars::new(
        server.socket(),
        &session,
        &worktree.path,
        project_name,
        worktree.branch.as_deref().unwrap_or_default(),
    )
    .with_agent_session(agent_session);
    let launch = agent::launch(
        template,
        &vars,
        &session,
        &worktree.path,
        &worktree.id,
        config.agents.accounting(),
        agent::systemd_available(runtime_dir),
    )?;
    server.run(launch.args.clone())?;
    Ok(launch)
}

/// Stamp polled session statuses onto a worktree list.
///
/// A worktree with no session gets no status at all, whatever the map says:
/// a status left over from a session that has since been closed would show a
/// row as working with nothing running in it.
pub fn apply_session_status(worktrees: &mut [Worktree], reports: &HashMap<String, SessionReport>) {
    for worktree in worktrees {
        let report = worktree
            .session
            .exists()
            .then(|| reports.get(&worktree.id))
            .flatten();
        worktree.status = report.map(|r| r.status);
        worktree.resources = report.and_then(SessionReport::resource_label);
        if worktree.status.is_none() {
            worktree.status_message = None;
        }
    }
}

/// Seconds since the Unix epoch, for comparing against tmux's timestamps.
///
/// A clock before the epoch is not a case worth an error: it yields 0, which
/// makes every session look stale rather than making the poll fail.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Gather one poll's status signals for every Grove session, keyed by
/// worktree id (DESIGN.md §6).
///
/// Two subprocesses for the whole server — one `list-sessions`, one
/// `list-panes -a` — because this runs every couple of seconds. Sessions
/// Grove did not create are ignored: their status is not Grove's business.
///
/// Runs subprocesses: worker thread only.
pub fn poll_session_signals(
    server: &TmuxServer,
    now_epoch: u64,
) -> Result<HashMap<String, SessionSignals>> {
    let sessions = tmux::list_sessions(server)?;
    let mut panes: HashMap<String, Vec<tmux::session::PaneInfo>> = HashMap::new();
    for pane in tmux::session::list_all_panes(server)? {
        panes.entry(pane.session.clone()).or_default().push(pane);
    }
    let mut signals = HashMap::new();
    for session in sessions {
        let Some(worktree_id) = session.worktree_id().map(str::to_string) else {
            continue;
        };
        let session_panes = panes.remove(&session.name).unwrap_or_default();
        let usage = scope_usage(
            Path::new("/proc"),
            Path::new(cgroup::CGROUP_ROOT),
            &session_panes,
        );
        let windows = tmux::windows_of(&session_panes);
        let commands = session_panes.into_iter().map(|p| p.command).collect();
        let mut signal = session.signals(now_epoch, commands);
        signal.usage = usage;
        signal.windows = windows;
        signals.insert(worktree_id, signal);
    }
    Ok(signals)
}

/// Sum the resource usage of a session's Grove agent scopes.
///
/// Only Grove's own scopes are counted. The shell's cgroup is the terminal's,
/// shared with much of the desktop, and reporting its memory beside a
/// worktree's name would be actively misleading. A session with no scoped
/// agent therefore reports `None`, not zero.
///
/// Reads `/proc` and `/sys`: cheap file reads, but still worker-thread work.
pub fn scope_usage(
    proc_root: &Path,
    cgroup_root: &Path,
    panes: &[tmux::session::PaneInfo],
) -> Option<cgroup::Usage> {
    let mut seen = std::collections::BTreeSet::new();
    for pane in panes {
        if let Some(path) = cgroup::cgroup_of_pid(proc_root, pane.pid)
            && cgroup::is_grove_scope(&path)
        {
            // Panes can share a cgroup; counting it twice would double the
            // memory figure.
            seen.insert(path);
        }
    }
    let mut total: Option<cgroup::Usage> = None;
    for path in seen {
        if let Some(usage) = cgroup::read_usage(cgroup_root, &path) {
            total = Some(total.unwrap_or_default().plus(usage));
        }
    }
    total
}

/// What activating a worktree actually did, so the UI can say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// An attached client was retargeted at the session.
    SwitchedClient { session: String, client_tty: String },
    /// No client was attached, so a terminal was launched.
    LaunchedTerminal { session: String, command: String },
}

impl Activation {
    pub fn session(&self) -> &str {
        match self {
            Activation::SwitchedClient { session, .. }
            | Activation::LaunchedTerminal { session, .. } => session,
        }
    }
}

/// The session Grove would create for a worktree of a project.
pub fn session_spec(project_name: &str, git_common_dir: &Path, worktree: &Worktree) -> SessionSpec {
    SessionSpec {
        worktree_id: worktree.id.clone(),
        worktree_path: worktree.path.clone(),
        project_name: project_name.to_string(),
        git_common_dir: git_common_dir.to_path_buf(),
    }
}

/// Open a worktree (DESIGN.md §5): verify the worktree still exists, ensure
/// its session exists, then switch the primary client if one is attached or
/// launch the configured terminal if not.
pub fn activate_worktree(
    server: &TmuxServer,
    config: &Config,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
) -> Result<Activation> {
    if !worktree.path.is_dir() {
        return Err(Error::WorktreeMissing(worktree.path.clone()));
    }
    let spec = session_spec(project_name, git_common_dir, worktree);
    let (session, _created) = tmux::ensure_session(server, &spec)?;
    attach_or_launch(server, config, project_name, worktree, session)
}

/// Open one tmux window of a worktree's session (DESIGN.md §5).
///
/// The same switch-or-launch as [`activate_worktree`], with the requested
/// window made current first: both branches then land the user on that window,
/// whether an attached client is retargeted or a terminal is launched.
pub fn activate_window(
    server: &TmuxServer,
    config: &Config,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
    window_index: u32,
) -> Result<Activation> {
    if !worktree.path.is_dir() {
        return Err(Error::WorktreeMissing(worktree.path.clone()));
    }
    let spec = session_spec(project_name, git_common_dir, worktree);
    let (session, _created) = tmux::ensure_session(server, &spec)?;
    // A window that has gone since the last poll is not an error: opening the
    // session on whatever window it still has beats refusing to open it.
    tmux::select_window(server, &format!("{session}:{window_index}"))?;
    attach_or_launch(server, config, project_name, worktree, session)
}

/// Switch the primary client to a session, or launch a terminal on it when no
/// client is attached. The session must already exist.
fn attach_or_launch(
    server: &TmuxServer,
    config: &Config,
    project_name: &str,
    worktree: &Worktree,
    session: String,
) -> Result<Activation> {
    let clients = tmux::list_clients(server)?;
    if let Some(client) = tmux::primary_client(&clients) {
        tmux::switch_client(server, client, &session)?;
        return Ok(Activation::SwitchedClient {
            session,
            client_tty: client.tty.to_string_lossy().into_owned(),
        });
    }

    if !config.has_terminal() {
        return Err(Error::EmptyTerminalTemplate);
    }
    let vars = TemplateVars::new(
        server.socket(),
        &session,
        &worktree.path,
        project_name,
        &worktree.label(),
    );
    let invocation = terminal::launch(&config.terminal.command, &vars)?;
    Ok(Activation::LaunchedTerminal {
        command: terminal::preview(&invocation),
        session,
    })
}

/// Open a session by name, without creating anything (DESIGN.md §11).
///
/// This is how an *orphaned* session is opened: it exists, it may hold work
/// the user wants to see, and Grove must be able to show it before the user
/// decides whether to associate or close it. `cwd` is only used to fill the
/// `{path}` template variable — the session already has its own directory.
///
/// Runs subprocesses: worker thread only.
pub fn open_session(
    server: &TmuxServer,
    config: &Config,
    session: &str,
    cwd: &Path,
) -> Result<Activation> {
    let clients = tmux::list_clients(server)?;
    if let Some(client) = tmux::primary_client(&clients) {
        tmux::switch_client(server, client, session)?;
        return Ok(Activation::SwitchedClient {
            session: session.to_string(),
            client_tty: client.tty.to_string_lossy().into_owned(),
        });
    }
    if !config.has_terminal() {
        return Err(Error::EmptyTerminalTemplate);
    }
    let vars = TemplateVars::new(server.socket(), session, cwd, "", "");
    let invocation = terminal::launch(&config.terminal.command, &vars)?;
    Ok(Activation::LaunchedTerminal {
        command: terminal::preview(&invocation),
        session: session.to_string(),
    })
}

/// Adopt an orphaned session as a worktree's session (DESIGN.md §11).
///
/// The session is renamed to `wt-<id>` and stamped with the `@grove_*`
/// options, so both reconciliation keys agree afterwards. Nothing is created
/// or killed: this is the same session, with the same panes and history,
/// under a name Grove can find again.
///
/// The worktree must already have no session of its own — associating over a
/// live one would leave two sessions fighting for the same name, which tmux
/// would refuse anyway.
///
/// Runs subprocesses: worker thread only.
pub fn associate_session(
    server: &TmuxServer,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
    orphan: &str,
) -> Result<String> {
    let spec = session_spec(project_name, git_common_dir, worktree);
    tmux::associate_session(server, orphan, &spec)
}

/// Attach an *additional* terminal client to a worktree's session without
/// retargeting the primary client (DESIGN.md §8).
///
/// Runs subprocesses: worker thread only.
pub fn open_in_new_terminal(
    server: &TmuxServer,
    config: &Config,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
) -> Result<Activation> {
    if !worktree.path.is_dir() {
        return Err(Error::WorktreeMissing(worktree.path.clone()));
    }
    if !config.has_terminal() {
        return Err(Error::EmptyTerminalTemplate);
    }
    let spec = session_spec(project_name, git_common_dir, worktree);
    let (session, _created) = tmux::ensure_session(server, &spec)?;
    let vars = TemplateVars::new(
        server.socket(),
        &session,
        &worktree.path,
        project_name,
        &worktree.label(),
    );
    let invocation = terminal::launch(&config.terminal.command, &vars)?;
    Ok(Activation::LaunchedTerminal {
        command: terminal::preview(&invocation),
        session,
    })
}

/// Open an extra shell window inside a worktree's tmux session (DESIGN.md §5).
///
/// This is the tmux-side counterpart of [`open_in_new_terminal`]: no terminal
/// emulator is launched, so an already-attached client simply sees a new
/// window appear. The session is ensured first, since asking for another
/// window is also a reasonable way to open a worktree.
///
/// Runs subprocesses: worker thread only.
pub fn open_new_window(
    server: &TmuxServer,
    project_name: &str,
    git_common_dir: &Path,
    worktree: &Worktree,
) -> Result<NewWindow> {
    if !worktree.path.is_dir() {
        return Err(Error::WorktreeMissing(worktree.path.clone()));
    }
    let spec = session_spec(project_name, git_common_dir, worktree);
    let (session, _created) = tmux::ensure_session(server, &spec)?;
    let window = tmux::session::new_window(server, &session, &worktree.path)?;
    Ok(NewWindow { session, window })
}

/// A shell window Grove opened inside a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWindow {
    pub session: String,
    /// The window index tmux reported, for the status line.
    pub window: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_RESUME_COMMAND;
    use crate::git::WorktreeEntry;
    use crate::status::SessionStatus;
    use std::path::PathBuf;

    fn worktree(path: &str) -> Worktree {
        Worktree::from_entry(
            &WorktreeEntry {
                path: PathBuf::from(path),
                branch: Some("main".into()),
                ..WorktreeEntry::default()
            },
            "p1",
            Path::new("/home/u/proj/.git"),
            true,
        )
    }

    #[test]
    fn presence_is_matched_by_deterministic_session_name() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        let mut presence = HashMap::new();
        presence.insert(worktrees[0].session_name(), SessionPresence::Attached);
        apply_session_presence(&mut worktrees, &presence);
        assert_eq!(worktrees[0].session, SessionPresence::Attached);
        assert_eq!(worktrees[1].session, SessionPresence::None);
    }

    fn window(session: &str, index: u32, name: &str) -> tmux::WindowInfo {
        tmux::WindowInfo {
            session: session.to_string(),
            index,
            name: name.to_string(),
            active: index == 0,
            bell: false,
        }
    }

    #[test]
    fn windows_are_matched_by_deterministic_session_name() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        let session = worktrees[0].session_name();
        let windows = group_windows(vec![
            window(&session, 0, "shell"),
            window(&session, 1, "agent"),
            window("scratch", 0, "shell"),
        ]);
        apply_session_windows(&mut worktrees, &windows);
        assert_eq!(worktrees[0].windows.len(), 2);
        assert_eq!(worktrees[0].windows[1].name, "agent");
        assert!(
            worktrees[1].windows.is_empty(),
            "a worktree with no session has no windows"
        );
    }

    #[test]
    fn windows_of_a_session_that_has_gone_are_dropped() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        let session = worktrees[0].session_name();
        apply_session_windows(
            &mut worktrees,
            &group_windows(vec![window(&session, 0, "shell")]),
        );
        apply_session_windows(&mut worktrees, &HashMap::new());
        assert!(
            worktrees[0].windows.is_empty(),
            "stale rows would offer windows that are not there"
        );
    }

    /// Resuming asks for a specific conversation. Every way of not having one
    /// is refused, because the alternative — starting a fresh conversation
    /// under the same menu entry — looks identical and loses the user's place.
    #[test]
    fn resuming_is_refused_rather_than_downgraded() {
        let mut config = Config::default();
        config.agents.command = "claude".into();

        // Blanked by a user whose agent cannot resume, or who does not want
        // the action offered.
        config.agents.resume_command = String::new();
        assert!(matches!(
            AgentStart::Resume("0f3a").template(&config, "acme-web"),
            Err(Error::NoResumeCommand)
        ));

        config.agents.resume_command = DEFAULT_RESUME_COMMAND.into();
        assert!(matches!(
            AgentStart::Resume("").template(&config, "acme-web"),
            Err(Error::NoAgentSession)
        ));
        assert_eq!(
            AgentStart::Resume("0f3a")
                .template(&config, "acme-web")
                .expect("configured"),
            ("claude --resume {agent_session}", "0f3a")
        );
    }

    /// The two commands are independent: one configured is not the other.
    #[test]
    fn a_fresh_start_never_uses_the_resume_command() {
        // The default config already carries a resume command and no other.
        let mut config = Config::default();
        assert!(matches!(
            AgentStart::Fresh.template(&config, "acme-web"),
            Err(Error::NoAgentCommand)
        ));

        config.agents.command = "claude".into();
        assert_eq!(
            AgentStart::Fresh
                .template(&config, "acme-web")
                .expect("configured"),
            ("claude", ""),
            "a fresh start carries no conversation id"
        );
    }

    fn window_report(id: &str, index: u32, message: &str) -> crate::ipc::Notification {
        crate::ipc::Notification::new(id, SessionStatus::Attention)
            .with_message(Some(message.to_string()))
            .with_window(Some(index))
    }

    #[test]
    fn window_notes_land_on_the_worktree_that_reported_them() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        worktrees[0].session = SessionPresence::Detached;
        worktrees[1].session = SessionPresence::Detached;
        let mut notices = Notices::default();
        notices.record(&window_report(&worktrees[0].id, 1, "needs permission"));

        apply_window_notes(&mut worktrees, &notices);
        assert_eq!(
            worktrees[0]
                .window_note(1)
                .and_then(|n| n.message.as_deref()),
            Some("needs permission")
        );
        assert_eq!(worktrees[0].window_note(0), None, "window 0 said nothing");
        assert!(
            !worktrees[1].reports_per_window(),
            "another worktree's report is not this one's"
        );
    }

    /// A message explains a state; with the session gone there is no state
    /// left for it to explain.
    #[test]
    fn notes_are_dropped_with_the_session() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].session = SessionPresence::Detached;
        let mut notices = Notices::default();
        notices.record(&window_report(&worktrees[0].id, 1, "needs permission"));
        apply_window_notes(&mut worktrees, &notices);
        assert!(worktrees[0].reports_per_window());

        worktrees[0].session = SessionPresence::None;
        apply_window_notes(&mut worktrees, &notices);
        assert!(!worktrees[0].reports_per_window());
    }

    #[test]
    fn a_note_for_a_window_tmux_no_longer_lists_is_dropped() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].session = SessionPresence::Detached;
        let session = worktrees[0].session_name();
        let mut notices = Notices::default();
        notices.record(&window_report(&worktrees[0].id, 1, "needs permission"));

        // Before the first poll there is no window list to check against, so
        // the note stands.
        apply_window_notes(&mut worktrees, &notices);
        assert!(worktrees[0].reports_per_window());

        // Once tmux has listed the windows, one that is not among them is gone.
        apply_session_windows(
            &mut worktrees,
            &group_windows(vec![window(&session, 0, "shell")]),
        );
        apply_window_notes(&mut worktrees, &notices);
        assert!(
            !worktrees[0].reports_per_window(),
            "the window that reported has been closed"
        );
    }

    #[test]
    fn statuses_are_matched_by_worktree_id_and_never_invented() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        let mut statuses = HashMap::new();
        statuses.insert(
            worktrees[0].id.clone(),
            StatusSummary {
                modified: 2,
                ..StatusSummary::default()
            },
        );
        statuses.insert("ffffff".to_string(), StatusSummary::default());
        apply_statuses(&mut worktrees, &statuses);
        assert_eq!(
            worktrees[0].git_status.as_ref().map(|s| s.modified),
            Some(2)
        );
        assert_eq!(
            worktrees[1].git_status, None,
            "a worktree with no reading must not be shown as clean"
        );
    }

    #[test]
    fn a_previous_status_survives_a_refresh_that_could_not_read_it() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].git_status = Some(StatusSummary {
            untracked: 1,
            ..StatusSummary::default()
        });
        apply_statuses(&mut worktrees, &HashMap::new());
        assert!(worktrees[0].git_status.is_some());
    }

    #[test]
    fn bare_and_missing_worktrees_are_not_asked_for_a_status() {
        let mut bare = worktree("/nonexistent-grove/bare");
        bare.is_bare = true;
        let missing = worktree("/nonexistent-grove/gone");
        // Neither runs git: both are skipped before any subprocess.
        assert!(worktree_statuses(&[bare, missing]).is_empty());
    }

    #[test]
    fn unrelated_sessions_are_ignored() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        let mut presence = HashMap::new();
        presence.insert("scratch".to_string(), SessionPresence::Attached);
        presence.insert("wt-ffffff".to_string(), SessionPresence::Detached);
        apply_session_presence(&mut worktrees, &presence);
        assert_eq!(worktrees[0].session, SessionPresence::None);
    }

    #[test]
    fn activating_a_missing_worktree_fails_before_touching_tmux() {
        let server = TmuxServer::new("/tmp/grove-test-never-used.sock");
        let err = activate_worktree(
            &server,
            &Config::default(),
            "proj",
            Path::new("/home/u/proj/.git"),
            &worktree("/nonexistent-grove/wt"),
        )
        .expect_err("worktree is gone");
        assert!(matches!(err, Error::WorktreeMissing(_)));
    }

    #[test]
    fn opening_a_new_terminal_on_a_missing_worktree_fails_before_touching_tmux() {
        let server = TmuxServer::new("/tmp/grove-test-never-used.sock");
        let err = open_in_new_terminal(
            &server,
            &Config::default(),
            "proj",
            Path::new("/home/u/proj/.git"),
            &worktree("/nonexistent-grove/wt"),
        )
        .expect_err("worktree is gone");
        assert!(matches!(err, Error::WorktreeMissing(_)));
    }

    #[test]
    fn opening_a_new_window_on_a_missing_worktree_fails_before_touching_tmux() {
        let server = TmuxServer::new("/tmp/grove-test-never-used.sock");
        let err = open_new_window(
            &server,
            "proj",
            Path::new("/home/u/proj/.git"),
            &worktree("/nonexistent-grove/wt"),
        )
        .expect_err("worktree is gone");
        assert!(matches!(err, Error::WorktreeMissing(_)));
    }

    #[test]
    fn a_session_spec_carries_the_whole_mapping() {
        let worktree = worktree("/home/u/wt/feature");
        let spec = session_spec("acme-web", Path::new("/home/u/proj/.git"), &worktree);
        assert_eq!(spec.session_name(), worktree.session_name());
        assert_eq!(spec.worktree_path, worktree.path);
        assert_eq!(spec.project_name, "acme-web");
        assert_eq!(spec.git_common_dir, Path::new("/home/u/proj/.git"));
    }

    #[test]
    fn status_is_stamped_only_onto_worktrees_that_have_a_session() {
        let mut worktrees = vec![worktree("/home/u/proj"), worktree("/home/u/wt/feature")];
        worktrees[0].session = SessionPresence::Detached;
        worktrees[1].session = SessionPresence::None;
        let reports = HashMap::from([
            (
                worktrees[0].id.clone(),
                SessionReport::new(SessionStatus::Working),
            ),
            // A status left over for a session that has since been closed.
            (
                worktrees[1].id.clone(),
                SessionReport::new(SessionStatus::Attention),
            ),
        ]);

        apply_session_status(&mut worktrees, &reports);
        assert_eq!(worktrees[0].status, Some(SessionStatus::Working));
        assert_eq!(
            worktrees[1].status, None,
            "a closed session must not keep showing a status"
        );
    }

    #[test]
    fn resource_figures_follow_the_status_onto_the_row() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].session = SessionPresence::Detached;
        let mut report = SessionReport::new(SessionStatus::Working);
        report.usage = Some(crate::cgroup::Usage {
            memory_bytes: 2 * 1024 * 1024,
            cpu_usec: 0,
        });
        let reports = HashMap::from([(worktrees[0].id.clone(), report)]);

        apply_session_status(&mut worktrees, &reports);
        assert_eq!(worktrees[0].resources.as_deref(), Some("2M"));

        // And they go when the session does, rather than lingering.
        worktrees[0].session = SessionPresence::None;
        apply_session_status(&mut worktrees, &reports);
        assert_eq!(worktrees[0].resources, None);
    }

    #[test]
    fn dropping_a_status_drops_its_message_with_it() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].session = SessionPresence::None;
        worktrees[0].status = Some(SessionStatus::Attention);
        worktrees[0].status_message = Some("needs permission".into());

        apply_session_status(&mut worktrees, &HashMap::new());
        assert_eq!(worktrees[0].status, None);
        assert_eq!(worktrees[0].status_message, None);
    }

    #[test]
    fn an_unpolled_worktree_keeps_no_status() {
        let mut worktrees = vec![worktree("/home/u/proj")];
        worktrees[0].session = SessionPresence::Detached;
        worktrees[0].status = Some(SessionStatus::Working);

        apply_session_status(&mut worktrees, &HashMap::new());
        assert_eq!(worktrees[0].status, None);
    }

    #[test]
    fn activation_reports_which_path_it_took() {
        let switched = Activation::SwitchedClient {
            session: "wt-a1b2c3".into(),
            client_tty: "/dev/pts/3".into(),
        };
        assert_eq!(switched.session(), "wt-a1b2c3");
        let launched = Activation::LaunchedTerminal {
            session: "wt-a1b2c3".into(),
            command: "foot tmux".into(),
        };
        assert_eq!(launched.session(), "wt-a1b2c3");
    }
}
