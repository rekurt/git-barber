use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Parser;
use clap_complete::Shell;

/// Trim stale merged git branches — classic merges, squash merges, and
/// branches whose upstream is gone. Runs a TUI by default; use --list or
/// --yes for scripts.
#[derive(Parser, Debug)]
#[command(name = "git-barber", bin_name = "git barber", version, about)]
pub struct Cli {
    /// Base branch to compare against (default: origin/HEAD, then origin/main,
    /// origin/master, main, master)
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Print candidates and exit without deleting (implied when not a TTY)
    #[arg(long, visible_alias = "dry-run", conflicts_with = "yes")]
    pub list: bool,

    /// Machine-readable JSON output (implies --list unless combined with --yes)
    #[arg(long)]
    pub json: bool,

    /// Delete all default-selected candidates without the TUI (for scripts/CI)
    #[arg(long)]
    pub yes: bool,

    /// Also delete the remote counterpart of each deleted branch (destructive:
    /// affects everyone using the remote). In the TUI this pre-arms the `r`
    /// toggle on local candidates with a live upstream. Remote-only merged
    /// branches are always shown and still need manual selection.
    #[arg(long)]
    pub remote: bool,

    /// With --yes: also delete branches whose upstream is gone
    #[arg(long, requires = "yes")]
    pub include_gone: bool,

    /// Extra protected branch names or globs like 'release/*'
    /// (repeatable or comma-separated; adds to main/master/develop and
    /// `git config barber.protect`)
    #[arg(long, value_name = "PATTERN", value_delimiter = ',')]
    pub protect: Vec<String>,

    /// Run `git fetch --prune` before scanning (non-fatal when offline)
    #[arg(long)]
    pub fetch: bool,

    /// Run as if started in PATH (passed to `git -C`)
    #[arg(short = 'C', value_name = "PATH")]
    pub dir: Option<PathBuf>,

    /// Ignore and do not update the verdict cache in `.git/barber`, forcing
    /// every branch to be re-verified from scratch
    #[arg(long)]
    pub no_cache: bool,

    /// Print a completion script for SHELL to stdout and exit
    #[arg(long, value_name = "SHELL")]
    pub completions: Option<Shell>,

    /// Print the roff man page to stdout and exit (used by the release build)
    #[arg(long, hide = true)]
    pub man: bool,
}

/// The name completions and the man page are generated for. Deliberately
/// NOT clap's `bin_name` ("git barber"): a completion script has to register
/// against the executable on PATH, and git dispatches its own completion for
/// the subcommand form.
pub const BIN_NAME: &str = "git-barber";

/// Write a shell completion script to `out`.
pub fn completions(shell: Shell, out: &mut dyn std::io::Write) {
    use clap::CommandFactory;
    clap_complete::generate(shell, &mut Cli::command(), BIN_NAME, out);
}

/// Write the roff man page to `out`.
pub fn man(out: &mut dyn std::io::Write) -> std::io::Result<()> {
    use clap::CommandFactory;
    clap_mangen::Man::new(Cli::command().name(BIN_NAME)).render(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Print a completion script or man page; never touches a repository.
    Generate,
    /// Interactive TUI (default in a terminal).
    Tui,
    /// Read-only listing, human or JSON.
    List,
    /// Non-interactive deletion (--yes).
    Execute,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        self.mode_with(tty)
    }

    fn mode_with(&self, tty: bool) -> Mode {
        if self.completions.is_some() || self.man {
            Mode::Generate
        } else if self.yes {
            Mode::Execute
        } else if self.list || self.json || !tty {
            Mode::List
        } else {
            Mode::Tui
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("git-barber").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn verify_cli() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn default_is_tui_in_tty_and_list_otherwise() {
        let cli = parse(&[]);
        assert_eq!(cli.mode_with(true), Mode::Tui);
        assert_eq!(cli.mode_with(false), Mode::List);
    }

    #[test]
    fn dry_run_and_delete_flags_conflict() {
        // A --dry-run appended to a scripted --yes must error out loudly,
        // never silently delete.
        assert!(Cli::try_parse_from(["git-barber", "--yes", "--list"]).is_err());
        assert!(Cli::try_parse_from(["git-barber", "--yes", "--dry-run"]).is_err());
        // --json --yes stays valid: it is the JSON result report.
        let cli = parse(&["--yes", "--json"]);
        assert_eq!(cli.mode_with(true), Mode::Execute);
    }

    #[test]
    fn include_gone_requires_yes() {
        assert!(Cli::try_parse_from(["git-barber", "--include-gone"]).is_err());
        assert!(Cli::try_parse_from(["git-barber", "--yes", "--include-gone"]).is_ok());
    }

    #[test]
    fn json_and_list_force_listing() {
        assert_eq!(parse(&["--json"]).mode_with(true), Mode::List);
        assert_eq!(parse(&["--list"]).mode_with(true), Mode::List);
        assert_eq!(parse(&["--dry-run"]).mode_with(true), Mode::List);
    }

    #[test]
    fn completions_and_man_are_a_repository_free_mode() {
        // Generating a completion script or man page must work anywhere,
        // including outside a repository, so it cannot fall through to a
        // mode that scans one. --man is checked with tty=false because the
        // release build pipes it to a file.
        assert_eq!(
            parse(&["--completions", "zsh"]).mode_with(true),
            Mode::Generate
        );
        assert_eq!(parse(&["--man"]).mode_with(false), Mode::Generate);
    }

    #[test]
    fn protect_accepts_commas_and_repeats() {
        let cli = parse(&["--protect", "release/*,staging", "--protect", "qa"]);
        assert_eq!(cli.protect, vec!["release/*", "staging", "qa"]);
    }
}
