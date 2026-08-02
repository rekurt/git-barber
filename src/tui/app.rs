use ratatui::crossterm::event::{KeyCode, KeyEvent};

use crate::ops::{self, DeletionResult, PlannedDeletion};
use crate::scan::{Candidate, Scan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    List,
    Confirm,
    Deleting,
    Results,
}

pub struct Item {
    pub candidate: Candidate,
    pub selected: bool,
    pub remote: bool,
}

impl Item {
    pub fn can_remote(&self) -> bool {
        ops::remote_branch(&self.candidate).is_some()
    }
}

/// The only side effect the UI can request from its driver.
pub enum Action {
    Execute(Vec<PlannedDeletion>),
}

pub struct App {
    pub base: String,
    pub items: Vec<Item>,
    pub cursor: usize,
    pub screen: Screen,
    pub results: Vec<DeletionResult>,
    pub should_quit: bool,
    pub now_unix: i64,
}

impl App {
    pub fn new(scan: Scan, preselect_remote: bool, now_unix: i64) -> Self {
        let items = scan
            .candidates
            .into_iter()
            .map(|c| Item {
                selected: c.selected_by_default(),
                remote: preselect_remote && ops::remote_branch(&c).is_some(),
                candidate: c,
            })
            .collect();
        Self {
            base: scan.base,
            items,
            cursor: 0,
            screen: Screen::List,
            results: Vec::new(),
            should_quit: false,
            now_unix,
        }
    }

    /// Pure state transition: no I/O, no terminal. Fully unit-testable.
    pub fn update(&mut self, key: KeyEvent) -> Option<Action> {
        match self.screen {
            Screen::List => self.update_list(key),
            Screen::Confirm => self.update_confirm(key),
            Screen::Deleting => None, // the driver is busy; ignore input
            Screen::Results => {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter) {
                    self.should_quit = true;
                }
                None
            }
        }
    }

    fn update_list(&mut self, key: KeyEvent) -> Option<Action> {
        let last = self.items.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('g') => self.cursor = 0,
            KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char(' ') => {
                if let Some(item) = self.items.get_mut(self.cursor) {
                    item.selected = !item.selected;
                }
            }
            KeyCode::Char('a') => self.items.iter_mut().for_each(|i| i.selected = true),
            KeyCode::Char('n') => self.items.iter_mut().for_each(|i| i.selected = false),
            KeyCode::Char('r') => {
                if let Some(item) = self.items.get_mut(self.cursor)
                    && item.can_remote()
                {
                    item.remote = !item.remote;
                }
            }
            KeyCode::Enter => {
                if self.selected_count() > 0 {
                    self.screen = Screen::Confirm;
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            _ => {}
        }
        None
    }

    fn update_confirm(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.screen = Screen::Deleting;
                Some(Action::Execute(self.planned()))
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                self.screen = Screen::List;
                None
            }
            _ => None,
        }
    }

    pub fn planned(&self) -> Vec<PlannedDeletion> {
        self.items
            .iter()
            .filter(|i| i.selected)
            .map(|i| PlannedDeletion {
                candidate: i.candidate.clone(),
                delete_remote: i.remote,
            })
            .collect()
    }

    pub fn apply_result(&mut self, result: DeletionResult) {
        self.results.push(result);
    }

    pub fn finish(&mut self) {
        self.screen = Screen::Results;
    }

    pub fn selected_count(&self) -> usize {
        self.items.iter().filter(|i| i.selected).count()
    }

    pub fn force_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.selected && i.candidate.needs_force())
            .count()
    }

    pub fn remote_count(&self) -> usize {
        self.items.iter().filter(|i| i.selected && i.remote).count()
    }

    pub fn any_failed(&self) -> bool {
        self.results.iter().any(DeletionResult::failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::MergeKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn candidate(name: &str, kind: MergeKind) -> Candidate {
        Candidate {
            name: name.into(),
            sha: "0123456789abcdef0123".into(),
            kind,
            upstream: (kind != MergeKind::Gone).then(|| format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: "subject".into(),
        }
    }

    fn app() -> App {
        let scan = Scan {
            base: "origin/main".into(),
            candidates: vec![
                candidate("merged-a", MergeKind::Merged),
                candidate("squash-b", MergeKind::Squash),
                candidate("gone-c", MergeKind::Gone),
            ],
            warnings: vec![],
        };
        App::new(scan, false, 0)
    }

    #[test]
    fn defaults_preselect_high_confidence_only() {
        let app = app();
        let sel: Vec<bool> = app.items.iter().map(|i| i.selected).collect();
        assert_eq!(sel, vec![true, true, false]);
        assert_eq!(app.selected_count(), 2);
        assert_eq!(app.force_count(), 1); // the squash one
    }

    #[test]
    fn cursor_moves_and_clamps() {
        let mut app = app();
        app.update(key(KeyCode::Char('k')));
        assert_eq!(app.cursor, 0, "must clamp at top");
        app.update(key(KeyCode::Char('j')));
        app.update(key(KeyCode::Down));
        app.update(key(KeyCode::Char('j')));
        assert_eq!(app.cursor, 2, "must clamp at bottom");
        app.update(key(KeyCode::Char('g')));
        assert_eq!(app.cursor, 0);
        app.update(key(KeyCode::Char('G')));
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn toggle_all_none_and_space() {
        let mut app = app();
        app.update(key(KeyCode::Char('n')));
        assert_eq!(app.selected_count(), 0);
        app.update(key(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 3);
        app.update(key(KeyCode::Char(' ')));
        assert_eq!(app.selected_count(), 2);
        assert!(!app.items[0].selected);
    }

    #[test]
    fn remote_toggle_needs_a_live_upstream() {
        let mut app = app();
        app.update(key(KeyCode::Char('r')));
        assert!(app.items[0].remote, "merged-a tracks origin, r must toggle");
        app.update(key(KeyCode::Char('G')));
        app.update(key(KeyCode::Char('r')));
        assert!(
            !app.items[2].remote,
            "gone-c has no live upstream, r is a no-op"
        );
    }

    #[test]
    fn enter_only_confirms_with_a_selection() {
        let mut app = app();
        app.update(key(KeyCode::Char('n')));
        app.update(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::List, "empty selection must not confirm");
        app.update(key(KeyCode::Char('a')));
        app.update(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Confirm);
    }

    #[test]
    fn confirm_yes_yields_exactly_the_selection() {
        let mut app = app();
        app.update(key(KeyCode::Char('r'))); // remote for merged-a
        app.update(key(KeyCode::Enter));
        let action = app.update(key(KeyCode::Char('y')));
        let Some(Action::Execute(plans)) = action else {
            panic!("expected Execute action");
        };
        assert_eq!(app.screen, Screen::Deleting);
        let got: Vec<(String, bool)> = plans
            .iter()
            .map(|p| (p.candidate.name.clone(), p.delete_remote))
            .collect();
        assert_eq!(
            got,
            vec![
                ("merged-a".to_string(), true),
                ("squash-b".to_string(), false)
            ]
        );
    }

    #[test]
    fn confirm_esc_backs_out() {
        let mut app = app();
        app.update(key(KeyCode::Enter));
        assert!(app.update(key(KeyCode::Esc)).is_none());
        assert_eq!(app.screen, Screen::List);
        assert!(!app.should_quit);
    }

    #[test]
    fn results_screen_exits_on_q() {
        let mut app = app();
        app.finish();
        app.update(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn quit_from_list_deletes_nothing() {
        let mut app = app();
        assert!(app.update(key(KeyCode::Char('q'))).is_none());
        assert!(app.should_quit);
        assert!(app.results.is_empty());
    }
}
