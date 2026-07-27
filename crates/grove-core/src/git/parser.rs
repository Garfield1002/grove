//! Parser for `git worktree list --porcelain`.
//!
//! The format is a sequence of records separated by blank lines. Each record
//! starts with a `worktree <path>` line; the remaining lines are attributes,
//! either bare (`bare`, `detached`) or `key value` (`HEAD <sha>`,
//! `branch <ref>`, `locked <reason>`, `prunable <reason>`). Unknown attributes
//! are ignored so a future git release does not break discovery.

use std::path::PathBuf;

use crate::error::ParseError;

const SOURCE: &str = "git worktree list --porcelain";

/// One worktree exactly as git reported it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// Commit at HEAD. Absent for bare repositories and for worktrees whose
    /// HEAD is unborn.
    pub head: Option<String>,
    /// Short branch name (`refs/heads/x` reported as `x`). `None` when
    /// detached or bare.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prune_reason: Option<String>,
}

impl WorktreeEntry {
    /// Display label for a row: the branch, or an abbreviated detached HEAD.
    pub fn label(&self) -> String {
        if let Some(branch) = &self.branch {
            return branch.clone();
        }
        if self.bare {
            return "(bare)".to_string();
        }
        match &self.head {
            Some(head) => format!("({})", &head[..head.len().min(7)]),
            None => "(no HEAD)".to_string(),
        }
    }
}

/// Parse the full porcelain output.
pub fn parse_worktree_list(output: &str) -> Result<Vec<WorktreeEntry>, ParseError> {
    let mut entries: Vec<WorktreeEntry> = Vec::new();
    let mut current: Option<WorktreeEntry> = None;

    for (index, raw) in output.lines().enumerate() {
        let line_no = index + 1;
        // git writes plain LF, but tolerate CRLF-mangled captures.
        let line = raw.strip_suffix('\r').unwrap_or(raw);

        if line.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }

        let (key, value) = match line.split_once(' ') {
            Some((key, value)) => (key, Some(value)),
            None => (line, None),
        };

        if key == "worktree" {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let path = value
                .filter(|v| !v.is_empty())
                .ok_or_else(|| ParseError::new(SOURCE, line_no, "`worktree` line has no path"))?;
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                ..WorktreeEntry::default()
            });
            continue;
        }

        let Some(entry) = current.as_mut() else {
            return Err(ParseError::new(
                SOURCE,
                line_no,
                format!("`{key}` appeared before any `worktree` line"),
            ));
        };

        match key {
            "HEAD" => {
                entry.head = value.filter(|v| !v.is_empty()).map(str::to_string);
                if entry.head.is_none() {
                    return Err(ParseError::new(
                        SOURCE,
                        line_no,
                        "`HEAD` line has no commit",
                    ));
                }
            }
            "branch" => {
                let reference = value
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| ParseError::new(SOURCE, line_no, "`branch` line has no ref"))?;
                entry.branch = Some(short_branch(reference).to_string());
            }
            "detached" => entry.detached = true,
            "bare" => entry.bare = true,
            "locked" => {
                entry.locked = true;
                entry.lock_reason = value.map(str::to_string).filter(|v| !v.is_empty());
            }
            "prunable" => {
                entry.prunable = true;
                entry.prune_reason = value.map(str::to_string).filter(|v| !v.is_empty());
            }
            // Forward compatibility: ignore attributes we do not model.
            _ => {}
        }
    }

    if let Some(entry) = current.take() {
        entries.push(entry);
    }
    Ok(entries)
}

