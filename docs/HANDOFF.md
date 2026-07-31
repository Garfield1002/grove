# Handoff

Written 2026-07-31. Everything here is either non-obvious or fragile; anything
you can get from `git log`, the code, or CLAUDE.md is deliberately not repeated.

## Read first

`MANIFESTO.md` is the destination and is newer than `docs/DESIGN.md`. DESIGN.md
§23's four milestones are all **complete** — that document describes a worktree
and session launcher, which Grove has been for several releases. The manifesto
resets the target to the *line of work*: lifecycle, review state, and answering
"what needs a person?" across 10–30 parallel efforts. Neither document is a
plan. There is no roadmap artifact; the old `docs/HANDOFF.md` was deleted with
the milestones it tracked.

## Landing — done 2026-07-31

`refactor/split-large-modules` was fast-forwarded into main, the manifesto
branch merged, and **main is pushed** (`475d8d6`). `status/done-and-attention-
reasons`, `label-windows-by-pane-title` and `manifesto/bulletproof-parallel-
work` are deleted.

**This repo contains two disjoint histories.** `main` roots at `f865d53`;
`fix/tmux-agent-colour-and-keys` and `ux/grove` root at `04a9f4c` and share *no
ancestor* with main. The earlier claim here that the fix branch was "fully
contained in main" was wrong — `git branch --merged` never listed it, and
`git merge-base` exits 1. Its commit subjects do all appear in main (the work
was replayed onto the new root) and `git diff main fix/…` is +2,031/−12,901, so
it holds nothing main lacks. But git cannot prove that, `-d` refuses it, and
`-D` on an unrelated history is a real decision. Same for `ux/grove`, checked
out at `~/dev/ux-grove`. Ask before deleting either.

`REPORT.txt` at the repo root is untracked and stale (it cites `service.rs` at
~1,000 lines; it is 1,559 after the split). Do not commit it. Regenerating a
review against the current branch is worth more than preserving it.

## The app.rs split — done 2026-07-31, not pushed

`app.rs` is **1,280 lines from 2,485**, in four commits on `main`
(`959ae7c`..`93e6ab4`), *not pushed*. Baseline is now **824 tests**.

Two of the four moved ownership, not just lines:

- `app/rows.rs` owns the project list, the state snapshot and the three caches
  stamped onto it, with **private fields**. The re-stamping that was a
  convention — touch `projects`, remember to call three `apply_*` methods — is
  now done on the way out of every mutator. It is the one piece with no egui,
  worker or service dependency, so it is the one that could finally be tested:
  **14 new tests**, on logic that had none because `GroveApp::new` starts
  threads and a daemon.
- `app/selection.rs` owns `selected` + `selected_window` + `filter`. The
  "exactly one row selected" rule was a stanza at the end of `apply_action`,
  so it did **not** hold for the six selections made while draining worker
  messages. `Selection::select` holds it everywhere now.

`app/service_events.rs` is a straight move of self-contained code. `app/dialogs.rs`
is a **file seam, not an ownership one** — its header records why a `Dialogs`
struct and a shared viewport helper were both considered and dropped, so that
is not relitigated from scratch.

What is left in `GroveApp` is genuinely cross-cutting: `drain_messages`,
`apply_action`, reconciliation, agent resumption, keyboard, frame. If it is
split further, that is the seam to argue about — and `drain_messages` is where
to look first.

## The gate

`cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo fmt --all` — all four, before any commit.
Current baseline: **824 tests, 12 suites**. Pedantic clippy is denied
workspace-wide (`Cargo.toml` `[workspace.lints.clippy] pedantic = "deny"`).

Correction to the earlier note here: `use super::*;` **is** available inside a
`#[cfg(test)] mod tests` — `wildcard_imports` exempts it, and every module in
`app/` uses it. It is unavailable in non-test code. Two pedantic lints that do
bite in new code: `field_reassign_with_default` (build the struct with
`..Default::default()` rather than assigning fields afterwards, which shows up
constantly in test fixtures) and `items_after_test_module`.

## Work in flight — done 2026-07-31

All 13 remaining raw `call_service(` sites are folded (`7537535`, landed on
main and pushed). `workers.rs` has **zero** raw sites and `call_service` is now private to
`service_call`. Two additions there: `NoReply` (a `serde::de::IgnoredAny`
alias) for calls whose reply nobody reads, and `removal_result` for the three
destructive steps, whose failure is a `RemovalFailed` naming the project and
operation rather than a `Message::Failed` banner.

