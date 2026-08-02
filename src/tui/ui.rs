use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use crate::ops::{LocalOutcome, RemoteOutcome};
use crate::output::kind_label;
use crate::scan::MergeKind;
use crate::tui::app::{App, Screen};

pub fn render(frame: &mut Frame, app: &App) {
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
        MergeKind::Squash => Style::new().fg(Color::Yellow),
        MergeKind::Gone => Style::new().fg(Color::Red),
    }
}

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

fn render_list(frame: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());

    let name_w = app
        .items
        .iter()
        .map(|i| i.candidate.name.len())
        .max()
        .unwrap_or(0);
    let items: Vec<ListItem> = app
        .items
        .iter()
        .map(|item| {
            let c = &item.candidate;
            let mut spans = vec![
                Span::raw(if item.selected { "[x] " } else { "[ ] " }),
                Span::raw(format!("{:name_w$}  ", c.name)),
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
                (Some(u), false) => {
                    spans.push(Span::styled(format!("  ↑ {u}"), DIM));
                    if item.remote {
                        spans.push(Span::styled(
                            "  r:[x] delete on remote",
                            Style::new().fg(Color::Red),
                        ));
                    } else {
                        spans.push(Span::styled("  r:[ ]", DIM));
                    }
                }
                (None, _) => {}
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(Block::bordered().title(format!(
            " git barber · base {} · {} candidates ",
            app.base,
            app.items.len()
        )))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    let mut state = ListState::default().with_selected(Some(app.cursor));
    frame.render_stateful_widget(list, main, &mut state);

    let hint = format!(
        " {} selected ({} force, {} remote) · space toggle · a all · n none · r remote · enter delete · q quit",
        app.selected_count(),
        app.force_count(),
        app.remote_count(),
    );
    frame.render_widget(Paragraph::new(Span::styled(hint, DIM)), footer);
}

fn render_confirm(frame: &mut Frame, app: &App) {
    let mut lines: Vec<Line> = Vec::new();
    let gentle: Vec<&str> = app
        .items
        .iter()
        .filter(|i| i.selected && !i.candidate.needs_force())
        .map(|i| i.candidate.name.as_str())
        .collect();
    let force: Vec<&str> = app
        .items
        .iter()
        .filter(|i| i.selected && i.candidate.needs_force())
        .map(|i| i.candidate.name.as_str())
        .collect();
    let remote: Vec<&str> = app
        .items
        .iter()
        .filter(|i| i.selected && i.remote)
        .map(|i| i.candidate.name.as_str())
        .collect();

    if !gentle.is_empty() {
        lines.push(Line::from("Delete (git branch -d):"));
        for name in &gentle {
            lines.push(Line::from(format!("  {name}")));
        }
    }
    if !force.is_empty() {
        lines.push(Line::styled(
            "Force delete (git branch -D):",
            Style::new().fg(Color::Yellow),
        ));
        for name in &force {
            lines.push(Line::from(format!("  {name}")));
        }
    }
    if !remote.is_empty() {
        lines.push(Line::styled(
            "DELETE ON REMOTE — affects everyone:",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        for name in &remote {
            lines.push(Line::styled(
                format!("  {name}"),
                Style::new().fg(Color::Red),
            ));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("y confirm · esc cancel", DIM));

    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let area = centered(frame.area(), width.max(30), height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" confirm deletion ")),
        area,
    );
}

fn render_results(frame: &mut Frame, app: &App) {
    let [main, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());

    let name_w = app.results.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    for r in &app.results {
        let mut spans = vec![Span::raw(format!("  {:name_w$}  ", r.name))];
        match &r.local {
            LocalOutcome::Deleted => {
                spans.push(Span::styled("deleted", Style::new().fg(Color::Green)))
            }
            LocalOutcome::ForceDeleted => spans.push(Span::styled(
                "force-deleted (verified merged)",
                Style::new().fg(Color::Green),
            )),
            LocalOutcome::Failed(msg) => {
                spans.push(Span::styled(
                    format!("FAILED: {msg}"),
                    Style::new().fg(Color::Red),
                ));
            }
        }
        match &r.remote {
            RemoteOutcome::Deleted { remote } => spans.push(Span::styled(
                format!(", deleted on {remote}"),
                Style::new().fg(Color::Red),
            )),
            RemoteOutcome::Skipped => {}
            RemoteOutcome::Failed(msg) => spans.push(Span::styled(
                format!(", remote FAILED: {msg}"),
                Style::new().fg(Color::Red),
            )),
        }
        lines.push(Line::from(spans));
    }

    let undo: Vec<&String> = app.results.iter().flat_map(|r| &r.undo).collect();
    if !undo.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("undo:"));
        for u in undo {
            lines.push(Line::styled(format!("  {u}"), DIM));
        }
        lines.push(Line::styled(
            "  (recent tips also linger in `git reflog`)",
            DIM,
        ));
    }

    let title = if app.screen == Screen::Deleting {
        format!(
            " deleting {}/{} … ",
            app.results.len(),
            app.selected_count()
        )
    } else {
        " results ".to_string()
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        main,
    );

    let hint = if app.screen == Screen::Deleting {
        " working…"
    } else {
        " q exit"
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
    use crate::scan::{Candidate, Scan};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    fn app() -> App {
        let candidate = |name: &str, kind| Candidate {
            name: name.into(),
            sha: "0123456789abcdef0123".into(),
            kind,
            upstream: Some(format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: "subject".into(),
        };
        let scan = Scan {
            base: "origin/main".into(),
            candidates: vec![
                candidate("merged-a", MergeKind::Merged),
                candidate("gone-c", MergeKind::Gone),
            ],
            warnings: vec![],
        };
        App::new(scan, false, 86_400)
    }

    fn screen_text(app: &App) -> String {
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
    fn list_screen_shows_names_badges_and_hints() {
        let text = screen_text(&app());
        for needle in [
            "merged-a",
            "gone-c",
            "merged",
            "gone",
            "origin/main",
            "enter delete",
        ] {
            assert!(text.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn confirm_modal_groups_by_severity() {
        let mut app = app();
        app.update(KeyEvent::from(KeyCode::Char('r'))); // remote for merged-a
        app.update(KeyEvent::from(KeyCode::Enter));
        let text = screen_text(&app);
        assert!(text.contains("confirm deletion"));
        assert!(text.contains("git branch -d"));
        assert!(text.contains("DELETE ON REMOTE"));
    }

    #[test]
    fn results_screen_shows_outcomes_and_undo() {
        let mut app = app();
        app.apply_result(crate::ops::DeletionResult {
            name: "merged-a".into(),
            sha: "0123456789abcdef0123".into(),
            local: LocalOutcome::Deleted,
            remote: RemoteOutcome::Skipped,
            undo: vec!["git branch merged-a 0123456789ab".into()],
        });
        app.finish();
        let text = screen_text(&app);
        assert!(text.contains("results"));
        assert!(text.contains("deleted"));
        assert!(text.contains("git branch merged-a 0123456789ab"));
    }
}
