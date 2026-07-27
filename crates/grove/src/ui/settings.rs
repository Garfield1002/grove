//! The settings pane, shown in its own OS window ([`crate::ui::chrome`]).
//!
//! `config.toml` stays user-owned (ARCHITECTURE.md §4): everything shown here
//! is also editable by hand, and saving goes through
//! [`grove_core::config_write`], which replaces one key at a time and leaves
//! the rest of the file — comments included — exactly as the user wrote it.
//! Nothing is written on a keystroke: edits live in [`Form`] until Save.
//!
//! Only keys that exist in [`Config`] today appear. A setting whose consumer
//! has not been built yet would be a lie, so agent commands, timeouts and the
//! rest arrive with their milestones.

use std::path::Path;

use grove_core::config::Config;
use grove_core::config_write::{self, Edit};
use grove_core::{Paths, terminal};

use super::{icons, theme};

/// Sample values used for the live command preview. Real socket, illustrative
/// worktree — the preview is never executed.
const SAMPLE_SESSION: &str = "wt-a1b2c3";
const SAMPLE_WORKTREE: &str = "/home/you/worktrees/acme-auth";
const SAMPLE_PROJECT: &str = "acme-web";
const SAMPLE_BRANCH: &str = "feature/auth";

/// What the pane is asking the app to do. Everything here needs the worker:
/// probing PATH, reading and writing files, and spawning `xdg-open` are all
/// off-limits on the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write the changed keys to `config.toml`.
    Save,
    /// Re-run terminal auto-detection and put the result in the field.
    DetectTerminal,
    /// Check whether this template's program exists on PATH.
    Probe(String),
    /// Open `config.toml` in the user's editor or file manager.
    OpenConfigFile,
    /// Pick the default worktree parent with the native directory picker.
    BrowseWorktreeParent,
}

/// The result of the worker's PATH probe, remembered with the exact template
/// it was run for so a stale answer is never shown as current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub command: String,
    pub program: String,
    pub found: bool,
}

/// Everything the pane holds between frames. A plain value with no egui in
/// it, so the save/dirty rules below are unit-tested.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Form {
    pub terminal_command: String,
    pub default_parent: String,
    /// The values as they are in `config.toml`, to tell what actually changed.
    loaded_terminal: String,
    loaded_parent: String,
    pub probe: Option<Probe>,
    /// A save is in flight; the button stays down until the worker answers.
    pub saving: bool,
    /// Confirmation of the last successful save.
    pub note: Option<String>,
}

impl Form {
    pub fn new(config: Option<&Config>) -> Self {
        let terminal = config
            .map(|c| c.terminal.command.clone())
            .unwrap_or_default();
        let parent = config
            .map(|c| c.worktrees.default_parent.clone())
            .unwrap_or_default();
        Self {
            terminal_command: terminal.clone(),
            default_parent: parent.clone(),
            loaded_terminal: terminal,
            loaded_parent: parent,
            probe: None,
            saving: false,
            note: None,
        }
    }

    pub fn is_dirty(&self) -> bool {
        !self.edits().is_empty()
    }

    /// The keys to write: only those the user actually changed. An untouched
    /// `default_parent` is never materialised into the file, so Grove's
    /// default does not turn into a recorded user choice.
    pub fn edits(&self) -> Vec<Edit> {
        let mut edits = Vec::new();
        if self.terminal_command != self.loaded_terminal {
            edits.push(Edit::string(
                config_write::TERMINAL_COMMAND,
                self.terminal_command.trim(),
            ));
        }
        if self.default_parent != self.loaded_parent {
            edits.push(Edit::string(
                config_write::WORKTREES_DEFAULT_PARENT,
                self.default_parent.trim(),
            ));
        }
        edits
    }

    /// Why the form cannot be saved yet, if it cannot.
    pub fn problem(&self) -> Option<String> {
        match terminal::tokenize(&self.terminal_command) {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        }
    }

    /// Adopt the file's values as the new baseline after a successful save or
    /// a reload.
    pub fn reloaded(&mut self, config: &Config) {
        self.loaded_terminal = config.terminal.command.clone();
        self.loaded_parent = config.worktrees.default_parent.clone();
        if !self.saving {
            return;
        }
        // The save that is landing: adopt what was written, so the fields do
        // not jump if the file normalised anything.
        self.terminal_command = self.loaded_terminal.clone();
        self.default_parent = self.loaded_parent.clone();
        self.saving = false;
    }

    /// The probe answer, if it belongs to what is in the field right now.
    pub fn current_probe(&self) -> Option<&Probe> {
        self.probe
            .as_ref()
            .filter(|probe| probe.command == self.terminal_command)
    }
}

