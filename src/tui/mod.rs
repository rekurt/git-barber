mod app;
mod ui;

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::git::Git;
use crate::ops;
use crate::scan::Scan;
use app::{Action, App};

/// Run the interactive TUI. `ratatui::init` installs a panic hook that
/// restores the terminal before any panic message prints.
pub fn run(git: &dyn Git, scan: Scan, preselect_remote: bool, now_unix: i64) -> Result<ExitCode> {
    let mut app = App::new(scan, preselect_remote, now_unix);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, git);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    git: &dyn Git,
) -> Result<ExitCode> {
    loop {
        terminal.draw(|f| ui::render(f, app))?;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press // Windows also reports Release/Repeat
            && let Some(Action::Execute(plans)) = app.update(key)
        {
            let base = app.base.clone();
            let current = ops::current_branch(git);
            for plan in plans {
                let result = ops::delete_one(git, &base, current.as_deref(), &plan);
                app.apply_result(result);
                // Redraw between branches so slow remote deletes show progress.
                terminal.draw(|f| ui::render(f, app))?;
            }
            app.finish();
        }
        if app.should_quit {
            return Ok(if app.any_failed() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            });
        }
    }
}
