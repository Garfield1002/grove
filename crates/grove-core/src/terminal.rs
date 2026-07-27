//! Terminal launching.
//!
//! The terminal command template is the *only* shell-interpreted string in
//! Grove, and it is documented as trusted user configuration
//! (ARCHITECTURE.md §8.4). To keep it from becoming an injection vector, the
//! template is tokenised **before** substitution and the values are then
//! placed into whole tokens. A worktree path such as `/tmp/x; rm -rf ~` can
//! therefore never split into extra arguments or reach a shell — it stays one
//! argv entry.

use std::path::Path;

use crate::error::{Error, Result};
use crate::process::{Invocation, is_on_path};

/// Terminals probed on first run, in preference order, with the template
/// written into `config.toml` when one is found.
pub const CANDIDATES: &[(&str, &str)] = &[
    (
        "ptyxis",
        "ptyxis -- tmux -S {socket} attach-session -t {session}",
    ),
    ("foot", "foot tmux -S {socket} attach-session -t {session}"),
    (
        "alacritty",
        "alacritty -e tmux -S {socket} attach-session -t {session}",
    ),
    (
        "kitty",
        "kitty tmux -S {socket} attach-session -t {session}",
    ),
    (
        "gnome-terminal",
        "gnome-terminal -- tmux -S {socket} attach-session -t {session}",
    ),
];

/// Template placeholders Grove substitutes. Anything else is left alone.
pub const PLACEHOLDERS: &[&str] = &["socket", "session", "worktree", "project", "branch"];

/// Values substituted into a terminal template.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateVars {
    pub socket: String,
    pub session: String,
    pub worktree: String,
    pub project: String,
    pub branch: String,
}

impl TemplateVars {
    pub fn new(socket: &Path, session: &str, worktree: &Path, project: &str, branch: &str) -> Self {
        Self {
            socket: socket.to_string_lossy().into_owned(),
            session: session.to_string(),
            worktree: worktree.to_string_lossy().into_owned(),
            project: project.to_string(),
            branch: branch.to_string(),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        match key {
            "socket" => Some(&self.socket),
            "session" => Some(&self.session),
            "worktree" => Some(&self.worktree),
            "project" => Some(&self.project),
            "branch" => Some(&self.branch),
            _ => None,
        }
    }
}

/// Split a template into argv tokens using shell quoting rules. This is the
/// only place shell syntax is honoured, and it happens before any Grove value
/// is inserted.
pub fn tokenize(template: &str) -> Result<Vec<String>> {
    let tokens =
        shell_words::split(template).map_err(|e| Error::TerminalTemplate(e.to_string()))?;
    if tokens.is_empty() {
        return Err(Error::EmptyTerminalTemplate);
    }
    Ok(tokens)
}

/// Replace `{placeholder}` occurrences inside a single token. The result is
/// always exactly one token, whatever the value contains.
pub fn substitute_token(token: &str, vars: &TemplateVars) -> String {
    let mut out = String::with_capacity(token.len());
    let mut rest = token;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let key = &after[..close];
                match vars.get(key) {
                    Some(value) => out.push_str(value),
                    // Unknown placeholder: preserve it verbatim.
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Expand a template into a program and its arguments.
pub fn expand(template: &str, vars: &TemplateVars) -> Result<Invocation> {
    let tokens = tokenize(template)?;
    let mut expanded = tokens.iter().map(|t| substitute_token(t, vars));
    let program = expanded.next().unwrap_or_default();
    if program.is_empty() {
        return Err(Error::EmptyTerminalTemplate);
    }
    Ok(Invocation::new(program).args(expanded))
}

/// Human-readable rendering of the expanded command, for the settings pane and
/// error diagnostics. Never executed.
pub fn preview(invocation: &Invocation) -> String {
    let mut parts = vec![invocation.program.to_string_lossy().into_owned()];
    parts.extend(
        invocation
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned()),
    );
    shell_words::join(parts.iter().map(String::as_str))
}

/// Expand the template and spawn the terminal detached, so it outlives Grove.
pub fn launch(template: &str, vars: &TemplateVars) -> Result<Invocation> {
    let invocation = expand(template, vars)?;
    invocation.spawn_detached()?;
    Ok(invocation)
}

/// Pick a terminal template using a caller-supplied "is this on PATH" probe.
pub fn detect_with(probe: impl Fn(&str) -> bool) -> Result<&'static str> {
    for (program, template) in CANDIDATES {
        if probe(program) {
            return Ok(template);
        }
    }
    let tried = CANDIDATES
        .iter()
        .map(|(program, _)| *program)
        .collect::<Vec<_>>()
        .join(", ");
    Err(Error::NoTerminalFound(tried))
}

/// Probe the real `PATH` for a supported terminal.
pub fn detect() -> Result<&'static str> {
    detect_with(is_on_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn vars() -> TemplateVars {
        TemplateVars::new(
            Path::new("/run/user/1000/grove/tmux.sock"),
            "wt-a1b2c3",
            Path::new("/home/u/wt/feature"),
            "acme-web",
            "feature/auth",
        )
    }

    fn argv(invocation: &Invocation) -> Vec<String> {
        let mut out = vec![invocation.program.to_string_lossy().into_owned()];
        out.extend(
            invocation
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned()),
        );
        out
    }

    #[test]
    fn tokenizes_a_default_template() {
        assert_eq!(
            tokenize("foot tmux -S {socket} attach-session -t {session}").expect("valid"),
            vec![
                "foot",
                "tmux",
                "-S",
                "{socket}",
                "attach-session",
                "-t",
                "{session}"
            ]
        );
    }

    #[test]
    fn honours_shell_quoting_in_the_template_itself() {
        let tokens = tokenize("'my terminal' -e tmux").expect("valid");
        assert_eq!(tokens, vec!["my terminal", "-e", "tmux"]);
    }

