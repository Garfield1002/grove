# Grove — Handoff

_Last updated: 2026-07-27. Repo state: `main` @ `4bf830c`, working tree clean,
all gates green — `just gate` (498 tests, clippy `-D warnings`, fmt,
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
| M3 — Persistence & restore (startup reconciliation, orphaned/missing handling, Restore UI) | **Not started** — the header Restore chip is a disabled placeholder |
| M4 — Agent workflow | **Done** — see the breakdown below |

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
| `just install-claude-hook` | **Done** — merges `grove notify` into Claude Code's settings.json |

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
  these are the primary reconciliation key for M3.
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
  (agent window + systemd scopes). No UI deps — keep it that way.
- `crates/grove` — `app.rs` (eframe app + viewport plumbing), `workers.rs`
  (worker thread, all subprocess work), `notify.rs` (the `notify`
  subcommand), `status_watch.rs` (poller thread + notify listener, sharing
  one `StatusEngine` behind a mutex), `ui/theme.rs` (ALL colors/spacing —
  no raw `Color32` outside it), `ui/icons.rs` (ALL icons are epaint shapes;
  egui's bundled fonts lack many glyphs — never use text glyphs for icons),
  `ui/chrome.rs` (detached-window lifecycle `Detached<T>` + chrome),
  `ui/window_edge.rs` (edge resize for undecorated windows), `ui/dialogs/`,
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

Tests: 498. Integration tests run real git in temp repos and real tmux on
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
2. **Milestone 3** — reconciliation in `grove-core/src/reconcile.rs` (file
   named in ARCHITECTURE §3, not yet created): startup/refresh/restore
   diffing state.toml ↔ `git worktree list` ↔ `list-sessions` with
   `@grove_*` options as primary key; missing-project / orphaned-session /
   missing-session flows (DESIGN §11); enable the Restore chip.
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
