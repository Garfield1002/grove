//! Per-agent RAM and CPU, read from the cgroup v2 filesystem.
//!
//! An agent started with resource accounting on runs in its own transient
//! systemd scope (see [`crate::agent`]), which is a cgroup. Its memory and CPU
//! are then two small files away.
//!
//! The cgroup is found through `/proc/<pid>/cgroup` rather than by guessing
//! where systemd puts a user scope: the pid comes from `tmux list-panes`, and
//! the kernel then says exactly which cgroup it is in. That works whatever the
//! slice layout, and it is also how a session started by some other means
//! would be found.
//!
//! Everything here degrades to `None`. cgroup v1, a container without
//! `/sys/fs/cgroup` mounted, a process that exited between listing and
//! reading — none of these are errors worth showing a user; they just mean no
//! figure to display.

use std::path::{Path, PathBuf};

/// The cgroup v2 mount point.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Memory and CPU for one cgroup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// `memory.current`, in bytes.
    pub memory_bytes: u64,
    /// `cpu.stat`'s `usage_usec`: CPU time consumed since the cgroup began.
    /// A cumulative counter — a rate needs two readings.
    pub cpu_usec: u64,
}

impl Usage {
    /// Add another cgroup's usage, for a session with more than one scope.
    ///
    /// Saturating rather than wrapping: a bogus counter must not panic the
    /// poller, and a clamped figure is the least wrong thing to show.
    pub fn plus(self, other: Usage) -> Usage {
        Usage {
            memory_bytes: self.memory_bytes.saturating_add(other.memory_bytes),
            cpu_usec: self.cpu_usec.saturating_add(other.cpu_usec),
        }
    }

    /// Memory rendered for a row: whole units, since this is a glanceable
    /// figure and not a measurement.
    pub fn memory_label(self) -> String {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;
        if self.memory_bytes >= GIB {
            // One decimal below 10 GiB, where the difference is still legible.
            let gib = self.memory_bytes as f64 / GIB as f64;
            if gib < 10.0 {
                return format!("{gib:.1} GB");
            }
            return format!("{} GB", self.memory_bytes / GIB);
        }
        format!("{} MB", self.memory_bytes / MIB)
    }

    /// CPU percentage between two readings of the same cgroup.
    ///
    /// Returns `None` when the counter went backwards — which happens when the
    /// scope was replaced between polls, so the two readings are of different
    /// cgroups and their difference is meaningless.
    pub fn cpu_percent(self, previous: Usage, elapsed: std::time::Duration) -> Option<f32> {
        let elapsed_usec = elapsed.as_micros();
        if elapsed_usec == 0 || self.cpu_usec < previous.cpu_usec {
            return None;
        }
        let delta = (self.cpu_usec - previous.cpu_usec) as f64;
        Some((delta / elapsed_usec as f64 * 100.0) as f32)
    }
}

/// The cgroup v2 path of a process, as `/proc/<pid>/cgroup` reports it.
///
/// The v2 line is the one with an empty controller list: `0::/path`. A v1-only
/// system has no such line, which is `None`.
pub fn parse_proc_cgroup(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
        .find(|path| !path.is_empty() && *path != "/")
        .map(str::to_string)
}