/// The expanded command, exactly as [`terminal::expand`] would build it, with
/// sample values. Tokenised first and substituted after, so this preview is
/// also a demonstration that a path can never split into extra arguments.
pub fn preview(command: &str, socket: &Path) -> Result<String, String> {
    let vars = terminal::TemplateVars::new(
        socket,
        SAMPLE_SESSION,
        Path::new(SAMPLE_WORKTREE),
        SAMPLE_PROJECT,
        SAMPLE_BRANCH,
    );
    terminal::expand(command, &vars)
        .map(|invocation| terminal::preview(&invocation))
        .map_err(|e| e.to_string())
}

/// Default inner size of the Settings window. Tall enough for both settings,
/// the command preview and the paths block without scrolling, and wide enough
/// for the label column plus a readable command line.
pub const SIZE: [f32; 2] = [560.0, 600.0];
/// Floor for the Settings window: the label column plus a usable field.
pub const MIN_SIZE: [f32; 2] = [420.0, 320.0];

/// Width of the left-hand label column. The pane is laid out as label/field
/// rows, which is what the detached window's width buys over the sliver.
const LABEL_COLUMN: f32 = 150.0;

/// The pane's contents. The window around it — chrome, sizing, scrolling — is
/// [`crate::ui::chrome`]'s.
pub fn body(
    ui: &mut egui::Ui,
    form: &mut Form,
    paths: &Paths,
    home: Option<&Path>,
) -> Option<Action> {
    let mut action = None;
    let fields = (ui.available_width() - LABEL_COLUMN - 12.0).max(200.0);

    egui::Grid::new("grove-settings-fields")
        .num_columns(2)
        .min_col_width(LABEL_COLUMN)
        .spacing([12.0, 14.0])
        .show(ui, |ui| {
            // -------------------------------------------------------- terminal
            ui.label(theme::caption("Terminal command"));
            ui.vertical(|ui| {
                ui.set_width(fields);
                ui.horizontal(|ui| {
                    let width = (fields - theme::ICON_BUTTON - 8.0).max(120.0);
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut form.terminal_command)
                                .font(egui::FontId::monospace(theme::FONT_SMALL))
                                .hint_text("foot tmux -S {socket} attach-session -t {session}")
                                .desired_width(width),
                        )
                        .changed()
                    {
                        form.note = None;
                        action = Some(Action::Probe(form.terminal_command.clone()));
                    }
                    if icons::button(ui, true, icons::refresh)
                        .on_hover_text("Detect the terminal again and put it in the field")
                        .clicked()
                    {
                        action = Some(Action::DetectTerminal);
                    }
                });

                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(theme::label(
                        "Split with shell quoting rules first, then the placeholders are \
                         substituted into whole arguments: {socket} {session} {worktree} \
                         {project} {branch}.",
                        theme::FONT_SMALL,
                        theme::TEXT_FAINT,
                    ))
                    .wrap(),
                );

                ui.add_space(8.0);
                match preview(&form.terminal_command, &paths.tmux_socket()) {
                    Ok(preview) => {
                        ui.label(theme::caption("Runs"));
                        ui.add_space(2.0);
                        code_block(ui, &preview);
                        ui.add_space(6.0);
                        probe_line(ui, form);
                    }
                    Err(problem) => {
                        ui.add_space(2.0);
                        bullet(ui, icons::cross, theme::DANGER, &problem);
                    }
                }
            });
            ui.end_row();

            // ------------------------------------------------------- worktrees
            ui.label(theme::caption("Default worktree parent"));
            ui.vertical(|ui| {
                ui.set_width(fields);
                ui.horizontal(|ui| {
                    let browse = super::NATIVE_FILE_PICKER;
                    let reserved = if browse {
                        theme::ICON_BUTTON + 8.0
                    } else {
                        0.0
                    };
                    let width = (fields - reserved).max(120.0);
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut form.default_parent)
                                .font(egui::FontId::monospace(theme::FONT_SMALL))
                                .hint_text(hint_parent(home))
                                .desired_width(width),
                        )
                        .changed()
                    {
                        form.note = None;
                    }
                    if browse
                        && icons::button(ui, true, icons::folder)
                            .on_hover_text("Choose a directory")
                            .clicked()
                    {
                        action = Some(Action::BrowseWorktreeParent);
                    }
                });
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(theme::label(
                        "Where new worktrees are suggested. Empty means beside the \
                         repository; the create dialog always lets you edit the path.",
                        theme::FONT_SMALL,
                        theme::TEXT_FAINT,
                    ))
                    .wrap(),
                );
            });
            ui.end_row();

            // ------------------------------------------------------------ save
            ui.label("");
            ui.horizontal(|ui| {
                let problem = form.problem();
                let can_save = form.is_dirty() && problem.is_none() && !form.saving;
                let save = egui::Button::new(theme::label(
                    "Save",
                    theme::FONT_BODY,
                    if can_save {
                        theme::TEXT_STRONG
                    } else {
                        theme::TEXT_FAINT
                    },
                ))
                .fill(theme::ACCENT_FILL)
                .stroke(egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)));
                if ui.add_enabled(can_save, save).clicked() {
                    form.saving = true;
                    form.note = None;
                    action = Some(Action::Save);
                }

                let status = match (&problem, form.saving, form.is_dirty(), &form.note) {
                    (Some(problem), ..) => (problem.clone(), theme::DANGER),
                    (_, true, ..) => ("Saving…".to_string(), theme::TEXT_MUTED),
                    (_, _, true, _) => ("Unsaved changes".to_string(), theme::WARNING),
                    (_, _, _, Some(note)) => (note.clone(), theme::TEXT_MUTED),
                    _ => (
                        "Edits are written key by key; your comments stay.".to_string(),
                        theme::TEXT_FAINT,
                    ),
                };
                ui.add(
                    egui::Label::new(theme::label(status.0, theme::FONT_SMALL, status.1))
                        .truncate(),
                );
            });
            ui.end_row();
        });

    // ------------------------------------------------------------------ paths
    ui.add_space(14.0);
    ui.separator();
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        ui.label(theme::caption("Files"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(theme::label("Open", theme::FONT_SMALL, theme::TEXT_MUTED))
                .on_hover_text("Open config.toml with xdg-open")
                .clicked()
            {
                action = Some(Action::OpenConfigFile);
            }
            if ui
                .button(theme::label(
                    "Copy path",
                    theme::FONT_SMALL,
                    theme::TEXT_MUTED,
                ))
                .on_hover_text("Copy the path to config.toml")
                .clicked()
            {
                ui.ctx()
                    .copy_text(paths.config_file().display().to_string());
                form.note = Some("Copied the path.".to_string());
            }
        });
    });
    ui.add_space(4.0);
    egui::Grid::new("grove-settings-paths")
        .num_columns(2)
        .min_col_width(LABEL_COLUMN)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (name, path) in [
                ("config.toml", paths.config_file()),
                ("state.toml", paths.state_file()),
                ("tmux.conf", paths.tmux_config_file()),
                ("tmux socket", paths.tmux_socket()),
            ] {
                ui.label(theme::caption(name));
                path_line(ui, &path);
                ui.end_row();
            }
        });

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.add(
        egui::Label::new(theme::label(
            "Ctrl+N new worktree · Ctrl+R refresh · Enter open · Delete remove · \
             Esc or Ctrl+W close this window · Ctrl+Q quit Grove",
            theme::FONT_SMALL,
            theme::TEXT_FAINT,
        ))
        .wrap(),
    );

    action
}

