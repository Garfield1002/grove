//! `state.toml` — app-owned state.
//!
//! Milestone 1 persists only the registered project list. The file is an
//! *index*, never a source of truth: git and tmux decide what exists, and
//! nothing is ever deleted because it is absent here (ARCHITECTURE.md §8.1).
//! Full startup reconciliation arrives in Milestone 3; the shapes below are
//! deliberately additive (`#[serde(default)]` everywhere) so a newer field or
//! table can be introduced without invalidating an older file, and an older
//! Grove tolerates a file written by a newer one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Current `state.toml` schema version.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub version: u32,
    #[serde(rename = "project")]
    pub projects: Vec<ProjectRecord>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            projects: Vec::new(),
        }
    }
}

/// A project the user registered. Removing one from this list removes it from
/// Grove only — never from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectRecord {
    /// Deterministic id derived from the git-common-dir.
    pub id: String,
    pub name: String,
    /// Main worktree (or bare repository) directory.
    pub repository_path: PathBuf,
    /// Repository identity: `git rev-parse --git-common-dir`.
    pub git_common_dir: PathBuf,
    /// Parent directory the create-worktree dialog defaults to.
    pub default_worktree_path: PathBuf,
    pub is_expanded: bool,
}

impl Default for ProjectRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            repository_path: PathBuf::new(),
            git_common_dir: PathBuf::new(),
            default_worktree_path: PathBuf::new(),
            is_expanded: true,
        }
    }
}

impl State {
    pub fn from_toml(text: &str, path: &Path) -> Result<Self> {
        toml::from_str(text).map_err(|source| Error::StateRead {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn find(&self, id: &str) -> Option<&ProjectRecord> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Add a project, or update the existing record with the same id. Never
    /// duplicates a repository.
    pub fn upsert(&mut self, record: ProjectRecord) {
        match self.projects.iter_mut().find(|p| p.id == record.id) {
            Some(existing) => {
                existing.name = record.name;
                existing.repository_path = record.repository_path;
                existing.git_common_dir = record.git_common_dir;
                existing.default_worktree_path = record.default_worktree_path;
                existing.is_expanded = record.is_expanded;
            }
            None => self.projects.push(record),
        }
    }

    /// Remove a project from Grove's index. This must never be accompanied by
    /// any filesystem or git operation.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.projects.len();
        self.projects.retain(|p| p.id != id);
        self.projects.len() != before
    }
}

/// Load `state.toml`. A missing file is an empty state, not an error.
pub fn load(path: &Path) -> Result<State> {
    match std::fs::read_to_string(path) {
        Ok(text) => State::from_toml(&text, path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(Error::io(format!("could not read {}", path.display()), e)),
    }
}

/// Write `state.toml` atomically: serialize into a temp file in the same
/// directory, fsync it, then `rename(2)` over the target. A crash mid-write
/// leaves the previous file intact.
pub fn save(path: &Path, state: &State) -> Result<()> {
    use std::io::Write;

    let text = state.to_toml()?;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::io(format!("could not create {}", dir.display()), e))?;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state.toml".to_string());
    let temp = dir.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        temp_counter()
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::io(format!("could not write {}", temp.display()), e));
    }

    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(Error::io(
            format!("could not replace {}", path.display()),
            e,
        ));
    }
    Ok(())
}

