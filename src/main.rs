mod cli;
mod git;
mod output;
mod scan;

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
        Mode::Execute => anyhow::bail!("--yes is not implemented yet"),
        Mode::Tui => anyhow::bail!("the TUI is not implemented yet; try --list"),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
