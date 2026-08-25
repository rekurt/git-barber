mod cache;
mod cli;
mod config;
mod git;
mod ops;
mod output;
mod parallel;
mod progress;
mod scan;
mod tui;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use crate::cache::Cache;
use crate::cli::{Cli, Mode};
use crate::git::{Git, SystemGit};
use crate::progress::{NullReporter, Reporter, WriteReporter};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mode = cli.mode();
    let result = match mode {
        // Pure renderings of the CLI definition. Dispatched here, before
        // anything constructs a Git or looks for a repository: a shell
        // sources completions at startup, from wherever it happens to be.
        Mode::Generate => generate(&cli),
        _ => run(&cli, mode),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn generate(cli: &Cli) -> anyhow::Result<ExitCode> {
    let mut out = std::io::stdout().lock();
    match cli.completions {
        Some(shell) => cli::completions(shell, &mut out),
        None => cli::man(&mut out)?,
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

fn run(cli: &Cli, mode: Mode) -> anyhow::Result<ExitCode> {
    let git = SystemGit::new(cli.dir.clone());
    // Fail early with a clear message when we're not inside a repository.
    git.run(&["rev-parse", "--git-dir"])?;

    // Fetch runs before the TUI takes the terminal, so credential helpers can
    // still prompt. Offline must not block a purely local scan.
    let mut fetch_warning = None;
    if cli.fetch
        && let Err(err) = git.run(&["fetch", "--prune", "--quiet"])
    {
        let warning = format!("fetch failed, scanning with possibly stale remote refs: {err:#}");
        eprintln!("warning: {warning}");
        fetch_warning = Some(warning);
    }

    // Progress goes to stderr, and only to a terminal: `--json` piped into
    // a script has to stay byte-for-byte clean.
    let reporter: Box<dyn Reporter> = if std::io::stderr().is_terminal() {
        Box::new(WriteReporter::new(std::io::stderr()))
    } else {
        Box::new(NullReporter)
    };
    // Verdicts survive between runs, keyed on the shas they were computed
    // from. A repository we cannot write to just pays full price every time,
    // and `--no-cache` opts out entirely: this is what authorises a force
    // delete, so there has to be a way to demand a cold re-verification.
    let git_dir = if cli.no_cache {
        None
    } else {
        git.run(&["rev-parse", "--absolute-git-dir"])
            .map(|d| PathBuf::from(d.trim()))
            .ok()
    };
    let mut cache = git_dir.as_deref().map(Cache::load).unwrap_or_default();

    let options = scan::Options {
        base: cli.base.as_deref(),
        protect: &cli.protect,
        jobs: config::jobs(&git),
    };
    let mut scan = scan::scan(&git, &options, reporter.as_ref(), &mut cache)?;
    if let Some(dir) = git_dir.as_deref() {
        cache.save(dir);
    }
    scan.warnings.extend(fetch_warning);

    match mode {
        Mode::Generate => unreachable!("dispatched in main() before the repo check"),
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
                    println!("{}", output::json_execute(&scan, &[])?);
                } else {
                    print!("{}", output::human_list(&scan, now_unix()));
                }
                return Ok(ExitCode::SUCCESS);
            }

            let results = ops::execute(&git, &scan.base, &plans);
            if cli.json {
                println!("{}", output::json_execute(&scan, &results)?);
            } else {
                print!(
                    "{}",
                    output::human_execute(&scan.base.name, &scan.warnings, &results)
                );
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
            tui::run(&git, scan, cli.remote, now_unix(), config::preview(&git))
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