fn temp_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, name: &str) -> ProjectRecord {
        ProjectRecord {
            id: id.to_string(),
            name: name.to_string(),
            repository_path: PathBuf::from(format!("/home/u/{name}")),
            git_common_dir: PathBuf::from(format!("/home/u/{name}/.git")),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        state.upsert(ProjectRecord {
            is_expanded: false,
            ..record("ddeeff", "design-system")
        });

        let text = state.to_toml().expect("serializes");
        let parsed = State::from_toml(&text, Path::new("state.toml")).expect("parses");
        assert_eq!(parsed, state);
        assert_eq!(parsed.version, STATE_VERSION);
        assert!(!parsed.find("ddeeff").expect("present").is_expanded);
    }

    #[test]
    fn round_trips_paths_with_spaces_and_unicode() {
        let mut state = State::default();
        state.upsert(ProjectRecord {
            repository_path: PathBuf::from("/home/u/my projects/wörk repo"),
            git_common_dir: PathBuf::from("/home/u/my projects/wörk repo/.git"),
            ..record("a1b2c3", "wörk repo")
        });
        let text = state.to_toml().expect("serializes");
        let parsed = State::from_toml(&text, Path::new("state.toml")).expect("parses");
        assert_eq!(parsed, state);
    }

    #[test]
    fn a_missing_file_loads_as_empty_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = load(&tmp.path().join("never-written.toml")).expect("loads");
        assert_eq!(state, State::default());
        assert!(state.projects.is_empty());
    }

    #[test]
    fn an_empty_file_loads_as_empty_state() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.toml");
        std::fs::write(&path, "").expect("write");
        assert_eq!(load(&path).expect("loads"), State::default());
    }

    #[test]
    fn a_partial_file_is_tolerated() {
        let state = State::from_toml(
            "[[project]]\nid = \"a1b2c3\"\nname = \"acme\"\n",
            Path::new("state.toml"),
        )
        .expect("partial file");
        let project = &state.projects[0];
        assert_eq!(project.name, "acme");
        assert_eq!(project.repository_path, PathBuf::new());
        assert!(project.is_expanded, "defaults to expanded");
        assert_eq!(state.version, STATE_VERSION);
    }

    #[test]
    fn unknown_future_fields_do_not_break_loading() {
        let state = State::from_toml(
            "version = 99\nsomething_new = true\n\n[[project]]\nid = \"a1b2c3\"\nname = \"acme\"\nfuture_field = 3\n",
            Path::new("state.toml"),
        )
        .expect("forward compatible");
        assert_eq!(state.version, 99);
        assert_eq!(state.projects.len(), 1);
    }

    #[test]
    fn malformed_toml_reports_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.toml");
        std::fs::write(&path, "[[project\n").expect("write");
        let err = load(&path).expect_err("bad toml");
        assert!(matches!(err, Error::StateRead { .. }));
        assert!(err.to_string().contains("state.toml"));
    }

    #[test]
    fn save_then_load_round_trips_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("grove").join("state.toml");
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        save(&path, &state).expect("saves");
        assert_eq!(load(&path).expect("loads"), state);
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.toml");
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        save(&path, &state).expect("saves");
        save(&path, &state).expect("saves again");

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "state.toml")
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// The point of temp+rename: the old file is never truncated in place, so
    /// a reader always sees a complete document.
    #[test]
    fn save_replaces_atomically_and_never_truncates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.toml");

        let mut first = State::default();
        first.upsert(record("a1b2c3", "acme-web"));
        save(&path, &first).expect("saves");
        let ino_before = inode(&path);

        let mut second = State::default();
        second.upsert(record("ddeeff", "design-system"));
        save(&path, &second).expect("saves");

        assert_ne!(
            ino_before,
            inode(&path),
            "the file must be replaced, not rewritten in place"
        );
        assert_eq!(load(&path).expect("loads"), second);
    }

    #[test]
    fn save_overwrites_a_read_only_directory_error_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.toml");
        std::fs::create_dir(&path).expect("a directory where the file should go");
        let err = save(&path, &State::default()).expect_err("cannot replace a directory");
        assert!(matches!(err, Error::Io { .. }));
    }

    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).expect("stat").ino()
    }

    #[test]
    fn upsert_updates_in_place_without_duplicating() {
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        state.upsert(ProjectRecord {
            name: "renamed".into(),
            is_expanded: false,
            ..record("a1b2c3", "acme-web")
        });
        assert_eq!(state.projects.len(), 1);
        let project = state.find("a1b2c3").expect("present");
        assert_eq!(project.name, "renamed");
        assert!(!project.is_expanded);
    }

    #[test]
    fn remove_only_touches_the_index() {
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        state.upsert(record("ddeeff", "design-system"));
        assert!(state.remove("a1b2c3"));
        assert!(!state.remove("a1b2c3"));
        assert_eq!(state.projects.len(), 1);
        assert!(state.find("ddeeff").is_some());
    }
}
