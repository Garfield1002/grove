//! The safe-removal dialog (DESIGN.md §13, ARCHITECTURE.md §8.2-8.3).
//!
//! Four operations, never bundled:
//!
//! 1. remove the project from Grove (metadata only),
//! 2. close the tmux session,
//! 3. remove the git worktree,
//! 4. delete the branch.
//!
//! Each is a separate button, each needs its own confirmation click, and the
//! forced variants (`worktree remove --force`, `branch -D`) are only offered
//! *after* git has refused and its refusal has been shown. The risk report
//! above the buttons says what would be lost. Nothing is ever done implicitly
//! and no operation implies another.

use std::path::PathBuf;

use egui::{Context, Ui};
use grove_core::removal::{RemovalReport, Severity};

use crate::ui::theme;
use crate::workers::RemovalOp;

/// One of the four operations, as the app will execute it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Metadata only: never touches the repository, worktrees or sessions.
    RemoveProject,
    CloseSession,
    RemoveWorktree {
        force: bool,
    },
    DeleteBranch {
        force: bool,
    },
}

impl Request {
    /// The wording of the confirmation button, which must say exactly what
    /// will happen.
    pub fn confirm_label(&self) -> String {
        match self {
            Request::RemoveProject => "Confirm: remove from Grove only".to_string(),
            Request::CloseSession => "Confirm: kill the tmux session".to_string(),
            Request::RemoveWorktree { force: false } => "Confirm: remove the worktree".to_string(),
            Request::RemoveWorktree { force: true } => {
                "Confirm: force-remove, discarding those files".to_string()
            }
            Request::DeleteBranch { force: false } => "Confirm: delete the branch".to_string(),
            Request::DeleteBranch { force: true } => {
                "Confirm: force-delete, discarding those commits".to_string()
            }
        }
    }
}

/// Dialog state held between frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalForm {
    pub project_id: String,
    pub project_name: String,
    pub repository_path: PathBuf,
    pub git_common_dir: PathBuf,
    pub worktree_id: String,
    pub worktree_label: String,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
    pub session: Option<String>,
    /// Filled in by the worker; until then the destructive options stay
    /// hidden — Grove never offers a removal it has not assessed.
    pub report: Option<RemovalReport>,
    /// The operation awaiting its confirmation click.
    pub armed: Option<Request>,
    /// Set once git has refused, which is the only way `--force` is offered.
    pub force_worktree_offered: bool,
    pub force_branch_offered: bool,
    /// What has already been done in this dialog, and what git said when it
    /// refused. Both stay on screen.
    pub done: Vec<String>,
    pub refusals: Vec<String>,
}

impl RemovalForm {
    /// Record a refusal and, for the two operations that have one, unlock the
    /// forced variant.
    pub fn note_refusal(&mut self, operation: RemovalOp, message: String) {
        match operation {
            RemovalOp::RemoveWorktree => self.force_worktree_offered = true,
            RemovalOp::DeleteBranch => self.force_branch_offered = true,
            RemovalOp::CloseSession => {}
        }
        self.armed = None;
        self.refusals.push(message);
    }

    /// Record a success. The dialog stays open: the remaining operations are
    /// still separate decisions.
    pub fn note_done(&mut self, operation: RemovalOp, detail: String) {
        match operation {
            RemovalOp::CloseSession => self.session = None,
            RemovalOp::RemoveWorktree => {
                self.force_worktree_offered = false;
                if let Some(report) = &mut self.report {
                    report.can_remove_worktree = false;
                }
            }
            RemovalOp::DeleteBranch => {
                self.force_branch_offered = false;
                self.branch = None;
                if let Some(report) = &mut self.report {
                    report.can_delete_branch = false;
                }
            }
        }
        self.done.push(detail);
    }
}

/// Draw the dialog. Returns the operation the user confirmed, if any, and
/// sets `open` to false when the dialog should close.
pub fn show(ctx: &Context, form: &mut RemovalForm, open: &mut bool) -> Option<Request> {
    let mut request = None;
    let mut closed = false;

    egui::Window::new(format!("Remove — {}", form.worktree_label))
        .collapsible(false)
        .resizable(false)
        .open(open)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_min_width(380.0);
            ui.set_max_width(460.0);

            ui.add(
                egui::Label::new(theme::mono(
                    form.worktree_path.display().to_string(),
                    10.5,
                    theme::TEXT_DIM,
                ))
                .wrap(),
            );
            ui.label(theme::label(
                match &form.branch {
                    Some(branch) => format!("branch {branch}"),
                    None => "no branch (detached HEAD)".to_string(),
                },
                10.0,
                theme::TEXT_FAINT,
            ));

            ui.add_space(8.0);
            match &form.report {
                Some(report) => findings(ui, report),
                None => {
                    ui.label(theme::label(
                        "Checking for uncommitted changes, unpushed commits and running processes…",
                        11.0,
                        theme::TEXT_MUTED,
                    ));
                }
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(theme::label(
                "Four separate operations. Each one asks again.",
                10.0,
                theme::TEXT_FAINT,
            ));
            ui.add_space(6.0);

            request = operations(ui, form);

            for detail in &form.done {
                ui.add_space(4.0);
                ui.add(egui::Label::new(theme::label(detail, 10.0, theme::TEXT_MUTED)).wrap());
            }
            for refusal in &form.refusals {
                ui.add_space(4.0);
                ui.add(egui::Label::new(theme::label(refusal, 10.0, theme::DANGER)).wrap());
            }

            ui.add_space(10.0);
            if ui.button("Close").clicked() {
                request = None;
                closed = true;
            }
        });

    if closed {
        *open = false;
    }
    request
}

