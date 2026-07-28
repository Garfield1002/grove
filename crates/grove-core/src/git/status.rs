//! Per-worktree git status summary (DESIGN.md §18).
//!
//! One `git status --porcelain=v2 --branch` call per worktree gives every
//! number the row sublabel needs: staged, modified, untracked and unmerged
//! counts, plus the branch, its upstream and the ahead/behind pair. The v2
//! format is stable and machine-readable — v1 is not, and `--porcelain=v2`
//! is the only form that reports ahead/behind together with per-file states.
//!
//! Porcelain v2 does *not* report an in-progress merge or rebase, so that one
//! fact comes from the presence of git's own state files, whose locations are
//! asked of git itself (`rev-parse --git-path`) rather than guessed: a linked
//! worktree keeps them under `.git/worktrees/<name>/`, not in the common dir.

use std::path::{Path, PathBuf};

use crate::error::{ParseError, Result};
use crate::git::commands::git_in;

const SOURCE: &str = "git status --porcelain=v2 --branch";

/// The status invocation's arguments. `--branch` adds the `# branch.*`
/// headers; without it there is no ahead/behind information at all.
pub const STATUS_ARGS: [&str; 3] = ["status", "--porcelain=v2", "--branch"];

/// A multi-step git operation the worktree is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl Operation {
    /// Uppercase marker for the row sublabel, as in direction 1c.
    pub fn label(self) -> &'static str {
        match self {
            Operation::Merge => "MERGING",
            Operation::Rebase => "REBASING",
            Operation::CherryPick => "CHERRY-PICKING",
            Operation::Revert => "REVERTING",
            Operation::Bisect => "BISECTING",
        }
    }
}

/// git's state markers, in the order they are probed. The first one present
/// wins: a conflicted rebase has both `rebase-merge` and `MERGE_HEAD`-like
/// state, and "REBASING" is the more useful answer there.
pub const OPERATION_MARKERS: [(&str, Operation); 6] = [
    ("rebase-merge", Operation::Rebase),
    ("rebase-apply", Operation::Rebase),
    ("MERGE_HEAD", Operation::Merge),
    ("CHERRY_PICK_HEAD", Operation::CherryPick),
    ("REVERT_HEAD", Operation::Revert),
    ("BISECT_LOG", Operation::Bisect),
];

/// Compact working-tree summary for one worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusSummary {
    /// Short branch name; `None` when HEAD is detached.
    pub branch: Option<String>,
    pub detached: bool,
    /// Upstream ref (`origin/main`), when the branch tracks one.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream. `None` when there is no upstream — the
    /// UI must say "unknown", never "0".
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    /// Entries with a staged change (index differs from HEAD).
    pub staged: u32,
    /// Entries with an unstaged change (working tree differs from the index).
    pub modified: u32,
    pub untracked: u32,
    /// Unmerged entries: a conflict is being resolved.
    pub conflicted: u32,
    pub operation: Option<Operation>,
}

impl StatusSummary {
    /// Nothing staged, modified, untracked or conflicted.
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.modified == 0 && self.untracked == 0 && self.conflicted == 0
    }

    /// Tracked content that would be lost with the worktree directory.
    pub fn has_uncommitted_changes(&self) -> bool {
        self.staged > 0 || self.modified > 0 || self.conflicted > 0
    }

    /// Does the branch track an upstream?
    pub fn has_upstream(&self) -> bool {
        self.upstream.is_some()
    }

    /// Sublabel text: `clean`, or the parts that are non-zero, most severe
    /// first (DESIGN.md §18).
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(operation) = self.operation {
            parts.push(operation.label().to_string());
        }
        if self.conflicted > 0 {
            parts.push(format!("{} conflicted", self.conflicted));
        }
        if self.staged > 0 {
            parts.push(format!("{} staged", self.staged));
        }
        if self.modified > 0 {
            parts.push(format!("{} mod", self.modified));
        }
        if self.untracked > 0 {
            parts.push(format!("{} untracked", self.untracked));
        }
        if let (Some(ahead), Some(behind)) = (self.ahead, self.behind) {
            if ahead > 0 {
                parts.push(format!("ahead {ahead}"));
            }
            if behind > 0 {
                parts.push(format!("behind {behind}"));
            }
        }
        if parts.is_empty() {
            return "clean".to_string();
        }
        parts.join(" · ")
    }
}

