# Grove — Architecture

Grove is a small native GUI for navigating Git projects, Git worktrees, and
persistent tmux sessions. It does **not** render terminals: it launches the
user's preferred terminal emulator attached to a private tmux server, and
switches the visible tmux client when the user selects a worktree.

This document records the *resolved* architecture. The full product design
(UI sketches, feature lists, milestones, acceptance criteria) lives in
[docs/DESIGN.md](docs/DESIGN.md).

## 1. Resolved decisions

| Decision | Choice | Rationale / constraint |
|---|---|---|
| GUI framework | **egui/eframe** (glow backend) | Immediate mode fits a small list UI; native Wayland. **Hard budget: ≤ 100 MB RSS.** If egui cannot stay under it, fall back to GPUI. |
| Persistence | **TOML only**, split into `config.toml` + `state.toml` | Config is hand-editable and never rewritten by the app; state is app-owned and written atomically (temp file + rename). No SQLite. |
| Concurrency | **OS threads + channels** (`std::thread`, `std::process::Command`, mpsc back to UI) | No async runtime. Subprocess work is short-lived and blocking-friendly. UI wakes via `egui::Context::request_repaint`. |
| Status detection | **Interval poller, paced by what is on screen** | Background thread polls tmux every ~2 s and git status every ~10 s per visible project, diffs against cache, sends deltas to the UI. While the UI paints nothing (minimised, another workspace, fully occluded) the tmux poll drops to ~30 s and the git poll stops entirely; the first frame after that gap polls immediately. See §6. tmux hooks are a possible v2 upgrade. |
| Worktree IDs | **Deterministic hash** | First 6 hex chars of a hash over `(git-common-dir, canonical worktree path)`. Session name = `wt-<id>`. Losing `state.toml` is recoverable: restore re-derives identical IDs and reattaches to live tmux sessions. |
| Terminal default | **Auto-detect on first run** | Probe PATH in order (`ptyxis`, `foot`, `alacritty`, `kitty`, `gnome-terminal`); write the winning template into `config.toml` so it is visible and editable. |
| Agent attention | **`grove notify` CLI + IPC** | The `grove` binary doubles as a CLI. Agent wrappers (e.g. Claude Code hooks) call `grove notify --session <id> --state attention`; the GUI receives it over a local IPC socket. Architected from v1, fully wired in Milestone 4. |
| Claude Code | **`grove notify --hook` + `grove hooks`** | One command on every hook event reads Claude Code's JSON payload on stdin and reports from it: the state, the message, the window, and the conversation id. `grove hooks install` merges that command into `settings.json`, preserving the user's own hooks. See §6.1. |
| Resource accounting | **systemd scopes (opt-in setting)** | On systemd machines, agent/user commands launched inside panes are wrapped in `systemd-run --user --scope --collect --unit=grove-<wt-id>-<kind>-<nonce>.scope --`. Each agent gets its own cgroup → per-agent/per-project RAM/CPU read from `/sys/fs/cgroup` (`memory.current`, `cpu.stat`), later `MemoryMax`/kill-by-scope. Auto-detected (systemd user manager present), off otherwise. Plain shells stay unwrapped. Implemented in Milestone 4. |
| Crate layout | **Workspace: `grove-core` + `grove`** | Core (git, tmux, state, reconcile — no UI deps, fully testable) plus the binary crate (egui UI + CLI subcommands). |
| Quality bar | **Strict** | Edition 2024, `clippy -D warnings`, rustfmt defaults, no `unwrap()`/`expect()` outside tests, `thiserror` error types, unit tests mandatory for all git/tmux output parsers. |

## 2. Process architecture

```text
┌─────────────────────────────┐
│ grove (GUI process)         │
│  egui event loop (main)     │
│  ├─ worker: git commands    │──▶ git CLI (arg arrays, never shell)
│  ├─ worker: tmux commands   │──▶ tmux -S $SOCKET … (private server)
│  ├─ poller thread (2s/10s,  │
│  │    30s while off screen) │
│  └─ IPC listener thread     │◀── grove notify (from agent hooks)
│                             │◀── grove toggle (from a WM shortcut)
└─────────────────────────────┘
         │ launches (detached)
         ▼
  user's terminal ──attach──▶ private tmux server ──▶ one session per worktree
```

- **Private tmux server** at `$XDG_RUNTIME_DIR/grove/tmux.sock`. Every tmux
  invocation passes `-S` explicitly. Never touch the user's default server.
