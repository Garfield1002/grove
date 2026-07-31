//! Claude Code integration: its hook payloads, and its `settings.json`.
//!
//! Grove's side of this is deliberately small. Claude Code runs a command on
//! each of a handful of events and hands it a JSON object on stdin; Grove's
//! command is `grove notify --hook`, which turns that object into the report
//! it would otherwise have to be told in flags. Nothing here reads a
//! transcript, watches a process or infers a state from output — the agent
//! says what is happening, and this only translates it.
//!
//! Both halves are pure string-to-string transformations so they can be tested
//! without a Claude Code installation:
//!
//! - [`HookPayload`] parses one event.
//! - [`install`] / [`uninstall`] rewrite a `settings.json` document.
//!
//! The settings file belongs to the user, exactly as `config.toml` does: hooks
//! they added themselves survive, ours are replaced rather than duplicated on
//! a second install, and the caller keeps a backup.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use grove_core::error::{Error, Result};
use grove_core::status::SessionStatus;

/// The command Claude Code is configured to run. It reads the payload itself,
/// so every event installs the same line.
pub const HOOK_COMMAND: &str = "grove notify --hook";

/// How an installed entry is recognised again — ours to replace, or ours to
/// remove. Matching on the prefix rather than the whole line means a user who
/// added their own flags keeps them through a reinstall of the others.
const HOOK_PREFIX: &str = "grove notify";

/// The events Grove asks for, and why each one earns a hook.
///
/// `Notification` is the one Grove cannot do without: it is the only signal
/// that says the user is *needed*, and CLAUDE.md forbids inferring that from
/// process names or output. The rest are cheap refinements of a status the
/// poller would reach a few seconds later anyway — except `SessionStart`,
/// which is where the conversation id first becomes knowable.
pub const HOOK_EVENTS: &[&str] = &[
    "Notification",
    "UserPromptSubmit",
    "Stop",
    "SessionStart",
    "SessionEnd",
];

/// The longest prompt summary Grove puts on a row. The wire protocol clamps
/// again at its own limit; this one exists so a long prompt does not push the
/// useful part of the line off the end of a narrow window.
const MAX_SUMMARY_LEN: usize = 120;

/// One hook event, as Claude Code delivers it.
///
/// Every field is optional: the payload's shape varies by event and grows
/// between releases, so an absent field means "this event did not say", never
/// an error. An event Grove does not recognise yields no state at all and the
/// hook does nothing, which is what keeps a future Claude Code from making
/// `grove notify` fail inside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookPayload {
    pub event: String,
    /// `Notification`: what Claude is asking for.
    pub message: Option<String>,
    /// `UserPromptSubmit`: what the user just asked for.
    pub prompt: Option<String>,
    /// The conversation id, on every event that carries one.
    pub session_id: Option<String>,
    /// Where Claude Code keeps this conversation's transcript.
    pub transcript_path: Option<String>,
}

impl HookPayload {
    /// Parse one payload. `None` when the input is not a JSON object — which
    /// is what running the hook by hand in a terminal produces, and is not
    /// worth failing an agent over.
    pub fn parse(input: &str) -> Option<Self> {
        let Value::Object(object) = serde_json::from_str::<Value>(input).ok()? else {
            return None;
        };
        let string = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Some(Self {
            event: string("hook_event_name").unwrap_or_default(),
            message: string("message"),
            prompt: string("prompt"),
            session_id: string("session_id"),
            transcript_path: string("transcript_path"),
        })
    }

    /// The status this event reports, if it reports one.
    ///
    /// `None` covers both an event Grove has no opinion about and one it has
    /// never heard of. Both mean the same thing to the caller: say nothing.
    /// Guessing at an unknown event would be the one way a hook could make a
    /// row lie.
    pub fn state(&self) -> Option<SessionStatus> {
        match self.event.as_str() {
            // The only event that means the user is actually needed.
            "Notification" => Some(SessionStatus::Attention),
            // A turn beginning, and the compaction that can interrupt one.
            "UserPromptSubmit" | "PreCompact" => Some(SessionStatus::Working),
            // A turn ending, which is the one thing the poller could never
            // work out: it sees a quiet session, and a quiet session that just
            // finished looks exactly like one that never began. Claude says
            // which, so the row can say "done" rather than the "idle" this
            // reported for as long as there was no truer word for it.
            //
            // It does not clear a raised attention. An agent that asked a
            // question and then ended its turn is still waiting for the answer,
            // and the latch outranks this by design.
            "Stop" | "SessionEnd" => Some(SessionStatus::Done),
            // Claude has attached to this worktree but is not doing anything
            // yet. Worth reporting for the conversation id it carries.
            "SessionStart" => Some(SessionStatus::Idle),
            _ => None,
        }
    }

