//! The project list: collapsible project headers with a worktree-count badge,
//! each followed by its worktree rows.

use egui::{Sense, Ui, vec2};
use grove_core::model::Project;

use super::worktree_row::RowAction;
use super::{Action, icons, theme, worktree_row};

/// Draw every project, applying the filter. Returns the user's action.
pub fn show(
    ui: &mut Ui,
    projects: &[Project],
    selected: Option<&str>,
    filter: &str,
    home: Option<&std::path::Path>,
) -> Option<Action> {
    let mut action = None;
    let needle = filter.trim().to_ascii_lowercase();

    for project in projects {
        let matches: Vec<&grove_core::model::Worktree> = project
            .worktrees
            .iter()
            .filter(|w| matches_filter(project, w, &needle))
            .collect();
        if !needle.is_empty()
            && matches.is_empty()
            && !project.name.to_ascii_lowercase().contains(&needle)
        {
            continue;
        }

        // `CollapsingState` owns the openness animation; Grove owns the
        // persisted flag, so the state is told what the flag says every frame
        // and the animation follows.
        let id = ui.make_persistent_id(("grove-project", &project.id));
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            id,
            project.is_expanded,
        );
        state.set_open(project.is_expanded);
        let openness = state.openness(ui.ctx());

        if let Some(header_action) = header(ui, project, matches.len(), openness) {
            action = Some(header_action);
        }

        let body = state.show_body_unindented(ui, |ui| {
            let mut inner = None;
            for worktree in &matches {
                let is_selected = selected == Some(worktree.id.as_str());
                if let Some(row_action) = worktree_row::show(ui, worktree, is_selected, home) {
                    let project_id = project.id.clone();
                    let worktree_id = worktree.id.clone();
                    inner = Some(match row_action {
                        RowAction::Activate => Action::ActivateWorktree {
                            project_id,
                            worktree_id,
                        },
                        RowAction::Select => Action::SelectWorktree {
                            project_id,
                            worktree_id,
                        },
                        RowAction::OpenInNewTerminal => Action::OpenInNewTerminal {
                            project_id,
                            worktree_id,
                        },
                        RowAction::StartAgent => Action::StartAgent {
                            project_id,
                            worktree_id,
                        },
                        RowAction::Refresh => Action::RefreshProject(project_id),
                        RowAction::Remove => Action::RemoveWorktree {
                            project_id,
                            worktree_id,
                        },
                    });
                }
            }
            if matches.is_empty() {
                ui.add_space(2.0);
                ui.label(theme::label(
                    "no worktrees match",
                    theme::FONT_SMALL,
                    theme::TEXT_FAINT,
                ));
            }
            inner
        });
        state.store(ui.ctx());
        if let Some(inner) = body.and_then(|body| body.inner) {
            action = Some(inner);
        }
        ui.add_space(8.0);
    }

    if projects.is_empty() {
        ui.add_space(28.0);
        ui.vertical_centered(|ui| {
            ui.label(theme::label(
                "No projects yet.",
                theme::FONT_BODY,
                theme::TEXT_MUTED,
            ));
            ui.add_space(6.0);
            ui.label(theme::label(
                "Use Open Project to register a Git repository.",
                theme::FONT_SMALL,
                theme::TEXT_FAINT,
            ));
        });
    }

    action
}

pub fn matches_filter(
    project: &Project,
    worktree: &grove_core::model::Worktree,
    needle: &str,
) -> bool {
    if needle.is_empty() {
        return true;
    }
    project.name.to_ascii_lowercase().contains(needle)
        || worktree.label().to_ascii_lowercase().contains(needle)
        || worktree
            .path
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains(needle)
}