/// Valid/invalid indicator for the template's executable.
fn probe_line(ui: &mut egui::Ui, form: &Form) {
    match form.current_probe() {
        Some(probe) if probe.found => bullet(
            ui,
            icons::check,
            theme::STATUS_WORKING,
            &format!("{} is on PATH", probe.program),
        ),
        Some(probe) => bullet(
            ui,
            icons::cross,
            theme::DANGER,
            &format!("{} was not found on PATH", probe.program),
        ),
        None => bullet(
            ui,
            icons::ellipsis,
            theme::TEXT_FAINT,
            "checking the executable…",
        ),
    }
}

fn bullet(
    ui: &mut egui::Ui,
    draw: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
    color: egui::Color32,
    text: &str,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::splat(11.0), egui::Sense::hover());
        draw(ui.painter(), rect, color);
        ui.add(egui::Label::new(theme::label(text, theme::FONT_SMALL, color)).wrap());
    });
}

fn code_block(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::FIELD)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(theme::mono(text, theme::FONT_SMALL, theme::TEXT_MUTED)).wrap(),
            );
        });
}

fn path_line(ui: &mut egui::Ui, path: &Path) {
    ui.add(
        egui::Label::new(theme::mono(
            path.display().to_string(),
            theme::FONT_SMALL,
            theme::TEXT_MUTED,
        ))
        .wrap(),
    );
}