    /// The one-line summary to show beside the status, if this event has
    /// something to say that the status does not.
    pub fn summary(&self) -> Option<String> {
        match self.event.as_str() {
            "Notification" => self.message.as_deref().map(summarize),
            "UserPromptSubmit" => self.prompt.as_deref().map(summarize),
            "PreCompact" => Some("compacting the transcript".to_string()),
            _ => None,
        }
    }
}

/// First line, clamped: a prompt is often several paragraphs, and a row has
/// one line.
fn summarize(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() <= MAX_SUMMARY_LEN {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_SUMMARY_LEN - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Why a `settings.json` could not be rewritten.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("settings.json is not valid JSON: {0}")]
    Invalid(String),
    #[error("settings.json holds {0} where an object was expected")]
    NotAnObject(&'static str),
}

/// Add Grove's hooks to a `settings.json` document, returning the new text.
///
/// An empty document is an empty object: a first install must work on a
/// machine that has never had a `settings.json`. Everything else in the file
/// is preserved, including hooks for the same events that are not Grove's, and
/// installing twice leaves exactly one Grove entry per event.
pub fn install(document: &str) -> std::result::Result<String, SettingsError> {
    let mut root = parse_document(document)?;
    let hooks = table(&mut root, "hooks")?;
    for event in HOOK_EVENTS {
        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Value::Array(list) = entries else {
            return Err(SettingsError::NotAnObject("a hook event"));
        };
        list.retain(|entry| !is_groves(entry));
        list.push(grove_entry());
    }
    render(&root)
}

/// Remove Grove's hooks again, leaving everything else — including an event
/// that has other hooks on it — exactly as it was.
pub fn uninstall(document: &str) -> std::result::Result<String, SettingsError> {
    let mut root = parse_document(document)?;
    let Some(Value::Object(hooks)) = root.get_mut("hooks") else {
        return render(&root);
    };
    for entries in hooks.values_mut() {
        if let Value::Array(list) = entries {
            list.retain(|entry| !is_groves(entry));
        }
    }
    // An event left with no hooks at all is noise in the user's file; so is a
    // `hooks` table with no events.
    hooks.retain(|_, entries| !matches!(entries, Value::Array(list) if list.is_empty()));
    if hooks.is_empty() {
        root.remove("hooks");
    }
    render(&root)
}

/// Which of Grove's events this document already has a hook for.
pub fn installed_events(document: &str) -> Vec<&'static str> {
    let Ok(root) = parse_document(document) else {
        return Vec::new();
    };
    let Some(Value::Object(hooks)) = root.get("hooks") else {
        return Vec::new();
    };
    HOOK_EVENTS
        .iter()
        .copied()
        .filter(|event| match hooks.get(*event) {
            Some(Value::Array(list)) => list.iter().any(is_groves),
            _ => false,
        })
        .collect()
}

/// Is Grove fully installed in this document?
pub fn is_installed(document: &str) -> bool {
    installed_events(document).len() == HOOK_EVENTS.len()
}

fn parse_document(document: &str) -> std::result::Result<Map<String, Value>, SettingsError> {
    if document.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(document) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(SettingsError::NotAnObject("a value")),
        Err(e) => Err(SettingsError::Invalid(e.to_string())),
    }
}

fn table<'a>(
    root: &'a mut Map<String, Value>,
    name: &'static str,
) -> std::result::Result<&'a mut Map<String, Value>, SettingsError> {
    match root
        .entry(name.to_string())
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(object) => Ok(object),
        _ => Err(SettingsError::NotAnObject(name)),
    }
}

/// One hook group as Claude Code expects it.
fn grove_entry() -> Value {
    json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": HOOK_COMMAND }],
    })
}

