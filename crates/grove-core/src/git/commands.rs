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

    #[test]
    fn canonical_falls_back_for_missing_paths() {
        let missing = Path::new("/nonexistent-grove-path/xyz");
        assert_eq!(canonical(missing), missing.to_path_buf());
    }
}
