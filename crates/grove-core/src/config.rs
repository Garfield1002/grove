//! `config.toml` — user-owned configuration.
//!
//! Grove reads this file. It creates it once, on first run, when no file
//! exists and a terminal has been auto-detected. After that the only writes
//! are the *surgical* per-key edits in [`crate::config_write`], made when the
//! user changes something in the Settings UI: comments, ordering and unknown
//! keys survive, and the whole `Config` struct is never serialized over a file
//! the user has touched (ARCHITECTURE.md §4).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent::Accounting;
use crate::error::{Error, Result};
use crate::status::StatusPolicy;
use crate::terminal;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub terminal: TerminalConfig,
    pub worktrees: WorktreeConfig,
    pub status: StatusConfig,
    pub agents: AgentConfig,
}

/// How Claude Code resumes a conversation, and the default `resume_command`.
///
/// Claude Code is the one agent Grove knows by name: the conversation ids this
/// template substitutes are the ones Claude Code itself reported through
/// `grove notify --hook`, so its spelling of "resume" is the one Grove can
/// know without guessing. Any other agent overrides it; blanking it turns the
/// action off.
pub const DEFAULT_RESUME_COMMAND: &str = "claude --resume {agent_session}";

/// The `[agents]` section: the command Grove starts in a session's agent
/// window, and whether to account for its resources (DESIGN.md §15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Shell-style command template, expanded like the terminal one. Empty
    /// means Grove offers no "start agent" action at all.
    pub command: String,
    /// Template that reopens the agent's last conversation in a worktree,
    /// with `{agent_session}` standing for the id the agent reported through
    /// `grove notify`. Defaults to [`DEFAULT_RESUME_COMMAND`], the spelling
    /// used by the agent that reports those ids; empty means Grove offers no
    /// "resume" action at all.
    pub resume_command: String,
    /// Whether starting Grove brings back the conversations `state.toml`
    /// recorded, for worktrees where no agent is running any more. On by
    /// default: an agent that outlived Grove is left alone, and one that did
    /// not is what the user quit with.
    pub resume_on_startup: bool,
    /// Per-project overrides, keyed by project name.
    pub per_project: std::collections::BTreeMap<String, String>,
    /// `auto` (wrap when a systemd user manager is present), `always` or
    /// `never`. An unrecognised value falls back to `auto` rather than
    /// refusing to load the file.
    pub resource_accounting: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            resume_command: DEFAULT_RESUME_COMMAND.to_string(),
            resume_on_startup: true,
            per_project: std::collections::BTreeMap::new(),
            resource_accounting: String::new(),
        }
    }
}

impl AgentConfig {
    /// The template for a project: its own if it has one, else the default.
    pub fn command_for(&self, project: &str) -> Option<&str> {
        self.per_project
            .get(project)
            .map(String::as_str)
            .or(Some(self.command.as_str()))
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }

    /// The template that resumes an agent's last conversation.
    ///
    /// [`DEFAULT_RESUME_COMMAND`] unless the file says otherwise; `None` only
    /// when the user blanked the key, which is how the action is turned off
    /// for an agent that spells resuming differently or not at all.
    pub fn resume_command(&self) -> Option<&str> {
        Some(self.resume_command.trim()).filter(|c| !c.is_empty())
    }

    pub fn accounting(&self) -> Accounting {
        Accounting::parse(&self.resource_accounting).unwrap_or_default()
    }
}

/// The `[status]` section: how sessions are judged working, idle or needing
/// attention (DESIGN.md §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatusConfig {
    /// Seconds of quiet after which a session stops counting as working.
    pub working_window_secs: u64,
    /// Process names that mean an agent is running in a session.
    pub agent_commands: Vec<String>,
    /// Whether a tmux bell raises attention. Off by default: bells are noisy,
    /// and `grove notify` is the reliable signal.
    pub bell_is_attention: bool,
    /// Whether raised attention also posts a desktop notification.
    pub desktop_notifications: bool,
}

impl Default for StatusConfig {
    fn default() -> Self {
        let policy = StatusPolicy::default();
        Self {
            working_window_secs: policy.working_window.as_secs(),
            agent_commands: policy.agent_commands,
            bell_is_attention: policy.bell_is_attention,
            desktop_notifications: true,
        }
    }
}

