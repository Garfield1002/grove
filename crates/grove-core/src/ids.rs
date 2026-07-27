//! Deterministic identifiers.
//!
//! A worktree id is the first 6 hex characters of a hash over
//! `(canonical git-common-dir, canonical worktree path)` and the tmux session
//! name is `wt-<id>` (ARCHITECTURE.md §1). The hash must be stable across
//! runs, processes and machines: losing `state.toml` is recoverable precisely
//! because restore re-derives identical ids and finds the live tmux sessions
//! again. `std::collections::hash_map::DefaultHasher` is *not* usable here —
//! SipHash keys are randomised per process.
//!
//! The hash below is FNV-1a/64 followed by a SplitMix64 finalizer. FNV-1a
//! alone mixes its low bits poorly (the last operation is a multiply), and the
//! id is taken from the low bits, so the finalizer is load-bearing.

use std::path::Path;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Number of hex characters in a worktree id.
pub const ID_LEN: usize = 6;

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn finalize(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

/// Stable 64-bit digest of the inputs, exposed for tests and future ids.
pub fn digest(git_common_dir: &Path, worktree_path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let mut hash = fnv1a(git_common_dir.as_os_str().as_bytes(), FNV_OFFSET_BASIS);
    // Separator that cannot occur inside a path, so ("/ab", "/c") and
    // ("/a", "b/c") cannot collide by concatenation.
    hash = fnv1a(&[0u8], hash);
    hash = fnv1a(worktree_path.as_os_str().as_bytes(), hash);
    finalize(hash)
}

/// The deterministic 6-hex-character id for a worktree.
pub fn worktree_id(git_common_dir: &Path, worktree_path: &Path) -> String {
    let value = digest(git_common_dir, worktree_path) & 0x00ff_ffff;
    format!("{value:06x}")
}

/// The tmux session name for a worktree id.
pub fn session_name(worktree_id: &str) -> String {
    format!("wt-{worktree_id}")
}

/// Could this string be a worktree id?
///
/// A shape check only — it says nothing about whether such a worktree exists.
/// `grove notify` uses it to reject a bad `--session` up front instead of
/// silently addressing a session that cannot exist.
pub fn is_worktree_id(value: &str) -> bool {
    value.len() == ID_LEN && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The worktree id encoded in a tmux session name, if it looks like one of
/// ours.
pub fn id_from_session_name(session_name: &str) -> Option<&str> {
    let id = session_name.strip_prefix("wt-")?;
    if is_worktree_id(id) { Some(id) } else { None }
}

/// A stable id for a project, derived from its canonical git-common-dir.
pub fn project_id(git_common_dir: &Path) -> String {
    let value = digest(git_common_dir, Path::new("")) & 0x00ff_ffff;
    format!("{value:06x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn ids_are_six_lowercase_hex_characters() {
        let id = worktree_id(Path::new("/repo/.git"), Path::new("/repo"));
        assert_eq!(id.len(), ID_LEN);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }

    /// Golden values: if these change, every existing tmux session becomes
    /// unreachable after a restart. Changing them is a breaking change.
    #[test]
    fn ids_are_stable_across_runs() {
        assert_eq!(
            worktree_id(Path::new("/home/u/proj/.git"), Path::new("/home/u/proj")),
            worktree_id(Path::new("/home/u/proj/.git"), Path::new("/home/u/proj"))
        );
        // Recorded golden values.
        assert_eq!(
            worktree_id(Path::new("/home/u/proj/.git"), Path::new("/home/u/proj")),
            "69f1b5"
        );
        assert_eq!(
            worktree_id(
                Path::new("/home/u/proj/.git"),
                Path::new("/home/u/wt/feature")
            ),
            "bceeb7"
        );
        assert_eq!(project_id(Path::new("/home/u/proj/.git")), "499342");
    }

    #[test]
    fn different_worktrees_of_a_project_differ() {
        let common = Path::new("/home/u/proj/.git");
        let a = worktree_id(common, Path::new("/home/u/proj"));
        let b = worktree_id(common, Path::new("/home/u/wt/feature"));
        assert_ne!(a, b);
    }

    #[test]
    fn same_path_in_different_projects_differs() {
        let path = Path::new("/home/u/work");
        assert_ne!(
            worktree_id(Path::new("/home/u/a/.git"), path),
            worktree_id(Path::new("/home/u/b/.git"), path)
        );
    }

    #[test]
    fn the_separator_prevents_concatenation_collisions() {
        assert_ne!(
            digest(Path::new("/ab"), Path::new("/c")),
            digest(Path::new("/a"), Path::new("b/c"))
        );
    }

    #[test]
    fn ids_are_well_distributed_over_realistic_inputs() {
        let common = Path::new("/home/u/proj/.git");
        let ids: HashSet<String> = (0..500)
            .map(|i| worktree_id(common, &PathBuf::from(format!("/home/u/wt/branch-{i}"))))
            .collect();
        // 500 draws from a 16.7M space: any collision at all would be
        // extraordinary, and a systematically bad hash would collapse badly.
        assert!(ids.len() >= 499, "only {} distinct ids", ids.len());
    }

    #[test]
    fn paths_with_spaces_and_unicode_hash() {
        let id = worktree_id(
            Path::new("/home/u/my repo/.git"),
            Path::new("/home/u/wörk tree"),
        );
        assert_eq!(id.len(), ID_LEN);
    }

    #[test]
    fn session_names_round_trip() {
        let id = worktree_id(Path::new("/repo/.git"), Path::new("/repo"));
        let name = session_name(&id);
        assert!(name.starts_with("wt-"));
        assert_eq!(id_from_session_name(&name), Some(id.as_str()));
    }

    #[test]
    fn foreign_session_names_are_rejected() {
        assert_eq!(id_from_session_name("main"), None);
        assert_eq!(id_from_session_name("wt-"), None);
        assert_eq!(id_from_session_name("wt-abc"), None);
        assert_eq!(id_from_session_name("wt-abcdefg"), None);
        assert_eq!(id_from_session_name("wt-zzzzzz"), None);
        assert_eq!(id_from_session_name("xwt-abcdef"), None);
    }
}
