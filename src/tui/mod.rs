mod app;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::git::{Git, SystemGit};
use crate::ops;
use crate::scan::{Base, Scan};
use app::{Action, App};

/// Run the interactive TUI. `ratatui::try_init` installs a panic hook that
/// restores the terminal before any panic message prints.
pub fn run(
    git: &dyn Git,
    scan: Scan,
    preselect_remote: bool,
    now_unix: i64,
    preview_open: bool,
    repo_dir: Option<PathBuf>,
) -> Result<ExitCode> {
    let mut app = App::new(scan, preselect_remote, now_unix, preview_open);
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            // try_init can fail AFTER enabling raw mode (alternate screen or
            // size query); restore is idempotent and saves the user's shell.
            ratatui::restore();
            return Err(err.into());
        }
    };
    let result = event_loop(&mut terminal, &mut app, git, repo_dir);
    ratatui::restore();
    // The alternate screen is gone the moment we restore — replay the
    // outcomes (and their undo commands) onto the real terminal so they
    // survive the TUI. This also covers the error path.
    if !app.results.is_empty() {
        print!(
            "{}",
            crate::output::human_execute(&app.base.name, &app.warnings, &app.results)
        );
    }
    result
}

/// What the branch carries that the base does not: its commits and the size
/// of its diff. Read-only, and a failure becomes the panel's text rather than
/// an error — a preview is never worth ending the session over.
///
/// Ranges are built from full refnames, so a branch called `-f` cannot turn
/// into a git option.
fn preview_text(git: &dyn Git, base: &Base, branch_ref: &str) -> String {
    const MAX_COMMITS: &str = "--max-count=20";
    let log = git.run(&[
        "log",
        "--oneline",
        "--no-decorate",
        MAX_COMMITS,
        &format!("{}..{}", base.rev(), branch_ref),
        "--",
    ]);
    // `--shortstat` renders a diff, so it can otherwise run a configured
    // external driver or textconv filter. Detection pins both off via
    // DIFF_FLAGS; the preview keeps the same "nothing configurable runs"
    // property. (`log --oneline` emits no patch, so neither applies there.)
    let stat = git.run(&[
        "diff",
        "--shortstat",
        "--no-ext-diff",
        "--no-textconv",
        &format!("{}...{}", base.rev(), branch_ref),
        "--",
    ]);
    match log {
        Err(e) => format!("preview unavailable: {e:#}"),
        Ok(log) => {
            let commits = log.trim_end();
            let mut text = if commits.is_empty() {
                "no commits the base does not already have".to_string()
            } else {
                commits.to_string()
            };
            if let Ok(stat) = stat
                && !stat.trim().is_empty()
            {
                text.push_str("\n\n");
                text.push_str(stat.trim());
            }
            text
        }
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    git: &dyn Git,
    repo_dir: Option<PathBuf>,
) -> Result<ExitCode> {
    // Previews run on their own thread. Fetching them inline would put two
    // git subprocesses between the draw and the next `event::poll`, so every
    // move onto an unseen branch would freeze input until they finished —
    // exactly the lag the panel is supposed to avoid.
    //
    // The thread is DETACHED, not scoped. A scope joins on the way out, and a
    // `q` pressed while a preview is mid-flight would then sit on the frozen
    // alternate screen until both git commands finished — measurably as long
    // as the preview itself takes. Quitting has to be immediate, so the
    // worker owns everything it needs (hence its own `SystemGit` rather than
    // a borrow) and is simply abandoned at exit. Its git children are
    // read-only and get reaped by init.
    let (want_tx, want_rx) = channel::<(String, String)>();
    let (done_tx, done_rx) = channel::<(String, String)>();
    let base = app.base.clone();
    std::thread::spawn(move || {
        let git = SystemGit::new(repo_dir);
        while let Ok(oldest) = want_rx.recv() {
            // Serve the NEWEST request, not the oldest. Scrolling through a
            // long list queues one per branch passed, and answering them in
            // order would leave the branch actually being looked at until
            // last. Anything skipped is simply re-requested if the cursor
            // comes back to it.
            let (name, branch_ref) = want_rx.try_iter().last().unwrap_or(oldest);
            let text = preview_text(&git, &base, &branch_ref);
            if done_tx.send((name, text)).is_err() {
                return;
            }
        }
    });
    drive(terminal, app, &want_tx, &done_rx, git)
}

fn drive(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    want: &Sender<(String, String)>,
    done: &Receiver<(String, String)>,
    git: &dyn Git,
) -> Result<ExitCode> {
    // The one request currently with the worker. Tracking just this — rather
    // than every branch ever asked for — is what makes a skipped preview
    // self-healing: nothing is permanently marked as requested, so returning
    // to a branch asks again.
    let mut in_flight: Option<String> = None;
    loop {
        for (name, text) in done.try_iter() {
            if in_flight.as_deref() == Some(name.as_str()) {
                in_flight = None;
            }
            app.set_preview(name, text);
        }

        terminal.draw(|f| ui::render(f, app))?;

        if let Some(wanted) = app.pending_preview().map(str::to_string)
            && in_flight.as_deref() != Some(wanted.as_str())
            && let Some(candidate) = app.highlighted_candidate()
        {
            let _ = want.send((wanted.clone(), candidate.refname.clone()));
            in_flight = Some(wanted);
        }

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
                app.in_flight = None;
                let _ = terminal.draw(|f| ui::render(f, app));
            }
            app.finish();
            // Keys typed impatiently during the batch must not dismiss the
            // results screen the instant it appears — but a Ctrl+C hammered
            // during a slow push still means "get me out".
            while event::poll(Duration::ZERO)? {
                if let Event::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                {
                    app.should_quit = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fake::FakeGit;

    fn base() -> Base {
        Base {
            name: "origin/main".into(),
            refname: Some("refs/remotes/origin/main".into()),
            sha: "base000".into(),
        }
    }

    /// Ranges are built from FULL refnames and terminated with `--`. Both
    /// matter: a branch named `-f` or one colliding with a path must not be
    /// able to turn into a git option. FakeGit matches argv exactly, so this
    /// fixture pins the invocation.
    fn git_for(name: &str, log: Result<&str, &str>, stat: Result<&str, &str>) -> FakeGit {
        FakeGit::default()
            .on(
                &[
                    "log",
                    "--oneline",
                    "--no-decorate",
                    "--max-count=20",
                    &format!("refs/remotes/origin/main..refs/heads/{name}"),
                    "--",
                ],
                log,
            )
            .on(
                &[
                    "diff",
                    "--shortstat",
                    "--no-ext-diff",
                    "--no-textconv",
                    &format!("refs/remotes/origin/main...refs/heads/{name}"),
                    "--",
                ],
                stat,
            )
    }

    #[test]
    fn preview_shows_the_commits_the_base_lacks_and_the_size_of_the_diff() {
        let git = git_for(
            "feat",
            Ok("abc1234 add thing\ndef5678 fix thing\n"),
            Ok(" 2 files changed, 10 insertions(+), 1 deletion(-)\n"),
        );
        let text = preview_text(&git, &base(), "refs/heads/feat");
        assert!(text.contains("abc1234 add thing"), "{text}");
        assert!(text.contains("def5678 fix thing"), "{text}");
        assert!(text.contains("2 files changed"), "{text}");
    }

    #[test]
    fn a_failed_preview_becomes_panel_text_rather_than_ending_the_session() {
        // The user is mid-way through choosing what to delete; a broken
        // preview must not throw that away.
        let git = git_for("feat", Err("fatal: bad object"), Err("fatal: bad object"));
        let text = preview_text(&git, &base(), "refs/heads/feat");
        assert!(text.contains("preview unavailable"), "{text}");
        assert!(text.contains("bad object"), "{text}");
    }

    #[test]
    fn a_branch_carrying_nothing_new_says_so_instead_of_showing_a_blank_panel() {
        let git = git_for("feat", Ok(""), Ok(""));
        let text = preview_text(&git, &base(), "refs/heads/feat");
        assert!(text.contains("no commits"), "{text}");
    }

    #[test]
    fn a_missing_diffstat_still_leaves_the_commit_list_usable() {
        // `git diff` can fail on its own (e.g. a corrupt index) — that must
        // cost the stat line, not the whole preview.
        let git = git_for("feat", Ok("abc1234 add thing\n"), Err("fatal: broken"));
        let text = preview_text(&git, &base(), "refs/heads/feat");
        assert!(text.contains("abc1234 add thing"), "{text}");
        assert!(!text.contains("preview unavailable"), "{text}");
    }
}