impl StatusConfig {
    /// The policy the status engine runs with.
    ///
    /// A zero or absurd window would make every session look permanently
    /// working or permanently idle, so it is clamped rather than trusted.
    pub fn policy(&self) -> StatusPolicy {
        StatusPolicy {
            working_window: Duration::from_secs(self.working_window_secs.clamp(1, 3600)),
            agent_commands: self
                .agent_commands
                .iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
            bell_is_attention: self.bell_is_attention,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// Parent directory new worktrees are created under (DESIGN.md §15).
    /// Empty means "beside the repository". Only ever a default: the create
    /// dialog's path field is editable.
    pub default_parent: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Shell-style command template. Trusted user configuration; see
    /// [`crate::terminal`] for the substitution rules.
    pub command: String,
}

impl Config {
    pub fn from_toml(text: &str, path: &Path) -> Result<Self> {
        toml::from_str(text).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Is the terminal command usable?
    pub fn has_terminal(&self) -> bool {
        !self.terminal.command.trim().is_empty()
    }

    /// The configured default worktree parent, if the user set one.
    pub fn default_worktree_parent(&self) -> Option<&Path> {
        let value = self.worktrees.default_parent.trim();
        (!value.is_empty()).then(|| Path::new(value))
    }
}

/// The commented file written on first run.
pub fn first_run_document(terminal_command: &str) -> String {
    format!(
        "# Grove configuration. This file is yours: Grove reads it and only\n\
         # ever wrote it once, on first run, to record the terminal it detected.\n\
         \n\
         [terminal]\n\
         # Shell-style command template. Grove splits it with shell quoting\n\
         # rules first and only then substitutes the placeholders, so paths and\n\
         # branch names can never add arguments.\n\
         # Placeholders: {{socket}} {{session}} {{worktree}} {{project}} {{branch}}\n\
         command = {}\n\
         \n\
         # [worktrees]\n\
         # Parent directory for worktrees created from the GUI. Unset means\n\
         # \"beside the repository\". The create dialog always lets you edit\n\
         # the path before anything is created.\n\
         # default_parent = \"/home/you/worktrees\"\n\
         \n\
         # [status]\n\
         # How sessions are judged working, idle or needing attention.\n\
         # Attention comes from `grove notify --state attention` (run it from\n\
         # your agent's hooks) and stays until you open the session.\n\
         # working_window_secs = 10\n\
         # agent_commands = [\"claude\", \"aider\", \"codex\", \"goose\"]\n\
         # bell_is_attention = false\n\
         # desktop_notifications = true\n\
         \n\
         # [agents]\n\
         # Command started in a session's `agent` window. Split with shell\n\
         # quoting rules first, then the placeholders above are substituted\n\
         # into the resulting arguments — never the other way round.\n\
         # command = \"claude\"\n\
         # resource_accounting = \"auto\"   # auto | always | never\n\
         #\n\
         # Reopens the last conversation `grove notify --agent-session` saw in\n\
         # a worktree. {{agent_session}} is the id the agent reported. The\n\
         # default below is Claude Code's spelling, since Claude Code is what\n\
         # reports those ids; set it to \"\" to offer no resume at all.\n\
         # resume_command = \"claude --resume {{agent_session}}\"\n\
         #\n\
         # Starting Grove brings those conversations back, in worktrees where\n\
         # no agent is running any more. An agent that outlived Grove is left\n\
         # alone. Set to false to resume only from the row menu.\n\
         # resume_on_startup = true\n\
         #\n\
         # [agents.per_project]\n\
         # acme-web = \"claude --resume\"\n",
        toml_string(terminal_command)
    )
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// Result of loading configuration, so the caller can tell the user that a
/// file was created for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub config: Config,
    /// True when this run created `config.toml`.
    pub created: bool,
}

/// Load `config.toml`, creating it on first run with an auto-detected
/// terminal.
///
/// `detect` returns the template to record; when it fails, the error is
/// propagated and no file is written, so a later run can still auto-detect.
pub fn load_or_init(
    path: &Path,
    detect: impl FnOnce() -> Result<&'static str>,
) -> Result<LoadedConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(LoadedConfig {
            config: Config::from_toml(&text, path)?,
            created: false,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let template = detect()?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::io(format!("could not create {}", parent.display()), e))?;
            }
            let document = first_run_document(template);
            std::fs::write(path, &document)
                .map_err(|e| Error::io(format!("could not write {}", path.display()), e))?;
            Ok(LoadedConfig {
                config: Config::from_toml(&document, path)?,
                created: true,
            })
        }
        Err(e) => Err(Error::io(format!("could not read {}", path.display()), e)),
    }
}

