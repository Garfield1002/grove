//! Status summaries against real repositories, including a real conflicted
//! merge and a real upstream (a `file://` clone, so no network is involved).

mod common;

use std::path::Path;

use common::{add_worktree, have, init_repo, must, skip};
use grove_core::git::status::{self, Operation};
use grove_core::removal::{self, Unpushed};

macro_rules! require_git {
    () => {
        if !have("git") {
            skip("git");
            return;
        }
    };
}

/// A repository cloned over `file://`, so `main` has a real upstream.
fn clone_with_upstream(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let origin = tmp.join("origin");
    init_repo(&origin);
    must(
        "git",
        &[
            "clone",
            "-q",
            &format!("file://{}", origin.to_string_lossy()),
            "clone",
        ],
        tmp,
    );
    (origin, tmp.join("clone"))
}

fn commit(repo: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(repo.join(file), contents).expect("write");
    must("git", &["add", file], repo);
    must("git", &["commit", "-q", "-m", message], repo);
}

#[test]
fn a_fresh_clone_is_clean_and_level_with_its_upstream() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_origin, clone) = clone_with_upstream(tmp.path());

    let status = status::status_summary(&clone).expect("reads status");
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(!status.detached);
    assert_eq!(status.upstream.as_deref(), Some("origin/main"));
    assert_eq!((status.ahead, status.behind), (Some(0), Some(0)));
    assert!(status.is_clean());
    assert_eq!(status.summary(), "clean");
    assert_eq!(status.operation, None);
}

#[test]
fn counts_staged_modified_and_untracked_files() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);

    std::fs::write(repo.join("staged.txt"), "staged\n").expect("write");
    must("git", &["add", "staged.txt"], &repo);
    std::fs::write(repo.join("README.md"), "modified\n").expect("write");
    std::fs::write(repo.join("untracked.txt"), "new\n").expect("write");
    std::fs::write(repo.join("a file with spaces.txt"), "new\n").expect("write");

    let status = status::status_summary(&repo).expect("reads status");
    assert_eq!(status.staged, 1);
    assert_eq!(status.modified, 1);
    assert_eq!(status.untracked, 2);
    assert_eq!(status.conflicted, 0);
    assert!(!status.is_clean());
    assert!(status.has_uncommitted_changes());
    assert_eq!(status.summary(), "1 staged · 1 mod · 2 untracked");
}

#[test]
fn counts_a_real_rename() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    must("git", &["mv", "README.md", "READ ME.md"], &repo);

    let status = status::status_summary(&repo).expect("reads status");
    assert_eq!(status.staged, 1, "a rename is one staged entry");
    assert_eq!(status.modified, 0);
}

#[test]
fn reports_ahead_and_behind_against_a_real_upstream() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (origin, clone) = clone_with_upstream(tmp.path());

    commit(&clone, "local.txt", "local\n", "local work");
    commit(&clone, "local2.txt", "local\n", "more local work");
    commit(&origin, "remote.txt", "remote\n", "remote work");
    must("git", &["fetch", "-q"], &clone);

    let status = status::status_summary(&clone).expect("reads status");
    assert_eq!(status.ahead, Some(2));
    assert_eq!(status.behind, Some(1));
    assert_eq!(status.summary(), "ahead 2 · behind 1");

    let unpushed = status::unpushed_count(&clone, status.upstream.as_deref().expect("upstream"))
        .expect("counts");
    assert_eq!(unpushed, 2, "two commits exist only here");
}

#[test]
fn a_branch_without_an_upstream_reports_unknown_rather_than_zero() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let path = tmp.path().join("wt-auth");
    add_worktree(&repo, &path, "feature/auth");
    commit(&path, "work.txt", "work\n", "unpushed work");

    let status = status::status_summary(&path).expect("reads status");
    assert_eq!(status.upstream, None);
    assert!(!status.has_upstream());
    assert_eq!(status.ahead, None);
    assert_eq!(status.behind, None);

    // This is what the removal dialog must show: unknown, never "none".
    let described = Unpushed::NoUpstream.describe();
    assert_eq!(described, "unknown — no upstream");
}

