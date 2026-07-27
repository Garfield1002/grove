//! The project list: collapsible project headers with a worktree-count badge,
//! each followed by its worktree rows.

use egui::{Sense, Ui, vec2};
use grove_core::model::Project;

use super::{Action, theme, worktree_row};

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

        if let Some(header_action) = header(ui, project, matches.len()) {
            action = Some(header_action);
        }

        if project.is_expanded {
            for worktree in &matches {
                let is_selected = selected == Some(worktree.id.as_str());
                if worktree_row::show(ui, worktree, is_selected, home) {
                    action = Some(Action::ActivateWorktree {
                        project_id: project.id.clone(),
                        worktree_id: worktree.id.clone(),
                    });
                }
            }
            if matches.is_empty() {
                ui.add_space(2.0);
                ui.label(theme::label("no worktrees match", 10.0, theme::TEXT_FAINT));
            }
        }
        ui.add_space(8.0);
    }

    if projects.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(theme::label("No projects yet.", 12.0, theme::TEXT_MUTED));
            ui.add_space(4.0);
            ui.label(theme::label(
                "Use Open Project to register a Git repository.",
                10.5,
                theme::TEXT_FAINT,
            ));
        });
    }

    action
}

fn matches_filter(project: &Project, worktree: &grove_core::model::Worktree, needle: &str) -> bool {
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

fn header(ui: &mut Ui, project: &Project, count: usize) -> Option<Action> {
    let mut action = None;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let triangle_center = egui::pos2(rect.left() + 9.0, rect.center().y);
        let points = if project.is_expanded {
            vec![
                triangle_center + vec2(-4.0, -2.0),
                triangle_center + vec2(4.0, -2.0),
                triangle_center + vec2(0.0, 3.0),
            ]
        } else {
            vec![
                triangle_center + vec2(-2.0, -4.0),
                triangle_center + vec2(3.0, 0.0),
                triangle_center + vec2(-2.0, 4.0),
            ]
        };
        painter.add(egui::Shape::convex_polygon(
            points,
            theme::TEXT_MUTED,
            egui::Stroke::NONE,
        ));

        painter.text(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            &project.name,
            egui::FontId::proportional(12.0),
            theme::TEXT_STRONG,
        );

        let badge = format!("{count}");
        let badge_width = 10.0 + badge.len() as f32 * 6.0;
        let badge_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - badge_width - 4.0, rect.center().y - 8.0),
            vec2(badge_width, 16.0),
        );
        painter.rect_filled(badge_rect, egui::CornerRadius::same(8), theme::BADGE);
        painter.text(
            badge_rect.center(),
            egui::Align2::CENTER_CENTER,
            badge,
            egui::FontId::monospace(9.5),
            theme::TEXT_MUTED,
        );
    }

    if response.clicked() {
        action = Some(Action::ToggleProject(project.id.clone()));
    }

    response.context_menu(|ui| {
        if ui.button("Refresh").clicked() {
            action = Some(Action::RefreshProject(project.id.clone()));
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
            10.0,
            theme::TEXT_FAINT,
        ));
        if ui.button("Remove from Grove").clicked() {
            action = Some(Action::RemoveProject(project.id.clone()));
            ui.close();
        }
    });

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
            is_expanded: true,
            worktrees: Vec::new(),
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
