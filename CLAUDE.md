# Grove

Native Rust GUI (egui/eframe) for navigating Git projects, worktrees, and
persistent tmux sessions. It launches terminals attached to a private tmux
server — it never renders terminals itself.

Read [ARCHITECTURE.md](ARCHITECTURE.md) before structural changes; the full
product design is in [docs/DESIGN.md](docs/DESIGN.md).

## Workspace

- `crates/grove-core` — library: git/tmux/state/reconcile logic. **No UI
  dependencies.** Everything here must be unit-testable without a display,
  a real repo, or a running tmux server (parsers take strings, commands are
  built as `(program, args)` values).
- `crates/grove` — binary: egui app plus CLI subcommands (`grove` launches
  the GUI, `grove notify` reports agent status over IPC).

`grove` may depend on `grove-core`; never the reverse.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo run -p grove
```

All four of build/test/clippy/fmt must pass before any commit.

## Code rules

- Edition 2024. rustfmt defaults — no config overrides.
- No `unwrap()` / `expect()` outside `#[cfg(test)]` code. Use `thiserror`
  error enums in grove-core; surface errors to the UI, never panic.
- No async runtime. Concurrency is `std::thread` workers + mpsc channels
  into the egui loop (`ctx.request_repaint()` after sending).
- This project is **heavily tested** — tests are not optional polish:
  - Every parser of git/tmux output (`worktree list --porcelain`, status,
    `list-sessions` formats) gets unit tests with captured real-world
    samples, including malformed input.
  - All grove-core logic (id hashing, template expansion, reconciliation,
    status state machine, atomic state writes) gets unit tests.
  - Integration tests exercise real `git` against temp repos (`tempfile` crate)
    and real `tmux` against a throwaway test socket; skip gracefully with a
    message if the binary is absent, never silently pass.
  - New functionality lands with its tests in the same commit.
- Subprocesses: `std::process::Command` with argument arrays. Never build
  shell strings from paths or branch names. The only shell-interpreted
  strings are the user's configured terminal/agent templates.
- tmux: always pass the private socket (`-S`). Never touch the user's
  default tmux server.

## Safety invariants (non-negotiable)

- Grove must never delete a worktree, branch, or repository because it is
  missing from `state.toml`. Git and tmux are the source of truth; TOML
  files are only an index.
- Removing a project from Grove, closing a tmux session, removing a git
  worktree, and deleting a branch are four separate operations, each with
  its own confirmation.
- Reconciliation (startup/refresh/restore) marks things missing, stopped,
  or orphaned — it never auto-deletes.
- `state.toml` writes are atomic (temp file in same dir + rename).
  `config.toml` is user-owned first: hand edits are first-class and must
  survive. The app may write it only via `toml_edit` (surgical per-key
  edits preserving comments and formatting) — first-run auto-detect and
  explicit changes made in the Settings UI. Never serialize the whole
  config over the file.
- Never log terminal contents.

## Design constraints

- Memory budget: ≤ 100 MB RSS. Prefer the glow backend; avoid per-frame
  allocations in the row list. If egui can't hold the budget, the agreed
  fallback is GPUI — flag it, don't work around it.
- No subprocess call on the UI thread, ever.
- Wayland-first (Fedora). Don't add compositor-specific window tricks or
  attempt to focus/move external terminal windows.
- Worktree IDs are deterministic: hash of (git-common-dir, canonical path),
  6 hex chars, session name `wt-<id>`. Don't switch to random IDs — restore
  after state loss depends on re-deriving them.
- Status precedence: attention > working > idle. Attention latches until
  the user opens the session. Working = pane activity within 10 s
  (configurable) or a known agent process. Don't infer attention from
  process names or by parsing terminal output.
