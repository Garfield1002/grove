# Grove — Handoff

_Last updated: 2026-07-27. Repo state: `ux/grove` @ `cb824db`, working tree
clean, all gates green — `just gate` (545 tests, clippy `-D warnings`, fmt,
`--no-default-features` build)._

## What this is

Grove is a narrow egui/eframe Wayland GUI for navigating Git projects,
worktrees, and persistent tmux sessions on a **private tmux server**. It
never renders terminals — it launches the user's terminal attached to its
server and switches the client between per-worktree sessions.

Read in this order: [CLAUDE.md](../CLAUDE.md) (binding rules),
[ARCHITECTURE.md](../ARCHITECTURE.md) (resolved decisions),
[DESIGN.md](DESIGN.md) (full product spec), `grove.dc.html` at the repo root
(visual mockup — direction **1c "color-forward"** is the chosen one).

## Where we are

| Milestone | Status |
|---|---|
| M1 — Navigation prototype (register project, list worktrees, private tmux sessions, terminal launch, click-to-switch) | **Done** |
| M2 — Worktree management (create worktree, refresh, git status sublabels, four-way safe removal with risk report) | **Done** |
| M2.5 — Fresh paint (theme.rs, epaint icons, undecorated window, drag + edge-resize, editable Settings via `toml_edit`, feature-gated native file picker, detached dialog windows) | **Done** |
| M3 — Persistence & restore (startup reconciliation, orphaned/missing handling, Restore UI) | **Done** — see the breakdown below |
| M4 — Agent workflow | **Done** — see the breakdown below |

### M3 breakdown

| Piece | Status |
|---|---|
| Reconciliation diff, pure (`grove-core/src/reconcile.rs`) | **Done** |
| Session matching: `@grove_*` options → `wt-<id>` name → worktree path | **Done** — the path fallback is scoped to one repository |
| `state.toml` `[[session]]` mappings + `ignored_sessions` | **Done** — additive; older files still load |
| Startup reconciliation, Restore chip, Ctrl+R | **Done** — replaces the per-project refresh on startup |
| Missing worktree → *unavailable* (marker + sublabel) | **Done** |
| Missing session → *stopped*; opening the row starts one again | **Done** |
| Missing project → *unavailable* + Retry / Locate / Remove from Grove | **Done** |
| Orphaned sessions → section with open / associate / close / ignore | **Done** — close is armed first; ignore is reversible |
| `tmux::associate_session` (rename + re-stamp options) | **Done** — same session, same panes |

Nothing in M3 is machine-verified GUI-side either; see the smoke list below.

### M4 breakdown

| Piece | Status |
|---|---|
| Status state machine + attention latch (`grove-core/src/status.rs`) | **Done** |
| tmux status signals (`@grove_attention`, `session_activity`, `session_alerts`) | **Done** |
| Poller thread (2 s) + notify listener thread (`grove/src/status_watch.rs`) | **Done** |
| `grove notify` CLI + IPC protocol (`grove-core/src/ipc.rs`, `grove/src/notify.rs`) | **Done** |
| Desktop notifications (`grove-core/src/desktop.rs`, `notify-send`) | **Done** |
| Status on rows: accent edge, dot, attention mark, agent message in tooltip | **Done** |
| `[status]` and `[agents]` config sections | **Done** |
| Agent commands in an `agent` window + systemd-scope wrapping | **Done** |
| Per-agent RAM/CPU from `/sys/fs/cgroup` (`grove-core/src/cgroup.rs`) | **Done** — shown in the row tooltip |
| Settings UI for `[status]` / `[agents]` | **Done** — except `agent_commands` and `[agents.per_project]`, a list and a map, which stay file-only |
| 10 s git-status poll | **Done** |
| `grove hooks install` | **Done** — merges `grove notify --hook` into Claude Code's settings.json, backing it up and leaving the user's own hooks alone. `just install-claude-hook` now just runs it. |
| Claude Code hook payloads (`grove-core/src/claude.rs`) | **Done** — `grove notify --hook` reads the event's JSON on stdin: state, message, `session_id`, `transcript_path` |
| Per-window reports (`grove-core/src/notice.rs`, `--window`) | **Done** — a report resolves `$TMUX_PANE` to a window index, so an agent's message lands on the agent's row and quiet windows stop repeating the worktree's line |
| Agent conversations in `state.toml` (`[[agent]]`) | **Done** — row menu offers *Resume agent conversation* (needs `[agents] resume_command`) and *Open agent transcript* |