/// Parse `git status --porcelain=v2 --branch` output.
///
/// Unknown headers and entry types are ignored so a future git release cannot
/// break the row; a line that announces a type we *do* handle but is truncated
/// is an error rather than a silently wrong count.
pub fn parse_status(output: &str) -> std::result::Result<StatusSummary, ParseError> {
    let mut summary = StatusSummary::default();

    for (index, raw) in output.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        let (kind, rest) = match line.split_once(' ') {
            Some((kind, rest)) => (kind, rest),
            None => (line, ""),
        };

        match kind {
            "#" => parse_header(rest, line_no, &mut summary)?,
            "1" | "2" => {
                let xy = field(rest, 0).ok_or_else(|| {
                    ParseError::new(SOURCE, line_no, "changed entry has no XY field")
                })?;
                let (staged, modified) = parse_xy(xy, line_no)?;
                // Fields of `rest`: XY sub mH mI mW hH hI [Xscore] path.
                // A `2` entry (rename/copy) carries the rename score before
                // the path, and the original path after a tab; nothing here
                // needs the paths themselves, only that they are present.
                let wanted = if kind == "1" { 7 } else { 8 };
                if field(rest, wanted).is_none() {
                    return Err(ParseError::new(
                        SOURCE,
                        line_no,
                        format!("`{kind}` entry is truncated before its path"),
                    ));
                }
                summary.staged += u32::from(staged);
                summary.modified += u32::from(modified);
            }
            "u" => {
                let xy = field(rest, 0).ok_or_else(|| {
                    ParseError::new(SOURCE, line_no, "unmerged entry has no XY field")
                })?;
                parse_xy(xy, line_no)?;
                // XY sub m1 m2 m3 mW h1 h2 h3 path.
                if field(rest, 9).is_none() {
                    return Err(ParseError::new(
                        SOURCE,
                        line_no,
                        "`u` entry is truncated before its path",
                    ));
                }
                summary.conflicted += 1;
            }
            "?" => {
                if rest.is_empty() {
                    return Err(ParseError::new(SOURCE, line_no, "`?` entry has no path"));
                }
                summary.untracked += 1;
            }
            // `!` is only emitted with --ignored, which Grove never passes;
            // anything else is a future entry type.
            _ => {}
        }
    }

    Ok(summary)
}

fn parse_header(
    rest: &str,
    line_no: usize,
    summary: &mut StatusSummary,
) -> std::result::Result<(), ParseError> {
    let (key, value) = match rest.split_once(' ') {
        Some((key, value)) => (key, value.trim()),
        None => (rest, ""),
    };
    match key {
        "branch.head" => {
            if value == "(detached)" {
                summary.detached = true;
                summary.branch = None;
            } else if !value.is_empty() {
                summary.branch = Some(value.to_string());
            }
        }
        "branch.upstream" if !value.is_empty() => summary.upstream = Some(value.to_string()),
        "branch.ab" => {
            let mut fields = value.split_whitespace();
            let (Some(ahead), Some(behind)) = (fields.next(), fields.next()) else {
                return Err(ParseError::new(
                    SOURCE,
                    line_no,
                    "`# branch.ab` needs both +N and -N",
                ));
            };
            summary.ahead = Some(signed(ahead, '+', line_no)?);
            summary.behind = Some(signed(behind, '-', line_no)?);
        }
        // branch.oid, stash and anything newer.
        _ => {}
    }
    Ok(())
}

fn signed(value: &str, sign: char, line_no: usize) -> std::result::Result<u32, ParseError> {
    value
        .strip_prefix(sign)
        .and_then(|digits| digits.parse::<u32>().ok())
        .ok_or_else(|| {
            ParseError::new(
                SOURCE,
                line_no,
                format!("`{value}` is not a `{sign}N` count"),
            )
        })
}

