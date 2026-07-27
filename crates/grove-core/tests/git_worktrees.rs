//! Creating, removing and deleting against real git repositories.
//!
//! The point of these tests is not that git works, but that Grove's argument
//! arrays reach it intact and that git's own refusals reach the user intact.

mod common;

use std::path::Path;

use common::{add_worktree, canonical, have, init_repo, must, skip};
use grove_core::git::{self, WorktreeAdd};

macro_rules! require_git {
    () => {
        if !have("git") {
            skip("git");
            return;
        }
    };
}

fn branches(repo: &Path) -> Vec<String> {
    must("git", &["branch", "--format=%(refname:short)"], repo)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn creates_a_worktree_with_a_new_branch() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let path = tmp.path().join("wt-auth");

    let created = git::worktree_add(
        &repo,
        &WorktreeAdd {
            path: path.clone(),
            new_branch: Some("feature/auth".into()),
            base_ref: Some("main".into()),
        },
    )
    .expect("creates the worktree");

    assert_eq!(created, canonical(&path));
    assert!(
        path.join("README.md").is_file(),
        "the base ref was checked out"
    );
    assert!(branches(&repo).contains(&"feature/auth".to_string()));

    let entries = git::worktree_list(&repo).expect("lists");
    let entry = entries
        .iter()
        .find(|e| e.path == canonical(&path))
        .expect("the new worktree is listed");
    assert_eq!(entry.branch.as_deref(), Some("feature/auth"));
}

#[test]
fn creates_a_worktree_from_an_existing_branch() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    must("git", &["branch", "release-1.4"], &repo);
    let path = tmp.path().join("wt-release");

    git::worktree_add(
        &repo,
        &WorktreeAdd {
            path: path.clone(),
            new_branch: None,
            base_ref: Some("release-1.4".into()),
        },
    )
    .expect("checks out the existing branch");

    let entries = git::worktree_list(&repo).expect("lists");
    let entry = entries
        .iter()
        .find(|e| e.path == canonical(&path))
        .expect("listed");
    assert_eq!(entry.branch.as_deref(), Some("release-1.4"));
    assert_eq!(
        branches(&repo).len(),
        2,
        "no branch should have been created"
    );
}

#[test]
fn creates_a_worktree_at_a_path_containing_spaces() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("my projects").join("the repo");
    init_repo(&repo);
    let path = tmp.path().join("my projects").join("a work tree");

    git::worktree_add(
        &repo,
        &WorktreeAdd {
            path: path.clone(),
            new_branch: Some("feature/spaced".into()),
            base_ref: None,
        },
    )
    .expect("a path with spaces is one argument");

    assert!(path.is_dir());
    let entries = git::worktree_list(&repo).expect("lists");
    assert!(entries.iter().any(|e| e.path == canonical(&path)));
}

/// DESIGN.md §14's worked example: git's refusal must survive verbatim.
#[test]
fn a_branch_already_checked_out_elsewhere_reports_gits_own_stderr() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let first = tmp.path().join("wt-auth");
    add_worktree(&repo, &first, "feature/auth");

    let err = git::worktree_add(
        &repo,
        &WorktreeAdd {
            path: tmp.path().join("wt-auth-again"),
            new_branch: None,
            base_ref: Some("feature/auth".into()),
        },
    )
    .expect_err("git refuses a second checkout of one branch");

    let message = err.to_string();
    assert!(
        message.contains("already checked out") || message.contains("already used by worktree"),
        "git's own message must survive: {message}"
    );
    let diagnostics = err.diagnostics().expect("diagnostics are retained");
    assert!(diagnostics.contains("worktree add"));
    assert!(diagnostics.contains("--- stderr ---"));
    assert!(
        !tmp.path().join("wt-auth-again").exists(),
        "a refused creation must leave nothing behind"
    );
}

#[test]
fn creating_over_an_existing_directory_surfaces_the_error() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let occupied = tmp.path().join("occupied");
    std::fs::create_dir(&occupied).expect("mkdir");
    std::fs::write(occupied.join("file.txt"), "mine").expect("write");

    let err = git::worktree_add(
        &repo,
        &WorktreeAdd {
            path: occupied.clone(),
            new_branch: Some("x".into()),
            base_ref: None,
        },
    )
    .expect_err("git refuses a non-empty directory");
    assert!(!err.to_string().is_empty());
    assert_eq!(
        std::fs::read_to_string(occupied.join("file.txt")).expect("read"),
        "mine",
        "the user's files must be untouched"
    );
}

