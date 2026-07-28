//! Grove: a native GUI for Git projects, worktrees and tmux sessions.
//!
//! `grove` launches the GUI; `grove notify` reports an agent's status to it.

mod app;
mod hooks;
mod notify;
mod query;
mod service;
mod status_watch;
mod toggle;
mod ui;
mod workers;

use grove_core::Paths;

const USAGE: &str = "\
grove — Git worktree and tmux session manager

Usage:
  grove            launch the GUI
  grove toggle     start or close Grove, or open a numbered worktree
                   (see `grove toggle --help`)
  grove notify     report a session's status (see `grove notify --help`)
  grove hooks      install Grove's hooks into Claude Code's settings
                   (see `grove hooks --help`)
  grove project list   list registered projects as JSON
  grove worktree list  list current Git worktrees as JSON
  grove session list   list live Grove tmux sessions as JSON
  grove snapshot       print one coherent service snapshot as JSON
  grove serve       run Grove's headless local service
  grove --help     show this message
  grove --version  show the version
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // What a `grove toggle` that found no running GUI asks this process to
    // open once the GUI is up. `None` for a plain `grove`.
    let mut pending_toggle = None;
    match args.first().map(String::as_str) {
        None => {}
        Some("notify") => {
            notify::run(&args[1..])?;
            return Ok(());
        }
        Some("hooks") => {
            hooks::run(&args[1..])?;
            return Ok(());
        }
        Some("project" | "worktree" | "session" | "snapshot") => {
            let paths = Paths::from_process_env()?;
            query::run(&args, &paths)?;
            return Ok(());
        }
        Some("serve") => {
            let paths = Paths::from_process_env()?;
            service::run(&args[1..], &paths)?;
            return Ok(());
        }
        // Falls through to the GUI when nothing was listening: starting Grove
        // is the other half of the toggle.
        Some("toggle") => match toggle::run(&args[1..])? {
            toggle::Next::Done => return Ok(()),
            toggle::Next::LaunchGui { slot } => pending_toggle = slot,
        },
        Some("-h" | "--help") => {
            print!("{USAGE}");
            return Ok(());
        }
        Some("-V" | "--version") => {
            println!("grove {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(other) => {
            eprintln!("grove: unknown argument `{other}`\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }

    let paths = Paths::from_process_env()?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Grove")
            .with_app_id("grove")
            // The main window wears the compositor's title bar: moving,
            // resizing and closing then work the way they do everywhere else
            // on the desktop, and Grove's own header is left to the filter
            // field alone. Detached dialogs still draw their own (see
            // `ui::chrome`).
            .with_decorations(true)
            .with_resizable(true)
            .with_inner_size(ui::theme::WINDOW_SIZE)
            .with_min_inner_size(ui::theme::MIN_WINDOW_SIZE),
        ..Default::default()
    };

    eframe::run_native(
        "Grove",
        options,
        Box::new(move |cc| Ok(Box::new(app::GroveApp::new(cc, paths, pending_toggle)))),
    )?;
    Ok(())
}