Two arms changed shape, not just boilerplate: `session.associate` now decodes
into a struct instead of by hand (so a missing field reports the associate
failure, not its own message), and `session.open`'s refresh moved below the
call because both match arms did it.

The wrinkle, hit six more times: a success closure cannot touch `worker`,
because `handle` takes `&mut WorkerState` and the call already reborrows it.
Clone `worker.tasks` out first and `send` on the clone — `enqueue` is only
`let _ = self.tasks.send(task)`.

## Decisions already made — do not relitigate

- **No external services for now** (user's explicit call). The lifecycle tier
  is git-derivable + agent-reported only; PR and CI state are out of scope.
- **`done` is durable in the `@grove_done` tmux session option**, not in
  `StatusEngine`. This is not merely restart-survival: `grove wait --status
  done` runs in a *different process*, and `service.rs`'s `status_get` uses the
  stateless `status::classify`. State held in GUI memory would be known only to
  the GUI and the wait could never be satisfied. The engine's in-memory copy was
  deliberately deleted rather than kept alongside.
- **`AttentionReason` qualifies attention; it is not a peer state.** blocked,
  failed and waiting are one condition — work stopped, only a person restarts
  it. Peer variants would be four names for it to disagree over, and would make
  precedence a 7-way ordering instead of 4-way.
- **Precedence is now `attention > working > done > idle`**, implemented as the
  `Ord` derive's declaration order in `status.rs`. Adding a variant in the wrong
  position silently changes precedence everywhere.
- **Function-over-data, not a trait, for the service arms.** The arms do not
  differ in behaviour, only in four values; a trait would reproduce identical
  boilerplate inside 25 impls. A trait *would* be right if the service adapter
  were collapsed and the arms did real differing local work again.
- **Explicit imports everywhere**, no globs — pedantic clippy, and seeing each
  module's dependencies is half the point of the split.
- The service submodule is `mutations`, not `state`, because `grove_core::state`
  is already in scope.

## Traps that cost real time here

- **Do not automate import pruning or code transforms with regex.** A greedy
  pattern rewrote `use grove_core::claude::HookChange;` into `use HookChange;`
  across three files; a brace-matching arm transform later left `workers.rs`
  unparseable. Both were recoverable only because a commit had just been made.
  **Commit a safety point before any bulk edit.**
- **The editor's diagnostics lag badly in this repo.** They repeatedly reported
  errors that `cargo check` showed were already fixed. Trust `cargo check`.
- **`grep -E` fails** — the shell hook rewrites `grep` to `rg`, which rejects
  `-E`. Use `rg` syntax. The same hook *compresses* command output, so a `head`
  of a file can look duplicated or truncated; use the Read tool for ground truth.
- Test-only imports belong inside `mod tests`, not at module top, or
  `-D unused-imports` fails the non-test build.

## Open questions for the user — not yours to decide

1. **Is the service an implementation detail or a public automation surface?**
   Still unanswered, and nearly every complexity complaint dissolves once it is.
   MANIFESTO.md currently argues both sides ("automation surfaces should express
   Grove's domain operations" vs "not a remote execution platform").
2. **"100% test coverage"** is asserted in MANIFESTO.md, measured nowhere, and
   has no tooling in the justfile.
3. **The 30-row UI has never been looked at.** The success metric is 10–30
   concurrent lines of work understood at a glance; `worktree_row.rs` is 1,619
   lines of increasingly dense row. Populate 30 rows and look before adding more
   row detail.

## Known small debts

- The **idempotency cache in `service.rs` never evicts** — `Mutex<HashMap<String,
  Response>>` holding full successful responses for the life of the daemon.
- **`control_gate` is one global lock** across all worktrees; a slow
  `worktree.create` blocks an unrelated `session.open`.
- The **"protocol is not a stable contract yet" note lives only in a commit
  message** (`b1996d4`), not in ARCHITECTURE.md where the method list is.
- `service.rs` still holds ~1,090 lines of tests that want moving beside their
  subjects.

## Suggested next steps

1. ~~Land and **push**.~~ Done; main is on origin.
2. ~~Finish the 13 `call_service` sites by hand.~~ Done and pushed.
3. ~~`app.rs` (2,485 lines) — the hard split.~~ Done; 1,280 lines, not pushed.
4. The base-branch gap: `StatusSummary` knows everything about a branch relative
   to its own upstream and nothing relative to its *destination*. One config key
   (project base branch, defaulting to the detected default branch) makes
   `merged`, `ready` and `stale` fall out of one or two local git commands each —
   and `merged` is the row state that tells a user what to *close*, which is the
   promise with machinery but no trigger.