/// `memory.current` is a single integer.
pub fn parse_memory_current(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `usage_usec` from `cpu.stat`'s `key value` lines.
pub fn parse_cpu_stat(text: &str) -> Option<u64> {
    text.lines()
        .filter_map(|line| line.strip_prefix("usage_usec"))
        .find_map(|rest| rest.trim().parse().ok())
}

/// Is this cgroup one of Grove's agent scopes?
///
/// Used to ignore the session's shell, which sits in whatever cgroup the
/// terminal was started in — often a large one shared with the whole desktop,
/// whose memory figure would be badly misleading next to a worktree's name.
pub fn is_grove_scope(cgroup_path: &str) -> bool {
    cgroup_path
        .rsplit('/')
        .next()
        .is_some_and(|unit| unit.starts_with("grove-") && unit.ends_with(".scope"))
}

/// The directory a cgroup path maps to under the cgroup root.
pub fn cgroup_dir(root: &Path, cgroup_path: &str) -> PathBuf {
    root.join(cgroup_path.trim_start_matches('/'))
}

/// Read one cgroup's usage.
///
/// Reads two small sysfs files: cheap enough for the poller, and not a
/// subprocess.
pub fn read_usage(root: &Path, cgroup_path: &str) -> Option<Usage> {
    let dir = cgroup_dir(root, cgroup_path);
    let memory_bytes = std::fs::read_to_string(dir.join("memory.current"))
        .ok()
        .and_then(|text| parse_memory_current(&text))?;
    let cpu_usec = std::fs::read_to_string(dir.join("cpu.stat"))
        .ok()
        .and_then(|text| parse_cpu_stat(&text))
        .unwrap_or(0);
    Some(Usage {
        memory_bytes,
        cpu_usec,
    })
}

/// The cgroup a pid belongs to, under a `/proc` root.
pub fn cgroup_of_pid(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    parse_proc_cgroup(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A real `/proc/self/cgroup` from a systemd user session.
    const PROC_CGROUP: &str = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/grove-a1b2c3-agent-17f2e.scope\n";

    /// A real `cpu.stat`.
    const CPU_STAT: &str = "usage_usec 4898531403\n\
                            user_usec 3058420481\n\
                            system_usec 1840110921\n\
                            nice_usec 0\n\
                            core_sched.force_idle_usec 0\n\
                            nr_periods 0\n";

    #[test]
    fn reads_the_v2_line_of_proc_cgroup() {
        assert_eq!(
            parse_proc_cgroup(PROC_CGROUP).as_deref(),
            Some(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/grove-a1b2c3-agent-17f2e.scope"
            )
        );
    }

    #[test]
    fn a_v1_only_process_has_no_v2_cgroup() {
        let v1 = "12:pids:/user.slice\n11:memory:/user.slice\n10:cpu,cpuacct:/user.slice\n";
        assert_eq!(parse_proc_cgroup(v1), None);
        assert_eq!(parse_proc_cgroup(""), None);
        // The root cgroup carries no useful per-agent figure.
        assert_eq!(parse_proc_cgroup("0::/\n"), None);
    }

    #[test]
    fn parses_the_two_counter_files() {
        assert_eq!(parse_memory_current("566853632\n"), Some(566_853_632));
        assert_eq!(parse_memory_current("max\n"), None);
        assert_eq!(parse_memory_current(""), None);
        assert_eq!(parse_cpu_stat(CPU_STAT), Some(4_898_531_403));
        assert_eq!(parse_cpu_stat("user_usec 5\n"), None);
        assert_eq!(parse_cpu_stat(""), None);
    }

    #[test]
    fn only_grove_scopes_are_measured() {
        assert!(is_grove_scope(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/grove-a1b2c3-agent-1.scope"
        ));
        // The shell's cgroup: the terminal's, shared with much of the desktop.
        assert!(!is_grove_scope(
            "/user.slice/user-1000.slice/user@1000.service/app.slice/tmux-spawn-383f8156.scope"
        ));
        assert!(!is_grove_scope("/user.slice"));
        assert!(!is_grove_scope(""));
        // Near misses.
        assert!(!is_grove_scope("/app.slice/grove-a1b2c3-agent-1.service"));
        assert!(!is_grove_scope("/app.slice/notgrove-a1b2c3.scope"));
    }

    #[test]
    fn a_cgroup_path_maps_under_the_root_without_doubling_the_slash() {
        assert_eq!(
            cgroup_dir(Path::new("/sys/fs/cgroup"), "/user.slice/x.scope"),
            PathBuf::from("/sys/fs/cgroup/user.slice/x.scope")
        );
    }

    #[test]
    fn reads_usage_from_a_cgroup_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = cgroup_dir(root.path(), "/user.slice/grove-a1b2c3-agent-1.scope");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("memory.current"), "566853632\n").expect("write");
        std::fs::write(dir.join("cpu.stat"), CPU_STAT).expect("write");

        let usage = read_usage(root.path(), "/user.slice/grove-a1b2c3-agent-1.scope")
            .expect("reads both files");
        assert_eq!(usage.memory_bytes, 566_853_632);
        assert_eq!(usage.cpu_usec, 4_898_531_403);
    }

    #[test]
    fn a_missing_cgroup_is_none_not_an_error() {
        // The scope exited between listing the pane and reading its files.
        let root = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_usage(root.path(), "/gone.scope"), None);
    }

    #[test]
    fn a_cgroup_without_cpu_stat_still_reports_memory() {
        // cpu.stat is absent until the cpu controller is enabled on the slice.
        let root = tempfile::tempdir().expect("tempdir");
        let dir = cgroup_dir(root.path(), "/x.scope");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("memory.current"), "1048576\n").expect("write");

        let usage = read_usage(root.path(), "/x.scope").expect("memory alone is enough");
        assert_eq!(usage.memory_bytes, 1_048_576);
        assert_eq!(usage.cpu_usec, 0);
    }

    #[test]
    fn finds_the_cgroup_of_a_pid_under_a_proc_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("4242");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("cgroup"), PROC_CGROUP).expect("write");

        assert!(cgroup_of_pid(root.path(), 4242).is_some_and(|p| is_grove_scope(&p)));
        // A pid that has exited.
        assert_eq!(cgroup_of_pid(root.path(), 9999), None);
    }

    #[test]
    fn memory_labels_read_at_a_glance() {
        let mb = |bytes: u64| {
            Usage {
                memory_bytes: bytes,
                cpu_usec: 0,
            }
            .memory_label()
        };
        assert_eq!(mb(0), "0 MB");
        assert_eq!(mb(566_853_632), "540 MB");
        assert_eq!(mb(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(mb(3 * 1024 * 1024 * 1024 + 512 * 1024 * 1024), "3.5 GB");
        assert_eq!(mb(12 * 1024 * 1024 * 1024), "12 GB");
    }

    #[test]
    fn cpu_percent_is_the_delta_over_the_interval() {
        let before = Usage {
            memory_bytes: 0,
            cpu_usec: 1_000_000,
        };
        // A full second of CPU over a two-second interval is 50%.
        let after = Usage {
            memory_bytes: 0,
            cpu_usec: 2_000_000,
        };
        let percent = after
            .cpu_percent(before, Duration::from_secs(2))
            .expect("a rate");
        assert!((percent - 50.0).abs() < 0.01, "got {percent}");
    }

    #[test]
    fn a_counter_that_went_backwards_yields_no_rate() {
        // The scope was replaced between polls: the two readings are of
        // different cgroups and their difference means nothing.
        let before = Usage {
            memory_bytes: 0,
            cpu_usec: 5_000_000,
        };
        let after = Usage {
            memory_bytes: 0,
            cpu_usec: 1_000,
        };
        assert_eq!(after.cpu_percent(before, Duration::from_secs(2)), None);
        // And a zero interval cannot produce one either.
        assert_eq!(after.cpu_percent(after, Duration::ZERO), None);
    }

    #[test]
    fn usage_sums_across_a_sessions_scopes() {
        let a = Usage {
            memory_bytes: 100,
            cpu_usec: 10,
        };
        let b = Usage {
            memory_bytes: 200,
            cpu_usec: 20,
        };
        assert_eq!(
            a.plus(b),
            Usage {
                memory_bytes: 300,
                cpu_usec: 30
            }
        );
        // Saturating, so a bogus counter cannot panic the poller.
        assert_eq!(
            Usage {
                memory_bytes: u64::MAX,
                cpu_usec: u64::MAX
            }
            .plus(a)
            .memory_bytes,
            u64::MAX
        );
    }
}
