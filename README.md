# Grove

A small native GUI for the work you actually have in flight: your Git
projects, the worktrees under them, and the tmux session living in each one.

Grove does not render terminals. It launches *your* terminal, attached to a
private tmux server, and gets out of the way. Close Grove and every session
keeps running.

<!-- Screenshot goes here -->
![Grove](docs/screenshot.png)

## The idea

If you run more than one branch at a time — a review here, a long build
there, an agent chewing on something in a third — you end up with a pile of
terminals and no idea which one wants you. Grove is the index for that pile.

One narrow window lists every worktree, each with a status: **idle**,
**working**, or **attention**. Click one, and the terminal you already had
switches to it. Nothing is duplicated, nothing is re-rendered, and the
session you were in is still exactly where you left it.

Attention is the point. A worktree raises it only when something *says* so —
never because Grove guessed from a process name or read your scrollback —
and once raised it stays until you open that session. It cannot scroll past
you.

## Requirements

- **Linux.** Wayland-first (developed on Fedora); X11 works too.
- **git** and **tmux** on `PATH`. tmux 3.5 or newer, which is where the
  extended-key handling Grove relies on arrived; 3.6 adds pane scrollbars,
  which Grove turns on when it finds them.
- **A terminal emulator.** On first run Grove probes for `ptyxis`, `foot`,
  `alacritty`, `kitty` and `gnome-terminal`, in that order, and writes the
  one it finds into your config. Anything else works if you write the
  command yourself.

## Install

```bash
cargo install --path crates/grove --locked
just install-desktop     # launcher entry and icon; optional
```

Then just run it:

```bash
grove
```

There is nothing to configure first. Grove writes a commented
`config.toml` on first run recording the terminal it detected, and that file
is yours from then on — Grove never rewrites it behind your back.

## Getting started

1. **Open a project.** The `+` in the header, or the folder in the footer.
   Point it at any directory inside a Git repository; Grove finds the
   repository itself and lists every worktree it has.
2. **Press Enter on a row.** That creates the tmux session if it does not
   exist yet, then either switches your attached terminal to it or launches
   a new one.
3. **Create worktrees from `Ctrl+N`**, pick a base ref, and Grove runs
   `git worktree add` — with the path always editable before anything
   happens.

Sessions outlive the GUI. Quitting Grove, closing its window, logging out of
Grove entirely — none of it touches a running session. That is deliberate,
and the one exception is the power button in the footer, which kills the
tmux server and needs two clicks to say so.

## Keyboard

| Key | Does |
| --- | --- |
| `↑` `↓` | Move through the list |
| `Enter` | Open the selected worktree, or switch to it |
| `Ctrl+N` | Create a worktree |
| `Ctrl+R` | Restore: rebuild the view from git and tmux |
| `Delete` | Open the removal dialog for the selected row |
| `Alt+1`…`Alt+9` | Give this worktree a number (see below); same key removes it |
| `Ctrl+Q` / `Ctrl+W` | Quit — sessions keep running |

The field at the top filters the list as you type.

Right-click a row for the rest: open in a new terminal, add a tmux window,
start an agent, refresh, remove.

## Reaching a worktree from anywhere

Grove cannot bind a desktop shortcut, so it gives you the other half of one:

```bash
grove toggle 3     # open the session of the worktree carrying number 3
grove toggle       # start Grove, or close the running one
```

Give a row its number with `Alt+<digit>`. Bind `grove toggle 1`…`9` to
`Super+1`…`9` in your compositor and any worktree is one keystroke away from
anywhere on the desktop. If Grove is not running, it starts and opens the
worktree as soon as it knows what exists.

The numbers are labels and nothing more. They live in Grove's state file and
never name anything git or tmux knows about.

## Agents

Any program can report a session's status:

```bash
grove notify --state attention --message "needs permission to run tests"
```

Inside a Grove session `$GROVE_SESSION` is already set, so there is nothing
to pass. `attention` is sticky and clears only when you open the session;
`working` and `idle` are hints the next poll confirms.

### Claude Code

Claude Code is the one agent Grove knows by name, and one command wires it up:

```bash
grove hooks install
```

That merges `grove notify --hook` into `~/.claude/settings.json` on five
events. Your own hooks survive, the file is backed up first, one that cannot
be parsed is reported rather than overwritten, and installing twice leaves
one entry per event. Restart Claude Code afterwards — it reads its settings
at startup. `grove hooks print` shows exactly what would be added and
changes nothing; `grove hooks uninstall` takes it back out.

From then on Grove knows which *window* an agent is talking from, what it is
waiting for, and which conversation it belongs to — so the row menu can
offer *Resume agent conversation* and *Open agent transcript*. Set
`[agents] resume_command` to make the first one work; Grove ships no default
because only you know how your agent spells it.

Nothing here parses terminal output or infers state from process names.

## Configuration

`config.toml` is yours. Hand edits are first-class and survive: the Settings
pane edits it key by key, preserving your comments and formatting.

```toml
[terminal]
command = "kitty tmux -S {socket} attach-session -t {session}"

[worktrees]
default_parent = "/home/you/worktrees"

[status]
working_window_secs = 10
agent_commands = ["claude", "aider", "codex", "goose"]
bell_is_attention = false
desktop_notifications = true

[agents]
command = "claude"
resume_command = "claude --resume {agent_session}"
resource_accounting = "auto"    # auto | always | never
```

Templates are split with shell quoting rules *first*, and the placeholders
(`{socket}` `{session}` `{worktree}` `{project}` `{branch}`) are substituted
into the resulting arguments afterwards — so a path with a space, or a
branch name with a semicolon in it, can never become a second command.

With `resource_accounting` on a systemd machine, each agent runs in its own
scope and its RAM and CPU show up on the row.

### tmux

Grove runs a **private tmux server** on its own socket. Your everyday tmux is
untouched. Its config sources your `~/.tmux.conf` if you have one, then
Grove's own managed settings, then leaves the tail of the file for your
overrides — so anything you set wins.

Grove's managed half turns the status bar off (Grove is already that UI) and
turns on a pane scrollbar, activity monitoring, `allow-passthrough`, and the
extended-key handling that makes `Shift+Enter` reach the program in the pane.
Want the status bar back? `set -g status on` in your `tmux.conf`.

## What Grove will not do

This matters more than any feature, so it is worth stating plainly.

- **Nothing is ever deleted because Grove lost track of it.** Git and tmux
  are the truth; Grove's files are only an index. A worktree missing from
  that index is marked, never removed.
- **Refreshing marks; it does not act.** Restore reconciles the view with
  reality — reattaching live sessions, flagging stopped ones, listing
  sessions with no worktree behind them. It deletes nothing, ever.
- **The destructive things are four separate operations**, each with its own
  confirmation: removing a project from Grove, closing a tmux session,
  removing a git worktree, and deleting a branch. Grove will not bundle
  them, and removing a project from the list touches no repository at all.
- **Before you remove anything**, Grove shows you what it found: uncommitted
  changes, commits that are on no upstream, and whether something is still
  running in a pane. Forcing past git's refusal takes a second, explicit
  confirmation.
- **Your terminal contents are never read or logged.**

## Where things live

| | |
| --- | --- |
| `~/.config/grove/config.toml` | Yours. Grove reads it; you own it. |
| `~/.config/grove/tmux.conf` | Yours. Written once, then never again. |
| `~/.config/grove/grove.tmux.conf` | Grove's. Rewritten every start — don't edit. |
| `~/.local/state/grove/state.toml` | The index: projects, sessions, numbers. |
| `$XDG_RUNTIME_DIR/grove/` | The tmux and notify sockets. |

Lose `state.toml` and nothing is lost. Worktree IDs are derived from the
repository and path, so Grove re-derives the same ones and reattaches to the
sessions still running.

## Building

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Or `just gate` to run all of it. All four must pass.

Architecture notes are in [ARCHITECTURE.md](ARCHITECTURE.md); the full
product design is in [docs/DESIGN.md](docs/DESIGN.md).

## License

MIT — see [LICENSE](LICENSE).
