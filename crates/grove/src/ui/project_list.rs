//! The project list: three levels, each of which disappears when it would
//! have exactly one child.
//!
//! A project is a dropdown header with a worktree-count badge; a worktree with
//! several tmux windows is the same dropdown, badged with its window count;
//! and a window is a leaf row. A project with one worktree is drawn as that
//! worktree under the project's name, and a worktree with one window is drawn
//! as that window under the worktree's — so the common case, one repository
//! with one shell, is still a single row.

use egui::{Sense, Ui, vec2};
use grove_core::model::Project;

use super::{Action, icons, theme, worktree_row};

/// Draw every project, applying the filter. Returns the user's action.
pub fn show(
    ui: &mut Ui,
    projects: &[Project],
    selected: Option<&str>,
    // The window row the user last opened, as (worktree id, window index).
    selected_window: Option<(&str, u32)>,
    filter: &str,
    home: Option<&std::path::Path>,
) -> Option<Action> {
    let mut action = None;
    let needle = filter.trim().to_ascii_lowercase();

    for project in projects {
        let level = Level {
            project,
            selected,
            selected_window,
            home,
        };
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

        // A project with a single worktree is drawn as that worktree, under the
        // project's name: a header with exactly one child says nothing the
        // child does not. An unavailable project keeps its header, which is
        // where its Retry and Locate live (DESIGN.md §11).
        if project.is_available() && project.worktrees.len() == 1 && matches.len() == 1 {
            let worktree = matches[0];
            if let Some(inner) = worktree_level(
                ui,
                level,
                worktree,
                worktree_row::Stands::Project(project),
                0,
            ) {
                action = Some(inner);
            }
            ui.add_space(8.0);
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
                if let Some(worktree_action) =
                    worktree_level(ui, level, worktree, worktree_row::Stands::Worktree, 1)
                {
                    inner = Some(worktree_action);
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

/// Everything a level of the tree needs beyond the worktree it is drawing:
/// which project it belongs to, what is selected, and where home is.
#[derive(Clone, Copy)]
struct Level<'a> {
    project: &'a Project,
    selected: Option<&'a str>,
    selected_window: Option<(&'a str, u32)>,
    home: Option<&'a std::path::Path>,
}

/// One worktree of a project: either a single leaf row, or a dropdown header
/// with one leaf row per tmux window.
///
/// A worktree with one window (or none reported yet) *is* the leaf row, named
/// after `stands` — which is why a worktree with a single shell looks exactly
/// as it did before windows entered the tree. Only a second window earns the
/// header, and the badge on it counts them.
fn worktree_level(
    ui: &mut Ui,
    level: Level,
    worktree: &grove_core::model::Worktree,
    stands: worktree_row::Stands,
    // How far in this worktree sits: 0 when it stands for its project and so
    // has no header above it, 1 under a project header.
    depth: u8,
) -> Option<Action> {
    let Level {
        project,
        selected,
        selected_window,
        home,
    } = level;
    let is_selected = selected == Some(worktree.id.as_str());
    if !has_window_rows(worktree) {
        let row_action = worktree_row::show(ui, worktree, is_selected, home, stands, depth)?;
        return row_action.into_action(&project.id, &worktree.id);
    }

    // egui owns this fold, unlike a project's: it is a view detail of a live
    // tmux session, not something `state.toml` should carry across restarts.
    let id = ui.make_persistent_id(("grove-worktree", &worktree.id));
    let mut state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
    let openness = state.openness(ui.ctx());

    let mut action = None;
    if let Some(row_action) = worktree_row::header(
        ui,
        worktree,
        stands,
        worktree.windows.len(),
        openness,
        depth,
    ) {
        if row_action == worktree_row::RowAction::Fold {
            state.toggle(ui);
        }
        action = row_action.into_action(&project.id, &worktree.id);
    }

    let body = state.show_body_unindented(ui, |ui| {
        let mut inner = None;
        for window in &worktree.windows {
            let selected = selected_window == Some((worktree.id.as_str(), window.index));
            let Some(row_action) = worktree_row::show(
                ui,
                worktree,
                selected,
                home,
                worktree_row::Stands::Window(window),
                depth + 1,
            ) else {
                continue;
            };
            // Opening a window row opens *that* window; everything else on it
            // is the worktree's, and means what it means anywhere else.
            inner = if row_action == worktree_row::RowAction::Activate {
                Some(Action::ActivateWindow {
                    project_id: project.id.clone(),
                    worktree_id: worktree.id.clone(),
                    window_index: window.index,
                })
            } else {
                row_action.into_action(&project.id, &worktree.id)
            };
        }
        inner
    });
    state.store(ui.ctx());
    body.and_then(|body| body.inner).or(action)
}

/// Does this worktree get a dropdown header with a row per window, or is it a
/// single leaf row?
///
/// One window is no reason for two levels, and no window at all — a worktree
/// with no session, or one Grove has not polled yet — is the same case.
pub fn has_window_rows(worktree: &grove_core::model::Worktree) -> bool {
    worktree.windows.len() > 1
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

    fn window(index: u32) -> grove_core::tmux::WindowInfo {
        grove_core::tmux::WindowInfo {
            session: "wt-a1b2c3".into(),
            index,
            name: "shell".into(),
            active: index == 0,
            bell: false,
        }
    }

    /// The case that must not have changed when windows joined the tree: one
    /// shell is still one row, drawn exactly as a worktree always was.
    #[test]
    fn a_worktree_with_one_window_stays_a_single_row() {
        let mut worktree = worktree("main", "/home/u/acme-web");
        assert!(!has_window_rows(&worktree), "no poll has landed yet");
        worktree.windows = vec![window(0)];
        assert!(!has_window_rows(&worktree), "one shell is not a level");
        worktree.windows.push(window(1));
        assert!(has_window_rows(&worktree), "a second window earns a header");
    }

    /// One step per header above a row, and nothing for a row that has none.
    #[test]
    fn each_level_indents_by_one_step() {
        assert_eq!(worktree_row::indent(0), 0.0);
        assert_eq!(worktree_row::indent(1), worktree_row::INDENT_STEP);
        assert_eq!(worktree_row::indent(2), 2.0 * worktree_row::INDENT_STEP);
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
