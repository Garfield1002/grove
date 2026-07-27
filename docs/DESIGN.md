# Worktree Session Manager — Design Document

> Original design document for Grove. Resolved implementation decisions are
> recorded in [../ARCHITECTURE.md](../ARCHITECTURE.md), which supersedes this
> document where they differ (persistence is TOML, not SQLite; crate layout
> is a two-crate workspace).

## 1. Summary

Worktree Session Manager is a small native GUI for navigating Git projects, Git worktrees, and persistent tmux sessions.

The application does not render terminals. It launches the user's preferred terminal and attaches it to a managed tmux server. Selecting a worktree in the GUI switches the visible tmux client to the corresponding session.

The main interface is a narrow vertical window containing projects and their worktrees.

## 2. Goals

The application should:

* Open or register existing Git projects.
* Display the worktrees belonging to each project.
* Create new worktrees.
* Open existing worktrees.
* Create and manage one tmux session per worktree.
* Launch the user's preferred terminal.
* Switch the managed terminal client when the user clicks a worktree.
* Display whether each worktree session is working, idle, or needs attention.
* Restore previously managed projects and sessions after restarting the application.
* Avoid implementing terminal emulation, rendering, clipboard handling, or shell integration.

## 3. Non-goals

The initial version will not:

* Render or embed a terminal.
* Replace Git, tmux, or the user's terminal emulator.
* Parse arbitrary terminal output to understand an agent.
* Act as a Wayland window manager.
* Move, resize, or reliably focus arbitrary terminal windows.
* Replace a full Git client.
* Provide an integrated editor.
* Synchronize sessions between machines.

## 4. Core Concepts

### Project

A registered Git repository.

```rust
struct Project {
    id: ProjectId,
    name: String,
    repository_path: PathBuf,
    default_worktree_path: PathBuf,
    is_expanded: bool,
}
```

The repository path may be:

* a normal Git working tree,
* the main worktree of a repository,
* or a bare repository used to manage multiple worktrees.

### Worktree

A Git worktree associated with a project.

```rust
struct Worktree {
    id: WorktreeId,
    project_id: ProjectId,
    path: PathBuf,
    branch: Option<String>,
    head_commit: String,
    is_main: bool,
    is_locked: bool,
    git_status: GitStatus,
}
```

A worktree may exist without a tmux session.

### Managed tmux session

A persistent tmux session associated with a worktree.

```rust
struct ManagedSession {
    id: SessionId,
    worktree_id: WorktreeId,
    tmux_session_name: String,
    state: SessionState,
    last_activity_at: Option<SystemTime>,
    created_at: SystemTime,
}
```

A tmux session may continue running without an attached terminal client.

### Terminal client

A temporary terminal window attached to the application's private tmux server.

The application should support one primary managed terminal client in the first version.

## 5. User Interface

The application uses a narrow vertical window.

Example:

```text
┌────────────────────────────────────┐
│ Worktrees                     ＋   │
├────────────────────────────────────┤
│ ▼ my-project                      │
│   ● main                    idle   │
│   ◉ feature/auth         working  │
│   ! fix/parser          attention │
│                                    │
│ ▶ another-project                 │
│                                    │
├────────────────────────────────────┤
│ Open project                       │
│ Settings                           │
└────────────────────────────────────┘
```

### Project rows

Each project row should provide:

* project name,
* expand or collapse control,
* project context menu,
* button to create a worktree,
* indication when the project path is unavailable.

Project context menu:

* Open project directory
* Refresh
* Create worktree
* Open terminal in main worktree
* Remove project from application
* Restore project
* Project settings

Removing a project from the application must not delete the repository or worktrees.

### Worktree rows

Each worktree row should display:

* branch name,
* abbreviated path when useful,
* session status icon,
* dirty working-tree indicator,
* detached-HEAD indicator,
* optional unread or attention badge.

Clicking a worktree should:

1. Verify that the worktree still exists.
2. Ensure that its tmux session exists.
3. Find the managed terminal client.
4. Switch the client to the selected session.
5. Launch a terminal if no managed client is attached.
6. Mark the selected worktree as active.

Worktree context menu:

* Open or switch to session
* Open in a new terminal
* Start agent
* Open shell window
* Rename branch
* Copy worktree path
* Open directory
* Stop managed processes
* Close tmux session
* Remove worktree
* Repair or restore session

Destructive operations must require confirmation.

## 6. Session Status

