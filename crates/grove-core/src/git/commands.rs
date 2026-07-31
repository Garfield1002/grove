//! git invocations. Every call is an argument array; nothing is ever passed
//! through a shell.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::git::parser::{WorktreeEntry, parse_worktree_list};
use crate::process::Invocation;

/// The git executable. Not configurable in Milestone 1.
pub const GIT: &str = "git";

/// Build `git -C <dir> <args…>` without running it.
pub fn git_in(dir: &Path, args: &[&str]) -> Invocation {
    Invocation::new(GIT)
        .arg("-C")
        .arg(dir.as_os_str())
        .args(args.iter().copied())
}

/// Resolve a path to its canonical form, falling back to the input when the
/// path does not exist (a missing worktree must still get a stable id).
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn rev_parse_one(dir: &Path, flag: &str) -> Result<PathBuf> {
    let out = git_in(dir, &["rev-parse", "--path-format=absolute", flag]).output()?;
    let line = out.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err(Error::NotARepository(dir.to_path_buf()));
    }
    Ok(PathBuf::from(line))
}

/// `git -C <dir> rev-parse --show-toplevel`. Fails for bare repositories,
/// which is why discovery does not depend on it alone.
pub fn show_toplevel(dir: &Path) -> Result<PathBuf> {
    rev_parse_one(dir, "--show-toplevel").map(|p| canonical(&p))
}

/// `git -C <dir> rev-parse --git-common-dir` — the repository identity shared
/// by every worktree, and half of the worktree-id hash input.
pub fn git_common_dir(dir: &Path) -> Result<PathBuf> {
    rev_parse_one(dir, "--git-common-dir").map(|p| canonical(&p))
}

/// `git -C <dir> worktree list --porcelain`.
pub fn worktree_list(dir: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git_in(dir, &["worktree", "list", "--porcelain"]).output()?;
    let mut entries = parse_worktree_list(&out)?;
    for entry in &mut entries {
        entry.path = canonical(&entry.path);
    }
    Ok(entries)
}

/// A worktree Grove is about to create (DESIGN.md §10).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeAdd {
    /// Directory to create. Never interpolated into a shell string.
    pub path: PathBuf,
    /// Branch to create with `-b`. `None` checks out `base_ref` as it is.
    pub new_branch: Option<String>,
    /// Base branch or commit. `None` lets git default to the current HEAD.
    pub base_ref: Option<String>,
}

/// Build `git -C <repo> worktree add [-b <new>] <path> [<ref>]`.
///
/// The path and the refs are separate arguments, so a branch called
/// `--force` or a path containing spaces cannot change what git does.
pub fn worktree_add_args(repository_path: &Path, add: &WorktreeAdd) -> Invocation {
    let mut invocation = Invocation::new(GIT)
        .arg("-C")
        .arg(repository_path.as_os_str())
        .args(["worktree", "add"]);
    if let Some(branch) = &add.new_branch {
        invocation = invocation.arg("-b").arg(branch.as_str());
    }
    invocation = invocation.arg(add.path.as_os_str());
    if let Some(base) = &add.base_ref {
        invocation = invocation.arg(base.as_str());
    }
    invocation
}

/// Create a worktree. git's own stderr is preserved on failure, which is what
/// the UI shows (DESIGN.md §10, §14).
///
/// Runs a subprocess: worker thread only.
pub fn worktree_add(repository_path: &Path, add: &WorktreeAdd) -> Result<PathBuf> {
    worktree_add_args(repository_path, add).output()?;
    Ok(canonical(&add.path))
}

/// Build `git -C <repo> worktree remove [--force] <path>`.
///
/// `--force` is only ever passed after the user confirmed it a second time,
/// having seen git's own refusal (ARCHITECTURE.md §8.3).
pub fn worktree_remove_args(repository_path: &Path, worktree: &Path, force: bool) -> Invocation {
    let mut invocation = Invocation::new(GIT)
        .arg("-C")
        .arg(repository_path.as_os_str())
        .args(["worktree", "remove"]);
    if force {
        invocation = invocation.arg("--force");
    }
    invocation.arg(worktree.as_os_str())
}

/// Remove a worktree directory and its administrative files.
///
/// Runs a subprocess: worker thread only.
pub fn worktree_remove(repository_path: &Path, worktree: &Path, force: bool) -> Result<()> {
    worktree_remove_args(repository_path, worktree, force).output()?;
    Ok(())
}

/// Build `git -C <repo> branch -d|-D <branch>`.
///
/// `-D` discards unmerged commits, so it is only reached through a second
/// explicit confirmation.
pub fn branch_delete_args(repository_path: &Path, branch: &str, force: bool) -> Invocation {
    Invocation::new(GIT)
        .arg("-C")
        .arg(repository_path.as_os_str())
        .arg("branch")
        .arg(if force { "-D" } else { "-d" })
        .arg("--")
        .arg(branch)
}