- **Grove-owned tmux config.** The server is started with
  `-f $XDG_CONFIG_HOME/grove/tmux.conf` (a bare `-S` server would still read
  `~/.tmux.conf`). The configuration is split in two so that fixes reach
  existing installs without ever overwriting a user's edits:
  - `tmux.conf` is **the user's**. Generated on first run, then never
    rewritten. It sources `~/.tmux.conf` when present, sources the managed
    file, and leaves its tail for overrides.
  - `grove.tmux.conf` is **Grove's**. Shipped in the binary
    (`grove-core/assets/grove.tmux.conf`, `include_str!`) and rewritten
    atomically on every start, so it must not be hand-edited. It holds what
    status detection depends on (`monitor-bell`, `monitor-activity`,
    `exit-empty off`) plus the terminal behaviour a pane needs to be usable
    (`mouse on`; `extended-keys always` with `extended-keys-format csi-u`
    and per-terminal `extkeys` features, without which Shift+Enter and
    friends never reach the agent running in the pane).

  Note that `terminal-features` is resolved when a *client attaches*:
  changing it only affects terminals opened afterwards.
- **Session metadata as tmux user options.** At creation, each session gets
  `@grove_id`, `@grove_project`, `@grove_worktree` (canonical path), and
  `@grove_repo` (git-common-dir) set via `set-option`. The tmux server thus
  carries the worktree ↔ session mapping itself, queryable with
  `list-sessions -F '#{@grove_worktree}'`.
- **Sessions outlive Grove.** Closing the GUI or the terminal never kills
  tmux sessions (FR-7).
- **One primary client** in v1: selecting a worktree runs
  `tmux switch-client -c <client_tty> -t wt-<id>`; if no client is attached,
  Grove launches the configured terminal instead. "Open in new terminal"
  spawns an additional client without retargeting the primary one.
- Each session exports `GROVE_SESSION=<id>` so wrappers and hooks can call
  `grove notify` without configuration.

## 3. Workspace layout

```text
grove/
├── Cargo.toml              # workspace
├── crates/
│   ├── grove-core/         # lib: no UI dependencies, fully unit-testable
│   │   └── src/
│   │       ├── model.rs        # Project, Worktree, ManagedSession, states
│   │       ├── ids.rs          # deterministic worktree-id hashing
│   │       ├── git/
│   │       │   ├── commands.rs # process invocation (arg arrays)
│   │       │   ├── parser.rs   # `worktree list --porcelain` etc. — TESTED
│   │       │   └── status.rs   # `status --porcelain=v2` summary — TESTED
│   │       ├── tmux/
│   │       │   ├── server.rs   # socket path, server lifecycle
│   │       │   ├── session.rs  # create/kill/list, activity timestamps
│   │       │   └── client.rs   # primary-client tracking, switch-client
│   │       ├── terminal.rs     # template expansion, auto-detect, launch
│   │       ├── config.rs       # config.toml (read-only for the app)
│   │       ├── state.rs        # state.toml (atomic temp+rename writes)
│   │       ├── removal.rs      # safe-removal risk report (pure) — TESTED
│   │       ├── reconcile.rs    # startup/restore reconciliation
│   │       ├── status.rs       # working/idle/attention state machine
│   │       └── ipc.rs          # notify socket protocol
│   └── grove/              # bin: GUI + CLI entry points
│       └── src/
│           ├── main.rs         # arg parsing: GUI by default, `notify` and
│           │                   # `hooks` subcommands
│           ├── notify.rs       # `grove notify`, incl. `--hook` (stdin JSON)
│           ├── hooks.rs        # `grove hooks`: Claude Code settings.json
│           ├── app.rs          # eframe::App, channel plumbing
│           ├── workers.rs      # thread pool, poller, IPC listener
│           └── ui/
│               ├── project_list.rs
│               ├── worktree_row.rs
│               ├── chrome.rs       # detached-window chrome and lifecycle
│               ├── window_edge.rs  # resize handles for undecorated windows
│               ├── dialogs/        # open-project, create-worktree,
│               │                   # safe-removal, error area
│               └── settings.rs
└── docs/DESIGN.md          # original full design document
```

Dependency rule: `grove` depends on `grove-core`; `grove-core` depends on
neither egui nor anything UI-related.

## 4. Data & persistence

Git and tmux are the **source of truth** for repository and session
existence. TOML files are only an index and configuration store.

