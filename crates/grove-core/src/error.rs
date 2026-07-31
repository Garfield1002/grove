//! Error types for grove-core.
//!
//! Every subprocess failure retains the executable, arguments, exit status,
//! stdout and stderr (ARCHITECTURE.md §8.5) so the UI can show a concise
//! message with expandable diagnostics without ever hiding git's stderr.

use std::fmt;
use std::path::PathBuf;

/// A subprocess that ran to completion but reported failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFailure {
    pub program: String,
    pub args: Vec<String>,
    /// `None` when the process was terminated by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandFailure {
    /// The command line as it would be typed, for diagnostics only. This is
    /// never fed back into a shell.
    pub fn command_line(&self) -> String {
        let mut out = String::from(&self.program);
        for arg in &self.args {
            out.push(' ');
            if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '"') {
                out.push('"');
                out.push_str(&arg.replace('"', "\\\""));
                out.push('"');
            } else {
                out.push_str(arg);
            }
        }
        out
    }

    /// Multi-line diagnostics block shown behind "show command output".
    pub fn diagnostics(&self) -> String {
        let mut out = format!("$ {}\n", self.command_line());
        match self.status {
            Some(code) => out.push_str(&format!("exit status: {code}\n")),
            None => out.push_str("exit status: terminated by signal\n"),
        }
        if !self.stdout.trim().is_empty() {
            out.push_str("--- stdout ---\n");
            out.push_str(self.stdout.trim_end());
            out.push('\n');
        }
        if !self.stderr.trim().is_empty() {
            out.push_str("--- stderr ---\n");
            out.push_str(self.stderr.trim_end());
            out.push('\n');
        }
        out
    }

    /// One-line summary: the diagnosis if the command gave one, else the
    /// first non-empty output line, else the exit status.
    ///
    /// git writes progress to stderr before it fails (`Preparing worktree…`
    /// precedes `fatal: … is already used by worktree …`), so the first line
    /// is often not the reason. The full stderr is never hidden either way —
    /// [`CommandFailure::diagnostics`] keeps all of it.
    pub fn summary(&self) -> String {
        let lines = || {
            self.stderr
                .lines()
                .chain(self.stdout.lines())
                .map(str::trim)
        };
        let first = lines()
            .find(|line| {
                let lowered = line.to_ascii_lowercase();
                lowered.starts_with("fatal:") || lowered.starts_with("error:")
            })
            .or_else(|| lines().find(|line| !line.is_empty()));
        match (first, self.status) {
            (Some(line), _) => line.to_string(),
            (None, Some(code)) => format!("{} exited with status {code}", self.program),
            (None, None) => format!("{} was terminated by a signal", self.program),
        }
    }
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

impl std::error::Error for CommandFailure {}

/// Failure while parsing the porcelain / format output of git or tmux.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("could not parse {source_name} output (line {line}): {reason}")]
pub struct ParseError {
    pub source_name: &'static str,
    pub line: usize,
    pub reason: String,
}