The application exposes three primary states.

### Working

The session appears to be actively producing output or running a known agent process.

Suggested icon:

```text
◉
```

Possible signals:

* recent tmux pane activity,
* known agent process running,
* recent output recorded by tmux,
* explicit status notification from an agent wrapper.

### Idle

The session exists but has not produced activity recently.

Suggested icon:

```text
●
```

Idle does not necessarily mean the agent has completed. It only means no recent activity has been detected.

### Needs attention

The session is waiting for user interaction or has emitted an attention signal.

Suggested icon:

```text
!
```

Possible signals:

* tmux bell or activity flag,
* an agent wrapper reports a permission request,
* an agent wrapper reports that input is required,
* the process exited with an error,
* the user manually marks the session,
* a configured command or hook reports attention.

### Important limitation

The application cannot reliably infer "needs attention" from the current process name alone.

A generic first version should use:

* tmux activity and bell signals,
* process exit state,
* optional agent-specific wrappers,
* manual status override.

Terminal-output parsing should not be required for the core application.

## 7. tmux Architecture

The application should use a private tmux server.

Example socket:

```text
$XDG_RUNTIME_DIR/worktree-manager/tmux.sock
```

Every tmux command should use the explicit socket:

```bash
tmux -S "$SOCKET" ...
```

This prevents interference with the user's normal tmux server and sessions.

### Session naming

Do not derive tmux session names directly from branch names.

Use stable internal identifiers:

```text
wt-7f3a9c
wt-b120de
```

Persist the mapping between:

* project,
* worktree path,
* worktree identifier,
* tmux session name.

### Session creation

Create a detached session rooted in the worktree:

```bash
tmux -S "$SOCKET" new-session \
  -d \
  -s "$SESSION" \
  -c "$WORKTREE"
```

The default session should start an interactive login shell.

An agent may run in a separate tmux window:

```bash
tmux -S "$SOCKET" new-window \
  -t "$SESSION" \
  -n agent \
  -c "$WORKTREE" \
  "$AGENT_COMMAND"
```

Suggested default windows:

```text
0: shell
1: agent
```

Additional windows such as tests or development servers should be user-created or added later.

## 8. Terminal Launching

The application should allow the user to configure a terminal command template.

Example:

```text
alacritty -e tmux -S {socket} attach-session -t {session}
```

Supported substitutions:

```text
{socket}
{session}
{worktree}
{project}
{branch}
```

The application should validate the configured executable and display the expanded command for debugging.

### Primary client behavior

The first version should designate one tmux client as the primary client.

When selecting another worktree:

```bash
tmux -S "$SOCKET" switch-client \
  -c "$CLIENT_TTY" \
  -t "$SESSION"
```

When no client is attached, the application launches the preferred terminal attached to the selected session.

### Additional terminal windows

"Open in new terminal" should create an additional client without changing the primary client.

The application should not depend on being able to focus an existing external terminal window under Wayland.

## 9. Project Management

### Open project

The user chooses a directory.

The application verifies it using commands such as:

```bash
git -C "$PATH" rev-parse --show-toplevel
git -C "$PATH" rev-parse --git-common-dir
```

The application should detect when the selected path is inside an existing worktree and register the containing project.

### Create project

Optional for the initial release.

Possible operations:

* initialize a new Git repository,
* clone a repository,
* register the resulting project.

Cloning can initially be delegated to:

```bash
git clone ...
```

### Refresh project

Refresh should reconcile the application database with:

```bash
git -C "$PROJECT" worktree list --porcelain
```

The application should detect:

* newly created external worktrees,
* removed worktrees,
* moved paths,
* changed branches,
* detached HEAD state,
* locked worktrees,
* prunable worktrees.

## 10. Worktree Creation

The creation dialog should request:

* branch name,
* base branch or commit,
* worktree directory,
* whether to create a new branch,
* whether to open the session after creation,
* optional agent command to start.

Examples:

Create a new branch and worktree:

```bash
git -C "$PROJECT" worktree add \
  -b "$NEW_BRANCH" \
  "$WORKTREE_PATH" \
  "$BASE_REF"
```

Open an existing branch:

```bash
git -C "$PROJECT" worktree add \
  "$WORKTREE_PATH" \
  "$BRANCH"
```

The application must display Git errors without hiding the original stderr output.

## 11. Restore Behavior

"Restore project" means reconstructing application state from Git and tmux rather than assuming the persisted state is correct.