- `$XDG_CONFIG_HOME/grove/config.toml` — user-owned. Terminal template,
  shell command, default worktree parent dir, agent command templates
  (global / per-project / per-worktree), timeouts, toggles. Hand edits are
  first-class; the app writes it only via `toml_edit` (surgical per-key
  changes preserving comments/formatting) on first-run auto-detect and on
  explicit Settings-UI changes — never whole-file serialization.
- `$XDG_STATE_HOME/grove/state.toml` — app-owned. Registered projects,
  worktree ↔ session mappings, selection, UI expansion, manual status
  overrides, last-activity timestamps. Written atomically: serialize to a
  temp file in the same directory, then `rename(2)`.

### Core types (in `grove-core::model`)

`Project { id, name, repository_path, default_worktree_path, is_expanded }` —
repository_path may be a normal working tree, a main worktree, or a bare repo.

`Worktree { id, project_id, path, branch, head_commit, is_main, is_locked, git_status }`
— may exist without a session.

`ManagedSession { id, worktree_id, tmux_session_name, state, last_activity_at, created_at }`
— may keep running with no attached client.

## 5. UI direction

The visual design lives in [grove.dc.html](grove.dc.html) (three explored
directions; **1c "color-forward"** is the chosen one, built at full
fidelity). Key elements beyond the original spec sketch:

- status **pills** (`WORKING`, `ACTION`) plus a colored accent edge per row,
  instead of bare `◉ ● !` glyphs;
- a sublabel per worktree row: agent activity ("claude · streaming output",
  "waiting for permission"), git summary ("clean · idle 2h", `+3 −1`), or
  "no session" with an inline **start** affordance;
- per-project worktree-count badges on collapsed rows;
- a filter field under the header; a **Restore** control in the header.

The main window is a narrow vertical sliver, so the three large dialogs —
**Settings**, **create worktree**, **safe removal** — are not drawn inside it.
Each opens as its own toplevel via egui multi-viewport
(`Context::show_viewport_immediate`, so a dialog keeps borrowing app state
directly; deferred viewports would need `Arc<Mutex<…>>` for no benefit at this
scale). `ui::chrome` owns their lifecycle (one instance per kind; asking again
raises it with `ViewportCommand::Focus`) and their chrome, which matches the
main window: undecorated, header as drag handle, `ui::window_edge` for resize,
Esc / Ctrl+W / ✕ to close, Ctrl+Q to quit Grove from any window. Placement is
the compositor's: Wayland toplevels are not positioned by the client. The error
strip and status line stay in the main window; dialog failures keep flowing
there. The small open-project prompt remains an in-window `egui::Window`.

## 6. Status model

Three states, evaluated by `grove-core::status` from poller + IPC inputs:

- **Working `◉`** — tmux pane activity within the last 10 s (configurable),
  or a known agent process found in the session's process tree.
- **Idle `●`** — session exists, no recent activity. Idle ≠ finished.
- **Attention `!`** — tmux bell/activity flag, non-zero process exit,
  a `grove notify` message, or a manual user override.

Precedence: attention > working > idle. Attention latches until the user
opens the session.

**Poll cadence follows the frames, not the focus.** Grove is normally used
beside the terminal it launched — visible but not focused — so focus is the
wrong signal for "is anyone looking"; a painted frame is the right one. A
Wayland surface that is minimised, on another workspace or fully occluded
stops receiving frame callbacks, so the repaint each poll asks for never
becomes a frame, and the poller reads that as nobody watching: the tmux poll
falls to 30 s (enough to keep raising desktop notifications for a bell) and
git status, which raises nothing, waits for the window to come back. The
first frame after such a gap asks for an immediate poll, so an unhidden
Grove is current by the time the user has read it. Where frame callbacks do
not stop (X11, a window merely behind another), nothing changes and the fast
cadence stands.

### 6.1 Claude Code

Claude Code is the agent Grove is developed against, and the only one it
knows by name. The integration is one command — `grove notify --hook` —
configured on five events (`Notification`, `UserPromptSubmit`, `Stop`,
`SessionStart`, `SessionEnd`) by `grove hooks install`, which merges it into
`~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`). That file is the user's:
their own hooks survive, a copy is taken before it is replaced, one that
cannot be parsed is reported rather than overwritten, and installing twice
leaves one entry per event. The Settings pane shows the same status and runs
the same code.

`--hook` reads the event's JSON object on stdin and takes from it what flags
would otherwise have to carry:

- **state** — `Notification` is attention (the one signal Grove refuses to
  infer for itself), a prompt is working, a turn or session ending is idle.
  An event this Grove has no opinion about reports *nothing at all*: a hook
  runs inside someone's agent, so an unknown event, an unparseable payload
  and a Claude Code started outside Grove all exit 0 in silence.
