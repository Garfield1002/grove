//! The open-project dialog, shown in its own OS window
//! ([`crate::ui::chrome`]).
//!
//! Asks for a path. The worker decides whether git calls it a repository, so
//! the field is always typeable and never validated here; with the
//! `native-file-picker` feature a folder button additionally fills it from the
//! desktop's own portal dialog.

use egui::Ui;

use crate::ui::{icons, theme};

/// Everything the dialog holds between frames.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenProjectForm {
    /// What the user has typed, or the path the window was opened on.
    pub path: String,
    /// Whether the field has been given focus already.
    ///
    /// Focus is asked for **once**, on the frame the window appears. Asking
    /// every frame would take the caret back from anything else the user
    /// clicked — including the main window's filter field — and would put the
    /// cursor back at the end of a prefilled path after every edit.
    focused: bool,
}

impl OpenProjectForm {
    /// An empty form: the footer's "Open Project".
    pub fn empty() -> Self {
        Self::default()
    }

    /// A form prefilled with a path, as "Locate project" does with the
    /// repository Grove has lost track of.
    pub fn at(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            focused: false,
        }
    }
}

/// What the dialog is asking the app to do.
#[derive(Default)]
pub enum Outcome {
    #[default]
    Idle,
    Cancelled,
    Confirmed(String),
    /// Ask for the desktop's directory picker; the field stays typeable.
    Browse,
}

/// Default inner size: two lines of explanation, a path field and a button row.
pub const SIZE: [f32; 2] = [460.0, 210.0];
/// Floor: the field still has to show a path worth reading.
pub const MIN_SIZE: [f32; 2] = [320.0, 180.0];

/// The window title.
pub const TITLE: &str = "Open project";

/// The dialog's contents. The window around it is [`crate::ui::chrome`]'s.
pub fn body(ui: &mut Ui, form: &mut OpenProjectForm) -> Outcome {
    let mut outcome = Outcome::Idle;
    let width = ui.available_width();

    ui.add(
        egui::Label::new(theme::label(
            "Path to a Git repository, or any directory inside one.",
            theme::FONT_BODY,
            theme::TEXT_MUTED,
        ))
        .wrap(),
    );

    ui.add_space(10.0);
    let mut field = None;
    ui.horizontal(|ui| {
        let browse = crate::ui::NATIVE_FILE_PICKER;
        let reserved = if browse {
            theme::ICON_BUTTON + 8.0
        } else {
            0.0
        };
        let response = ui.add(
            egui::TextEdit::singleline(&mut form.path)
                // "e.g." because a bare path reads as a value the field
                // already holds, and the window opens prefilled often enough
                // ("Locate project") that the difference matters.
                .hint_text(theme::hint("e.g. /home/you/projects/acme-web"))
                .desired_width((width - reserved).max(120.0)),
        );
        if !form.focused {
            response.request_focus();
            form.focused = true;
        }
        field = Some(response);
        if browse
            && icons::button(ui, true, icons::folder)
                .on_hover_text("Choose a directory")
                .clicked()
        {
            outcome = Outcome::Browse;
        }
    });

    ui.add_space(8.0);
    ui.add(
        egui::Label::new(theme::label(
            "Choosing a linked worktree registers its project.",
            theme::FONT_SMALL,
            theme::TEXT_FAINT,
        ))
        .wrap(),
    );

    let submitted = field
        .is_some_and(|field| field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let can_open = !form.path.trim().is_empty();
        let open = egui::Button::new(theme::label(
            "Open",
            theme::FONT_BODY,
            if can_open {
                theme::TEXT_STRONG
            } else {
                theme::TEXT_FAINT
            },
        ))
        .fill(theme::ACCENT_FILL)
        .stroke(egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.6)));
        if (ui.add_enabled(can_open, open).clicked() || submitted) && can_open {
            outcome = Outcome::Confirmed(form.path.trim().to_string());
        }
        if ui
            .button(theme::label("Cancel", theme::FONT_BODY, theme::TEXT_DIM))
            .clicked()
        {
            outcome = Outcome::Cancelled;
        }
    });

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_form_has_asked_for_no_focus_yet() {
        let form = OpenProjectForm::empty();
        assert!(form.path.is_empty());
        assert!(!form.focused, "the first frame gives the field the caret");
    }

    #[test]
    fn a_prefilled_form_carries_the_path_and_still_wants_focus() {
        let form = OpenProjectForm::at("/home/u/acme");
        assert_eq!(form.path, "/home/u/acme");
        assert!(!form.focused);
    }
}