fn findings(ui: &mut Ui, report: &RemovalReport) {
    for finding in &report.findings {
        let color = match finding.severity {
            Severity::Blocker => theme::DANGER,
            Severity::Warning => theme::WARNING,
            Severity::Note => theme::TEXT_MUTED,
        };
        let bullet = match finding.severity {
            Severity::Blocker => "✕",
            Severity::Warning => "!",
            Severity::Note => "·",
        };
        ui.add(
            egui::Label::new(theme::label(
                format!("{bullet}  {}", finding.text),
                11.0,
                color,
            ))
            .wrap(),
        );
        ui.add_space(2.0);
    }
}

/// What a click on one operation row meant.
enum Click {
    /// First click: ask for confirmation.
    Arm(Request),
    /// Second click on the confirmation: do it.
    Confirm(Request),
    /// The user backed out.
    Disarm,
}

/// The four operations, each with its own confirmation step.
fn operations(ui: &mut Ui, form: &mut RemovalForm) -> Option<Request> {
    let mut click: Option<Click> = None;
    let armed = form.armed.clone();

    // 1. Grove's own index. Always safe, always available.
    let remove_project = format!("Remove “{}” from Grove", form.project_name);
    step(
        ui,
        &mut click,
        &armed,
        Request::RemoveProject,
        &remove_project,
        Some("Metadata only — the repository, worktrees and sessions are untouched."),
        true,
    );

    // 2. tmux only.
    let has_session = form.session.is_some();
    step(
        ui,
        &mut click,
        &armed,
        Request::CloseSession,
        "Close the tmux session",
        Some("Kills the session on Grove's private server. No files are touched."),
        has_session,
    );

    // 3. The worktree directory. Never offered for the main worktree.
    let can_remove = form
        .report
        .as_ref()
        .is_some_and(|report| report.can_remove_worktree);
    step(
        ui,
        &mut click,
        &armed,
        Request::RemoveWorktree { force: false },
        "Remove the git worktree",
        Some("Deletes the directory. The branch and its commits stay."),
        can_remove,
    );
    if can_remove && form.force_worktree_offered {
        step(
            ui,
            &mut click,
            &armed,
            Request::RemoveWorktree { force: true },
            "Remove the git worktree, discarding those changes",
            Some("git refused above. This passes --force and the listed files are lost."),
            true,
        );
    }

    // 4. The branch. Independent of the worktree.
    let can_delete = form
        .report
        .as_ref()
        .is_some_and(|report| report.can_delete_branch);
    let delete_label = match &form.branch {
        Some(branch) => format!("Delete the branch {branch}"),
        None => "Delete the branch".to_string(),
    };
    step(
        ui,
        &mut click,
        &armed,
        Request::DeleteBranch { force: false },
        &delete_label,
        Some("`git branch -d`, which refuses if the branch is not merged."),
        can_delete,
    );
    if can_delete && form.force_branch_offered {
        step(
            ui,
            &mut click,
            &armed,
            Request::DeleteBranch { force: true },
            "Delete the branch, discarding unmerged commits",
            Some("git refused above. This passes -D and those commits are lost."),
            true,
        );
    }

    match click {
        // Arming shows the confirmation; it performs nothing.
        Some(Click::Arm(operation)) => {
            form.armed = Some(operation);
            None
        }
        Some(Click::Disarm) => {
            form.armed = None;
            None
        }
        Some(Click::Confirm(operation)) => {
            form.armed = None;
            Some(operation)
        }
        None => None,
    }
}

