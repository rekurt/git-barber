use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};

use crate::ops::{self, LocalOutcome, RemoteOutcome};
use crate::output::kind_label;
use crate::scan::MergeKind;
use crate::tui::app::{App, Screen};

const MAX_NAME_W: usize = 40;
/// Below this the terminal is too narrow to split side by side without
/// squeezing the branch list, so the preview goes underneath instead.
const PREVIEW_SIDE_BY_SIDE_W: u16 = 100;
/// Rows the preview gets when it sits under the list.
const PREVIEW_BOTTOM_H: u16 = 10;
const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const RED: Style = Style::new().fg(Color::Red);
const YELLOW: Style = Style::new().fg(Color::Yellow);

pub fn render(frame: &mut Frame, app: &mut App) {
    match app.screen {
        Screen::List => render_list(frame, app),
        Screen::Confirm => {
            render_list(frame, app);
            render_confirm(frame, app);
        }
        Screen::Deleting | Screen::Results => render_results(frame, app),
    }
}

fn kind_style(kind: MergeKind) -> Style {
    match kind {
        MergeKind::Merged => Style::new().fg(Color::Green),
        MergeKind::Squash | MergeKind::Rebase => YELLOW,
        MergeKind::Gone => RED,
    }
}

/// Truncate long branch names so one monster name can't push every other
/// column (including the destructive remote marker) off screen.
fn clip(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        name.to_string()
    } else {
        let cut: String = name.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Split the body into the list and, when the panel is open, the preview.
/// `Min` on the list side means a short or narrow terminal shrinks the
/// preview rather than the thing being chosen from.
fn split_body(area: Rect, preview_open: bool) -> (Rect, Option<Rect>) {
    if !preview_open {
        return (area, None);
    }
    if area.width >= PREVIEW_SIDE_BY_SIDE_W {
        let [list, preview] =
            Layout::horizontal([Constraint::Min(40), Constraint::Percentage(45)]).areas(area);
        (list, Some(preview))
    } else {
        let [list, preview] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(PREVIEW_BOTTOM_H)])
                .areas(area);
        (list, Some(preview))
    }
}

