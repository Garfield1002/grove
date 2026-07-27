//! `state.toml` — app-owned state.
//!
//! It holds the registered project list, the worktree ↔ session mappings Grove
//! has seen, and the orphaned sessions the user asked it to stop mentioning.
//! The file is an *index*, never a source of truth: git and tmux decide what
//! exists, and nothing is ever deleted because it is absent here
//! (ARCHITECTURE.md §8.1). Conversely nothing here can resurrect a session:
//! a mapping whose session tmux no longer reports is shown as *stopped*
//! (DESIGN.md §11), never recreated behind the user's back.
//!
//! The shapes below are deliberately additive (`#[serde(default)]`
//! everywhere) so a newer field or table can be introduced without
//! invalidating an older file, and an older Grove tolerates a file written by
//! a newer one.

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
    /// Worktree ↔ session mappings Grove has seen. Purely an index: a record
    /// whose session is gone makes the row say *stopped*, and never recreates
    /// anything.
    #[serde(rename = "session")]
    pub sessions: Vec<SessionRecord>,
    /// Orphaned tmux sessions the user chose to ignore, by session name.
    /// Ignoring hides a session from the restore report; it never closes it.
    pub ignored_sessions: Vec<String>,
    /// The numbers the user has put on worktrees, so `grove toggle <n>` can
    /// name one from a keyboard shortcut. A label and nothing more: a number
    /// pointing at a worktree that no longer exists selects nothing, and is
    /// never a reason to touch anything on disk.
    #[serde(rename = "slot")]
    pub slots: Vec<SlotRecord>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            projects: Vec::new(),
            sessions: Vec::new(),
            ignored_sessions: Vec::new(),
            slots: Vec::new(),
        }
    }
}

/// The highest number a worktree can be given. Nine because the point of the
/// numbers is a one-keystroke shortcut, and there are nine digit keys.
pub const MAX_SLOT: u8 = 9;

/// Parse a slot number, accepting only 1..=[`MAX_SLOT`].
///
/// Used by the CLI and by the IPC decoder, so a number that could never name
/// anything is rejected where it is typed rather than silently ignored later.
pub fn parse_slot(raw: &str) -> Option<u8> {
    let number: u8 = raw.trim().parse().ok()?;
    (1..=MAX_SLOT).contains(&number).then_some(number)
}

/// One number the user put on a worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotRecord {
    /// 1..=[`MAX_SLOT`].
    pub number: u8,
    /// Deterministic worktree id the number points at.
    pub worktree_id: String,
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

/// A tmux session Grove has seen for a worktree.
///
/// Written when reconciliation finds a live session and kept afterwards, so a
/// session that disappears can be reported as *stopped* rather than as "there
/// was never one" (DESIGN.md §11). It is dropped only when the user closes
/// that session themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionRecord {
    /// Deterministic worktree id; the session is named `wt-<id>`.
    pub worktree_id: String,
    pub project_id: String,
    pub worktree_path: PathBuf,
    pub session_name: String,
    /// Seconds since the Unix epoch of the last activity tmux reported, or 0
    /// when it never reported one.
    pub last_activity_at: u64,
}

impl State {
    pub fn from_toml(text: &str, path: &Path) -> Result<Self> {
        let mut state: Self = toml::from_str(text).map_err(|source| Error::StateRead {
            path: path.to_path_buf(),
            source,
        })?;
        state.sanitize_slots();
        Ok(state)
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
    ///
    /// Its session records go with it — they are an index of that project's
    /// sessions and would otherwise describe rows that are no longer shown.
    /// The tmux sessions themselves keep running; closing one is a separate,
    /// separately confirmed operation (ARCHITECTURE.md §8.2).
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.projects.len();
        self.projects.retain(|p| p.id != id);
        self.sessions.retain(|s| s.project_id != id);
        self.projects.len() != before
    }

    pub fn session(&self, worktree_id: &str) -> Option<&SessionRecord> {
        self.sessions.iter().find(|s| s.worktree_id == worktree_id)
    }

    /// Record (or refresh) the session mapping for a worktree.
    pub fn record_session(&mut self, record: SessionRecord) {
        match self
            .sessions
            .iter_mut()
            .find(|s| s.worktree_id == record.worktree_id)
        {
            Some(existing) => *existing = record,
            None => self.sessions.push(record),
        }
    }