/// The nth space-separated field of a line, or `None` when it is truncated.
fn field(rest: &str, index: usize) -> Option<&str> {
    rest.split(' ').nth(index).filter(|f| !f.is_empty())
}

/// Split an `XY` state pair into (staged, unstaged). `.` means unchanged.
fn parse_xy(xy: &str, line_no: usize) -> std::result::Result<(bool, bool), ParseError> {
    let mut chars = xy.chars();
    let (Some(x), Some(y), None) = (chars.next(), chars.next(), chars.next()) else {
        return Err(ParseError::new(
            SOURCE,
            line_no,
            format!("`{xy}` is not an XY state pair"),
        ));
    };
    Ok((x != '.', y != '.'))
}

/// The first operation whose marker is present, in [`OPERATION_MARKERS`]
/// order. `present` is parallel to that table.
pub fn operation_from_markers(present: &[bool]) -> Option<Operation> {
    OPERATION_MARKERS
        .iter()
        .zip(present)
        .find(|(_, present)| **present)
        .map(|((_, operation), _)| *operation)
}

/// Arguments that ask git for every marker path in one call. `rev-parse`
/// answers one line per `--git-path`, in order.
pub fn marker_args() -> Vec<&'static str> {
    let mut args: Vec<&'static str> = vec!["rev-parse", "--path-format=absolute"];
    for (marker, _) in OPERATION_MARKERS {
        args.push("--git-path");
        args.push(marker);
    }
    args
}