fn render_preview(frame: &mut Frame, app: &App, area: Rect) {
    let name = app
        .highlighted_candidate()
        .map(|c| c.name.as_str())
        .unwrap_or("");
    // No text yet means the driver has not answered for this branch: say so
    // rather than showing a blank box that reads as "nothing here".
    let text = app.preview().unwrap_or("loading…");
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(format!(" {} ", clip(name, MAX_NAME_W))))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_list(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let (main, preview) = split_body(body, app.preview_open);

    let name_w = app
        .items
        .iter()
        .map(|i| i.candidate.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME_W);
    let items: Vec<ListItem> = app
        .items
        .iter()
        .map(|item| {
            let c = &item.candidate;
            // The remote-delete marker sits in a fixed column right after the
            // checkbox: destructive state must never be truncated away.
            let remote_marker = if item.remote {
                Span::styled("r:[x] ", RED)
            } else if item.can_remote() {
                Span::styled("r:[ ] ", DIM)
            } else {
                Span::raw("      ")
            };
            let mut spans = vec![
                Span::raw(if item.selected { "[x] " } else { "[ ] " }),
                remote_marker,
                Span::raw(format!("{:name_w$}  ", clip(&c.name, MAX_NAME_W))),
                Span::styled(format!("{:6}", kind_label(c.kind)), kind_style(c.kind)),
                Span::styled(
                    format!(
                        "  {:>4} ago",
                        crate::output::age(app.now_unix, c.last_commit_unix)
                    ),
                    DIM,
                ),
            ];
            match (&c.upstream, c.upstream_gone) {
                (Some(u), true) => spans.push(Span::styled(format!("  ↑ {u} (gone)"), DIM)),
                (Some(u), false) => spans.push(Span::styled(format!("  ↑ {u}"), DIM)),
                (None, _) => {}
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut block = Block::bordered().title(format!(
        " git barber · base {} · {} candidates ",
        app.base.name,
        app.items.len()
    ));
    if let Some(w) = app.warnings.first() {
        let more = match app.warnings.len() {
            0 | 1 => String::new(),
            n => format!(" (+{} more)", n - 1),
        };
        block = block.title_bottom(Line::styled(format!(" ! {w}{more} "), YELLOW));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    app.list_state.select(Some(app.cursor));
    frame.render_stateful_widget(list, main, &mut app.list_state);

    if let Some(area) = preview {
        render_preview(frame, app, area);
    }

    let hint = format!(
        " {} sel · {} force · {} remote │ space a n r p · enter delete · q quit",
        app.selected_count(),
        app.force_count(),
        app.remote_count(),
    );
    frame.render_widget(Paragraph::new(Span::styled(hint, DIM)), footer);
}

fn render_confirm(frame: &mut Frame, app: &App) {
    // Severity first: if anything must be cut on a small terminal, it is the
    // safe tail, never the destructive head.
    let mut lines: Vec<Line> = Vec::new();

    let remote: Vec<String> = app
        .items
        .iter()
        .filter(|i| i.selected && i.remote)
        .filter_map(|i| {
            // Target first: if the terminal is narrow, the ref actually being
            // deleted must survive, not the local nickname.
            ops::remote_branch(&i.candidate).map(|(r, b)| {
                format!(
                    "  deletes {r}/{b}  (local: {})",
                    clip(&i.candidate.name, MAX_NAME_W)
                )
            })
        })
        .collect();
    if !remote.is_empty() {
        lines.push(Line::styled(
            "DELETE ON REMOTE — affects everyone:",
            RED.add_modifier(Modifier::BOLD),
        ));
        lines.extend(remote.into_iter().map(|l| Line::styled(l, RED)));
    }

    let gone: Vec<&str> = app
        .items
        .iter()
        .filter(|i| i.selected && i.candidate.kind == MergeKind::Gone)
        .map(|i| i.candidate.name.as_str())
        .collect();
    if !gone.is_empty() {
        lines.push(Line::styled(
            "Force delete (upstream gone — may hold unmerged commits):",
            RED,
        ));
        lines.extend(gone.into_iter().map(|n| Line::from(format!("  {n}"))));
    }

    let force: Vec<&str> = app
        .items
        .iter()
        .filter(|i| i.selected && i.candidate.needs_force() && i.candidate.kind != MergeKind::Gone)
        .map(|i| i.candidate.name.as_str())
        .collect();
    if !force.is_empty() {
        lines.push(Line::styled(
            "Force delete (verified squash/rebase-merged):",
            YELLOW,
        ));
        lines.extend(force.into_iter().map(|n| Line::from(format!("  {n}"))));
    }

    let gentle: Vec<&str> = app
        .items
        .iter()
        .filter(|i| i.selected && !i.candidate.needs_force())
        .map(|i| i.candidate.name.as_str())
        .collect();
    if !gentle.is_empty() {
        lines.push(Line::from("Delete (git branch -d):"));
        lines.extend(gentle.into_iter().map(|n| Line::from(format!("  {n}"))));
    }

    // Height budget: borders take 2 rows; everything beyond becomes a count.
    let budget = frame.area().height.saturating_sub(2) as usize;
    if lines.len() > budget {
        let dropped = lines.len() - budget.saturating_sub(1);
        lines.truncate(budget.saturating_sub(1));
        lines.push(Line::styled(format!("… and {dropped} more"), DIM));
    }

    let width = (lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4).max(40);
    let height = lines.len() as u16 + 2;
    let area = centered(frame.area(), width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" confirm deletion ")
                // Bottom title so the way out is visible even when clipped.
                .title_bottom(Line::from(" y confirm · n/q/esc cancel ")),
        ),
        area,
    );
}

fn render_results(frame: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());

    let name_w = app
        .results
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_NAME_W);
    let mut lines: Vec<Line> = Vec::new();
    for r in &app.results {
        let mut spans = vec![Span::raw(format!(
            "  {:name_w$}  ",
            clip(&r.name, MAX_NAME_W)
        ))];
        let label = crate::output::local_label(r);
        let style = if matches!(r.local, LocalOutcome::Failed(_)) {
            RED
        } else {
            Style::new().fg(Color::Green)
        };
        spans.push(Span::styled(label, style));
        match &r.remote {
            RemoteOutcome::Deleted { target, .. } => {
                spans.push(Span::styled(format!(", deleted {target} on remote"), RED))
            }
            RemoteOutcome::Skipped => {}
            RemoteOutcome::Failed(msg) => {
                spans.push(Span::styled(format!(", remote FAILED: {msg}"), RED))
            }
        }
        lines.push(Line::from(spans));
    }
    if let Some(name) = &app.in_flight {
        lines.push(Line::styled(format!("  → {name} …"), DIM));
    }

    let undo: Vec<&String> = app.results.iter().flat_map(|r| &r.undo).collect();
    if !undo.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("undo:"));
        for u in undo {
            lines.push(Line::from(format!("  {u}")));
        }
        lines.push(Line::styled(
            "  (recent tips also linger in `git reflog`)",
            DIM,
        ));
    }

    // Clamp the scroll so the user can't scroll past the content.
    let visible = main.height.saturating_sub(2);
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    app.results_scroll = app.results_scroll.min(max_scroll);

    let title = match (&app.screen, &app.in_flight) {
        (Screen::Deleting, Some(name)) => {
            format!(
                " deleting {}/{} → {name} … ",
                app.results.len() + 1,
                app.selected_count()
            )
        }
        (Screen::Deleting, None) => {
            format!(
                " deleting {}/{} … ",
                app.results.len(),
                app.selected_count()
            )
        }
        _ => " results ".to_string(),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((app.results_scroll, 0))
            .block(Block::bordered().title(title)),
        main,
    );

    let hint = if app.screen == Screen::Deleting {
        " working… (results are reprinted after exit)"
    } else {
        " j/k scroll · q exit (results are reprinted after exit)"
    };
    frame.render_widget(Paragraph::new(Span::styled(hint, DIM)), footer);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Base, Candidate, Scan};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    fn candidate(name: &str, kind: MergeKind) -> Candidate {
        Candidate {
            name: name.into(),
            refname: format!("refs/heads/{name}"),
            sha: "0123456789abcdef0123".into(),
            kind,
            upstream: Some(format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: "subject".into(),
            upstream_ref: Some(format!("refs/remotes/origin/{name}")),
        }
    }

    fn app() -> App {
        let scan = Scan {
            base: Base {
                name: "origin/main".into(),
                refname: Some("refs/remotes/origin/main".into()),
                sha: "base000".into(),
            },
            candidates: vec![
                candidate("merged-a", MergeKind::Merged),
                candidate("gone-c", MergeKind::Gone),
            ],
            warnings: vec!["shallow repository: probes disabled".into()],
        };
        App::new(scan, false, 86_400, false)
    }

    fn screen_text(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn list_screen_shows_names_badges_warning_and_hints() {
        let mut app = app();
        let text = screen_text(&mut app);
        for needle in [
            "merged-a",
            "gone-c",
            "merged",
            "gone",
            "origin/main",
            "enter delete",
            "shallow repository",
            "r:[ ]",
        ] {
            assert!(text.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn confirm_modal_puts_remote_first_with_real_target() {
        let mut app = app();
        app.update(KeyEvent::from(KeyCode::Char('r'))); // remote for merged-a
        app.update(KeyEvent::from(KeyCode::Enter));
        let text = screen_text(&mut app);
        assert!(text.contains("confirm deletion"));
        assert!(text.contains("y confirm"));
        let remote_pos = text.find("DELETE ON REMOTE").expect("remote section");
        let gentle_pos = text.find("git branch -d").expect("gentle section");
        assert!(remote_pos < gentle_pos, "remote section must render first");
        assert!(text.contains("deletes origin/merged-a"));
    }

    #[test]
    fn results_screen_shows_outcomes_and_undo() {
        let mut app = app();
        app.apply_result(crate::ops::DeletionResult {
            name: "merged-a".into(),
            sha: "0123456789abcdef0123".into(),
            kind: MergeKind::Merged,
            local: LocalOutcome::Deleted,
            remote: RemoteOutcome::Deleted {
                target: "origin/merged-a".into(),
                sha: "0123456789abcdef0123".into(),
            },
            undo: vec!["git branch merged-a 0123456789ab".into()],
        });
        app.finish();
        let text = screen_text(&mut app);
        assert!(text.contains("results"));
        assert!(text.contains("deleted"));
        assert!(text.contains("deleted origin/merged-a on remote"));
        assert!(text.contains("git branch merged-a 0123456789ab"));
    }

    #[test]
    fn long_names_are_clipped_not_row_breaking() {
        let mut app = app();
        app.items[0].candidate.name = "feature/".repeat(12); // 96 chars
        let text = screen_text(&mut app);
        assert!(
            text.contains('…'),
            "long names must be clipped with an ellipsis"
        );
    }
    #[test]
    fn a_wide_terminal_puts_the_preview_beside_the_list() {
        let (list, preview) = split_body(Rect::new(0, 0, 120, 30), true);
        let preview = preview.expect("an open panel must get an area");
        assert_eq!(list.y, preview.y, "expected a side-by-side split");
        assert!(preview.x >= list.x + list.width, "panes must not overlap");
    }

    #[test]
    fn a_narrow_terminal_stacks_the_preview_under_the_list() {
        // Side by side under ~100 columns would squeeze branch names to
        // nothing, which is worse than losing rows.
        let (list, preview) = split_body(Rect::new(0, 0, 80, 30), true);
        let preview = preview.expect("an open panel must get an area");
        assert_eq!(list.x, preview.x, "expected a stacked split");
        assert!(preview.y >= list.y + list.height, "panes must not overlap");
    }

    #[test]
    fn a_closed_panel_gives_the_whole_body_to_the_list() {
        let area = Rect::new(0, 0, 120, 30);
        let (list, preview) = split_body(area, false);
        assert_eq!(list, area);
        assert!(preview.is_none());
    }

    #[test]
    fn a_short_terminal_shrinks_the_preview_rather_than_the_list() {
        // The list is what the user is choosing from; it must never collapse
        // to nothing just because the panel wants ten rows.
        let (list, _) = split_body(Rect::new(0, 0, 80, 6), true);
        assert!(list.height >= 3, "list collapsed to {} rows", list.height);
    }
}
