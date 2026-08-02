mod cli;
mod git;
mod ops;
mod output;
mod scan;
mod tui;

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use crate::cli::{Cli, Mode};
use crate::git::{Git, SystemGit};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    let git = SystemGit::new(cli.dir.clone());
    // Fail early with a clear message when we're not inside a repository.
    git.run(&["rev-parse", "--git-dir"])?;

    if cli.fetch {
        // Before the TUI takes the terminal, so credential helpers can prompt.
        git.run(&["fetch", "--prune", "--quiet", "origin"])?;
    }

    let scan = scan::scan(&git, cli.base.as_deref(), &cli.protect)?;

    match cli.mode() {
        Mode::List => {
            if cli.json {
                println!("{}", output::json_list(&scan)?);
            } else {
                print!("{}", output::human_list(&scan, now_unix()));
            }
            Ok(ExitCode::SUCCESS)
        }
        Mode::Execute => {
            let plans: Vec<ops::PlannedDeletion> = scan
                .candidates
                .iter()
                .filter(|c| {
                    c.selected_by_default() || (cli.include_gone && c.kind == scan::MergeKind::Gone)
                })
                .map(|c| ops::PlannedDeletion {
                    candidate: c.clone(),
                    delete_remote: cli.remote,
                })
                .collect();

            if plans.is_empty() {
                // Nothing auto-deletable; still show what exists (gone-only
                // repos get the list plus the consent tip).
                if cli.json {
                    println!("{}", output::json_execute(&scan.base, &[])?);
                } else {
                    print!("{}", output::human_list(&scan, now_unix()));
                }
                return Ok(ExitCode::SUCCESS);
            }

            let results = ops::execute(&git, &scan.base, &plans);
            if cli.json {
                println!("{}", output::json_execute(&scan.base, &results)?);
            } else {
                print!("{}", output::human_execute(&scan.base, &results));
            }
            Ok(if results.iter().any(ops::DeletionResult::failed) {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Mode::Tui => {
            if scan.candidates.is_empty() {
                // Don't take over the terminal just to say there's no work.
                print!("{}", output::human_list(&scan, now_unix()));
                return Ok(ExitCode::SUCCESS);
            }
            tui::run(&git, scan, cli.remote, now_unix())
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
