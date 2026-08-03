use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Parser;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
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
        if self.yes {
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
    fn protect_accepts_commas_and_repeats() {
        let cli = parse(&["--protect", "release/*,staging", "--protect", "qa"]);
        assert_eq!(cli.protect, vec!["release/*", "staging", "qa"]);
    }
}