Nothing in M4 is machine-verified GUI-side; see the smoke list below.

Commit history is linear on `main` and each milestone landed as focused
commits with tests; `git log --oneline` is a usable index.

## Load-bearing decisions (full table in ARCHITECTURE.md §1)

- **egui/eframe, glow backend.** Hard budget ≤ 100 MB RSS (last measured
  ~74 MB debug). Agreed fallback if egui can't hold it: GPUI — flag, don't
  work around.
- **No async runtime.** `std::thread` workers + mpsc + `request_repaint`.
  No subprocess or file IO on the UI thread.
- **TOML only.** `config.toml` (user-owned; app writes ONLY surgical
  `toml_edit` per-key edits — comment-preservation is tested) and
  `state.toml` (app-owned, atomic temp+fsync+rename).
- **Deterministic IDs.** 6 hex chars of FNV-1a/64+SplitMix64 over
  (git-common-dir, canonical path); session `wt-<id>`. Golden values pinned
  in tests. Never switch to random IDs.
- **Private tmux server** at `$XDG_RUNTIME_DIR/grove/tmux.sock`, started
  with Grove-owned `-f …/grove/tmux.conf`. Sessions carry
  `@grove_id/@grove_project/@grove_worktree/@grove_repo` user options —
  these are the primary reconciliation key, with the `wt-<id>` name and then
  the recorded worktree path as fallbacks.
- **Reconciliation marks, never deletes.** `state.toml` is an index in both
  directions: a live session absent from it is adopted, and a mapping whose
  session is gone shows the row as *stopped* rather than recreating
  anything. Orphaned sessions are reported, never closed.
- **No daemon.** `grove notify` = same binary, Unix-socket IPC to the GUI
  *plus* a durable `@grove_attention` tmux option so notifications survive
  the GUI being closed. Both deliveries are best-effort and notify exits 0
  when neither lands: it runs inside an agent's hook and must never fail it.
- **Attention latches.** Raised attention survives later quiet polls and
  survives an agent reporting `working`; only the user opening the session
  clears it, and clearing means *both* the in-memory latch and the durable
  tmux option — clear one and the next poll re-raises it.
- **Attention is never inferred.** Not from a process name, not from
  terminal output. Only `grove notify` or an opted-in tmux bell.
- **Claude Code is the one agent Grove knows by name**, and it knows it
  through one command (`grove notify --hook`) on five events — see
  ARCHITECTURE.md §6.1. Everything it learns is something the agent said:
  no transcript is read, and an event Grove has no opinion about reports
  nothing rather than guessing. `state.toml`'s `[[agent]]` table is an index
  of conversation ids, never a reason to remove anything.
- **systemd scopes (opt-in, auto when a user manager is present)**: agent
  commands are wrapped in `systemd-run --user --scope --collect` with a
  nonced unit name, for per-agent cgroup RAM/CPU. Plain shells are never
  wrapped. The scopes exist; nothing reads their cgroup counters yet.
- **Safety invariants** (CLAUDE.md, non-negotiable): Grove never deletes
  anything because it's missing from state; four separate confirmed
  operations for the four kinds of removal; reconciliation marks, never
  deletes; argument arrays everywhere; never touch the user's default tmux
  server; never log terminal contents.

## Code map

- `crates/grove-core` — everything testable: git porcelain parsers
  (worktree list, status v2), command builders, tmux server/session/client
  + pane listing, `removal.rs` risk report (pure), `config_write.rs`
  (toml_edit), `state.rs`, `terminal.rs` (tokenize-THEN-substitute — this
  ordering is the injection defense — `agent.rs` reuses it), `workflow.rs`
  (activation sequencing, `poll_session_signals`), `status.rs` (the
  working/idle/attention machine and the attention latch), `ipc.rs` (the
  `grove notify` wire format), `desktop.rs` (`notify-send`), `agent.rs`
  (agent window + systemd scopes), `reconcile.rs` (the startup/restore diff
  — the `reconcile` function itself is pure; `reconcile_all` is the thin IO
  wrapper). No UI deps — keep it that way.