/// Delete a branch. Separate from worktree removal, always separately
/// confirmed (ARCHITECTURE.md §8.2).
///
/// Runs a subprocess: worker thread only.
pub fn branch_delete(repository_path: &Path, branch: &str, force: bool) -> Result<()> {
    branch_delete_args(repository_path, branch, force).output()?;
    Ok(())
}

/// A ref offered as the base of a new worktree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefEntry {
    /// Short name: `main`, or `origin/main` for a remote-tracking branch.
    pub name: String,
    pub is_remote: bool,
}

/// Parse `git for-each-ref --format=%(refname) refs/heads refs/remotes`.
///
/// `origin/HEAD` is a symbolic alias rather than a branch, and checking it out
/// is never what the user meant, so it is dropped.
pub fn parse_refs(output: &str) -> Vec<RefEntry> {
    let mut refs = Vec::new();
    for raw in output.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix("refs/heads/") {
            refs.push(RefEntry {
                name: name.to_string(),
                is_remote: false,
            });
        } else if let Some(name) = line.strip_prefix("refs/remotes/") {
            if name.ends_with("/HEAD") {
                continue;
            }
            refs.push(RefEntry {
                name: name.to_string(),
                is_remote: true,
            });
        }
    }
    refs
}

/// Local branches and remote-tracking branches, for the base-ref chooser.
///
/// Runs a subprocess: worker thread only.
pub fn list_refs(repository_path: &Path) -> Result<Vec<RefEntry>> {
    let out = git_in(
        repository_path,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .output()?;
    Ok(parse_refs(&out))
}

/// The branch currently checked out at `dir`, if any. A detached HEAD or a
/// bare repository simply has none — not an error.
///
/// Runs a subprocess: worker thread only.
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    let out =
        git_in(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).output_allow_failure()?;
    if !out.success {
        return Ok(None);
    }
    let name = out.stdout.trim();
    Ok((!name.is_empty()).then(|| name.to_string()))
}

/// Everything Grove needs to register a project from an arbitrary path
/// somewhere inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDiscovery {
    /// Canonical `--git-common-dir`: the repository's identity.
    pub git_common_dir: PathBuf,
    /// The main worktree (or the bare repository directory). This is what
    /// Grove registers, even when the user picked a linked worktree.
    pub repository_path: PathBuf,
    pub name: String,
    pub worktrees: Vec<WorktreeEntry>,
}

/// Derive the display name of a project from its main worktree path.
pub fn project_name(repository_path: &Path) -> String {
    let name = repository_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repository_path.to_string_lossy().into_owned());
    // A bare repository is conventionally `<name>.git`.
    match name.strip_suffix(".git") {
        Some(stripped) if !stripped.is_empty() => stripped.to_string(),
        _ => name,
    }
}

