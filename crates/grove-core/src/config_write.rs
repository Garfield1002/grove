//! Surgical edits to `config.toml`.
//!
//! `config.toml` is user-owned (ARCHITECTURE.md §4): hand edits are
//! first-class and must survive everything Grove does to the file. So Grove
//! never serializes a [`Config`](crate::config::Config) over it. Instead it
//! parses the document with `toml_edit`, replaces the *value* of exactly the
//! keys the user changed in the Settings UI, and writes the result back
//! atomically. Comments, key order, blank lines, quoting style and keys Grove
//! knows nothing about all come through untouched.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, value};

use crate::atomic;
use crate::error::{Error, Result};

/// A key Grove is allowed to edit: a top-level table and a key inside it.
///
/// Editing goes through the constants below, so no code path can invent a key
/// in the user's file by typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub table: &'static str,
    pub name: &'static str,
}

impl Key {
    /// Dotted rendering, for messages: `terminal.command`.
    pub fn dotted(&self) -> String {
        format!("{}.{}", self.table, self.name)
    }
}

/// `terminal.command` — the shell-style terminal template.
pub const TERMINAL_COMMAND: Key = Key {
    table: "terminal",
    name: "command",
};

/// `worktrees.default_parent` — parent directory for new worktrees.
pub const WORKTREES_DEFAULT_PARENT: Key = Key {
    table: "worktrees",
    name: "default_parent",
};

/// `status.working_window_secs` — quiet period before a session stops
/// counting as working.
pub const STATUS_WORKING_WINDOW: Key = Key {
    table: "status",
    name: "working_window_secs",
};

/// `status.bell_is_attention` — whether a tmux bell raises attention.
pub const STATUS_BELL_IS_ATTENTION: Key = Key {
    table: "status",
    name: "bell_is_attention",
};

/// `status.desktop_notifications` — whether attention also posts a desktop
/// notification.
pub const STATUS_DESKTOP_NOTIFICATIONS: Key = Key {
    table: "status",
    name: "desktop_notifications",
};

/// `agents.command` — the default agent command template.
pub const AGENTS_COMMAND: Key = Key {
    table: "agents",
    name: "command",
};

/// `agents.resume_command` — the template that reopens the agent's last
/// conversation in a worktree.
pub const AGENTS_RESUME_COMMAND: Key = Key {
    table: "agents",
    name: "resume_command",
};

/// `agents.resume_on_startup` — whether starting Grove brings back the
/// conversations whose agents are gone.
pub const AGENTS_RESUME_ON_STARTUP: Key = Key {
    table: "agents",
    name: "resume_on_startup",
};

/// `agents.resource_accounting` — auto | always | never.
pub const AGENTS_RESOURCE_ACCOUNTING: Key = Key {
    table: "agents",
    name: "resource_accounting",
};

/// One typed key assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Str(Key, String),
    Bool(Key, bool),
    Int(Key, i64),
}

impl Edit {
    /// A string-valued edit.
    pub fn string(key: Key, text: impl Into<String>) -> Self {
        Edit::Str(key, text.into())
    }

    /// A path-valued edit. Paths are stored as strings; a non-UTF-8 path is
    /// refused rather than silently mangled.
    pub fn path(key: Key, path: &Path) -> Result<Self> {
        match path.to_str() {
            Some(text) => Ok(Edit::Str(key, text.to_string())),
            None => Err(Error::ConfigEditKey {
                key: key.dotted(),
                reason: format!("{} is not valid UTF-8", path.display()),
            }),
        }
    }

    pub fn key(&self) -> Key {
        match self {
            Edit::Str(key, _) | Edit::Bool(key, _) | Edit::Int(key, _) => *key,
        }
    }

    fn item(&self) -> Item {
        match self {
            Edit::Str(_, text) => value(text.as_str()),
            Edit::Bool(_, flag) => value(*flag),
            Edit::Int(_, number) => value(*number),
        }
    }
}

