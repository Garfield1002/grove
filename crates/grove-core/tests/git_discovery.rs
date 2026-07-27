//! End-to-end worktree discovery against real git repositories.

mod common;

use std::path::Path;

use common::{add_worktree, canonical, have, init_repo, must, skip};
use grove_core::git::{self, discover_project};
use grove_core::ids;
use grove_core::model::worktrees_from_entries;

macro_rules! require_git {
    () => {
        if !have("git") {
            skip("git");
            return;
        }
    };
}

#[test]
fn discovers_a_plain_repository() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);

    let discovery = discover_project(&repo).expect("discovers");
    assert_eq!(discovery.name, "acme-web");
    assert_eq!(discovery.repository_path, canonical(&repo));
    assert_eq!(discovery.git_common_dir, canonical(&repo.join(".git")));
    assert_eq!(discovery.worktrees.len(), 1);

    let main = &discovery.worktrees[0];
    assert_eq!(main.branch.as_deref(), Some("main"));
    assert!(!main.detached && !main.bare && !main.locked && !main.prunable);
    assert_eq!(
        main.head.as_deref(),
        Some(common::head_commit(&repo).as_str())
    );
}

#[test]
fn lists_linked_worktrees() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    add_worktree(&repo, &tmp.path().join("wt-auth"), "feature/auth");
    add_worktree(&repo, &tmp.path().join("wt-parser"), "fix/parser");

    let discovery = discover_project(&repo).expect("discovers");
    assert_eq!(discovery.worktrees.len(), 3);
    // git reports the main worktree first.
    assert_eq!(discovery.worktrees[0].path, canonical(&repo));

    let branches: Vec<_> = discovery
        .worktrees
        .iter()
        .filter_map(|w| w.branch.clone())
        .collect();
    assert!(branches.contains(&"feature/auth".to_string()));
    assert!(branches.contains(&"fix/parser".to_string()));
}

#[test]
fn registers_the_containing_project_from_a_subdirectory() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let nested = repo.join("src").join("deep");
    std::fs::create_dir_all(&nested).expect("mkdir");

    let discovery = discover_project(&nested).expect("discovers from a subdirectory");
    assert_eq!(discovery.repository_path, canonical(&repo));
    assert_eq!(discovery.name, "acme-web");
}

/// The path the user picked is inside a *linked* worktree: Grove must register
/// the containing project, not the worktree (DESIGN.md §9).
#[test]
fn registers_the_containing_project_from_inside_a_linked_worktree() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let linked = tmp.path().join("wt-auth");
    add_worktree(&repo, &linked, "feature/auth");
    let nested = linked.join("src");
    std::fs::create_dir_all(&nested).expect("mkdir");

    for start in [&linked, &nested] {
        let discovery = discover_project(start).expect("discovers");
        assert_eq!(
            discovery.repository_path,
            canonical(&repo),
            "picking {} must register the project, not the worktree",
            start.display()
        );
        assert_eq!(discovery.git_common_dir, canonical(&repo.join(".git")));
        assert_eq!(discovery.name, "acme-web");
        assert_eq!(discovery.worktrees.len(), 2);
    }
}

/// Discovery from anywhere in the repository yields the same project id and
/// the same worktree ids — the property restore depends on.
#[test]
fn ids_are_identical_wherever_discovery_starts() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let linked = tmp.path().join("wt-auth");
    add_worktree(&repo, &linked, "feature/auth");

    let from_repo = discover_project(&repo).expect("discovers");
    let from_linked = discover_project(&linked).expect("discovers");
    assert_eq!(from_repo.git_common_dir, from_linked.git_common_dir);

    let id = ids::project_id(&from_repo.git_common_dir);
    let a = worktrees_from_entries(&from_repo.worktrees, &id, &from_repo.git_common_dir);
    let b = worktrees_from_entries(&from_linked.worktrees, &id, &from_linked.git_common_dir);
    let mut a_ids: Vec<_> = a.iter().map(|w| w.id.clone()).collect();
    let mut b_ids: Vec<_> = b.iter().map(|w| w.id.clone()).collect();
    a_ids.sort();
    b_ids.sort();
    assert_eq!(a_ids, b_ids);
    assert_eq!(a_ids.len(), 2);
    assert_ne!(a_ids[0], a_ids[1]);
}

#[test]
fn detects_a_detached_head_worktree() {
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

    let discovery = discover_project(&repo).expect("discovers");
    let entry = discovery
        .worktrees
        .iter()
        .find(|w| w.path == canonical(&detached))
        .expect("detached worktree listed");
    assert!(entry.detached);
    assert_eq!(entry.branch, None);
    assert_eq!(entry.head.as_deref(), Some(head.as_str()));
    assert_eq!(entry.label(), format!("({})", &head[..7]));
}