/// `refs/heads/feature/auth` -> `feature/auth`.
pub fn short_branch(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL: &str = "\
worktree /home/u/proj
HEAD 0f2c8a1b3d4e5f60718293a4b5c6d7e8f9012345
branch refs/heads/main

worktree /home/u/wt/feature-auth
HEAD 1122334455667788990011223344556677889900
branch refs/heads/feature/auth

";

    #[test]
    fn parses_a_normal_listing() {
        let entries = parse_worktree_list(NORMAL).expect("valid porcelain");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/home/u/proj"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(
            entries[0].head.as_deref(),
            Some("0f2c8a1b3d4e5f60718293a4b5c6d7e8f9012345")
        );
        assert!(!entries[0].detached && !entries[0].bare && !entries[0].locked);
        assert_eq!(entries[1].branch.as_deref(), Some("feature/auth"));
        assert_eq!(entries[1].label(), "feature/auth");
    }

    #[test]
    fn parses_detached_head() {
        let text = "\
worktree /home/u/wt/review
HEAD abcdef1234567890abcdef1234567890abcdef12
detached
";
        let entries = parse_worktree_list(text).expect("valid porcelain");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].detached);
        assert_eq!(entries[0].branch, None);
        assert_eq!(entries[0].label(), "(abcdef1)");
    }

    #[test]
    fn parses_bare_repository() {
        let text = "worktree /home/u/proj.git\nbare\n\nworktree /home/u/wt/main\nHEAD aaaabbbbccccddddeeeeffff0000111122223333\nbranch refs/heads/main\n";
        let entries = parse_worktree_list(text).expect("valid porcelain");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].bare);
        assert_eq!(entries[0].head, None);
        assert_eq!(entries[0].label(), "(bare)");
        assert!(!entries[1].bare);
    }

    #[test]
    fn parses_locked_with_and_without_reason() {
        let text = "\
worktree /home/u/wt/a
HEAD 1111111111111111111111111111111111111111
branch refs/heads/a
locked

worktree /home/u/wt/b
HEAD 2222222222222222222222222222222222222222
branch refs/heads/b
locked on a removable drive
";
        let entries = parse_worktree_list(text).expect("valid porcelain");
        assert!(entries[0].locked);
        assert_eq!(entries[0].lock_reason, None);
        assert!(entries[1].locked);
        assert_eq!(
            entries[1].lock_reason.as_deref(),
            Some("on a removable drive")
        );
    }

    #[test]
    fn parses_prunable_moved_worktree() {
        let text = "\
worktree /home/u/wt/gone
HEAD 3333333333333333333333333333333333333333
branch refs/heads/gone
prunable gitdir file points to non-existent location
";
        let entries = parse_worktree_list(text).expect("valid porcelain");
        assert!(entries[0].prunable);
        assert_eq!(
            entries[0].prune_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );
    }

    #[test]
    fn parses_paths_containing_spaces() {
        let text = "\
worktree /home/u/my projects/the repo
HEAD 4444444444444444444444444444444444444444
branch refs/heads/feature/some thing
";
        let entries = parse_worktree_list(text).expect("valid porcelain");
        assert_eq!(
            entries[0].path,
            PathBuf::from("/home/u/my projects/the repo")
        );
        assert_eq!(entries[0].branch.as_deref(), Some("feature/some thing"));
    }

    #[test]
    fn accepts_output_truncated_before_the_final_blank_line() {
        let text = "worktree /home/u/proj\nHEAD 5555555555555555555555555555555555555555\nbranch refs/heads/main";
        let entries = parse_worktree_list(text).expect("valid porcelain");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn accepts_a_record_truncated_mid_way() {
        let entries = parse_worktree_list("worktree /home/u/proj\n").expect("valid porcelain");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].head, None);
        assert_eq!(entries[0].label(), "(no HEAD)");
    }

    #[test]
    fn empty_output_is_an_empty_list() {
        assert!(parse_worktree_list("").expect("valid").is_empty());
        assert!(parse_worktree_list("\n\n\n").expect("valid").is_empty());
    }

    #[test]
    fn tolerates_crlf() {
        let entries = parse_worktree_list(
            "worktree /home/u/proj\r\nHEAD 6666666666666666666666666666666666666666\r\nbranch refs/heads/main\r\n",
        )
        .expect("valid porcelain");
        assert_eq!(entries[0].path, PathBuf::from("/home/u/proj"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn ignores_unknown_attributes() {
        let entries = parse_worktree_list(
            "worktree /home/u/proj\nHEAD 7777777777777777777777777777777777777777\nbranch refs/heads/main\nsomething-new yes\n",
        )
        .expect("valid porcelain");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn rejects_attributes_before_any_worktree_line() {
        let err = parse_worktree_list("HEAD abc\nbranch refs/heads/main\n")
            .expect_err("must not silently accept");
        assert_eq!(err.line, 1);
        assert!(err.reason.contains("before any `worktree` line"));
    }

    #[test]
    fn rejects_a_worktree_line_without_a_path() {
        let err = parse_worktree_list("worktree\n").expect_err("must not accept");
        assert_eq!(err.line, 1);
        assert!(err.reason.contains("no path"));

        let err = parse_worktree_list("worktree \n").expect_err("must not accept");
        assert!(err.reason.contains("no path"));
    }

    #[test]
    fn rejects_empty_head_and_branch_values() {
        let err = parse_worktree_list("worktree /a\nHEAD \n").expect_err("must not accept");
        assert!(err.reason.contains("no commit"));
        let err = parse_worktree_list("worktree /a\nbranch \n").expect_err("must not accept");
        assert!(err.reason.contains("no ref"));
    }

    #[test]
    fn rejects_garbage_that_is_not_porcelain_at_all() {
        assert!(parse_worktree_list("fatal: not a git repository\n").is_err());
    }

    #[test]
    fn short_branch_strips_only_the_heads_prefix() {
        assert_eq!(short_branch("refs/heads/main"), "main");
        assert_eq!(short_branch("refs/heads/feature/x"), "feature/x");
        assert_eq!(
            short_branch("refs/remotes/origin/main"),
            "refs/remotes/origin/main"
        );
        assert_eq!(short_branch("main"), "main");
    }
}