#[test]
fn detects_a_real_conflicted_merge() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);

    must("git", &["switch", "-q", "-c", "other"], &repo);
    commit(&repo, "README.md", "their side\n", "their change");
    must("git", &["switch", "-q", "main"], &repo);
    commit(&repo, "README.md", "our side\n", "our change");

    let merge = std::process::Command::new("git")
        .args(["merge", "other"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git merge");
    assert!(!merge.status.success(), "the merge must conflict");

    let status = status::status_summary(&repo).expect("reads status");
    assert_eq!(status.conflicted, 1);
    assert_eq!(status.operation, Some(Operation::Merge));
    assert!(status.summary().starts_with("MERGING"));
    assert!(status.has_uncommitted_changes());
}

#[test]
fn detects_a_rebase_in_progress() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);

    must("git", &["switch", "-q", "-c", "topic"], &repo);
    commit(&repo, "README.md", "topic side\n", "topic change");
    must("git", &["switch", "-q", "main"], &repo);
    commit(&repo, "README.md", "main side\n", "main change");
    must("git", &["switch", "-q", "topic"], &repo);

    let rebase = std::process::Command::new("git")
        .args(["rebase", "main"])
        .current_dir(&repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git rebase");
    assert!(
        !rebase.status.success(),
        "the rebase must stop on a conflict"
    );

    let status = status::status_summary(&repo).expect("reads status");
    assert_eq!(status.operation, Some(Operation::Rebase));
    assert!(status.summary().starts_with("REBASING"));
}

/// Linked worktrees keep their state files under `.git/worktrees/<name>/`, so
/// a merge in one worktree must not be reported in another.
#[test]
fn an_operation_in_one_worktree_is_not_reported_in_another() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    commit(&repo, "shared.txt", "base\n", "base");
    let linked = tmp.path().join("wt-topic");
    add_worktree(&repo, &linked, "topic");

    commit(&linked, "shared.txt", "their side\n", "their change");
    commit(&repo, "shared.txt", "our side\n", "our change");
    let merge = std::process::Command::new("git")
        .args(["merge", "topic"])
        .current_dir(&linked)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("run git merge");
    let _ = merge;

    let main_status = status::status_summary(&repo).expect("reads status");
    assert_eq!(
        main_status.operation, None,
        "the main worktree is not merging"
    );
}

#[test]
fn a_detached_worktree_reports_no_branch() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
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

    let status = status::status_summary(&detached).expect("reads status");
    assert!(status.detached);
    assert_eq!(status.branch, None);
    assert_eq!(status.upstream, None);
    assert!(status.is_clean());
}

#[test]
fn a_status_read_of_a_missing_directory_is_an_error_not_a_panic() {
    require_git!();
    let err = status::status_summary(Path::new("/nonexistent-grove-path/wt")).expect_err("fails");
    assert!(!err.to_string().is_empty());
}

/// The risk report is assembled from real readings, end to end.
#[test]
fn the_risk_report_reflects_a_real_dirty_unpushed_worktree() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_origin, clone) = clone_with_upstream(tmp.path());
    commit(&clone, "work.txt", "work\n", "unpushed work");
    std::fs::write(clone.join("scratch.txt"), "untracked\n").expect("write");

    let status = status::status_summary(&clone).expect("reads status");
    let upstream = status.upstream.clone().expect("upstream");
    let unpushed = Unpushed::Count(status::unpushed_count(&clone, &upstream).expect("counts"));

    let report = removal::assemble(&removal::RemovalInputs {
        branch: status.branch.clone(),
        status: Some(status),
        unpushed,
        ..removal::RemovalInputs::new(&clone)
    });

    let text: String = report
        .findings
        .iter()
        .map(|f| f.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("1 untracked file(s)"), "{text}");
    assert!(text.contains("1 commit not on the upstream"), "{text}");
    assert!(report.loses_work);
    assert!(!report.has_blockers());
}
