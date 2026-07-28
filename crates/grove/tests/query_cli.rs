//! End-to-end tests of Grove's read-only JSON commands.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use grove_core::ids;
use grove_core::state::{self, ProjectRecord, State};

const GROVE: &str = env!("CARGO_BIN_EXE_grove");

struct Isolated {
    root: tempfile::TempDir,
}

impl Isolated {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(GROVE)
            .args(args)
            .env("XDG_RUNTIME_DIR", self.root.path().join("run"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .output()
            .expect("runs grove")
    }

    fn state_path(&self) -> PathBuf {
        self.root.path().join("state/grove/state.toml")
    }

    fn save(&self, value: &State) {
        state::save(&self.state_path(), value).expect("saves isolated state");
    }
}

fn have(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok()
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("runs git");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo).expect("mkdir");
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.email", "grove@example.invalid"]);
    git(repo, &["config", "user.name", "Grove Test"]);
    std::fs::write(repo.join("README.md"), "test\n").expect("fixture");
    git(repo, &["add", "README.md"]);
    git(repo, &["commit", "-m", "initial"]);
}

#[test]
fn project_list_prints_versioned_json() {
    let isolated = Isolated::new();
    isolated.save(&State {
        projects: vec![ProjectRecord {
            id: "abc123".into(),
            name: "grove".into(),
            repository_path: "/src/grove".into(),
            git_common_dir: "/src/grove/.git".into(),
            default_worktree_path: "/src".into(),
            is_expanded: false,
        }],
        ..State::default()
    });

    let output = isolated.run(&["project", "list"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["version"], 1);
    assert_eq!(json["projects"][0]["id"], "abc123");
    assert_eq!(json["projects"][0]["name"], "grove");
    assert!(json["projects"][0].get("is_expanded").is_none());
}

#[test]
fn worktree_list_reads_git_and_uses_deterministic_ids() {
    if !have("git") || !have("tmux") {
        eprintln!("skipping: git or tmux is not installed");
        return;
    }
    let isolated = Isolated::new();
    let repo = isolated.root.path().join("grove");
    init_repo(&repo);
    let canonical_repo = repo.canonicalize().expect("canonical repo");
    let git_common_dir = canonical_repo.join(".git");
    let project_id = ids::project_id(&git_common_dir);
    isolated.save(&State {
        projects: vec![ProjectRecord {
            id: project_id.clone(),
            name: "grove".into(),
            repository_path: canonical_repo.clone(),
            git_common_dir: git_common_dir.clone(),
            default_worktree_path: isolated.root.path().into(),
            is_expanded: true,
        }],
        ..State::default()
    });

    let output = isolated.run(&["worktree", "list"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["version"], 1);
    assert_eq!(json["unavailable_projects"], serde_json::json!([]));
    assert_eq!(json["worktrees"][0]["project_id"], project_id);
    assert_eq!(
        json["worktrees"][0]["id"],
        ids::worktree_id(&git_common_dir, &canonical_repo)
    );
    assert_eq!(json["worktrees"][0]["branch"], "main");
    assert_eq!(json["worktrees"][0]["session_state"], "none");
}

#[test]
fn session_list_without_a_server_is_an_empty_success() {
    if !have("tmux") {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let isolated = Isolated::new();
    let output = isolated.run(&["session", "list"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json, serde_json::json!({"version": 1, "sessions": []}));
}

#[test]
fn malformed_query_is_rejected() {
    let isolated = Isolated::new();
    let output = isolated.run(&["project", "remove"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected `project list`"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