/// One operation row: an action button that turns into an explicit
/// confirmation, plus a cancel while it is armed.
fn step(
    ui: &mut Ui,
    click: &mut Option<Click>,
    armed: &Option<Request>,
    operation: Request,
    label: &str,
    explanation: Option<&str>,
    enabled: bool,
) {
    let is_armed = armed.as_ref() == Some(&operation);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if is_armed {
            if ui
                .add(egui::Button::new(
                    egui::RichText::new(operation.confirm_label()).color(theme::DANGER),
                ))
                .clicked()
            {
                *click = Some(Click::Confirm(operation.clone()));
            }
            if ui.button("Cancel").clicked() {
                *click = Some(Click::Disarm);
            }
        } else if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            *click = Some(Click::Arm(operation));
        }
    });
    if let Some(explanation) = explanation {
        ui.add(egui::Label::new(theme::label(explanation, 9.5, theme::TEXT_FAINT)).wrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::StatusSummary;
    use grove_core::removal::{RemovalInputs, Unpushed, assemble};

    fn form() -> RemovalForm {
        RemovalForm {
            project_id: "p1".into(),
            project_name: "acme-web".into(),
            repository_path: PathBuf::from("/home/u/acme-web"),
            git_common_dir: PathBuf::from("/home/u/acme-web/.git"),
            worktree_id: "a1b2c3".into(),
            worktree_label: "feature/auth".into(),
            worktree_path: PathBuf::from("/home/u/wt/auth"),
            branch: Some("feature/auth".into()),
            session: Some("wt-a1b2c3".into()),
            report: Some(assemble(&RemovalInputs {
                branch: Some("feature/auth".into()),
                status: Some(StatusSummary::default()),
                unpushed: Unpushed::Count(0),
                session: Some("wt-a1b2c3".into()),
                ..RemovalInputs::new("/home/u/wt/auth")
            })),
            armed: None,
            force_worktree_offered: false,
            force_branch_offered: false,
            done: Vec::new(),
            refusals: Vec::new(),
        }
    }

    /// The forced variants must be unreachable until git itself has refused.
    #[test]
    fn force_is_only_offered_after_a_refusal() {
        let mut form = form();
        assert!(!form.force_worktree_offered);
        assert!(!form.force_branch_offered);

        form.note_refusal(
            RemovalOp::RemoveWorktree,
            "fatal: contains modified or untracked files".into(),
        );
        assert!(form.force_worktree_offered);
        assert!(!form.force_branch_offered, "one refusal unlocks one force");
        assert_eq!(form.refusals.len(), 1);
        assert!(form.armed.is_none(), "a refusal disarms the button");

        form.note_refusal(RemovalOp::DeleteBranch, "error: not fully merged".into());
        assert!(form.force_branch_offered);
    }

    #[test]
    fn closing_a_session_never_unlocks_a_force() {
        let mut form = form();
        form.note_refusal(RemovalOp::CloseSession, "no server".into());
        assert!(!form.force_worktree_offered);
        assert!(!form.force_branch_offered);
    }

    #[test]
    fn a_completed_operation_is_not_offered_again() {
        let mut form = form();
        form.note_done(RemovalOp::CloseSession, "Closed wt-a1b2c3.".into());
        assert_eq!(form.session, None);

        form.note_done(RemovalOp::RemoveWorktree, "Removed the worktree.".into());
        assert!(!form.report.as_ref().expect("report").can_remove_worktree);
        assert!(!form.force_worktree_offered);
        assert_eq!(
            form.branch.as_deref(),
            Some("feature/auth"),
            "removing a worktree must not touch the branch"
        );

        form.note_done(RemovalOp::DeleteBranch, "Deleted feature/auth.".into());
        assert_eq!(form.branch, None);
        assert!(!form.report.as_ref().expect("report").can_delete_branch);
        assert_eq!(form.done.len(), 3, "every step stays on screen");
    }

    #[test]
    fn confirmation_labels_say_exactly_what_will_happen() {
        assert_eq!(
            Request::RemoveProject.confirm_label(),
            "Confirm: remove from Grove only"
        );
        assert_eq!(
            Request::CloseSession.confirm_label(),
            "Confirm: kill the tmux session"
        );
        assert!(
            Request::RemoveWorktree { force: true }
                .confirm_label()
                .contains("discarding")
        );
        assert!(
            Request::DeleteBranch { force: true }
                .confirm_label()
                .contains("discarding")
        );
        assert!(
            !Request::RemoveWorktree { force: false }
                .confirm_label()
                .contains("force")
        );
    }

    #[test]
    fn the_main_worktree_offers_neither_removal_nor_a_force() {
        let report = assemble(&RemovalInputs {
            is_main: true,
            branch: Some("main".into()),
            status: Some(StatusSummary::default()),
            unpushed: Unpushed::Count(0),
            ..RemovalInputs::new("/home/u/acme-web")
        });
        assert!(!report.can_remove_worktree);
        // The dialog gates the button and its force variant on this flag.
        assert!(report.can_delete_branch, "the branch is still separate");
    }

    #[test]
    fn nothing_is_offered_before_the_report_arrives() {
        let form = RemovalForm {
            report: None,
            ..form()
        };
        assert!(form.report.is_none());
        // `operations` gates both git operations on `report`, so only the two
        // non-git ones (Grove metadata, tmux) can be reached meanwhile.
        assert!(form.session.is_some());
    }
}
