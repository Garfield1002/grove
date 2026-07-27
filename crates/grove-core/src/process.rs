//! Subprocess execution.
//!
//! Every command is built as a `(program, args)` value and run through
//! [`std::process::Command`]. Grove never builds a shell string from a path or
//! a branch name; the sole shell-interpreted string in the application is the
//! user's terminal template (see [`crate::terminal`]).

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{CommandFailure, Error, Result};

/// A command Grove intends to run, kept as data so it can be inspected and
/// unit-tested without executing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl Invocation {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    fn program_lossy(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    fn args_lossy(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Run the command, capturing stdout and stderr. Returns stdout on
    /// success; a non-zero exit becomes [`Error::Command`] with the full
    /// diagnostics attached.
    pub fn output(&self) -> Result<String> {
        self.output_in(None)
    }

    /// Like [`Invocation::output`] but with an explicit working directory.
    pub fn output_in(&self, cwd: Option<&Path>) -> Result<String> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output().map_err(|source| Error::Spawn {
            program: self.program_lossy(),
            args: self.args_lossy(),
            source,
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        Err(Error::Command(CommandFailure {
            program: self.program_lossy(),
            args: self.args_lossy(),
            status: output.status.code(),
            stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }))
    }

    /// Run the command, returning the captured output whether or not it
    /// succeeded. Used where a non-zero exit is a meaningful answer rather
    /// than an error (`tmux list-sessions` with no server running).
    pub fn output_allow_failure(&self) -> Result<CommandOutput> {
        let output = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|source| Error::Spawn {
                program: self.program_lossy(),
                args: self.args_lossy(),
                source,
            })?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            failure: CommandFailure {
                program: self.program_lossy(),
                args: self.args_lossy(),
                status: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
        })
    }

    /// Spawn the command fully detached: its own process group, no inherited
    /// standard streams, and a reaper thread so the child never becomes a
    /// zombie. Grove does not wait for it and it outlives Grove.
    pub fn spawn_detached(&self) -> Result<()> {
        use std::os::unix::process::CommandExt;

        let child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|source| Error::Spawn {
                program: self.program_lossy(),
                args: self.args_lossy(),
                source,
            })?;

        std::thread::Builder::new()
            .name("grove-reaper".into())
            .spawn(move || {
                let mut child = child;
                let _ = child.wait();
            })
            .map_err(|source| Error::io("could not start the process reaper thread", source))?;
        Ok(())
    }
}

/// Captured result of a command that is allowed to fail.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    /// Ready-to-report failure record, meaningful when `success` is false.
    pub failure: CommandFailure,
}

/// Is `program` an executable file on `PATH`?
pub fn is_on_path(program: &str) -> bool {
    lookup_on_path(program, std::env::var_os("PATH").as_deref())
}

/// Testable core of [`is_on_path`].
pub fn lookup_on_path(program: &str, path_var: Option<&OsStr>) -> bool {
    if program.is_empty() {
        return false;
    }
    if program.contains('/') {
        return is_executable(Path::new(program));
    }
    let Some(path_var) = path_var else {
        return false;
    };
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| is_executable(&dir.join(program)))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_builds_argument_arrays() {
        let inv = Invocation::new("git")
            .arg("-C")
            .arg("/tmp/my repo")
            .args(["worktree", "list"]);
        assert_eq!(inv.program, OsString::from("git"));
        assert_eq!(
            inv.args_lossy(),
            vec!["-C", "/tmp/my repo", "worktree", "list"]
        );
    }

    #[test]
    fn output_returns_stdout_on_success() {
        let out = Invocation::new("printf")
            .args(["hello %s", "world"])
            .output()
            .expect("printf should succeed");
        assert_eq!(out, "hello world");
    }

    #[test]
    fn output_captures_failure_details() {
        let err = Invocation::new("false")
            .output()
            .expect_err("false should fail");
        match err {
            Error::Command(failure) => {
                assert_eq!(failure.program, "false");
                assert_eq!(failure.status, Some(1));
            }
            other => panic!("expected a command failure, got {other:?}"),
        }
    }

    #[test]
    fn missing_executable_is_a_spawn_error() {
        let err = Invocation::new("grove-definitely-not-a-real-binary")
            .output()
            .expect_err("should not spawn");
        assert!(matches!(err, Error::Spawn { .. }));
        assert!(err.diagnostics().is_some());
    }

    #[test]
    fn output_allow_failure_keeps_going() {
        let out = Invocation::new("false")
            .output_allow_failure()
            .expect("should still run");
        assert!(!out.success);
        assert_eq!(out.failure.status, Some(1));
    }

    #[test]
    fn arguments_are_never_shell_interpreted() {
        let out = Invocation::new("printf")
            .args(["%s", "; rm -rf /tmp/nope"])
            .output()
            .expect("printf should succeed");
        assert_eq!(out, "; rm -rf /tmp/nope");
    }

    #[test]
    fn path_lookup_finds_and_rejects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("grove-fake-tool");
        std::fs::write(&exe, "#!/bin/sh\n").expect("write");
        let plain = dir.path().join("grove-not-exec");
        std::fs::write(&plain, "data").expect("write");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644))
                .expect("chmod");
        }
        let path_var = OsString::from(dir.path());
        assert!(lookup_on_path("grove-fake-tool", Some(&path_var)));
        assert!(!lookup_on_path("grove-not-exec", Some(&path_var)));
        assert!(!lookup_on_path("grove-absent", Some(&path_var)));
        assert!(!lookup_on_path("", Some(&path_var)));
        assert!(!lookup_on_path("grove-fake-tool", None));
        assert!(lookup_on_path(exe.to_str().expect("utf-8 temp path"), None));
    }
}