    #[test]
    fn rejects_empty_and_unbalanced_templates() {
        assert!(matches!(tokenize("   "), Err(Error::EmptyTerminalTemplate)));
        assert!(matches!(
            tokenize("foot 'unclosed"),
            Err(Error::TerminalTemplate(_))
        ));
    }

    #[test]
    fn expands_every_placeholder() {
        let inv = expand(
            "term {socket} {session} {worktree} {project} {branch}",
            &vars(),
        )
        .expect("expands");
        assert_eq!(
            argv(&inv),
            vec![
                "term",
                "/run/user/1000/grove/tmux.sock",
                "wt-a1b2c3",
                "/home/u/wt/feature",
                "acme-web",
                "feature/auth",
            ]
        );
    }

    #[test]
    fn expands_all_default_templates() {
        for (program, template) in CANDIDATES {
            let inv = expand(template, &vars()).expect("default templates expand");
            assert_eq!(inv.program, std::ffi::OsString::from(*program));
            let args = argv(&inv);
            assert!(args.contains(&"/run/user/1000/grove/tmux.sock".to_string()));
            assert!(args.contains(&"wt-a1b2c3".to_string()));
            assert!(!args.iter().any(|a| a.contains('{')));
        }
    }

    #[test]
    fn placeholders_may_be_embedded_in_a_token() {
        let inv = expand(
            "term --title=grove:{project}/{branch} --dir={worktree}",
            &vars(),
        )
        .expect("expands");
        assert_eq!(
            argv(&inv),
            vec![
                "term",
                "--title=grove:acme-web/feature/auth",
                "--dir=/home/u/wt/feature",
            ]
        );
    }

    /// The load-bearing security test: values are inserted into already-split
    /// tokens, so no value can create a new argument or a shell operator.
    #[test]
    fn a_path_with_spaces_stays_one_argument() {
        let vars = TemplateVars {
            worktree: "/home/u/my projects/the repo".into(),
            ..vars()
        };
        let inv = expand("term -e tmux -c {worktree}", &vars).expect("expands");
        assert_eq!(
            argv(&inv),
            vec!["term", "-e", "tmux", "-c", "/home/u/my projects/the repo"]
        );
    }

    #[test]
    fn shell_metacharacters_in_values_are_inert() {
        let vars = TemplateVars {
            worktree: "/tmp/x; rm -rf ~".into(),
            branch: "$(touch /tmp/pwned)".into(),
            project: "a && b | c > /tmp/out".into(),
            ..vars()
        };
        let inv = expand("term -c {worktree} -t {branch} -p {project}", &vars).expect("expands");
        assert_eq!(
            argv(&inv),
            vec![
                "term",
                "-c",
                "/tmp/x; rm -rf ~",
                "-t",
                "$(touch /tmp/pwned)",
                "-p",
                "a && b | c > /tmp/out",
            ]
        );
    }

    #[test]
    fn a_value_containing_quotes_does_not_reopen_tokenization() {
        let vars = TemplateVars {
            branch: "feature/\"; rm -rf /\"".into(),
            ..vars()
        };
        let inv = expand("term -t {branch} tail", &vars).expect("expands");
        assert_eq!(
            argv(&inv),
            vec!["term", "-t", "feature/\"; rm -rf /\"", "tail"]
        );
    }

    #[test]
    fn a_value_containing_a_placeholder_is_not_re_expanded() {
        let vars = TemplateVars {
            branch: "{worktree}".into(),
            ..vars()
        };
        let inv = expand("term -t {branch}", &vars).expect("expands");
        assert_eq!(argv(&inv), vec!["term", "-t", "{worktree}"]);
    }

    #[test]
    fn unknown_and_unclosed_placeholders_are_left_verbatim() {
        assert_eq!(substitute_token("{nope}", &vars()), "{nope}");
        assert_eq!(substitute_token("{unclosed", &vars()), "{unclosed");
        assert_eq!(substitute_token("a{}b", &vars()), "a{}b");
        assert_eq!(
            substitute_token("{session}{nope}{session}", &vars()),
            "wt-a1b2c3{nope}wt-a1b2c3"
        );
    }

    #[test]
    fn a_template_that_is_only_a_placeholder_expanding_to_nothing_is_rejected() {
        let vars = TemplateVars::default();
        assert!(matches!(
            expand("{session}", &vars),
            Err(Error::EmptyTerminalTemplate)
        ));
    }

    #[test]
    fn preview_quotes_for_human_reading_only() {
        let vars = TemplateVars {
            worktree: "/home/u/my repo".into(),
            ..vars()
        };
        let inv = expand("term -c {worktree}", &vars).expect("expands");
        assert_eq!(preview(&inv), "term -c '/home/u/my repo'");
    }

    #[test]
    fn detection_follows_the_documented_preference_order() {
        let picked = detect_with(|p| p == "alacritty" || p == "kitty").expect("found");
        assert!(picked.starts_with("alacritty "));
        let picked = detect_with(|p| p == "ptyxis" || p == "foot").expect("found");
        assert!(picked.starts_with("ptyxis "));
    }

    #[test]
    fn detection_reports_what_it_tried() {
        let err = detect_with(|_| false).expect_err("nothing on PATH");
        let message = err.to_string();
        for (program, _) in CANDIDATES {
            assert!(
                message.contains(program),
                "{message} should mention {program}"
            );
        }
    }

    #[test]
    fn template_vars_render_paths() {
        let vars = TemplateVars::new(
            &PathBuf::from("/s"),
            "wt-000000",
            &PathBuf::from("/w"),
            "p",
            "b",
        );
        assert_eq!(vars.socket, "/s");
        assert_eq!(vars.worktree, "/w");
    }
}