- **message** — what Claude is waiting for, or the first line of the prompt.
  Shown as the row's second line and as the first line of its tooltip.
- **window** — not from the payload but from `$TMUX_PANE`, resolved against
  the tmux server. A report that names a window marks *that* row; a worktree
  where no window has ever reported keeps showing the session's status on
  every window row, exactly as before.
- **conversation id and transcript path** — recorded in `state.toml` under
  `[[agent]]`, which is what the row menu's *Resume agent conversation* and
  *Open agent transcript* act on. An index like every other table there:
  Grove never reads the transcript, and a record pointing at a conversation
  the agent has forgotten produces a command that says so, never a deletion.
  Resuming runs `[agents] resume_command`, which defaults to
  `claude --resume {agent_session}`: the ids it substitutes are the ones
  Claude Code reported, so that spelling is known rather than guessed. Another
  agent overrides the key; blanking it removes the action.

Nothing here parses terminal output or infers state from process names.

There is **no Grove daemon**: the only long-lived processes are the tmux
server and (when open) the GUI. `grove notify` delivers over the IPC socket
when the GUI is running, and *always* also stamps the session with a
`@grove_attention` tmux user option — so attention raised while the GUI is
closed is held durably by tmux and picked up by the first poll after the
next launch. The socket is a latency optimization, not the source of truth.
Clearing attention (on session open) clears the user option too. The same
socket carries `grove toggle` in the other direction (DESIGN.md §16): a
keyboard shortcut asking the running GUI to open a numbered worktree, or —
with no number — to close, since a Wayland client cannot hide and re-show
itself. Nothing listening is not an error there either: the CLI starts the
GUI instead and hands it the number to open after the first reconciliation. Process-name sniffing alone is explicitly *not* trusted
to infer attention; terminal-output parsing is out of scope.

## 7. Reconciliation & restore

On startup and on "Refresh"/"Restore project":

1. Load `state.toml` (tolerate missing/partial file).
2. `git -C <project> worktree list --porcelain` → actual worktrees; match by
   canonical path + repository identity (not by branch name).
3. `tmux -S $SOCKET list-sessions` → actual sessions; match primarily by the
   `@grove_*` user options each session carries (id, project, worktree path,
   repo), falling back to the `wt-<id>` name — deterministic IDs and
   session-embedded metadata both survive `state.toml` loss.
4. Diff: mark missing worktree paths *unavailable*, missing sessions
   *stopped*, sessions with no worktree *orphaned* (offer open / associate /
   close / ignore). **Never delete anything automatically.**

## 8. Safety invariants

These are load-bearing; every feature must preserve them.

1. Grove never deletes a worktree or branch because it is absent from its
   own state file.
2. "Remove from Grove", "close tmux session", "remove git worktree", and
   "delete branch" are four separate, individually confirmed operations.
3. Before worktree removal: check dirty files, untracked files, unpushed
   commits, active panes/processes, lock status, and is-main.
4. All subprocess invocations use argument arrays. Paths and branch names
   are never interpolated into shell strings. The single exception is the
   user's own terminal/agent command templates, which are documented as
   trusted shell configuration.
5. CLI failures retain executable, args, exit status, stdout, and stderr;
   the UI shows a concise message with expandable diagnostics and never
   hides git's original stderr.
6. Logs never record terminal contents.

## 9. Non-functional constraints

- **Memory:** ≤ 100 MB RSS steady-state (the egui/GPUI decision gate).
- **Responsiveness:** no subprocess call on the UI thread, ever.
- **Portability:** Fedora Linux + Wayland first; no compositor-specific
  window management (no focusing/moving external terminal windows).
- **Recovery:** stale state entries, dead sessions, and moved repositories
  degrade to visible, actionable UI states — never crashes, never data loss.

## 10. Milestones

1. **Navigation prototype** — register project, list worktrees, create
   sessions on the private server, launch terminal, switch primary client.
2. **Worktree management** — create worktrees, refresh git state, dirty
   indicators, safe-removal dialogs.
3. **Persistence & restore** — state.toml, startup reconciliation, missing/
   orphaned handling.
4. **Agent workflow** — agent command templates, `grove notify` wiring,
   attention notifications, project-specific commands, systemd-scope
   wrapping with per-agent/per-project RAM & CPU display.

Acceptance criteria for the first release are in docs/DESIGN.md §24.
