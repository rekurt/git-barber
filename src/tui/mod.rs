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

/// Run the interactive TUI. `ratatui::try_init` installs a panic hook that
/// restores the terminal before any panic message prints.
pub fn run(git: &dyn Git, scan: Scan, preselect_remote: bool, now_unix: i64) -> Result<ExitCode> {
    let mut app = App::new(scan, preselect_remote, now_unix);
    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, &mut app, git);
    ratatui::restore();
    // The alternate screen is gone the moment we restore — replay the
    // outcomes (and their undo commands) onto the real terminal so they
    // survive the TUI. This also covers the error path.
    if !app.results.is_empty() {
        print!(
            "{}",
            crate::output::human_execute(&app.base.name, &app.results)
        );
    }
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
                app.in_flight = Some(plan.candidate.name.clone());
                // Draw errors must not abort the batch: results (and undo
                // hints) matter more than one bad frame.
                let _ = terminal.draw(|f| ui::render(f, app));
                let result = ops::delete_one(git, &base, current.as_deref(), &plan);
                app.apply_result(result);
                let _ = terminal.draw(|f| ui::render(f, app));
            }
            app.finish();
            // Keys typed impatiently during the batch must not dismiss the
            // results screen the instant it appears.
            while event::poll(Duration::ZERO)? {
                let _ = event::read()?;
            }
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