- `crates/grove` — `app.rs` (eframe app + viewport plumbing), `workers.rs`
  (worker thread, all subprocess work), `notify.rs` (the `notify`
  subcommand), `status_watch.rs` (poller thread + notify listener, sharing
  one `StatusEngine` behind a mutex), `ui/theme.rs` (ALL colors/spacing —
  no raw `Color32` outside it), `ui/icons.rs` (ALL icons are epaint shapes;
  egui's bundled fonts lack many glyphs — never use text glyphs for icons),
  `ui/chrome.rs` (detached-window lifecycle `Detached<T>` + chrome),
  `ui/window_edge.rs` (edge resize for undecorated windows), `ui/dialogs/`,
  `ui/orphans.rs` (the orphaned-session section and its four choices),
  `ui/settings.rs`. Feature `native-file-picker` (default-on, `rfd`/portal);
  `--no-default-features` must always build and is part of the gate.

## The gate (before every commit)

`just gate` runs all five:

```bash
cargo build --workspace
cargo build -p grove --no-default-features
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Tests: 545. Integration tests run real git in temp repos and real tmux on
throwaway sockets in tempdirs (auto-killed by guard structs), and run the
real `grove notify` binary against a temp `XDG_RUNTIME_DIR`. New features
land with tests in the same commit.

## Operational notes for agents working here

- The user runs their own Claude Code session **inside a grove-managed tmux
  session** (`wt-7da7c9`) on the live socket `/run/user/1000/grove/tmux.sock`.
  Never kill that server or its sessions. Tests must stay on throwaway
  sockets.
- **Do not launch the GUI, screenshot, or send synthetic input** (xdotool
  etc.) — the user stopped an agent for this. Verification is
  compile + tests + gate; hand the user a manual smoke checklist instead.
  (KWin here doesn't support the screencopy protocol anyway.)
- A Claude Code hook rewrites some shell commands through `rtk` (a
  token-saving proxy); output may look filtered. `rtk proxy <cmd>` runs the
  raw command.
- rust-analyzer diagnostics in this environment routinely lag behind the
  real tree state (false alarms in every session so far, including one that
  led to a wasted "fix" brief). Trust `cargo`, not the IDE stream.
- Commit trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## Pending / next steps

1. **Manual smoke pass by the user** (nothing GUI-side is machine-verified):
   detached windows (drag, edge-resize, Esc/Ctrl+W vs Ctrl+Q policy,
   one-instance focus, no white flash), settings save preserving hand
   comments, native picker on all three entry points, feature-off build
   behavior, tofu-free icons, header drag.
2. **Smoke-test Milestone 3** (nothing GUI-side is machine-verified):
   the Restore chip and Ctrl+R producing the summary status line; killing
   a session outside Grove and seeing the row say "session stopped", then
   clicking it to start one again; `git worktree remove`-ing a worktree
   with a live session and seeing the orphan section, then trying each of
   open / associate / ignore / close (close needs the second, armed
   click); "Show again" after ignoring; renaming a project directory and
   seeing "Project unavailable" with Retry and "Locate project…" — and
   confirming its worktrees and branches are still on disk afterwards;
   deleting a worktree directory by hand and seeing the warning marker.
3. **Smoke-test Milestone 4** (nothing GUI-side is machine-verified):
   attention appearing on a row (`grove notify --state attention` from
   inside a session), it clearing when the row is opened and *staying*
   clear across the next poll, the desktop notification firing once rather
   than every 2 s, "Start agent" opening an `agent` window, RAM/CPU in the
   row (selected) and tooltip when accounting is on, and the new Settings
   fields saving
   without disturbing hand-written comments.
4. **Deferred small items**: window corner rounding
   (mockup has 14 px radius; needs transparent viewport + manual shadow —
   consciously skipped); config hot-reload on external edits (mtime check
   on the poller — discussed, agreed as cheap, not yet implemented).