Restore should:

1. Verify the project path.
2. Read the project's current Git worktree list.
3. Match worktrees using canonical paths and repository identity.
4. Inspect sessions on the private tmux server.
5. Reconnect existing worktrees to matching tmux sessions.
6. mark missing sessions as stopped,
7. mark missing worktree paths as unavailable,
8. offer to recreate sessions,
9. retain user configuration and historical metadata.

### Startup reconciliation

On application startup:

* load persisted projects,
* inspect Git worktrees,
* inspect tmux sessions,
* reconcile differences,
* never delete anything automatically.

### Missing project path

When a project directory has moved or a drive is unavailable, display:

```text
Project unavailable
```

Actions:

* Locate project
* Retry
* Remove from application

### Orphaned session

A tmux session without a matching worktree should be marked as orphaned.

Actions:

* Open session
* Associate with worktree
* Close session
* Ignore

### Missing session

A worktree without a session should remain usable.

Clicking it may recreate the session automatically after user confirmation or according to a setting.

## 12. Persistence

Persist application metadata in a small SQLite database or structured file.

Recommended persisted data:

* registered projects,
* project display names,
* stable project identifiers,
* known worktrees,
* tmux session mappings,
* selected project and worktree,
* terminal command template,
* agent command templates,
* UI expansion state,
* manual status overrides,
* last activity timestamps.

Git and tmux remain the source of truth for repository and session existence.

The database is only an index and configuration store.

## 13. Safety Requirements

The application must never delete a worktree or branch merely because it is missing from the application database.

Before removing a worktree, check:

* uncommitted changes,
* untracked files,
* unpushed commits when detectable,
* active tmux panes,
* running processes,
* whether the worktree is locked,
* whether it is the main worktree.

The confirmation dialog should clearly distinguish:

* remove from application,
* close tmux session,
* remove Git worktree,
* delete branch.

These must be separate operations.

Shell commands must be executed using argument arrays rather than concatenated shell strings unless the user explicitly configures a shell command template.

Paths and branch names must not be interpolated into shell commands without proper argument handling.

## 14. Error Handling

CLI failures should retain:

* executable,
* arguments,
* exit status,
* stdout,
* stderr.

The UI should display a concise error with an expandable diagnostics section.

Example:

```text
Could not create worktree.

fatal: 'feature/auth' is already checked out at '/home/user/auth'

Show command output
```

The application should handle:

* missing Git executable,
* missing tmux executable,
* missing terminal executable,
* stale tmux socket,
* inaccessible directories,
* malformed Git repositories,
* session-name collisions,
* terminal launch failures.

## 15. Configuration

Initial settings:

* preferred terminal command template,
* shell command,
* default worktree parent directory,
* default agent command,
* auto-create session when selecting a worktree,
* auto-launch terminal when no client exists,
* idle timeout,
* whether bells mark a session as needing attention,
* whether to restore projects at startup.

Example agent command templates:

```text
claude
codex
aider
```

Commands should be configurable per project and per worktree.

## 16. Keyboard Navigation

Although rows are clickable, the application should remain keyboard-friendly.

Suggested shortcuts:

```text
Up / Down       Select previous or next worktree
Enter           Open selected worktree session
Ctrl+N          Create worktree
Ctrl+O          Open project
Ctrl+R          Refresh project
Ctrl+Shift+T    Open selected session in new terminal
Delete          Open safe removal dialog
```

Shortcuts should be configurable later.

## 17. Notifications

Optional desktop notifications should be supported for:

* session needs attention,
* agent process exits,
* agent process fails,
* long-running operation completes.

Notifications should identify:

* project,
* branch or worktree,
* reason for notification.

Selecting a notification should bring the GUI forward when supported, but the application should not depend on focusing the external terminal.

## 18. Useful Additional Features

### Git status summary

Show compact indicators for:

* modified files,
* staged changes,
* untracked files,
* ahead or behind upstream,
* merge or rebase in progress.

This should be refreshed asynchronously to avoid blocking the UI.

### Search and filtering

Allow filtering worktrees by:

* project name,
* branch,
* path,
* session state,
* dirty state.

### Recent activity ordering

Allow worktrees to be sorted by:

* project order,
* branch name,
* creation date,
* last session activity,
* attention status.

A useful default is to keep projects grouped while moving attention-required worktrees to the top of each project.

### Project-specific commands

Allow users to configure commands such as:

* run tests,
* start development server,
* open editor,
* start coding agent.

These commands should run in new tmux windows inside the selected session.

### Session reset

Provide a safe action that:

* closes the managed tmux session,
* creates a new session in the same worktree,
* does not modify Git files.

### Archive worktree

Allow a completed worktree to be hidden without deleting it.

Archived worktrees can be restored to the visible list.

### Import existing tmux sessions

This should not be required initially, but the application may later associate an existing tmux session with a worktree.

## 19. Functional Requirements

### FR-1: Register project

The user can select an existing Git repository and add it to the project list.

### FR-2: Discover worktrees

The application lists all worktrees reported by Git for a registered project.

### FR-3: Create worktree

The user can create a worktree and optional branch through the GUI.

### FR-4: Create session

The application can create a private tmux session rooted in a worktree.

### FR-5: Launch terminal

The application can launch the configured terminal attached to a managed session.

### FR-6: Switch session

Clicking a worktree switches the primary tmux client to that worktree's session.

### FR-7: Preserve session

Closing the GUI or terminal does not automatically terminate tmux sessions.

### FR-8: Display status

Each worktree displays working, idle, or needs-attention state.

### FR-9: Restore state

The application reconciles persisted projects with current Git worktrees and tmux sessions.

### FR-10: Safe removal

The application separates removing UI metadata, closing sessions, removing worktrees, and deleting branches.

## 20. Non-functional Requirements

### Responsiveness

Git and tmux commands must not block the GUI event loop.

### Reliability

The application must recover gracefully from stale database entries, dead sessions, and moved repositories.

### Portability

The initial target is Fedora Linux under Wayland.

The architecture should avoid compositor-specific window-management APIs.

### Security

CLI commands should use direct process invocation with explicit arguments.

User-configured shell templates should be treated as trusted configuration and clearly identified as shell commands.

### Observability

Debug logging should include:

* Git command execution,
* tmux command execution,
* session reconciliation,
* terminal launch attempts,
* state transitions.

Logs must avoid recording sensitive terminal contents.

## 21. Suggested Implementation Components

```text
Native Rust GUI:
    egui/eframe, GPUI, or another native Rust framework

Git integration:
    git CLI

Session management:
    tmux CLI using a private socket

Persistence:
    SQLite or a versioned JSON/TOML file

Process execution:
    std::process::Command or tokio::process::Command

Filesystem monitoring:
    optional notify crate

Desktop notifications:
    freedesktop notification implementation
```

The first version should avoid `git2`, custom tmux protocols, terminal emulation, and compositor-specific integrations.

## 22. Suggested Internal Modules

```text
src/
├── app.rs
├── model/
│   ├── project.rs
│   ├── worktree.rs
│   └── session.rs
├── git/
│   ├── commands.rs
│   ├── parser.rs
│   └── status.rs
├── tmux/
│   ├── server.rs
│   ├── session.rs
│   └── client.rs
├── terminal/
│   └── launcher.rs
├── persistence/
│   └── store.rs
├── restore/
│   └── reconcile.rs
└── ui/
    ├── project_list.rs
    ├── worktree_row.rs
    ├── dialogs.rs
    └── settings.rs
```

Note: superseded by the two-crate workspace layout in ARCHITECTURE.md §3.

## 23. Initial Delivery Milestones

### Milestone 1: Navigation prototype

* Register existing project.
* List worktrees.
* Create private tmux sessions.
* Launch configured terminal.
* Switch one attached tmux client by clicking a worktree.

### Milestone 2: Worktree management

* Create worktrees.
* Refresh Git state.
* Display dirty status.
* Add safe removal dialogs.

### Milestone 3: Persistence and restore

* Persist projects and mappings.
* Reconcile state at startup.
* Handle missing projects, missing sessions, and orphaned sessions.

### Milestone 4: Agent workflow

* Add configurable agent commands.
* Show process and activity status.
* Add attention notifications.
* Add project-specific commands.

## 24. Acceptance Criteria for the First Release

The first release is complete when a user can:

1. Open an existing Git project.
2. See all its Git worktrees.
3. Create a new worktree.
4. Click a worktree to create or open its tmux session.
5. View that session in their configured terminal.
6. Click another worktree and have the same terminal switch sessions.
7. Close and restart the GUI without losing tmux sessions.
8. Restore the project list and session associations.
9. See basic working, idle, and attention indicators.
10. Remove a project from the GUI without deleting repository data.