/// Apply `edits` to the document `text`, returning the new document.
///
/// Pure, which is where every formatting guarantee is tested. A missing table
/// is created; an existing one keeps its position, comments and decor, and so
/// does every key the edits do not name.
pub fn edit_document(text: &str, edits: &[Edit], path: &Path) -> Result<String> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|source| Error::ConfigEdit {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;

    for edit in edits {
        let key = edit.key();
        let entry = document
            .as_table_mut()
            .entry(key.table)
            .or_insert_with(|| Item::Table(Table::new()));
        // A table with no header of its own (created here, or implied by a
        // `[table.sub]` elsewhere) must gain one, or the key would land at
        // the top level when the document is rendered.
        if let Some(table) = entry.as_table_mut() {
            table.set_implicit(false);
        }
        let table = entry
            .as_table_like_mut()
            .ok_or_else(|| Error::ConfigEditKey {
                key: key.dotted(),
                reason: format!("`{}` is not a table in {}", key.table, path.display()),
            })?;

        match table.get_mut(key.name) {
            // Replace the value in place, carrying the existing decor over:
            // the key, its comments and the surrounding whitespace stay
            // exactly where the user put them.
            Some(existing) => {
                let decor = existing.as_value().map(|v| v.decor().clone());
                let mut item = edit.item();
                if let (Some(new), Some(decor)) = (item.as_value_mut(), decor) {
                    *new.decor_mut() = decor;
                }
                *existing = item;
            }
            None => {
                table.insert(key.name, edit.item());
            }
        }
    }

    Ok(document.to_string())
}

/// Read `path`, apply `edits`, write it back atomically (temp + fsync +
/// rename, as `state.toml`).
///
/// A missing file is treated as an empty document, so the first Settings save
/// on a machine without `config.toml` creates one holding just the keys the
/// user set.
pub fn apply(path: &Path, edits: &[Edit]) -> Result<()> {
    if edits.is_empty() {
        return Ok(());
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(Error::io(format!("could not read {}", path.display()), e)),
    };
    let edited = edit_document(&text, edits, path)?;
    atomic::write(path, &edited)
}

/// Set one string key.
pub fn set_string(path: &Path, key: Key, text: &str) -> Result<()> {
    apply(path, &[Edit::string(key, text)])
}

/// Set one path-valued key.
pub fn set_path(path: &Path, key: Key, dir: &Path) -> Result<()> {
    apply(path, &[Edit::path(key, dir)?])
}

/// Set one boolean key.
pub fn set_bool(path: &Path, key: Key, flag: bool) -> Result<()> {
    apply(path, &[Edit::Bool(key, flag)])
}

