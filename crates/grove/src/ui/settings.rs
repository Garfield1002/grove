//! A read-only settings pane.
//!
//! `config.toml` is user-owned (ARCHITECTURE.md §4): Grove shows what it read
//! and where the file lives, and the user edits it in their editor. An
//! in-app editor would mean rewriting a file Grove promised not to rewrite.

use egui::Context;
use grove_core::Paths;
use grove_core::config::Config;

use super::theme;

pub fn show(ctx: &Context, open: &mut bool, paths: &Paths, config: Option<&Config>) {
    egui::Window::new("Settings")
        .collapsible(false)
        .resizable(false)
        .open(open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(320.0);

            row(
                ui,
                "config.toml",
                &paths.config_file().display().to_string(),
            );
            row(ui, "state.toml", &paths.state_file().display().to_string());
            row(
                ui,
                "tmux socket",
                &paths.tmux_socket().display().to_string(),
            );

            ui.add_space(8.0);
            ui.label(theme::label("Terminal command", 11.0, theme::TEXT_DIM));
            let command = config
                .map(|c| c.terminal.command.clone())
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| "(none configured)".to_string());
            ui.add(egui::Label::new(theme::mono(command, 10.0, theme::TEXT_MUTED)).wrap());

            ui.add_space(6.0);
            ui.add(
                egui::Label::new(theme::label(
                    "Placeholders: {socket} {session} {worktree} {project} {branch}. \
                     Edit config.toml to change it; Grove only ever wrote it once, \
                     when it auto-detected your terminal.",
                    10.0,
                    theme::TEXT_FAINT,
                ))
                .wrap(),
            );
        });
}

fn row(ui: &mut egui::Ui, name: &str, value: &str) {
    ui.add_space(4.0);
    ui.label(theme::label(name, 11.0, theme::TEXT_DIM));
    ui.add(egui::Label::new(theme::mono(value, 10.0, theme::TEXT_MUTED)).wrap());
}