/// Is this hook group Grove's?
///
/// A group counts as Grove's only when every command in it is one of ours, so
/// a group where the user has added a command of their own beside Grove's is
/// left alone rather than silently taken away from them.
fn is_groves(entry: &Value) -> bool {
    let Some(Value::Array(hooks)) = entry.get("hooks") else {
        return false;
    };
    !hooks.is_empty()
        && hooks.iter().all(|hook| {
            hook.get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.trim_start().starts_with(HOOK_PREFIX))
        })
}

fn render(root: &Map<String, Value>) -> std::result::Result<String, SettingsError> {
    let text =
        serde_json::to_string_pretty(root).map_err(|e| SettingsError::Invalid(e.to_string()))?;
    Ok(format!("{text}\n"))
}

// ------------------------------------------------------------------- files

/// Where Claude Code keeps its user-level settings: `$CLAUDE_CONFIG_DIR`, or
/// `~/.claude`.
pub fn settings_path(config_dir: Option<&str>, home: &Path) -> PathBuf {
    let dir = config_dir
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    dir.join(SETTINGS_FILE)
}

/// File name of Claude Code's user settings.
pub const SETTINGS_FILE: &str = "settings.json";

/// The environment variable Claude Code uses to move its configuration
/// directory.
pub const CONFIG_DIR_ENV_VAR: &str = "CLAUDE_CONFIG_DIR";

/// Where Claude Code's settings live for the current process.
pub fn settings_path_from_env() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(Error::NoHomeDirectory(CONFIG_DIR_ENV_VAR))?;
    Ok(settings_path(
        std::env::var(CONFIG_DIR_ENV_VAR).ok().as_deref(),
        &home,
    ))
}

/// What a hook install or removal did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookChange {
    pub path: PathBuf,
    /// The copy taken before writing, when there was a file to copy.
    pub backup: Option<PathBuf>,
    /// False when the file already said what it was going to say. Nothing is
    /// written and no backup is taken in that case.
    pub changed: bool,
    /// Grove's events present in the file afterwards.
    pub installed: Vec<&'static str>,
}

impl HookChange {
    pub fn is_installed(&self) -> bool {
        self.installed.len() == HOOK_EVENTS.len()
    }
}

/// Add Grove's hooks to the settings file at `path`.
///
/// The user's file is backed up before it is replaced, the replacement is
/// atomic (temp file plus rename, as `state.toml` is written), and a file that
/// cannot be parsed is reported rather than overwritten.
pub fn install_hooks(path: &Path) -> Result<HookChange> {
    rewrite(path, install)
}

/// Remove Grove's hooks from the settings file at `path`.
pub fn uninstall_hooks(path: &Path) -> Result<HookChange> {
    rewrite(path, uninstall)
}

/// Which of Grove's hooks the settings file currently has. A missing file has
/// none, which is a normal state and not an error.
pub fn hook_status(path: &Path) -> Result<HookChange> {
    let current = read_settings(path)?;
    Ok(HookChange {
        path: path.to_path_buf(),
        backup: None,
        changed: false,
        installed: installed_events(&current),
    })
}

fn rewrite(
    path: &Path,
    transform: fn(&str) -> std::result::Result<String, SettingsError>,
) -> Result<HookChange> {
    let current = read_settings(path)?;
    let updated = transform(&current).map_err(|source| {
        Error::integration(format!("could not update {}", path.display()), source)
    })?;
    let installed = installed_events(&updated);
    if updated == current {
        return Ok(HookChange {
            path: path.to_path_buf(),
            backup: None,
            changed: false,
            installed,
        });
    }
    let backup = back_up(path, &current)?;
    grove_core::atomic::write(path, &updated)?;
    Ok(HookChange {
        path: path.to_path_buf(),
        backup,
        changed: true,
        installed,
    })
}

/// A missing settings file reads as an empty document: installing into a
/// machine that has never run Claude Code is a first install, not an error.
fn read_settings(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(Error::io(format!("could not read {}", path.display()), e)),
    }
}