#[test]
fn detects_a_locked_worktree() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let locked = tmp.path().join("wt-locked");
    add_worktree(&repo, &locked, "feature/locked");
    must(
        "git",
        &[
            "worktree",
            "lock",
            "--reason",
            "on a removable drive",
            &locked.to_string_lossy(),
        ],
        &repo,
    );

    let discovery = discover_project(&repo).expect("discovers");
    let entry = discovery
        .worktrees
        .iter()
        .find(|w| w.path == canonical(&locked))
        .expect("locked worktree listed");
    assert!(entry.locked);
    assert_eq!(entry.lock_reason.as_deref(), Some("on a removable drive"));
}

#[test]
fn detects_a_prunable_worktree_whose_directory_was_removed() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    let gone = tmp.path().join("wt-gone");
    add_worktree(&repo, &gone, "feature/gone");
    let gone_canonical = canonical(&gone);
    std::fs::remove_dir_all(&gone).expect("remove the worktree directory behind git's back");

    let discovery = discover_project(&repo).expect("discovers");
    let entry = discovery
        .worktrees
        .iter()
        .find(|w| w.path == gone_canonical)
        .expect("the removed worktree is still listed, not silently dropped");
    assert!(entry.prunable, "git should report it as prunable");
    assert!(entry.prune_reason.is_some());
    assert!(
        !entry.path.exists(),
        "grove must not recreate or delete anything here"
    );
}

#[test]
fn handles_a_bare_repository_with_worktrees() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("acme-web.git");
    let seed = tmp.path().join("seed");
    init_repo(&seed);
    must(
        "git",
        &[
            "clone",
            "-q",
            "--bare",
            &seed.to_string_lossy(),
            "acme-web.git",
        ],
        tmp.path(),
    );
    let linked = tmp.path().join("wt-main");
    must(
        "git",
        &[
            "worktree",
            "add",
            "-q",
            &linked.to_string_lossy(),
            "-b",
            "work",
        ],
        &bare,
    );

    let discovery = discover_project(&bare).expect("discovers a bare repository");
    assert_eq!(discovery.name, "acme-web", "the .git suffix is stripped");
    assert_eq!(discovery.repository_path, canonical(&bare));
    assert_eq!(discovery.worktrees.len(), 2);
    assert!(discovery.worktrees[0].bare);
    assert_eq!(discovery.worktrees[0].branch, None);
    assert_eq!(discovery.worktrees[1].branch.as_deref(), Some("work"));

    let id = ids::project_id(&discovery.git_common_dir);
    let worktrees = worktrees_from_entries(&discovery.worktrees, &id, &discovery.git_common_dir);
    assert!(worktrees[0].is_bare && worktrees[0].is_main);
    assert_eq!(worktrees[0].label(), "(bare)");
}

#[test]
fn handles_paths_containing_spaces() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("my projects").join("the repo");
    init_repo(&repo);
    let linked = tmp.path().join("my projects").join("a work tree");
    add_worktree(&repo, &linked, "feature/spaced-work");

    let discovery = discover_project(&repo).expect("discovers");
    assert_eq!(discovery.name, "the repo");
    assert_eq!(discovery.worktrees.len(), 2);
    let entry = discovery
        .worktrees
        .iter()
        .find(|w| w.path == canonical(&linked))
        .expect("the spaced worktree is listed as one path");
    assert_eq!(entry.branch.as_deref(), Some("feature/spaced-work"));
}

#[test]
fn a_directory_that_is_not_a_repository_reports_gits_own_error() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let plain = tmp.path().join("not-a-repo");
    std::fs::create_dir(&plain).expect("mkdir");

    let err = discover_project(&plain).expect_err("must not register a non-repository");
    let message = err.to_string().to_ascii_lowercase();
    assert!(
        message.contains("not a git repository"),
        "git's own message must survive: {message}"
    );
    assert!(
        err.diagnostics()
            .expect("diagnostics available")
            .contains("rev-parse"),
        "diagnostics must name the failing command"
    );
}

#[test]
fn a_nonexistent_path_is_an_error_not_a_panic() {
    require_git!();
    let err = discover_project(Path::new("/nonexistent-grove-path/really/not/here"))
        .expect_err("must fail");
    assert!(!err.to_string().is_empty());
}

#[test]
fn worktree_list_paths_are_canonical() {
    require_git!();
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("acme-web");
    init_repo(&repo);
    // A path with a `.` component must still hash to the same id.
    let via_dot = repo.join(".");
    let entries = git::worktree_list(&via_dot).expect("lists");
    assert_eq!(entries[0].path, canonical(&repo));
}
