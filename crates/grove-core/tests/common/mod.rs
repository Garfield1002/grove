//! Shared helpers for the integration tests.
//!
//! These tests drive the real `git` and `tmux` binaries against throwaway
//! directories and a throwaway socket. When a binary is absent the test prints
//! a skip message and returns — it never silently passes as if it had run.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// True when `program` is on `PATH`.
pub fn have(program: &str) -> bool {
    grove_core::process::is_on_path(program)
}

/// Print a skip notice. Returns true so call sites read
/// `if !have("git") { return skip("git"); }`.
pub fn skip(program: &str) {
    eprintln!("SKIP: `{program}` is not installed; this test did not run");
}

/// Run a command that must succeed, returning stdout.
pub fn must(program: &str, args: &[&str], cwd: &Path) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Grove Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Grove Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap_or_else(|e| panic!("could not run {program} {args:?}: {e}"));
    assert!(
        output.status.success(),
        "{program} {args:?} in {} failed ({}):\n{}",
        cwd.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Initialise a repository at `path` with one commit on `main`.
pub fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    must("git", &["init", "-q", "--initial-branch=main", "."], path);
    must("git", &["config", "user.name", "Grove Test"], path);
    must(
        "git",
        &["config", "user.email", "test@example.invalid"],
        path,
    );
    std::fs::write(path.join("README.md"), "grove test repo\n").expect("write README");
    must("git", &["add", "README.md"], path);
    must("git", &["commit", "-q", "-m", "initial commit"], path);
}

/// `git worktree add` with a new branch.
pub fn add_worktree(repo: &Path, worktree: &Path, branch: &str) {
    let worktree = worktree.to_string_lossy().into_owned();
    must(
        "git",
        &["worktree", "add", "-q", "-b", branch, &worktree],
        repo,
    );
}

/// The canonical HEAD commit of a repository.
pub fn head_commit(repo: &Path) -> String {
    must("git", &["rev-parse", "HEAD"], repo).trim().to_string()
}

/// Canonicalised path, as grove-core stores them.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
}