/// Register the project containing `path`.
///
/// `path` may be the main worktree, a linked worktree, a subdirectory of
/// either, or a bare repository; in every case the project registered is the
/// containing repository, identified by its git-common-dir. The first record
/// of `git worktree list --porcelain` is always the main worktree.
///
/// Runs subprocesses: call from a worker thread, never from the UI thread.
pub fn discover_project(path: &Path) -> Result<ProjectDiscovery> {
    let git_common_dir = git_common_dir(path)?;
    let worktrees = worktree_list(path)?;
    let main = worktrees
        .first()
        .ok_or_else(|| Error::NoWorktrees(path.to_path_buf()))?;
    let repository_path = main.path.clone();
    Ok(ProjectDiscovery {
        name: project_name(&repository_path),
        git_common_dir,
        repository_path,
        worktrees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn git_in_uses_an_argument_array() {
        let inv = git_in(Path::new("/home/u/my repo"), &["worktree", "list"]);
        assert_eq!(inv.program, OsString::from("git"));
        assert_eq!(
            inv.args,
            vec![
                OsString::from("-C"),
                OsString::from("/home/u/my repo"),
                OsString::from("worktree"),
                OsString::from("list"),
            ]
        );
    }

    #[test]
    fn project_name_uses_the_directory_name() {
        assert_eq!(project_name(Path::new("/home/u/acme-web")), "acme-web");
    }

    #[test]
    fn project_name_strips_the_bare_suffix() {
        assert_eq!(project_name(Path::new("/home/u/acme-web.git")), "acme-web");
        assert_eq!(project_name(Path::new("/home/u/.git")), ".git");
    }

    fn lossy(inv: &Invocation) -> Vec<String> {
        inv.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn worktree_add_creates_a_new_branch_from_a_base_ref() {
        let add = WorktreeAdd {
            path: PathBuf::from("/home/u/wt/auth"),
            new_branch: Some("feature/auth".into()),
            base_ref: Some("origin/main".into()),
        };
        let inv = worktree_add_args(Path::new("/home/u/proj"), &add);
        assert_eq!(inv.program, OsString::from("git"));
        assert_eq!(
            lossy(&inv),
            vec![
                "-C",
                "/home/u/proj",
                "worktree",
                "add",
                "-b",
                "feature/auth",
                "/home/u/wt/auth",
                "origin/main",
            ]
        );
    }

    #[test]
    fn worktree_add_checks_out_an_existing_branch_without_dash_b() {
        let add = WorktreeAdd {
            path: PathBuf::from("/home/u/wt/auth"),
            new_branch: None,
            base_ref: Some("feature/auth".into()),
        };
        let inv = worktree_add_args(Path::new("/home/u/proj"), &add);
        assert_eq!(
            lossy(&inv),
            vec![
                "-C",
                "/home/u/proj",
                "worktree",
                "add",
                "/home/u/wt/auth",
                "feature/auth",
            ]
        );
    }

    #[test]
    fn worktree_add_without_a_base_ref_lets_git_default_to_head() {
        let add = WorktreeAdd {
            path: PathBuf::from("/home/u/wt/x"),
            new_branch: Some("x".into()),
            base_ref: None,
        };
        assert_eq!(
            lossy(&worktree_add_args(Path::new("/home/u/proj"), &add)),
            vec![
                "-C",
                "/home/u/proj",
                "worktree",
                "add",
                "-b",
                "x",
                "/home/u/wt/x"
            ]
        );
    }

    #[test]
    fn worktree_add_keeps_spaces_and_dashes_inside_single_arguments() {
        let add = WorktreeAdd {
            path: PathBuf::from("/home/u/my worktrees/a work tree"),
            new_branch: Some("--force".into()),
            base_ref: Some("origin/my branch".into()),
        };
        let args = lossy(&worktree_add_args(Path::new("/home/u/my proj"), &add));
        assert!(args.contains(&"/home/u/my worktrees/a work tree".to_string()));
        // A branch that looks like a flag is still just the -b value.
        assert_eq!(args[args.len() - 3], "--force");
        assert_eq!(args[args.len() - 1], "origin/my branch");
    }

    #[test]
    fn worktree_remove_passes_force_only_when_asked() {
        let repo = Path::new("/home/u/proj");
        let path = Path::new("/home/u/wt/auth");
        assert_eq!(
            lossy(&worktree_remove_args(repo, path, false)),
            vec![
                "-C",
                "/home/u/proj",
                "worktree",
                "remove",
                "/home/u/wt/auth"
            ]
        );
        assert_eq!(
            lossy(&worktree_remove_args(repo, path, true)),
            vec![
                "-C",
                "/home/u/proj",
                "worktree",
                "remove",
                "--force",
                "/home/u/wt/auth",
            ]
        );
    }

    #[test]
    fn branch_delete_uses_lowercase_d_until_forced() {
        let repo = Path::new("/home/u/proj");
        assert_eq!(
            lossy(&branch_delete_args(repo, "feature/auth", false)),
            vec!["-C", "/home/u/proj", "branch", "-d", "--", "feature/auth"]
        );
        assert_eq!(
            lossy(&branch_delete_args(repo, "feature/auth", true)),
            vec!["-C", "/home/u/proj", "branch", "-D", "--", "feature/auth"]
        );
    }

    #[test]
    fn branch_delete_separates_flags_from_the_branch_name() {
        // `--` means a branch named like an option is still a branch name.
        let args = lossy(&branch_delete_args(Path::new("/p"), "-D", false));
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], "-D");
    }

    #[test]
    fn parses_local_and_remote_refs() {
        let text = "\
refs/heads/main
refs/heads/feature/auth
refs/remotes/origin/HEAD
refs/remotes/origin/main
refs/tags/v1.0
";
        let refs = parse_refs(text);
        assert_eq!(
            refs,
            vec![
                RefEntry {
                    name: "main".into(),
                    is_remote: false
                },
                RefEntry {
                    name: "feature/auth".into(),
                    is_remote: false
                },
                RefEntry {
                    name: "origin/main".into(),
                    is_remote: true
                },
            ],
            "origin/HEAD is an alias, and tags are not offered"
        );
    }

    #[test]
    fn ref_parsing_tolerates_empty_output_and_crlf() {
        assert!(parse_refs("").is_empty());
        assert!(parse_refs("\n\n").is_empty());
        let refs = parse_refs("refs/heads/main\r\n");
        assert_eq!(refs[0].name, "main");
    }

    #[test]
    fn canonical_falls_back_for_missing_paths() {
        let missing = Path::new("/nonexistent-grove-path/xyz");
        assert_eq!(canonical(missing), missing.to_path_buf());
    }
}
