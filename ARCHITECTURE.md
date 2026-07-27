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
| Status detection | **Fixed-interval poller** | Background thread polls tmux every ~2 s and git status every ~10 s per visible project, diffs against cache, sends deltas to the UI. tmux hooks are a possible v2 upgrade. |
| Worktree IDs | **Deterministic hash** | First 6 hex chars of a hash over `(git-common-dir, canonical worktree path)`. Session name = `wt-<id>`. Losing `state.toml` is recoverable: restore re-derives identical IDs and reattaches to live tmux sessions. |
| Terminal default | **Auto-detect on first run** | Probe PATH in order (`ptyxis`, `foot`, `alacritty`, `kitty`, `gnome-terminal`); write the winning template into `config.toml` so it is visible and editable. |
| Agent attention | **`grove notify` CLI + IPC** | The `grove` binary doubles as a CLI. Agent wrappers (e.g. Claude Code hooks) call `grove notify --session <id> --state attention`; the GUI receives it over a local IPC socket. Architected from v1, fully wired in Milestone 4. |
| Crate layout | **Workspace: `grove-core` + `grove`** | Core (git, tmux, state, reconcile — no UI deps, fully testable) plus the binary crate (egui UI + CLI subcommands). |
| Quality bar | **Strict** | Edition 2024, `clippy -D warnings`, rustfmt defaults, no `unwrap()`/`expect()` outside tests, `thiserror` error types, unit tests mandatory for all git/tmux output parsers. |

## 2. Process architecture

```text
┌─────────────────────────────┐
│ grove (GUI process)         │
│  egui event loop (main)     │
│  ├─ worker: git commands    │──▶ git CLI (arg arrays, never shell)
│  ├─ worker: tmux commands   │──▶ tmux -S $SOCKET … (private server)
│  ├─ poller thread (2s/10s)  │
│  └─ IPC listener thread     │◀── grove notify (from agent hooks)
└─────────────────────────────┘
         │ launches (detached)
         ▼
  user's terminal ──attach──▶ private tmux server ──▶ one session per worktree
```

- **Private tmux server** at `$XDG_RUNTIME_DIR/grove/tmux.sock`. Every tmux
  invocation passes `-S` explicitly. Never touch the user's default server.
- **Grove-owned tmux config.** The server is started with
  `-f $XDG_CONFIG_HOME/grove/tmux.conf` (a bare `-S` server would still read
  `~/.tmux.conf`). The file is generated on first run and user-editable;
  the default sources `~/.tmux.conf` when present, then applies the
  settings Grove depends on (`monitor-bell on`, `monitor-activity on`,
  `exit-empty off`).
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
│           ├── main.rs         # arg parsing: GUI by default, `notify` subcommand
│           ├── app.rs          # eframe::App, channel plumbing
│           ├── workers.rs      # thread pool, poller, IPC listener
│           └── ui/
│               ├── project_list.rs
│               ├── worktree_row.rs
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
  (global / per-project / per-worktree), timeouts, toggles. The app reads it
  and writes it only once (first-run auto-detect); it never clobbers edits.
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

## 6. Status model

Three states, evaluated by `grove-core::status` from poller + IPC inputs:

- **Working `◉`** — tmux pane activity within the last 10 s (configurable),
  or a known agent process found in the session's process tree.
- **Idle `●`** — session exists, no recent activity. Idle ≠ finished.
- **Attention `!`** — tmux bell/activity flag, non-zero process exit,
  a `grove notify` message, or a manual user override.

Precedence: attention > working > idle. Attention latches until the user
opens the session.

There is **no Grove daemon**: the only long-lived processes are the tmux
server and (when open) the GUI. `grove notify` delivers over the IPC socket
when the GUI is running, and *always* also stamps the session with a
`@grove_attention` tmux user option — so attention raised while the GUI is
closed is held durably by tmux and picked up by the first poll after the
next launch. The socket is a latency optimization, not the source of truth.
Clearing attention (on session open) clears the user option too. Process-name sniffing alone is explicitly *not* trusted
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
   attention notifications, project-specific commands.

Acceptance criteria for the first release are in docs/DESIGN.md §24.
