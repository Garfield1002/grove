//! Grove's coding-agent layer: everything that knows an agent exists.
//!
//! Grove itself has no opinion about agents. A worktree has a session, a
//! session has a status, and something inside it may report through
//! `grove notify` — a build, a test run, a deploy, or a coding agent, and
//! Grove cannot tell which and does not need to. That reporting vocabulary
//! (working, idle, attention, done, and the reason and message beside them)
//! is `grove_core`'s, because it describes a *session*.
//!
//! What lives here is the part that is specific to coding agents:
//!
//! - starting one in its own tmux window, optionally inside a systemd scope,
//! - the conversation it reports, so a later launch can resume it,
//! - the vendor integrations that make an agent report at all — Claude Code's
//!   hooks, today,
//! - the inference rule that a running `claude` or `aider` means work is
//!   happening, which is the stand-in for an agent that cannot say so itself.
//!
//! `grove` depends on this crate behind the `agents` feature. Building without
//! it leaves a Git worktree and tmux session manager whose sessions can still
//! report their own status; nothing in `grove_core` refers to anything here.
//!
//! **Data schemas deliberately stay in `grove_core`**, even the agent-shaped
//! ones: `AgentRecord` in `state.toml`, and the `agent_session` and
//! `transcript` fields of a notification. A build without this crate must
//! still round-trip a state file written by a build with it, or turning the
//! feature off would silently drop the user's records — and Grove does not
//! lose an index. Schemas are shared; behaviour is optional.

pub mod claude;