impl ParseError {
    pub fn new(source_name: &'static str, line: usize, reason: impl Into<String>) -> Self {
        Self {
            source_name,
            line,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        args: Vec<String>,
        #[source]
        source: std::io::Error,
    },

    #[error("{0}")]
    Command(#[from] CommandFailure),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Parse(#[from] ParseError),

    #[error("{0} is not inside a git repository")]
    NotARepository(PathBuf),

    #[error("git reported no worktrees for {0}")]
    NoWorktrees(PathBuf),

    #[error("the worktree {0} no longer exists")]
    WorktreeMissing(PathBuf),

    #[error("could not read {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// `config.toml` could not be parsed for a surgical edit. Boxed because
    /// `toml_edit::TomlError` is large and this variant is rare.
    #[error("could not parse {path}: {source}")]
    ConfigEdit {
        path: PathBuf,
        #[source]
        source: Box<toml_edit::TomlError>,
    },

    #[error("cannot set {key}: {reason}")]
    ConfigEditKey { key: String, reason: String },

    #[error("could not read {path}: {source}")]
    StateRead {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not serialize state: {0}")]
    StateWrite(#[from] toml::ser::Error),

    /// A failure inside a layer built on top of Grove rather than inside
    /// Grove — the agent harness, today.
    ///
    /// Core does not know what those layers are, and must not: naming
    /// `grove_harness`'s types here is exactly the dependency the crate split
    /// removed. They still surface to the same UI and the same CLI, so they
    /// need a way in, and this is it. The `context` says which operation
    /// failed; the source carries the layer's own error whole, so nothing is
    /// flattened to a string on the way through.
    #[error("{context}: {source}")]
    Integration {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    #[error("no agent command is configured — set `command` under [agents] in config.toml")]
    NoAgentCommand,

    #[error("no resume command is configured — set `resume_command` under [agents] in config.toml")]
    NoResumeCommand,

    #[error("no agent conversation has been reported for this worktree yet")]
    NoAgentSession,

    #[error("the terminal command template is empty")]
    EmptyTerminalTemplate,

    #[error("could not parse the terminal command template: {0}")]
    TerminalTemplate(String),

    #[error(
        "no supported terminal emulator found on PATH (tried {0}); set terminal.command in config.toml"
    )]
    NoTerminalFound(String),

    #[error("no home directory: neither {0} nor HOME is set")]
    NoHomeDirectory(&'static str),
}

impl Error {
    /// Diagnostics for the expandable section of the UI error area, if the
    /// error carries any.
    pub fn diagnostics(&self) -> Option<String> {
        match self {
            Error::Command(failure) => Some(failure.diagnostics()),
            Error::Spawn { program, args, .. } => Some(format!(
                "$ {}\n",
                CommandFailure {
                    program: program.clone(),
                    args: args.clone(),
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                }
                .command_line()
            )),
            _ => None,
        }
    }

    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// Wrap a failure from a layer built on top of Grove. See
    /// [`Error::Integration`].
    pub fn integration(
        context: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Error::Integration {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> CommandFailure {
        CommandFailure {
            program: "git".into(),
            args: vec![
                "-C".into(),
                "/home/u/my repo".into(),
                "worktree".into(),
                "add".into(),
            ],
            status: Some(128),
            stdout: String::new(),
            stderr: "fatal: 'feature/auth' is already checked out at '/home/u/auth'\n".into(),
        }
    }

    #[test]
    fn command_line_quotes_paths_with_spaces() {
        assert_eq!(
            failure().command_line(),
            "git -C \"/home/u/my repo\" worktree add"
        );
    }

    #[test]
    fn summary_is_the_first_stderr_line() {
        assert_eq!(
            failure().summary(),
            "fatal: 'feature/auth' is already checked out at '/home/u/auth'"
        );
    }

    /// git prints progress before it fails; the concise line must be the
    /// diagnosis, not the noise that preceded it.
    #[test]
    fn summary_skips_progress_output_to_find_the_diagnosis() {
        let failure = CommandFailure {
            stderr: "Preparing worktree (checking out 'feature/auth')\n\
                     fatal: 'feature/auth' is already used by worktree at '/home/u/auth'\n"
                .into(),
            ..failure()
        };
        assert_eq!(
            failure.summary(),
            "fatal: 'feature/auth' is already used by worktree at '/home/u/auth'"
        );
        // Nothing is hidden: the progress line is still in the diagnostics.
        assert!(failure.diagnostics().contains("Preparing worktree"));
    }

    #[test]
    fn summary_finds_an_error_line_too() {
        let failure = CommandFailure {
            stderr: "some noise\nerror: the branch 'x' is not fully merged.\n".into(),
            ..failure()
        };
        assert_eq!(
            failure.summary(),
            "error: the branch 'x' is not fully merged."
        );
    }

    #[test]
    fn summary_falls_back_to_the_first_line_when_nothing_is_labelled() {
        let failure = CommandFailure {
            stderr: "something went wrong\nand then more\n".into(),
            ..failure()
        };
        assert_eq!(failure.summary(), "something went wrong");
    }

    #[test]
    fn summary_falls_back_to_exit_status() {
        let failure = CommandFailure {
            stderr: String::new(),
            stdout: "  \n".into(),
            ..failure()
        };
        assert_eq!(failure.summary(), "git exited with status 128");
    }

    #[test]
    fn summary_reports_signal_termination() {
        let failure = CommandFailure {
            stderr: String::new(),
            stdout: String::new(),
            status: None,
            ..failure()
        };
        assert_eq!(failure.summary(), "git was terminated by a signal");
    }

    #[test]
    fn diagnostics_retain_everything() {
        let failure = CommandFailure {
            stdout: "some output\n".into(),
            ..failure()
        };
        let text = failure.diagnostics();
        assert!(text.contains("$ git -C \"/home/u/my repo\" worktree add"));
        assert!(text.contains("exit status: 128"));
        assert!(text.contains("--- stdout ---\nsome output"));
        assert!(text.contains("--- stderr ---\nfatal: 'feature/auth'"));
    }

    #[test]
    fn diagnostics_omit_empty_streams() {
        let text = failure().diagnostics();
        assert!(!text.contains("--- stdout ---"));
        assert!(text.contains("--- stderr ---"));
    }

    #[test]
    fn error_display_does_not_hide_stderr() {
        let err = Error::from(failure());
        assert_eq!(
            err.to_string(),
            "fatal: 'feature/auth' is already checked out at '/home/u/auth'"
        );
        let diagnostics = err.diagnostics().unwrap_or_default();
        assert!(diagnostics.contains("fatal: 'feature/auth'"));
    }

    #[test]
    fn parse_error_display_mentions_line_and_source() {
        let err = ParseError::new(
            "git worktree list --porcelain",
            3,
            "expected `worktree <path>`",
        );
        assert_eq!(
            err.to_string(),
            "could not parse git worktree list --porcelain output (line 3): expected `worktree <path>`"
        );
    }

    #[test]
    fn plain_errors_have_no_diagnostics() {
        assert!(
            Error::NotARepository(PathBuf::from("/tmp/x"))
                .diagnostics()
                .is_none()
        );
    }
}