/// Ask git where each state marker lives for this worktree. Linked worktrees
/// keep them per-worktree, so these paths cannot be derived from the common
/// dir.
///
/// Runs a subprocess: worker thread only.
pub fn marker_paths(worktree: &Path) -> Result<Vec<PathBuf>> {
    let out = git_in(worktree, &marker_args()).output()?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Which multi-step operation, if any, this worktree is in the middle of.
///
/// Runs a subprocess: worker thread only.
pub fn operation(worktree: &Path) -> Result<Option<Operation>> {
    let paths = marker_paths(worktree)?;
    if paths.len() != OPERATION_MARKERS.len() {
        // An unexpected reply is not worth an error: the row simply shows no
        // operation marker rather than a wrong one.
        return Ok(None);
    }
    let present: Vec<bool> = paths.iter().map(|path| path.exists()).collect();
    Ok(operation_from_markers(&present))
}

/// Full status summary for one worktree.
///
/// Runs subprocesses: worker thread only.
pub fn status_summary(worktree: &Path) -> Result<StatusSummary> {
    let out = git_in(worktree, &STATUS_ARGS).output()?;
    let mut summary = parse_status(&out)?;
    summary.operation = operation(worktree)?;
    Ok(summary)
}

/// `git rev-list --count <upstream>..HEAD`: commits that exist only here.
///
/// Runs a subprocess: worker thread only.
pub fn unpushed_count(worktree: &Path, upstream: &str) -> Result<u32> {
    let range = format!("{upstream}..HEAD");
    let out = git_in(worktree, &["rev-list", "--count", &range]).output()?;
    Ok(out.trim().parse::<u32>().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLEAN: &str = "\
# branch.oid 084d8114812b995087fec985f1357de223ebdaa9
# branch.head main
# branch.upstream origin/main
# branch.ab +0 -0
";

    #[test]
    fn parses_a_clean_tracking_worktree() {
        let status = parse_status(CLEAN).expect("valid");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(!status.detached);
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (Some(0), Some(0)));
        assert!(status.is_clean());
        assert!(!status.has_uncommitted_changes());
        assert_eq!(status.summary(), "clean");
    }

    #[test]
    fn counts_staged_modified_and_untracked() {
        let text = "\
# branch.oid 084d8114812b995087fec985f1357de223ebdaa9
# branch.head feature/auth
1 M. N... 100644 100644 100644 7898192261 0f7bc76605 staged.rs
1 .M N... 100644 100644 100644 7898192261 0f7bc76605 modified.rs
1 MM N... 100644 100644 100644 7898192261 0f7bc76605 both.rs
1 A. N... 000000 100644 100644 0000000000 0f7bc76605 added.rs
? new.rs
? another.rs
";
        let status = parse_status(text).expect("valid");
        assert_eq!(status.staged, 3, "M., MM and A. are staged");
        assert_eq!(status.modified, 2, ".M and MM are unstaged");
        assert_eq!(status.untracked, 2);
        assert_eq!(status.conflicted, 0);
        assert!(!status.is_clean());
        assert!(status.has_uncommitted_changes());
        assert_eq!(status.summary(), "3 staged · 2 mod · 2 untracked");
    }

    #[test]
    fn counts_renamed_entries_and_keeps_their_two_paths_together() {
        let text = "\
# branch.head main
2 R. N... 100644 100644 100644 7898192261 0f7bc76605 R100 new name.rs\told name.rs
2 RM N... 100644 100644 100644 7898192261 0f7bc76605 R090 b.rs\ta.rs
";
        let status = parse_status(text).expect("valid");
        assert_eq!(status.staged, 2);
        assert_eq!(status.modified, 1);
        assert_eq!(status.summary(), "2 staged · 1 mod");
    }

    #[test]
    fn counts_unmerged_entries_as_conflicts() {
        let text = "\
# branch.head main
u UU N... 100644 100644 100644 100644 7898192261 0f7bc76605 aabbccddee conflict.rs
1 .M N... 100644 100644 100644 7898192261 0f7bc76605 other.rs
";
        let status = parse_status(text).expect("valid");
        assert_eq!(status.conflicted, 1);
        assert_eq!(status.modified, 1);
        assert!(status.has_uncommitted_changes());
        assert_eq!(status.summary(), "1 conflicted · 1 mod");
    }

    #[test]
    fn parses_paths_containing_spaces() {
        let text = "\
# branch.head main
1 .M N... 100644 100644 100644 7898192261 0f7bc76605 src/a file with spaces.rs
? another file.rs
";
        let status = parse_status(text).expect("valid");
        assert_eq!(status.modified, 1);
        assert_eq!(status.untracked, 1);
    }

    #[test]
    fn reports_ahead_and_behind() {
        let text = "# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n";
        let status = parse_status(text).expect("valid");
        assert_eq!((status.ahead, status.behind), (Some(2), Some(1)));
        assert_eq!(status.summary(), "ahead 2 · behind 1");
    }

    #[test]
    fn a_branch_without_an_upstream_has_no_ahead_behind() {
        let text = "# branch.oid abc\n# branch.head local-only\n";
        let status = parse_status(text).expect("valid");
        assert_eq!(status.upstream, None);
        assert!(!status.has_upstream());
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        // Absent must not be rendered as "ahead 0".
        assert_eq!(status.summary(), "clean");
    }

    #[test]
    fn parses_a_detached_head() {
        let text = "# branch.oid 084d811\n# branch.head (detached)\n";
        let status = parse_status(text).expect("valid");
        assert!(status.detached);
        assert_eq!(status.branch, None);
    }

    #[test]
    fn an_unborn_branch_reports_an_initial_oid() {
        let text = "# branch.oid (initial)\n# branch.head main\n? README.md\n";
        let status = parse_status(text).expect("valid");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.untracked, 1);
    }

    #[test]
    fn empty_output_is_a_clean_default() {
        let status = parse_status("").expect("valid");
        assert_eq!(status, StatusSummary::default());
        assert!(status.is_clean());
    }

    #[test]
    fn unknown_headers_and_entry_types_are_ignored() {
        let text = "\
# branch.head main
# stash 3
# something.new whatever
! ignored.rs
x 1 2 3
";
        let status = parse_status(text).expect("forward compatible");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.is_clean());
    }

    #[test]
    fn tolerates_crlf() {
        let status = parse_status("# branch.head main\r\n? new.rs\r\n").expect("valid");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.untracked, 1);
    }

    #[test]
    fn rejects_a_truncated_changed_entry() {
        let err = parse_status("1 M. N... 100644 100644\n").expect_err("truncated");
        assert_eq!(err.line, 1);
        assert!(err.reason.contains("truncated before its path"));
    }

    #[test]
    fn rejects_a_rename_entry_without_a_path() {
        let err = parse_status("2 R. N... 100644 100644 100644 7898192261 0f7bc76605 R100\n")
            .expect_err("truncated");
        assert!(err.reason.contains("truncated before its path"));
    }

    #[test]
    fn rejects_a_truncated_unmerged_entry() {
        let err =
            parse_status("u UU N... 100644 100644 100644 100644 a b c\n").expect_err("truncated");
        assert!(err.reason.contains("truncated before its path"));
    }

    #[test]
    fn rejects_a_bad_xy_pair() {
        let err = parse_status("1 MMM N... 1 2 3 4 5 f.rs\n").expect_err("bad xy");
        assert!(err.reason.contains("not an XY state pair"));
        let err = parse_status("1\n").expect_err("no xy");
        assert!(err.reason.contains("no XY field"));
    }

    #[test]
    fn rejects_an_untracked_entry_without_a_path() {
        let err = parse_status("?\n").expect_err("no path");
        assert!(err.reason.contains("`?` entry has no path"));
        let err = parse_status("? \n").expect_err("no path");
        assert!(err.reason.contains("`?` entry has no path"));
    }

    #[test]
    fn rejects_a_malformed_ahead_behind_header() {
        let err = parse_status("# branch.ab +2\n").expect_err("half a pair");
        assert!(err.reason.contains("both +N and -N"));
        let err = parse_status("# branch.ab 2 -1\n").expect_err("unsigned");
        assert!(err.reason.contains("not a `+N` count"));
        let err = parse_status("# branch.ab +2 1\n").expect_err("unsigned");
        assert!(err.reason.contains("not a `-N` count"));
    }

    #[test]
    fn operation_markers_are_probed_in_priority_order() {
        let none = [false; 6];
        assert_eq!(operation_from_markers(&none), None);

        let mut rebase = none;
        rebase[0] = true;
        assert_eq!(operation_from_markers(&rebase), Some(Operation::Rebase));

        let mut merge = none;
        merge[2] = true;
        assert_eq!(operation_from_markers(&merge), Some(Operation::Merge));

        // A conflicted rebase looks like a merge too; rebase wins.
        let mut both = none;
        both[0] = true;
        both[2] = true;
        assert_eq!(operation_from_markers(&both), Some(Operation::Rebase));

        let mut bisect = none;
        bisect[5] = true;
        assert_eq!(operation_from_markers(&bisect), Some(Operation::Bisect));
    }

    #[test]
    fn a_short_marker_reply_yields_no_operation() {
        assert_eq!(operation_from_markers(&[]), None);
        assert_eq!(operation_from_markers(&[false, false]), None);
    }

    #[test]
    fn operation_labels_are_the_1c_uppercase_markers() {
        assert_eq!(Operation::Merge.label(), "MERGING");
        assert_eq!(Operation::Rebase.label(), "REBASING");
        assert_eq!(Operation::CherryPick.label(), "CHERRY-PICKING");
        assert_eq!(Operation::Revert.label(), "REVERTING");
        assert_eq!(Operation::Bisect.label(), "BISECTING");
    }

    #[test]
    fn the_operation_leads_the_summary() {
        let status = StatusSummary {
            operation: Some(Operation::Merge),
            conflicted: 2,
            modified: 1,
            ..StatusSummary::default()
        };
        assert_eq!(status.summary(), "MERGING · 2 conflicted · 1 mod");
    }

    #[test]
    fn a_clean_worktree_mid_operation_still_shows_the_operation() {
        let status = StatusSummary {
            operation: Some(Operation::Rebase),
            ..StatusSummary::default()
        };
        assert!(status.is_clean());
        assert_eq!(status.summary(), "REBASING");
    }

    #[test]
    fn the_marker_probe_asks_git_for_every_path_in_one_call() {
        let args = marker_args();
        assert_eq!(&args[..2], &["rev-parse", "--path-format=absolute"]);
        assert_eq!(args.len(), 2 + OPERATION_MARKERS.len() * 2);
        for (index, (marker, _)) in OPERATION_MARKERS.iter().enumerate() {
            assert_eq!(args[2 + index * 2], "--git-path");
            assert_eq!(&args[3 + index * 2], marker);
        }
    }
}