/// Copy the file aside before replacing it. Named with the epoch second so a
/// second install cannot overwrite the copy taken by the first.
fn back_up(path: &Path, current: &str) -> Result<Option<PathBuf>> {
    if current.is_empty() && !path.exists() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{stamp}.bak"));
    let backup = path.with_file_name(name);
    std::fs::write(&backup, current)
        .map_err(|e| Error::io(format!("could not write {}", backup.display()), e))?;
    Ok(Some(backup))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- payloads

    fn payload(event: &str, rest: &str) -> HookPayload {
        HookPayload::parse(&format!(
            "{{\"hook_event_name\": \"{event}\", \"session_id\": \"0f3a\"{rest}}}"
        ))
        .expect("valid payload")
    }

    #[test]
    fn a_notification_is_the_only_event_that_raises_attention() {
        let notification = payload("Notification", ", \"message\": \"Claude needs permission\"");
        assert_eq!(notification.state(), Some(SessionStatus::Attention));
        assert_eq!(
            notification.summary().as_deref(),
            Some("Claude needs permission")
        );
        for quiet in ["Stop", "SessionStart", "SessionEnd", "UserPromptSubmit"] {
            assert_ne!(
                payload(quiet, "").state(),
                Some(SessionStatus::Attention),
                "{quiet} must not raise attention"
            );
        }
    }

    #[test]
    fn a_turn_beginning_is_working_and_a_turn_ending_is_done() {
        assert_eq!(
            payload("UserPromptSubmit", "").state(),
            Some(SessionStatus::Working)
        );
        // The distinction the poller cannot draw: both of these leave a quiet
        // session, and only Claude can say the quiet means "finished".
        assert_eq!(payload("Stop", "").state(), Some(SessionStatus::Done));
        assert_eq!(payload("SessionEnd", "").state(), Some(SessionStatus::Done));
        assert_eq!(
            payload("SessionStart", "").state(),
            Some(SessionStatus::Idle),
            "attached, but nothing has run yet — that is not finished"
        );
    }

    /// A future Claude Code will send events this Grove has never heard of.
    /// Reporting nothing is the only safe reading of one.
    #[test]
    fn an_unknown_event_reports_nothing() {
        assert_eq!(payload("SomethingNew", "").state(), None);
        assert_eq!(payload("SomethingNew", "").summary(), None);
        assert_eq!(payload("", "").state(), None);
    }

    #[test]
    fn the_prompt_becomes_the_working_line() {
        let submitted = payload("UserPromptSubmit", ", \"prompt\": \"fix the parser\"");
        assert_eq!(submitted.summary().as_deref(), Some("fix the parser"));
    }

    /// Prompts are multi-line and long; a row is one line and narrow.
    #[test]
    fn a_long_or_multiline_prompt_is_summarised() {
        assert_eq!(summarize("\n\nfirst line\nsecond line"), "first line");
        let long = "x".repeat(MAX_SUMMARY_LEN + 40);
        let short = summarize(&long);
        assert_eq!(short.chars().count(), MAX_SUMMARY_LEN);
        assert!(short.ends_with('…'));
        // A prompt that fits is left exactly as written.
        assert_eq!(summarize("  fix the parser  "), "fix the parser");
    }

    #[test]
    fn the_conversation_id_and_transcript_are_read_when_present() {
        let full = payload(
            "SessionStart",
            ", \"transcript_path\": \"/home/u/.claude/projects/x/0f3a.jsonl\"",
        );
        assert_eq!(full.session_id.as_deref(), Some("0f3a"));
        assert_eq!(
            full.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/x/0f3a.jsonl")
        );
        // And an event without them is not an error.
        let bare = HookPayload::parse("{\"hook_event_name\": \"Stop\"}").expect("valid");
        assert_eq!(bare.session_id, None);
        assert_eq!(bare.transcript_path, None);
    }

    #[test]
    fn empty_strings_are_the_same_as_absent_fields() {
        let payload = HookPayload::parse(
            "{\"hook_event_name\": \"Notification\", \"message\": \"  \", \"session_id\": \"\"}",
        )
        .expect("valid");
        assert_eq!(payload.message, None);
        assert_eq!(payload.session_id, None);
        assert_eq!(payload.summary(), None);
    }

    /// Run by hand, the hook gets a terminal rather than a payload. That must
    /// be a quiet no-op, not a failure inside someone's agent.
    #[test]
    fn input_that_is_not_a_payload_is_not_a_payload() {
        assert_eq!(HookPayload::parse(""), None);
        assert_eq!(HookPayload::parse("not json"), None);
        assert_eq!(HookPayload::parse("[1, 2, 3]"), None);
        assert_eq!(HookPayload::parse("\"a string\""), None);
    }

    /// Fields of the wrong type are absent fields, not errors.
    #[test]
    fn a_field_of_the_wrong_type_is_ignored() {
        let payload =
            HookPayload::parse("{\"hook_event_name\": \"Notification\", \"message\": 42}")
                .expect("valid object");
        assert_eq!(payload.state(), Some(SessionStatus::Attention));
        assert_eq!(payload.message, None);
    }

    // ------------------------------------------------------------- settings

    fn commands(document: &str, event: &str) -> Vec<String> {
        let root: Value = serde_json::from_str(document).expect("valid json");
        root["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["hooks"].as_array().into_iter().flatten())
            .filter_map(|hook| hook["command"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn installing_into_nothing_writes_every_event() {
        let document = install("").expect("installs");
        assert!(is_installed(&document));
        for event in HOOK_EVENTS {
            assert_eq!(commands(&document, event), vec![HOOK_COMMAND.to_string()]);
        }
    }

    #[test]
    fn installing_into_an_empty_object_is_the_same() {
        assert_eq!(install("{}").expect("installs"), install("").expect("ok"));
    }

    /// The file is the user's. Their keys, and their hooks on the same events,
    /// come through untouched.
    #[test]
    fn installing_preserves_everything_else() {
        let before = r#"{
          "model": "opus",
          "hooks": {
            "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "say done"}]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "audit"}]}]
          }
        }"#;
        let after = install(before).expect("installs");
        let root: Value = serde_json::from_str(&after).expect("valid json");
        assert_eq!(root["model"], "opus");
        assert_eq!(commands(&after, "PreToolUse"), vec!["audit".to_string()]);
        assert_eq!(
            commands(&after, "Stop"),
            vec!["say done".to_string(), HOOK_COMMAND.to_string()],
            "the user's hook stays, and Grove's is added after it"
        );
    }

    #[test]
    fn installing_twice_leaves_one_entry_per_event() {
        let once = install("").expect("installs");
        let twice = install(&once).expect("installs again");
        assert_eq!(once, twice);
        for event in HOOK_EVENTS {
            assert_eq!(commands(&twice, event).len(), 1, "{event}");
        }
    }

    /// An older Grove installed a per-event flag rather than `--hook`. A
    /// reinstall must replace it, not sit beside it reporting twice.
    #[test]
    fn installing_replaces_an_older_grove_entry() {
        let older = r#"{"hooks": {"Notification": [{"matcher": "", "hooks": [
            {"type": "command", "command": "grove notify --state attention"}]}]}}"#;
        let after = install(older).expect("installs");
        assert_eq!(
            commands(&after, "Notification"),
            vec![HOOK_COMMAND.to_string()]
        );
    }

    #[test]
    fn uninstalling_removes_only_groves_hooks() {
        let before = r#"{
          "model": "opus",
          "hooks": {
            "Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "say done"}]}]
          }
        }"#;
        let installed = install(before).expect("installs");
        let after = uninstall(&installed).expect("uninstalls");
        let root: Value = serde_json::from_str(&after).expect("valid json");
        assert_eq!(root["model"], "opus");
        assert_eq!(commands(&after, "Stop"), vec!["say done".to_string()]);
        assert!(!is_installed(&after));
        assert!(installed_events(&after).is_empty());
    }

    /// Uninstalling from a file Grove never touched leaves it alone, and does
    /// not leave empty tables behind in a file it did.
    #[test]
    fn uninstalling_tidies_up_after_itself() {
        assert_eq!(
            uninstall("{\"model\": \"opus\"}").expect("ok"),
            "{\n  \"model\": \"opus\"\n}\n"
        );
        let installed = install("").expect("installs");
        let after = uninstall(&installed).expect("uninstalls");
        let root: Value = serde_json::from_str(&after).expect("valid json");
        assert!(root.get("hooks").is_none(), "no empty hooks table: {after}");
    }

    /// A group holding one of Grove's commands and one of the user's belongs
    /// to the user; taking it away would take their command with it.
    #[test]
    fn a_shared_group_is_left_alone() {
        let shared = r#"{"hooks": {"Stop": [{"matcher": "", "hooks": [
            {"type": "command", "command": "grove notify --hook"},
            {"type": "command", "command": "say done"}]}]}}"#;
        let after = uninstall(shared).expect("uninstalls");
        assert_eq!(commands(&after, "Stop").len(), 2, "{after}");
    }

    #[test]
    fn a_partial_installation_is_not_an_installation() {
        let partial = r#"{"hooks": {"Notification": [{"matcher": "", "hooks": [
            {"type": "command", "command": "grove notify --hook"}]}]}}"#;
        assert!(!is_installed(partial));
        assert_eq!(installed_events(partial), vec!["Notification"]);
        // And installing over it completes the set rather than duplicating.
        let after = install(partial).expect("installs");
        assert!(is_installed(&after));
        assert_eq!(commands(&after, "Notification").len(), 1);
    }

    /// A settings file Grove cannot understand is never overwritten: the
    /// caller reports the problem and leaves the user's file alone.
    #[test]
    fn a_broken_settings_file_is_refused_not_replaced() {
        assert!(matches!(
            install("{ not json"),
            Err(SettingsError::Invalid(_))
        ));
        assert!(matches!(
            install("[]"),
            Err(SettingsError::NotAnObject("a value"))
        ));
        assert!(matches!(
            install("{\"hooks\": \"none please\"}"),
            Err(SettingsError::NotAnObject("hooks"))
        ));
        assert!(matches!(
            install("{\"hooks\": {\"Stop\": \"nope\"}}"),
            Err(SettingsError::NotAnObject("a hook event"))
        ));
        // And a file that cannot be read reports nothing installed rather
        // than claiming it is.
        assert!(!is_installed("{ not json"));
    }

    // ---------------------------------------------------------------- files

    #[test]
    fn the_settings_path_follows_the_environment_then_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            settings_path(None, home),
            PathBuf::from("/home/u/.claude/settings.json")
        );
        assert_eq!(
            settings_path(Some("/opt/claude"), home),
            PathBuf::from("/opt/claude/settings.json")
        );
        // An empty variable is an unset one.
        assert_eq!(settings_path(Some("  "), home), settings_path(None, home));
    }

    #[test]
    fn installing_creates_a_settings_file_that_was_never_there() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let change = install_hooks(&path).expect("installs");
        assert!(change.changed);
        assert!(change.is_installed());
        assert_eq!(change.backup, None, "there was nothing to back up");
        assert!(is_installed(
            &std::fs::read_to_string(&path).expect("written")
        ));
    }

    #[test]
    fn installing_over_a_file_backs_it_up_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"model\": \"opus\"}\n").expect("write");

        let change = install_hooks(&path).expect("installs");
        let backup = change.backup.expect("a backup was taken");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("readable"),
            "{\"model\": \"opus\"}\n",
            "the backup is the file exactly as it was"
        );
        assert!(is_installed(
            &std::fs::read_to_string(&path).expect("written")
        ));
    }

    /// Nothing to do means nothing written — and so no backup either, which
    /// keeps a Settings pane that checks on open from littering the directory.
    #[test]
    fn installing_twice_writes_nothing_the_second_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        install_hooks(&path).expect("installs");
        let again = install_hooks(&path).expect("installs again");
        assert!(!again.changed);
        assert!(again.is_installed());
        assert_eq!(again.backup, None);
    }

    #[test]
    fn uninstalling_reports_what_is_left_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        install_hooks(&path).expect("installs");

        let removed = uninstall_hooks(&path).expect("uninstalls");
        assert!(removed.changed);
        assert!(removed.installed.is_empty());
        assert!(removed.backup.is_some());

        let again = uninstall_hooks(&path).expect("uninstalls again");
        assert!(!again.changed);
    }

    #[test]
    fn the_status_of_a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = hook_status(&dir.path().join("nothing.json")).expect("reads");
        assert!(status.installed.is_empty());
        assert!(!status.is_installed());
    }

    /// The user's file is the last thing to overwrite when Grove cannot make
    /// sense of it.
    #[test]
    fn a_settings_file_that_cannot_be_parsed_is_left_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ this is not json").expect("write");
        let err = install_hooks(&path).expect_err("refused");
        assert!(matches!(err, Error::Integration { .. }));
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            "{ this is not json"
        );
    }

    #[test]
    fn the_document_ends_with_a_newline() {
        assert!(install("").expect("installs").ends_with("}\n"));
        assert!(uninstall("{\"a\": 1}").expect("ok").ends_with("}\n"));
    }
}