fn hint_parent(home: Option<&Path>) -> String {
    home.map(|home| home.join("worktrees").display().to_string())
        .unwrap_or_else(|| "/home/you/worktrees".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "foot tmux -S {socket} attach-session -t {session}";

    fn config(command: &str, parent: &str) -> Config {
        Config::from_toml(
            &format!(
                "[terminal]\ncommand = \"{command}\"\n[worktrees]\ndefault_parent = \"{parent}\"\n"
            ),
            Path::new("config.toml"),
        )
        .expect("valid test config")
    }

    #[test]
    fn a_fresh_form_has_nothing_to_save() {
        let form = Form::new(Some(&config(TEMPLATE, "/home/u/wt")));
        assert!(!form.is_dirty());
        assert!(form.edits().is_empty());
        assert_eq!(form.problem(), None);
    }

    #[test]
    fn a_form_without_a_config_is_empty_and_invalid_until_filled() {
        let mut form = Form::new(None);
        assert!(!form.is_dirty());
        assert!(form.problem().is_some(), "an empty template cannot run");
        form.terminal_command = TEMPLATE.to_string();
        assert!(form.is_dirty());
        assert_eq!(form.problem(), None);
    }

    /// Only what changed is written: an untouched key must not be created in
    /// the user's file.
    #[test]
    fn only_edited_keys_are_written() {
        let mut form = Form::new(Some(&config(TEMPLATE, "/home/u/wt")));
        form.terminal_command = "kitty tmux -S {socket}".to_string();
        assert_eq!(
            form.edits(),
            vec![Edit::string(
                config_write::TERMINAL_COMMAND,
                "kitty tmux -S {socket}"
            )]
        );

        form.default_parent = "/tmp/trees".to_string();
        assert_eq!(form.edits().len(), 2);
        assert_eq!(
            form.edits()[1].key(),
            config_write::WORKTREES_DEFAULT_PARENT
        );
    }

    #[test]
    fn values_are_trimmed_on_the_way_into_the_file() {
        let mut form = Form::new(Some(&config(TEMPLATE, "")));
        form.default_parent = "  /home/u/wt  ".to_string();
        assert_eq!(
            form.edits(),
            vec![Edit::string(
                config_write::WORKTREES_DEFAULT_PARENT,
                "/home/u/wt"
            )]
        );
    }

    #[test]
    fn a_broken_template_blocks_the_save() {
        let mut form = Form::new(Some(&config(TEMPLATE, "")));
        form.terminal_command = "foot 'unclosed".to_string();
        assert!(form.is_dirty());
        assert!(form.problem().is_some());
    }

    #[test]
    fn a_reload_adopts_the_file_as_the_new_baseline() {
        let mut form = Form::new(Some(&config(TEMPLATE, "")));
        form.terminal_command = "kitty".to_string();
        form.saving = true;
        form.reloaded(&config("kitty", ""));
        assert!(!form.saving);
        assert!(!form.is_dirty(), "the save landed, nothing left to write");
    }

    /// A config reloaded for another reason must not silently discard what
    /// the user is typing.
    #[test]
    fn a_reload_while_editing_keeps_the_users_text() {
        let mut form = Form::new(Some(&config(TEMPLATE, "")));
        form.terminal_command = "kitty".to_string();
        form.reloaded(&config(TEMPLATE, ""));
        assert_eq!(form.terminal_command, "kitty");
        assert!(form.is_dirty());
    }

    #[test]
    fn a_probe_only_counts_for_the_command_it_ran_against() {
        let mut form = Form::new(Some(&config(TEMPLATE, "")));
        form.probe = Some(Probe {
            command: TEMPLATE.to_string(),
            program: "foot".to_string(),
            found: true,
        });
        assert!(form.current_probe().is_some());
        form.terminal_command = "kitty".to_string();
        assert!(
            form.current_probe().is_none(),
            "a stale answer must not be shown as current"
        );
    }

    #[test]
    fn the_preview_expands_the_placeholders_without_running_anything() {
        let preview =
            preview(TEMPLATE, Path::new("/run/user/1000/grove/tmux.sock")).expect("valid");
        assert_eq!(
            preview,
            "foot tmux -S /run/user/1000/grove/tmux.sock attach-session -t wt-a1b2c3"
        );
    }

    #[test]
    fn the_preview_shows_that_a_path_stays_one_argument() {
        let preview = preview("term -c {worktree} -t {branch}", Path::new("/s")).expect("valid");
        assert!(preview.contains(SAMPLE_WORKTREE));
        assert!(preview.contains(SAMPLE_BRANCH));
    }

    #[test]
    fn the_preview_reports_a_broken_template() {
        assert!(preview("foot 'unclosed", Path::new("/s")).is_err());
        assert!(preview("   ", Path::new("/s")).is_err());
    }
}