/// One project header: disclosure triangle, name, worktree-count badge and a
/// "more actions" ellipsis, laid out as in the mockup.
fn header(ui: &mut Ui, project: &Project, count: usize, openness: f32) -> Option<Action> {
    let mut action = None;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::PROJECT_ROW_HEIGHT),
        Sense::click(),
    );
    let hovered = response.hovered();
    let response = match &project.unavailable {
        Some(reason) => response.on_hover_text(format!(
            "Project unavailable — {reason}\nNothing has been removed; use the menu to retry or locate it."
        )),
        None => response,
    };

    // The ellipsis is a target inside the header, so it is interacted with
    // after (and therefore above) the header itself.
    let more_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 11.0, rect.center().y),
        egui::Vec2::splat(18.0),
    );
    let more = ui.interact(
        more_rect,
        ui.id().with(("grove-project-more", &project.id)),
        Sense::click(),
    );

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if hovered {
            painter.rect_filled(
                rect,
                egui::CornerRadius::same(theme::ROW_RADIUS),
                theme::BADGE.gamma_multiply(0.6),
            );
        }

        icons::disclosure(
            painter,
            egui::Rect::from_center_size(
                egui::pos2(rect.left() + 10.0, rect.center().y),
                egui::Vec2::splat(9.0),
            ),
            openness,
            theme::TEXT_MUTED,
        );

        let name = painter.layout_no_wrap(
            project.name.clone(),
            egui::FontId::proportional(theme::FONT_PROJECT),
            theme::TEXT_STRONG,
        );
        let name_left = rect.left() + 21.0;
        // An unavailable project keeps its row and its name; only the label
        // beside it says what reconciliation found (DESIGN.md §11).
        let name_y = if project.is_available() {
            rect.center().y - name.size().y / 2.0
        } else {
            rect.top() + 4.0
        };
        painter.galley(
            egui::pos2(name_left, name_y),
            name.clone(),
            theme::TEXT_STRONG,
        );
        if !project.is_available() {
            let notice = painter.layout_no_wrap(
                "Project unavailable".to_owned(),
                egui::FontId::proportional(theme::FONT_SUB),
                theme::WARNING,
            );
            painter.galley(
                egui::pos2(name_left, rect.bottom() - notice.size().y - 3.0),
                notice,
                theme::WARNING,
            );
        }

        // Count badge, immediately after the name as in the mockup.
        let badge = painter.layout_no_wrap(
            count.to_string(),
            egui::FontId::monospace(theme::FONT_SUB),
            theme::TEXT_GHOST,
        );
        let badge_rect = egui::Rect::from_center_size(
            egui::pos2(
                name_left + name.size().x + 8.0 + (badge.size().x + 12.0) / 2.0,
                rect.center().y,
            ),
            vec2(badge.size().x + 12.0, 15.0),
        );
        painter.rect_filled(
            badge_rect,
            egui::CornerRadius::same(theme::BADGE_RADIUS),
            theme::BADGE,
        );
        let badge_size = badge.size();
        painter.galley(
            badge_rect.center() - badge_size / 2.0,
            badge,
            theme::TEXT_GHOST,
        );

        if hovered || more.hovered() {
            icons::ellipsis(
                painter,
                more_rect.shrink(4.0),
                if more.hovered() {
                    theme::TEXT_DIM
                } else {
                    theme::TEXT_FAINT
                },
            );
        }
    }

    if response.clicked() {
        action = Some(Action::ToggleProject(project.id.clone()));
    }

    let menu = |ui: &mut Ui, action: &mut Option<Action>| {
        // The unavailable-project actions come first, because they are the
        // only ones that can get the project back (DESIGN.md §11): retry,
        // locate, or remove it from Grove — never anything on disk.
        if let Some(reason) = &project.unavailable {
            ui.label(theme::label(
                format!("Project unavailable — {reason}"),
                theme::FONT_SMALL,
                theme::WARNING,
            ));
            if ui.button("Locate project…").clicked() {
                *action = Some(Action::LocateProject(project.id.clone()));
                ui.close();
            }
            ui.separator();
        }
        if ui
            .button(if project.is_available() {
                "Refresh"
            } else {
                "Retry"
            })
            .clicked()
        {
            *action = Some(Action::RefreshProject(project.id.clone()));
            ui.close();
        }
        if ui.button("Create worktree…").clicked() {
            *action = Some(Action::CreateWorktree(project.id.clone()));
            ui.close();
        }
        if ui.button("Copy repository path").clicked() {
            ui.ctx()
                .copy_text(project.repository_path.display().to_string());
            ui.close();
        }
        ui.separator();
        ui.label(theme::label(
            "Removing a project only removes it from Grove.",
            theme::FONT_SMALL,
            theme::TEXT_FAINT,
        ));
        if ui
            .button(theme::label(
                "Remove project from Grove",
                theme::FONT_BODY,
                theme::DANGER,
            ))
            .clicked()
        {
            *action = Some(Action::RemoveProject(project.id.clone()));
            ui.close();
        }
    };

    response.context_menu(|ui| menu(ui, &mut action));
    let more = more.on_hover_cursor(egui::CursorIcon::PointingHand);
    egui::Popup::menu(&more).show(|ui| menu(ui, &mut action));

    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::git::WorktreeEntry;
    use grove_core::model::Worktree;
    use std::path::{Path, PathBuf};

    fn project() -> Project {
        Project {
            id: "p1".into(),
            name: "acme-web".into(),
            repository_path: PathBuf::from("/home/u/acme-web"),
            git_common_dir: PathBuf::from("/home/u/acme-web/.git"),
            default_worktree_path: PathBuf::from("/home/u"),
            is_expanded: true,
            worktrees: Vec::new(),
            unavailable: None,
        }
    }

    fn worktree(branch: &str, path: &str) -> Worktree {
        Worktree::from_entry(
            &WorktreeEntry {
                path: PathBuf::from(path),
                branch: Some(branch.to_string()),
                ..WorktreeEntry::default()
            },
            "p1",
            Path::new("/home/u/acme-web/.git"),
            false,
        )
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        assert!(matches_filter(
            &project(),
            &worktree("feature/auth", "/home/u/wt/auth"),
            ""
        ));
    }

    #[test]
    fn matches_branch_path_and_project_name() {
        let project = project();
        let worktree = worktree("feature/auth", "/home/u/wt/auth-work");
        assert!(matches_filter(&project, &worktree, "auth"));
        assert!(matches_filter(&project, &worktree, "wt/auth-work"));
        assert!(matches_filter(&project, &worktree, "acme"));
        assert!(!matches_filter(&project, &worktree, "parser"));
    }

    #[test]
    fn filtering_is_case_insensitive() {
        // The caller lowercases the needle; the haystacks are lowercased here.
        let project = project();
        let worktree = worktree("Feature/AUTH", "/home/u/wt/Auth");
        assert!(matches_filter(&project, &worktree, "auth"));
    }
}
