//! `config.toml` — user-owned configuration.
//!
//! Grove reads this file. It writes it exactly once: on first run, when no
//! file exists and a terminal has been auto-detected. It never rewrites or
//! reformats a file the user has touched (ARCHITECTURE.md §4).

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

/// Grove's own `tmux.conf`, written on first run.
///
/// A private server started with `-S` alone still reads `~/.tmux.conf`, so
/// Grove passes `-f` and owns the file (ARCHITECTURE.md §2). The default
/// sources the user's own configuration when it exists and then applies the
/// settings Grove's status detection depends on. Like `config.toml`, this is
/// written once and never rewritten.
pub const TMUX_CONFIG_DOCUMENT: &str = "\
# Grove's private tmux server configuration.
#
# Grove starts its server with `-f` pointing here, so ~/.tmux.conf is not
# read automatically. This file was generated once, on first run; it is
# yours to edit and Grove will never rewrite it.

# Your own configuration first (silently skipped when absent). If you keep
# yours at ~/.config/tmux/tmux.conf, add a second source-file line for it.
source-file -q ~/.tmux.conf

# Settings Grove depends on. Removing these degrades status detection.
set -g monitor-bell on
set -g monitor-activity on
set -s exit-empty off
";

/// Create `tmux.conf` if it does not exist. Returns true when it was created.
pub fn ensure_tmux_config(path: &Path) -> Result<bool> {
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
            file.write_all(TMUX_CONFIG_DOCUMENT.as_bytes())
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

        assert!(ensure_tmux_config(&path).expect("creates"));
        let written = std::fs::read_to_string(&path).expect("read");
        assert_eq!(written, TMUX_CONFIG_DOCUMENT);

        // A user edit must survive every later run.
        std::fs::write(&path, "set -g mouse on\n").expect("user edit");
        assert!(!ensure_tmux_config(&path).expect("keeps the file"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "set -g mouse on\n"
        );
    }

    #[test]
    fn the_tmux_config_sources_the_users_own_and_sets_what_grove_needs() {
        // source-file -q so a missing ~/.tmux.conf is not an error.
        assert!(TMUX_CONFIG_DOCUMENT.contains("source-file -q ~/.tmux.conf"));
        assert!(TMUX_CONFIG_DOCUMENT.contains("set -g monitor-bell on"));
        assert!(TMUX_CONFIG_DOCUMENT.contains("set -g monitor-activity on"));
        assert!(TMUX_CONFIG_DOCUMENT.contains("set -s exit-empty off"));
    }
}
