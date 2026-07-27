//! `config.toml` — user-owned configuration.
//!
//! Grove reads this file. It creates it once, on first run, when no file
//! exists and a terminal has been auto-detected. After that the only writes
//! are the *surgical* per-key edits in [`crate::config_write`], made when the
//! user changes something in the Settings UI: comments, ordering and unknown
//! keys survive, and the whole `Config` struct is never serialized over a file
//! the user has touched (ARCHITECTURE.md §4).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::terminal;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub terminal: TerminalConfig,
    pub worktrees: WorktreeConfig,
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
         # default_parent = \"/home/you/worktrees\"\n",
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
        // Shift+Enter reaches the pane only as a CSI-u sequence, and only
        // when the outer terminal is declared able to carry extended keys.
        assert!(document.contains("set -s extended-keys on"));
        assert!(document.contains("set -s extended-keys-format csi-u"));
        assert!(document.contains("extkeys"));
        // tmux ignores an application's kitty-protocol request and folds
        // Shift+Enter into Enter, so the sequence is injected by hand.
        assert!(document.contains("bind -n S-Enter send-keys -H 1b 5b 31 33 3b 32 75"));
        // Agents signal attention with OSC sequences; tmux must pass them on.
        assert!(document.contains("set -g allow-passthrough on"));
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