/// Default template for tests and for the settings pane's "what would be
/// detected" hint.
pub fn detect_terminal_template() -> Result<&'static str> {
    terminal::detect()
}

/// The Grove-owned tmux settings, shipped inside the binary.
///
/// Kept in the source tree (`assets/grove.tmux.conf`) rather than inlined
/// into the user's file so that a fix — a missing `mouse on`, a new
/// `terminal-features` entry — reaches existing installs on upgrade. The
/// user's own `tmux.conf` only sources this.
pub const MANAGED_TMUX_CONFIG_DOCUMENT: &str = include_str!("../assets/grove.tmux.conf");

/// Grove's own `tmux.conf`, written on first run.
///
/// A private server started with `-S` alone still reads `~/.tmux.conf`, so
/// Grove passes `-f` and owns the file (ARCHITECTURE.md §2). The default
/// sources the user's own configuration, then Grove's managed settings, and
/// leaves the tail of the file for overrides. Like `config.toml`, this is
/// written once and never rewritten.
///
/// `managed` must be absolute: tmux resolves a relative `source-file`
/// against the working directory, not against the file doing the sourcing.
pub fn tmux_config_document(managed: &Path) -> String {
    format!(
        "\
# Grove's private tmux server configuration.
#
# Grove starts its server with `-f` pointing here, so ~/.tmux.conf is not
# read automatically. This file was generated once, on first run; it is
# yours to edit and Grove will never rewrite it.

# Your own configuration first (silently skipped when absent). If you keep
# yours at ~/.config/tmux/tmux.conf, add a second source-file line for it.
source-file -q ~/.tmux.conf

# The settings Grove depends on. That file is regenerated on every start,
# so edit it here instead — anything below this line wins.
source-file '{}'
",
        managed.display()
    )
}

/// Rewrite the managed half of the configuration from the shipped copy.
///
/// Unconditional and atomic: the file is Grove's, not the user's, and a
/// half-written one would make `tmux -f` fail.
pub fn write_managed_tmux_config(path: &Path) -> Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == MANAGED_TMUX_CONFIG_DOCUMENT
    {
        return Ok(());
    }
    crate::atomic::write(path, MANAGED_TMUX_CONFIG_DOCUMENT)
}