    /// Forget a worktree's session mapping. Called when the user closes that
    /// session: a session that vanished on its own keeps its record, which is
    /// what makes it show as *stopped* instead of silently disappearing.
    pub fn forget_session(&mut self, worktree_id: &str) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.worktree_id != worktree_id);
        self.sessions.len() != before
    }

    /// Every worktree id Grove has a session record for.
    pub fn recorded_session_ids(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|s| s.worktree_id.clone())
            .collect()
    }

    pub fn is_ignored(&self, session_name: &str) -> bool {
        self.ignored_sessions.iter().any(|n| n == session_name)
    }

    /// Stop reporting an orphaned session. Nothing is closed or deleted.
    pub fn ignore_session(&mut self, session_name: &str) {
        if !self.is_ignored(session_name) {
            self.ignored_sessions.push(session_name.to_string());
        }
    }

    /// Report ignored sessions again.
    pub fn clear_ignored_sessions(&mut self) -> bool {
        let before = self.ignored_sessions.len();
        self.ignored_sessions.clear();
        before != 0
    }

    /// The number on a worktree, if it has one.
    pub fn slot(&self, worktree_id: &str) -> Option<u8> {
        self.slots
            .iter()
            .find(|s| s.worktree_id == worktree_id)
            .map(|s| s.number)
    }

    /// The worktree a number points at, if any.
    pub fn slot_worktree(&self, number: u8) -> Option<&str> {
        self.slots
            .iter()
            .find(|s| s.number == number)
            .map(|s| s.worktree_id.as_str())
    }

    /// Put `number` on a worktree, taking it off whoever held it and off
    /// whatever number that worktree had. Both directions are unique: one
    /// worktree per number, one number per worktree.
    ///
    /// Returns false for a number outside 1..=[`MAX_SLOT`], which is the only
    /// way this can fail.
    pub fn assign_slot(&mut self, number: u8, worktree_id: &str) -> bool {
        if !(1..=MAX_SLOT).contains(&number) {
            return false;
        }
        self.slots
            .retain(|s| s.number != number && s.worktree_id != worktree_id);
        self.slots.push(SlotRecord {
            number,
            worktree_id: worktree_id.to_string(),
        });
        self.slots.sort_by_key(|s| s.number);
        true
    }

    /// Take the number off a worktree.
    pub fn clear_slot(&mut self, worktree_id: &str) -> bool {
        let before = self.slots.len();
        self.slots.retain(|s| s.worktree_id != worktree_id);
        self.slots.len() != before
    }

    /// Drop slots a hand-edited (or newer) file could hold but Grove cannot
    /// act on: numbers out of range, an empty id, and duplicates in either
    /// direction. First entry wins, so the file reads as it behaves.
    fn sanitize_slots(&mut self) {
        let mut kept: Vec<SlotRecord> = Vec::with_capacity(self.slots.len());
        for slot in std::mem::take(&mut self.slots) {
            let usable = (1..=MAX_SLOT).contains(&slot.number)
                && !slot.worktree_id.is_empty()
                && !kept
                    .iter()
                    .any(|k| k.number == slot.number || k.worktree_id == slot.worktree_id);
            if usable {
                kept.push(slot);
            }
        }
        kept.sort_by_key(|s| s.number);
        self.slots = kept;
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
    crate::atomic::write(path, &state.to_toml()?)
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
    fn a_number_is_unique_in_both_directions() {
        let mut state = State::default();
        assert!(state.assign_slot(3, "a1b2c3"));
        assert_eq!(state.slot("a1b2c3"), Some(3));
        assert_eq!(state.slot_worktree(3), Some("a1b2c3"));

        // Giving 3 to another worktree takes it off the first.
        assert!(state.assign_slot(3, "ddeeff"));
        assert_eq!(state.slot("a1b2c3"), None);
        assert_eq!(state.slot_worktree(3), Some("ddeeff"));

        // And giving a numbered worktree a new number frees the old one.
        assert!(state.assign_slot(5, "ddeeff"));
        assert_eq!(state.slot_worktree(3), None);
        assert_eq!(state.slot("ddeeff"), Some(5));
        assert_eq!(state.slots.len(), 1);
    }

    #[test]
    fn numbers_outside_one_to_nine_are_refused() {
        let mut state = State::default();
        assert!(!state.assign_slot(0, "a1b2c3"));
        assert!(!state.assign_slot(10, "a1b2c3"));
        assert!(state.slots.is_empty());
        assert_eq!(parse_slot("0"), None);
        assert_eq!(parse_slot("10"), None);
        assert_eq!(parse_slot("-1"), None);
        assert_eq!(parse_slot("three"), None);
        assert_eq!(parse_slot(""), None);
        assert_eq!(parse_slot(" 4 "), Some(4));
        assert_eq!(parse_slot("9"), Some(9));
    }

    #[test]
    fn clearing_a_number_leaves_the_others() {
        let mut state = State::default();
        state.assign_slot(1, "a1b2c3");
        state.assign_slot(2, "ddeeff");
        assert!(state.clear_slot("a1b2c3"));
        assert!(!state.clear_slot("a1b2c3"), "already cleared");
        assert_eq!(state.slot_worktree(2), Some("ddeeff"));
    }

    #[test]
    fn slots_round_trip_through_toml_in_order() {
        let mut state = State::default();
        state.assign_slot(4, "ddeeff");
        state.assign_slot(1, "a1b2c3");
        let text = state.to_toml().expect("serializes");
        let parsed = State::from_toml(&text, Path::new("state.toml")).expect("parses");
        assert_eq!(parsed, state);
        assert_eq!(
            parsed.slots.iter().map(|s| s.number).collect::<Vec<_>>(),
            vec![1, 4]
        );
    }

    /// A hand-edited file is first-class input: unusable entries are dropped
    /// rather than silently shadowing a working one.
    #[test]
    fn a_hand_edited_file_with_bad_slots_loads_with_the_usable_ones() {
        let state = State::from_toml(
            "[[slot]]\nnumber = 3\nworktree_id = \"a1b2c3\"\n\n\
             [[slot]]\nnumber = 3\nworktree_id = \"ddeeff\"\n\n\
             [[slot]]\nnumber = 99\nworktree_id = \"ccddee\"\n\n\
             [[slot]]\nnumber = 0\nworktree_id = \"ffaabb\"\n\n\
             [[slot]]\nnumber = 5\nworktree_id = \"\"\n\n\
             [[slot]]\nnumber = 7\nworktree_id = \"a1b2c3\"\n",
            Path::new("state.toml"),
        )
        .expect("parses");
        assert_eq!(
            state.slots,
            vec![SlotRecord {
                number: 3,
                worktree_id: "a1b2c3".into()
            }],
            "first entry wins; out-of-range, empty and duplicate entries go"
        );
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

    // -------------------------------------------------- session records (M3)

    fn session(worktree_id: &str, project_id: &str) -> SessionRecord {
        SessionRecord {
            worktree_id: worktree_id.to_string(),
            project_id: project_id.to_string(),
            worktree_path: PathBuf::from(format!("/home/u/wt/{worktree_id}")),
            session_name: format!("wt-{worktree_id}"),
            last_activity_at: 1_753_600_000,
        }
    }

    #[test]
    fn session_records_round_trip_through_toml() {
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        state.record_session(session("bceeb7", "a1b2c3"));
        state.ignore_session("wt-999999");

        let text = state.to_toml().expect("serializes");
        let parsed = State::from_toml(&text, Path::new("state.toml")).expect("parses");
        assert_eq!(parsed, state);
        assert_eq!(
            parsed.session("bceeb7").map(|s| s.session_name.as_str()),
            Some("wt-bceeb7")
        );
        assert!(parsed.is_ignored("wt-999999"));
    }

    #[test]
    fn recording_a_session_twice_updates_it_in_place() {
        let mut state = State::default();
        state.record_session(session("bceeb7", "a1b2c3"));
        state.record_session(SessionRecord {
            last_activity_at: 1_753_609_999,
            ..session("bceeb7", "a1b2c3")
        });
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(
            state.session("bceeb7").map(|s| s.last_activity_at),
            Some(1_753_609_999)
        );
    }

    #[test]
    fn forgetting_a_session_removes_only_that_mapping() {
        let mut state = State::default();
        state.record_session(session("bceeb7", "a1b2c3"));
        state.record_session(session("69f1b5", "a1b2c3"));
        assert!(state.forget_session("bceeb7"));
        assert!(!state.forget_session("bceeb7"));
        assert_eq!(state.recorded_session_ids(), vec!["69f1b5".to_string()]);
    }

    /// Removing a project takes its session index with it — but that is still
    /// only an index: no tmux session is touched by this call.
    #[test]
    fn removing_a_project_drops_its_session_records_only() {
        let mut state = State::default();
        state.upsert(record("a1b2c3", "acme-web"));
        state.upsert(record("ddeeff", "design-system"));
        state.record_session(session("bceeb7", "a1b2c3"));
        state.record_session(session("111111", "ddeeff"));

        state.remove("a1b2c3");
        assert_eq!(state.recorded_session_ids(), vec!["111111".to_string()]);
    }

    #[test]
    fn ignoring_a_session_is_idempotent_and_reversible() {
        let mut state = State::default();
        state.ignore_session("wt-999999");
        state.ignore_session("wt-999999");
        assert_eq!(state.ignored_sessions.len(), 1);
        assert!(state.is_ignored("wt-999999"));
        assert!(!state.is_ignored("wt-000000"));
        assert!(state.clear_ignored_sessions());
        assert!(!state.clear_ignored_sessions());
        assert!(!state.is_ignored("wt-999999"));
    }

    /// A file written by an older Grove has neither table; it must load as a
    /// state with no session index rather than failing.
    #[test]
    fn a_file_without_the_session_tables_still_loads() {
        let state = State::from_toml(
            "version = 1\n\n[[project]]\nid = \"a1b2c3\"\nname = \"acme\"\n",
            Path::new("state.toml"),
        )
        .expect("older file");
        assert!(state.sessions.is_empty());
        assert!(state.ignored_sessions.is_empty());
    }

    #[test]
    fn a_partial_session_record_is_tolerated() {
        let state = State::from_toml(
            "[[session]]\nworktree_id = \"bceeb7\"\n",
            Path::new("state.toml"),
        )
        .expect("partial record");
        let record = state.session("bceeb7").expect("present");
        assert_eq!(record.session_name, "");
        assert_eq!(record.last_activity_at, 0);
        assert_eq!(record.worktree_path, PathBuf::new());
    }
}