#[test]
fn removes_a_clean_worktree() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let path = tmp.path().join("wt-auth");
    add_worktree(&repo, &path, "feature/auth");

    git::worktree_remove(&repo, &path, false).expect("removes a clean worktree");
    assert!(!path.exists());
    assert_eq!(git::worktree_list(&repo).expect("lists").len(), 1);
    assert!(
        branches(&repo).contains(&"feature/auth".to_string()),
        "removing a worktree must not delete its branch"
    );
}

/// The core of the safe-removal flow: git refuses, Grove shows the refusal,
/// and only a second explicit confirmation reaches `--force`.
#[test]
fn a_dirty_worktree_is_refused_until_forced() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let path = tmp.path().join("wt-auth");
    add_worktree(&repo, &path, "feature/auth");
    std::fs::write(path.join("README.md"), "local edit\n").expect("dirty the worktree");

    let err = git::worktree_remove(&repo, &path, false).expect_err("git refuses");
    assert!(
        err.to_string().contains("modified or untracked files"),
        "git's refusal must reach the user: {err}"
    );
    assert!(path.exists(), "nothing may be removed by a refused attempt");

    git::worktree_remove(&repo, &path, true).expect("the forced path removes it");
    assert!(!path.exists());
}

#[test]
fn deleting_an_unmerged_branch_is_refused_until_forced() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let path = tmp.path().join("wt-auth");
    add_worktree(&repo, &path, "feature/auth");
    std::fs::write(path.join("work.txt"), "unmerged work\n").expect("write");
    must("git", &["add", "work.txt"], &path);
    must("git", &["commit", "-q", "-m", "unmerged work"], &path);
    git::worktree_remove(&repo, &path, false).expect("the worktree itself is clean");

    let err = git::branch_delete(&repo, "feature/auth", false).expect_err("git refuses -d");
    assert!(
        err.to_string().contains("not fully merged"),
        "git's refusal must reach the user: {err}"
    );
    assert!(branches(&repo).contains(&"feature/auth".to_string()));

    git::branch_delete(&repo, "feature/auth", true).expect("-D deletes it");
    assert!(!branches(&repo).contains(&"feature/auth".to_string()));
}

#[test]
fn deleting_a_merged_branch_needs_no_force() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    must("git", &["branch", "merged-already"], &repo);

    git::branch_delete(&repo, "merged-already", false).expect("-d is enough");
    assert!(!branches(&repo).contains(&"merged-already".to_string()));
}

#[test]
fn deleting_a_branch_that_is_checked_out_is_refused() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let path = tmp.path().join("wt-auth");
    add_worktree(&repo, &path, "feature/auth");

    // Even with -D: the worktree must be removed first, and Grove offers the
    // two operations separately for exactly this reason.
    let err = git::branch_delete(&repo, "feature/auth", true).expect_err("git refuses");
    assert!(err.to_string().contains("used by worktree"), "{err}");
    assert!(path.exists());
}

#[test]
fn lists_local_and_remote_refs_for_the_base_chooser() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let origin = tmp.path().join("origin");
    init_repo(&origin);
    must("git", &["branch", "release-1.4"], &origin);
    let clone = tmp.path().join("clone");
    must(
        "git",
        &[
            "clone",
            "-q",
            &format!("file://{}", origin.to_string_lossy()),
            "clone",
        ],
        tmp.path(),
    );

    let refs = git::list_refs(&clone).expect("lists refs");
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"main"),
        "local branches are offered: {names:?}"
    );
    assert!(
        names.contains(&"origin/release-1.4"),
        "remote-tracking branches are offered: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.ends_with("/HEAD")),
        "origin/HEAD is not a branch: {names:?}"
    );
    assert!(refs.iter().any(|r| r.name == "main" && !r.is_remote));
    assert!(refs.iter().any(|r| r.is_remote));
}

#[test]
fn reports_the_current_branch_and_tolerates_a_detached_head() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    assert_eq!(
        git::current_branch(&repo).expect("reads HEAD").as_deref(),
        Some("main")
    );

    let head = common::head_commit(&repo);
    let detached = tmp.path().join("wt-review");
    must(
        "git",
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &detached.to_string_lossy(),
            &head,
        ],
        &repo,
    );
    assert_eq!(
        git::current_branch(&detached).expect("a detached HEAD is not an error"),
        None
    );
}