/// Create `tmux.conf` if it does not exist. Returns true when it was created.
pub fn ensure_tmux_config(path: &Path, managed: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io(format!("could not create {}", parent.display()), e))?;
    }
    match std::fs::File::create_new(path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(tmux_config_document(managed).as_bytes())
                .map_err(|e| Error::io(format!("could not write {}", path.display()), e))?;
            Ok(true)
        }
        // Another Grove process won the race; its file is just as good.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(Error::io(format!("could not create {}", path.display()), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DETECTED: &str = "foot tmux -S {socket} attach-session -t {session}";

    #[test]
    fn no_agent_command_means_no_agent_action() {
        let config = Config::default();
        assert_eq!(config.agents.command_for("acme-web"), None);
        assert_eq!(config.agents.accounting(), Accounting::Auto);
    }

    /// Resuming is the one agent command Grove can spell for the user: the ids
    /// it substitutes came from Claude Code's own hooks. A user whose agent
    /// spells it differently — or who wants no resume offered — says so by
    /// blanking the key, exactly as with every other command here.
    #[test]
    fn resuming_defaults_to_the_agent_that_reports_the_ids() {
        let config = Config::default();
        assert_eq!(
            config.agents.resume_command(),
            Some("claude --resume {agent_session}")
        );

        let untouched = Config::from_toml("[agents]\ncommand = \"claude\"\n", Path::new("c.toml"))
            .expect("valid");
        assert_eq!(
            untouched.agents.resume_command(),
            Some("claude --resume {agent_session}"),
            "an [agents] section that says nothing about resuming keeps the default"
        );

        let opted_out = Config::from_toml("[agents]\nresume_command = \"\"\n", Path::new("c.toml"))
            .expect("valid");
        assert_eq!(
            opted_out.agents.resume_command(),
            None,
            "blanking the key is how the resume action is turned off"
        );

        let overridden = Config::from_toml(
            "[agents]\nresume_command = \"aider --restore-chat-history\"\n",
            Path::new("c.toml"),
        )
        .expect("valid");
        assert_eq!(
            overridden.agents.resume_command(),
            Some("aider --restore-chat-history")
        );
    }

    #[test]
    fn a_per_project_agent_command_beats_the_default() {
        let text = "[agents]\n\
                    command = \"claude\"\n\
                    resource_accounting = \"never\"\n\
                    \n\
                    [agents.per_project]\n\
                    acme-web = \"claude --resume\"\n\
                    quiet-repo = \"  \"\n";
        let config = Config::from_toml(text, Path::new("c.toml")).expect("valid");
        assert_eq!(
            config.agents.command_for("acme-web"),
            Some("claude --resume")
        );
        assert_eq!(config.agents.command_for("other"), Some("claude"));
        assert_eq!(
            config.agents.command_for("quiet-repo"),
            None,
            "a project can opt out by blanking its command"
        );
        assert_eq!(config.agents.accounting(), Accounting::Never);
    }

    #[test]
    fn an_unrecognised_accounting_value_falls_back_rather_than_failing() {
        // The file is the user's; a typo must not stop Grove from loading it.
        let config = Config::from_toml(
            "[agents]\nresource_accounting = \"sometimes\"\n",
            Path::new("c.toml"),
        )
        .expect("still loads");
        assert_eq!(config.agents.accounting(), Accounting::Auto);
    }

    #[test]
    fn an_absent_status_section_is_the_default_policy() {
        let config = Config::from_toml("[terminal]\ncommand = \"foot\"\n", Path::new("c.toml"))
            .expect("valid");
        assert_eq!(config.status.policy(), StatusPolicy::default());
        assert!(config.status.desktop_notifications);
    }

    #[test]
    fn the_status_section_is_read_into_the_policy() {
        let text = "[status]\n\
                    working_window_secs = 45\n\
                    agent_commands = [\"myagent\", \" spaced \", \"\"]\n\
                    bell_is_attention = true\n\
                    desktop_notifications = false\n";
        let config = Config::from_toml(text, Path::new("c.toml")).expect("valid");
        let policy = config.status.policy();
        assert_eq!(policy.working_window, Duration::from_secs(45));
        assert_eq!(policy.agent_commands, vec!["myagent", "spaced"]);
        assert!(policy.bell_is_attention);
        assert!(!config.status.desktop_notifications);
    }

    #[test]
    fn an_absurd_working_window_is_clamped_not_trusted() {
        // 0 would make every session permanently idle; a huge value would make
        // every session permanently working.
        let zero = Config::from_toml("[status]\nworking_window_secs = 0\n", Path::new("c.toml"))
            .expect("valid");
        assert_eq!(zero.status.policy().working_window, Duration::from_secs(1));

        let huge = Config::from_toml(
            "[status]\nworking_window_secs = 99999999\n",
            Path::new("c.toml"),
        )
        .expect("valid");
        assert_eq!(
            huge.status.policy().working_window,
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn an_empty_agent_list_is_honoured() {
        // Not "fall back to the defaults": a user who empties the list means it.
        let config = Config::from_toml("[status]\nagent_commands = []\n", Path::new("c.toml"))
            .expect("valid");
        assert!(config.status.policy().agent_commands.is_empty());
    }

    #[test]
    fn the_first_run_document_documents_the_status_section() {
        let text = first_run_document(DETECTED);
        // Commented out, so the defaults stay live, but discoverable.
        assert!(text.contains("# [status]"));
        assert!(text.contains("grove notify --state attention"));
        let parsed = Config::from_toml(&text, Path::new("c.toml")).expect("valid toml");
        assert_eq!(parsed.status, StatusConfig::default());
    }

    #[test]
    fn first_run_writes_the_detected_template_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("grove").join("config.toml");

        let first = load_or_init(&path, || Ok(DETECTED)).expect("first run");
        assert!(first.created);
        assert_eq!(first.config.terminal.command, DETECTED);
        assert!(path.exists());

        // A second run must not touch the file, whatever detection would say.
        let before = std::fs::read_to_string(&path).expect("read");
        let second = load_or_init(&path, || panic!("must not detect again")).expect("second run");
        assert!(!second.created);
        assert_eq!(second.config.terminal.command, DETECTED);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
    }

    #[test]
    fn user_edits_are_never_clobbered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.toml");
        let user =
            "# my notes\n[terminal]\ncommand = \"kitty tmux -S {socket} attach -t {session}\"\n";
        std::fs::write(&path, user).expect("write");

        let loaded = load_or_init(&path, || Ok(DETECTED)).expect("loads");
        assert!(!loaded.created);
        assert_eq!(
            loaded.config.terminal.command,
            "kitty tmux -S {socket} attach -t {session}"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), user);
    }

    #[test]
    fn nothing_is_written_when_detection_fails() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.toml");
        let err = load_or_init(&path, || Err(Error::NoTerminalFound("foot".into())))
            .expect_err("detection failed");
        assert!(matches!(err, Error::NoTerminalFound(_)));
        assert!(!path.exists(), "no file may be left behind");
    }

    #[test]
    fn the_generated_document_is_valid_toml_and_keeps_placeholders() {
        let document = first_run_document(DETECTED);
        assert!(document.contains("{socket}"));
        assert!(document.contains("{worktree}"));
        let config = Config::from_toml(&document, Path::new("config.toml")).expect("valid toml");
        assert_eq!(config.terminal.command, DETECTED);
    }

    #[test]
    fn a_template_with_quotes_round_trips() {
        let template = "'my term' -e tmux -S {socket} attach -t {session}";
        let document = first_run_document(template);
        let config = Config::from_toml(&document, Path::new("config.toml")).expect("valid toml");
        assert_eq!(config.terminal.command, template);
        assert!(terminal::tokenize(&config.terminal.command).is_ok());
    }

    #[test]
    fn malformed_toml_reports_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "[terminal\ncommand =").expect("write");
        let err = load_or_init(&path, || Ok(DETECTED)).expect_err("bad toml");
        assert!(err.to_string().contains("config.toml"));
        assert!(matches!(err, Error::ConfigRead { .. }));
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        let err = Config::from_toml(
            "[terminal]\ncommand = \"foot\"\ntypo = true\n",
            Path::new("config.toml"),
        )
        .expect_err("unknown key");
        assert!(err.to_string().contains("typo"));
    }

    #[test]
    fn a_missing_terminal_section_is_a_default_config() {
        let config = Config::from_toml("", Path::new("config.toml")).expect("valid");
        assert_eq!(config, Config::default());
        assert!(!config.has_terminal());
    }

    #[test]
    fn the_default_worktree_parent_is_optional_and_trimmed() {
        let config = Config::default();
        assert_eq!(config.default_worktree_parent(), None);

        let config = Config::from_toml(
            "[worktrees]\ndefault_parent = \"/home/u/my worktrees\"\n",
            Path::new("config.toml"),
        )
        .expect("valid");
        assert_eq!(
            config.default_worktree_parent(),
            Some(Path::new("/home/u/my worktrees"))
        );

        let config = Config::from_toml(
            "[worktrees]\ndefault_parent = \"  \"\n",
            Path::new("c.toml"),
        )
        .expect("valid");
        assert_eq!(config.default_worktree_parent(), None);
    }

    #[test]
    fn the_generated_document_only_comments_the_worktree_section() {
        // Writing the key would make Grove's default look like a user choice.
        let document = first_run_document(DETECTED);
        assert!(document.contains("# [worktrees]"));
        assert!(document.contains("# default_parent ="));
        let config = Config::from_toml(&document, Path::new("config.toml")).expect("valid toml");
        assert_eq!(config.default_worktree_parent(), None);
    }

    #[test]
    fn a_blank_command_is_not_a_terminal() {
        let config =
            Config::from_toml("[terminal]\ncommand = \"   \"\n", Path::new("c.toml")).expect("ok");
        assert!(!config.has_terminal());
    }

    #[test]
    fn the_tmux_config_is_written_once_and_never_rewritten() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("grove").join("tmux.conf");
        let managed = tmp.path().join("grove").join("grove.tmux.conf");

        assert!(ensure_tmux_config(&path, &managed).expect("creates"));
        let written = std::fs::read_to_string(&path).expect("read");
        assert_eq!(written, tmux_config_document(&managed));

        // A user edit must survive every later run.
        std::fs::write(&path, "set -g status off\n").expect("user edit");
        assert!(!ensure_tmux_config(&path, &managed).expect("keeps the file"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "set -g status off\n"
        );
    }

    #[test]
    fn the_tmux_config_sources_the_users_own_and_then_groves_managed_file() {
        let managed = Path::new("/home/u/.config/grove/grove.tmux.conf");
        let document = tmux_config_document(managed);

        // source-file -q so a missing ~/.tmux.conf is not an error.
        assert!(document.contains("source-file -q ~/.tmux.conf"));
        // Absolute and quoted: tmux resolves relative paths against the
        // working directory, and the path may contain spaces.
        assert!(document.contains("source-file '/home/u/.config/grove/grove.tmux.conf'"));
        // Grove's settings load after the user's, and before the tail of the
        // file that the user is invited to override in.
        let users = document.find("~/.tmux.conf").expect("sources the user's");
        let groves = document.find("grove.tmux.conf").expect("sources Grove's");
        assert!(users < groves);
    }

    #[test]
    fn the_managed_file_carries_the_settings_grove_depends_on() {
        let document = MANAGED_TMUX_CONFIG_DOCUMENT;
        assert!(document.contains("set -g monitor-bell on"));
        assert!(document.contains("set -g monitor-activity on"));
        assert!(document.contains("set -s exit-empty off"));
        // The wheel is dead without this: tmux holds the alternate screen.
        assert!(document.contains("set -g mouse on"));
        // Grove is the session-and-window UI; tmux's own status bar is a
        // second one saying the same thing.
        assert!(document.contains("set -g status off"));
        // Scrollbars are a window option, so `-wg` and not `-g`.
        assert!(document.contains("set -wg pane-scrollbars on"));
        assert!(document.contains("set -wg pane-scrollbars-style"));
        // Shift+Enter reaches the pane only as a CSI-u sequence, and only
        // when the outer terminal is declared able to carry extended keys.
        // `always` rather than `on`: tmux otherwise waits for the application
        // to ask, and folds Shift+Enter into Enter until it does.
        assert!(document.contains("set -g extended-keys always"));
        assert!(document.contains("set -g extended-keys-format csi-u"));
        assert!(document.contains("extkeys"));
        // Agents signal attention with OSC sequences; tmux must pass them on.
        assert!(document.contains("set -g allow-passthrough on"));
        // Claude Code rounds every colour to the 256-palette whenever `$TMUX`
        // is set. The pane negotiates RGB above, so the cap is pure loss —
        // and neither FORCE_COLOR nor any TERM undoes it, only this.
        assert!(document.contains("set-environment -g CLAUDE_CODE_TMUX_TRUECOLOR 1"));
    }

    #[test]
    fn the_managed_file_is_rewritten_over_any_local_edit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("grove").join("grove.tmux.conf");

        write_managed_tmux_config(&path).expect("creates");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            MANAGED_TMUX_CONFIG_DOCUMENT
        );

        // Unlike tmux.conf, this file is Grove's; edits here are not kept.
        std::fs::write(&path, "set -g mouse off\n").expect("stale edit");
        write_managed_tmux_config(&path).expect("rewrites");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            MANAGED_TMUX_CONFIG_DOCUMENT
        );
    }
}
