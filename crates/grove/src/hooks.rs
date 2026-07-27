//! `grove hooks` — install Grove's hooks into Claude Code's `settings.json`.
//!
//! This is the other half of `grove notify --hook`: the hooks it writes all
//! run that one command, which reads the event on stdin and reports it. Doing
//! it here rather than in a shell recipe means it works wherever the binary
//! does — including a `cargo install` with no checkout to run `just` in — and
//! that the merge is the same tested code the Settings pane uses.
//!
//! `settings.json` is the user's file. Their own hooks survive an install and
//! a removal, a copy is taken before it is replaced, and a file Grove cannot
//! parse is reported rather than overwritten.

use grove_core::claude::{self, HOOK_COMMAND, HOOK_EVENTS, HookChange};

pub const USAGE: &str = "\
grove hooks — Grove's hooks in Claude Code's settings

Usage:
  grove hooks status      show whether the hooks are installed
  grove hooks install     add them, backing the file up first
  grove hooks uninstall   remove them again
  grove hooks print       show what would be added, changing nothing

The hooks all run `grove notify --hook`, which reads the event Claude Code
sends it and reports the session's status, the agent's own message and the
conversation id to a running Grove.

Claude Code reads its settings at startup: restart it after installing.
";

/// Run `grove hooks`.
pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args.first().map(String::as_str) {
        Some("-h" | "--help") | Some("help") => {
            print!("{USAGE}");
            Ok(())
        }
        None | Some("status") => report(claude::hook_status(&path()?)?, "status"),
        Some("install") => report(claude::install_hooks(&path()?)?, "install"),
        Some("uninstall") => report(claude::uninstall_hooks(&path()?)?, "uninstall"),
        Some("print") => {
            print!("{}", claude::install("")?);
            Ok(())
        }
        Some(other) => {
            eprintln!("grove hooks: unknown command `{other}`\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn path() -> Result<std::path::PathBuf, grove_core::Error> {
    claude::settings_path_from_env()
}

/// Say what the file holds now, and what changed getting there.
fn report(change: HookChange, command: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", change.path.display());
    if let Some(backup) = &change.backup {
        println!("backed up to {}", backup.display());
    }
    match (command, change.changed) {
        ("install", true) => println!("installed `{HOOK_COMMAND}` on {}", events(&change)),
        ("install", false) => println!("already installed on {}", events(&change)),
        ("uninstall", true) => println!("removed Grove's hooks"),
        ("uninstall", false) => println!("nothing of Grove's was there"),
        _ if change.is_installed() => println!("installed on {}", events(&change)),
        _ if change.installed.is_empty() => {
            println!("not installed — run `grove hooks install`");
        }
        // A partial installation is worth naming both halves of: it is what an
        // interrupted install, or a hand-edited file, leaves behind.
        _ => println!(
            "partly installed: {} (missing {})",
            events(&change),
            missing(&change)
        ),
    }
    if change.changed && command == "install" {
        println!("restart Claude Code for them to take effect");
    }
    Ok(())
}

fn events(change: &HookChange) -> String {
    change.installed.join(", ")
}

fn missing(change: &HookChange) -> String {
    HOOK_EVENTS
        .iter()
        .filter(|event| !change.installed.contains(event))
        .copied()
        .collect::<Vec<_>>()
        .join(", ")
}
