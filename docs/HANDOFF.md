# Grove — Handoff

_Last updated: 2026-07-27. Repo state: `main` @ `f8430f8`, working tree clean,
all gates green (358 tests, clippy `-D warnings`, fmt, `--no-default-features`
build)._

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
| M4 — Agent workflow (status poller, working/idle/attention pills, `grove notify` + IPC, `@grove_attention`, desktop notifications, agent commands, systemd-scope resource accounting) | **Not started** |

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
- **No daemon.** M4's `grove notify` = same binary, Unix-socket IPC to the
  GUI *plus* a durable `@grove_attention` tmux option so notifications
  survive the GUI being closed.
- **systemd scopes (M4, opt-in)**: wrap pane-launched agent commands in
  `systemd-run --user --scope --collect` for per-agent cgroup RAM/CPU.
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
  ordering is the injection defense), `workflow.rs` (activation sequencing).
  No UI deps — keep it that way.
- `crates/grove` — `app.rs` (eframe app + viewport plumbing), `workers.rs`
  (worker thread, all subprocess work), `ui/theme.rs` (ALL colors/spacing —
  no raw `Color32` outside it), `ui/icons.rs` (ALL icons are epaint shapes;
  egui's bundled fonts lack many glyphs — never use text glyphs for icons),
  `ui/chrome.rs` (detached-window lifecycle `Detached<T>` + chrome),
  `ui/window_edge.rs` (edge resize for undecorated windows), `ui/dialogs/`,
  `ui/settings.rs`. Feature `native-file-picker` (default-on, `rfd`/portal);
  `--no-default-features` must always build and is part of the gate.

## The gate (before every commit)

```bash
cargo build --workspace
cargo build -p grove --no-default-features
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Tests: 358. Integration tests run real git in temp repos and real tmux on
throwaway sockets in tempdirs (auto-killed by guard structs). New features
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
  real tree state (three separate false alarms so far — including one that
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
3. **Milestone 4** — status engine (`grove-core/src/status.rs` state
   machine; 2 s tmux / 10 s git poller thread), status pills + accent edges
   per mockup 1c (colors already reserved in `theme.rs` as
   `STATUS_WORKING`/`STATUS_ATTENTION`), `grove notify` subcommand + IPC
   listener + `@grove_attention`, desktop notifications, agent command
   templates, systemd-scope wrapping + RAM/CPU display.
4. **Deferred small items**: `.direnv`-style dev loop (`cargo watch -x 'run
   -p grove'` recipe was discussed, not added); window corner rounding
   (mockup has 14 px radius; needs transparent viewport + manual shadow —
   consciously skipped); config hot-reload on external edits (mtime check
   on the poller — discussed, agreed as cheap, not yet implemented).
