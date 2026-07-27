//! Grove: a native GUI for Git projects, worktrees and tmux sessions.
//!
//! `grove` launches the GUI. The `notify` subcommand reserved in
//! ARCHITECTURE.md §1 arrives with the agent workflow in Milestone 4.

mod app;
mod ui;
mod workers;

use grove_core::Paths;

const USAGE: &str = "\
grove — Git worktree and tmux session manager

Usage:
  grove            launch the GUI
  grove --help     show this message
  grove --version  show the version
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {}
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
            .with_inner_size(ui::theme::WINDOW_SIZE)
            .with_min_inner_size(ui::theme::MIN_WINDOW_SIZE),
        ..Default::default()
    };

    eframe::run_native(
        "Grove",
        options,
        Box::new(move |cc| Ok(Box::new(app::GroveApp::new(cc, paths)))),
    )?;
    Ok(())
}
