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
         command = {}\n",
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
    fn a_blank_command_is_not_a_terminal() {
        let config =
            Config::from_toml("[terminal]\ncommand = \"   \"\n", Path::new("c.toml")).expect("ok");
        assert!(!config.has_terminal());
    }
}