/// Set one integer key.
pub fn set_integer(path: &Path, key: Key, number: i64) -> Result<()> {
    apply(path, &[Edit::Int(key, number)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writing a Milestone 4 key into the first-run file must create the real
    /// table without disturbing the commented-out documentation block that
    /// describes it.
    #[test]
    fn status_and_agent_keys_land_beside_their_own_documentation() {
        let original = crate::config::first_run_document("foot");
        let edits = vec![
            Edit::Int(STATUS_WORKING_WINDOW, 45),
            Edit::Bool(STATUS_BELL_IS_ATTENTION, true),
            Edit::Bool(STATUS_DESKTOP_NOTIFICATIONS, false),
            Edit::string(AGENTS_COMMAND, "claude"),
            Edit::string(AGENTS_RESOURCE_ACCOUNTING, "never"),
        ];
        let edited = edit_document(&original, &edits, Path::new("config.toml")).expect("edits");

        // The commented documentation survives untouched.
        assert!(edited.contains("# [status]"));
        assert!(edited.contains("# working_window_secs = 10"));
        assert!(edited.contains("# [agents.per_project]"));
        assert!(edited.contains("# ever wrote it once, on first run"));

        // And the values are readable back as themselves.
        let config = crate::config::Config::from_toml(&edited, Path::new("config.toml"))
            .expect("still valid toml");
        assert_eq!(config.status.working_window_secs, 45);
        assert!(config.status.bell_is_attention);
        assert!(!config.status.desktop_notifications);
        assert_eq!(config.agents.command, "claude");
        assert_eq!(config.agents.accounting(), crate::agent::Accounting::Never);
        // The terminal the file was created with is untouched.
        assert_eq!(config.terminal.command, "foot");
    }

    #[test]
    fn editing_one_status_key_leaves_the_others_alone() {
        let original = "# my notes\n\
                        [status]\n\
                        # why I raised this\n\
                        working_window_secs = 120\n\
                        bell_is_attention = true\n";
        let edited = edit_document(
            original,
            &[Edit::Bool(STATUS_DESKTOP_NOTIFICATIONS, false)],
            Path::new("config.toml"),
        )
        .expect("edits");
        assert!(edited.contains("# my notes"));
        assert!(edited.contains("# why I raised this"));
        assert!(edited.contains("working_window_secs = 120"));
        assert!(edited.contains("bell_is_attention = true"));
        assert!(edited.contains("desktop_notifications = false"));
    }

    const COMMENTED: &str = "\
# Grove configuration. Mine, hand-written.

[terminal]
# Shell-style command template.
# Placeholders: {socket} {session} {worktree} {project} {branch}
command = \"foot tmux -S {socket} attach-session -t {session}\"   # my terminal

# A table Grove knows nothing about.
[custom]
mine = 42
nested = { a = 1, b = \"two\" }

[worktrees]
default_parent = \"/home/u/worktrees\"
";

    fn path() -> &'static Path {
        Path::new("/home/u/.config/grove/config.toml")
    }

    #[test]
    fn editing_one_key_leaves_the_file_byte_identical_elsewhere() {
        let edited = edit_document(
            COMMENTED,
            &[Edit::string(TERMINAL_COMMAND, "kitty tmux -S {socket}")],
            path(),
        )
        .expect("edits");

        let expected = COMMENTED.replace(
            "\"foot tmux -S {socket} attach-session -t {session}\"",
            "\"kitty tmux -S {socket}\"",
        );
        assert_eq!(
            edited, expected,
            "only the edited value may differ from the original document"
        );
        // Spelled out, so a regression names what was lost.
        assert!(edited.contains("# Grove configuration. Mine, hand-written."));
        assert!(edited.contains("# Placeholders: {socket}"));
        assert!(edited.contains("# my terminal"));
        assert!(edited.contains("[custom]"));
        assert!(edited.contains("mine = 42"));
        assert!(edited.contains("nested = { a = 1, b = \"two\" }"));
    }

    #[test]
    fn key_and_table_order_survive_an_edit() {
        let edited = edit_document(
            COMMENTED,
            &[Edit::string(
                WORKTREES_DEFAULT_PARENT,
                "/home/u/trees with spaces",
            )],
            path(),
        )
        .expect("edits");
        let tables: Vec<&str> = edited
            .lines()
            .filter(|line| line.starts_with('['))
            .collect();
        assert_eq!(tables, vec!["[terminal]", "[custom]", "[worktrees]"]);
        assert!(edited.contains("default_parent = \"/home/u/trees with spaces\""));
    }

    #[test]
    fn several_keys_are_edited_in_one_pass() {
        let edited = edit_document(
            COMMENTED,
            &[
                Edit::string(TERMINAL_COMMAND, "alacritty -e tmux"),
                Edit::string(WORKTREES_DEFAULT_PARENT, "/tmp/wt"),
            ],
            path(),
        )
        .expect("edits");
        assert!(edited.contains("command = \"alacritty -e tmux\""));
        assert!(edited.contains("default_parent = \"/tmp/wt\""));
        assert!(edited.contains("# my terminal"), "decor is kept");
    }

    #[test]
    fn a_missing_table_is_created_with_its_header() {
        let original = "[terminal]\ncommand = \"foot\"\n";
        let edited = edit_document(
            original,
            &[Edit::string(WORKTREES_DEFAULT_PARENT, "/home/u/wt")],
            path(),
        )
        .expect("edits");
        assert!(edited.starts_with(original), "the original text is kept");
        assert!(edited.contains("[worktrees]"));
        let config = crate::config::Config::from_toml(&edited, path()).expect("valid toml");
        assert_eq!(
            config.default_worktree_parent(),
            Some(Path::new("/home/u/wt"))
        );
    }

    #[test]
    fn a_missing_key_is_added_to_an_existing_table() {
        let edited = edit_document(
            "# note\n[worktrees]\n# parent\ndefault_parent = \"/a\"\n",
            &[Edit::string(TERMINAL_COMMAND, "foot")],
            path(),
        )
        .expect("edits");
        assert!(edited.contains("# note"));
        assert!(edited.contains("# parent"));
        assert!(edited.contains("[terminal]"));
        assert!(edited.contains("command = \"foot\""));
    }

    #[test]
    fn a_table_that_only_existed_implicitly_gains_its_header() {
        let edited = edit_document(
            "[terminal.env]\nTERM = \"xterm\"\n",
            &[Edit::string(TERMINAL_COMMAND, "foot")],
            path(),
        )
        .expect("edits");
        assert!(edited.contains("[terminal]"));
        assert!(edited.contains("[terminal.env]"));
        assert!(edited.contains("TERM = \"xterm\""));
        let document = edited.parse::<DocumentMut>().expect("valid toml");
        assert_eq!(
            document["terminal"]["command"].as_str(),
            Some("foot"),
            "the key must belong to [terminal], not to the document root"
        );
    }

    #[test]
    fn an_empty_document_becomes_a_minimal_file() {
        let edited =
            edit_document("", &[Edit::string(TERMINAL_COMMAND, "foot")], path()).expect("edits");
        let config = crate::config::Config::from_toml(&edited, path()).expect("valid toml");
        assert_eq!(config.terminal.command, "foot");
    }

    #[test]
    fn values_needing_escapes_round_trip() {
        let template = "'my term' -e tmux -c \"{worktree}\\x\"";
        let edited = edit_document(
            COMMENTED,
            &[Edit::string(TERMINAL_COMMAND, template)],
            path(),
        )
        .expect("edits");
        let document = edited.parse::<DocumentMut>().expect("valid toml");
        assert_eq!(
            document["terminal"]["command"].as_str(),
            Some(template),
            "quotes and backslashes survive the round trip"
        );
    }

    #[test]
    fn typed_edits_write_their_own_toml_types() {
        let key = Key {
            table: "custom",
            name: "k",
        };
        assert_eq!(Edit::Bool(key, true).item().to_string().trim(), "true");
        assert_eq!(Edit::Int(key, -7).item().to_string().trim(), "-7");
        assert_eq!(
            Edit::path(WORKTREES_DEFAULT_PARENT, Path::new("/home/u/wt")).expect("utf-8"),
            Edit::Str(WORKTREES_DEFAULT_PARENT, "/home/u/wt".to_string())
        );
    }

    #[test]
    fn malformed_toml_is_reported_with_the_path() {
        let err = edit_document(
            "[terminal\ncommand =",
            &[Edit::string(TERMINAL_COMMAND, "foot")],
            path(),
        )
        .expect_err("bad toml");
        assert!(matches!(err, Error::ConfigEdit { .. }));
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn a_key_taken_by_a_non_table_is_refused() {
        let err = edit_document(
            "terminal = \"foot\"\n",
            &[Edit::string(TERMINAL_COMMAND, "kitty")],
            path(),
        )
        .expect_err("terminal is not a table");
        assert!(matches!(err, Error::ConfigEditKey { .. }));
        assert!(err.to_string().contains("terminal.command"));
    }

    // ------------------------------------------------------------ on disk

    #[test]
    fn apply_preserves_the_users_file_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, COMMENTED).expect("write");

        set_string(&file, TERMINAL_COMMAND, "kitty tmux").expect("edits");

        let after = std::fs::read_to_string(&file).expect("read");
        assert_eq!(
            after,
            COMMENTED.replace(
                "\"foot tmux -S {socket} attach-session -t {session}\"",
                "\"kitty tmux\""
            )
        );
    }

    #[test]
    fn apply_creates_the_file_and_its_directory_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("grove").join("config.toml");
        assert!(!file.exists());

        set_path(&file, WORKTREES_DEFAULT_PARENT, Path::new("/home/u/wt")).expect("creates");

        let text = std::fs::read_to_string(&file).expect("read");
        let config = crate::config::Config::from_toml(&text, &file).expect("valid toml");
        assert_eq!(
            config.default_worktree_parent(),
            Some(Path::new("/home/u/wt"))
        );
    }

    #[test]
    fn apply_with_no_edits_does_not_create_a_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("config.toml");
        apply(&file, &[]).expect("nothing to do");
        assert!(!file.exists());
    }

    #[test]
    fn apply_replaces_atomically_and_leaves_no_temp_files() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, COMMENTED).expect("write");
        let before = std::fs::metadata(&file).expect("stat").ino();

        set_bool(
            &file,
            Key {
                table: "custom",
                name: "flag",
            },
            true,
        )
        .expect("edits");

        assert_ne!(before, std::fs::metadata(&file).expect("stat").ino());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn an_integer_key_round_trips_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "# keep me\n").expect("write");
        set_integer(
            &file,
            Key {
                table: "status",
                name: "idle_seconds",
            },
            10,
        )
        .expect("edits");
        let text = std::fs::read_to_string(&file).expect("read");
        assert!(text.contains("# keep me"), "the user's comment survives");
        assert!(text.contains("[status]"));
        assert!(text.contains("idle_seconds = 10"));
    }

    #[test]
    fn a_malformed_file_on_disk_is_left_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("config.toml");
        std::fs::write(&file, "[terminal\n").expect("write");
        assert!(set_string(&file, TERMINAL_COMMAND, "foot").is_err());
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "[terminal\n");
    }

    #[test]
    fn the_first_run_document_survives_an_edit_with_its_comments() {
        let original = crate::config::first_run_document("foot tmux -S {socket}");
        let edited = edit_document(
            &original,
            &[Edit::string(TERMINAL_COMMAND, "kitty tmux -S {socket}")],
            path(),
        )
        .expect("edits");
        assert!(edited.contains("# Placeholders: {socket} {session}"));
        assert!(edited.contains("# [worktrees]"));
        let config = crate::config::Config::from_toml(&edited, path()).expect("valid toml");
        assert_eq!(config.terminal.command, "kitty tmux -S {socket}");
        assert_eq!(
            config.default_worktree_parent(),
            None,
            "the commented-out example stays commented out"
        );
    }
}
