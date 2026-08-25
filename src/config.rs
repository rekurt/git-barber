//! Repository-level settings, read from `git config`.
//!
//! git config is the only settings store on purpose: it is what users already
//! know, it already supports per-repo and global scopes, and it means there
//! is no bespoke file format to document or parse.

use crate::git::Git;

/// Upper bound on scan workers. Each one forks `git` subprocesses, so an
/// unbounded value from config would fork-bomb a big repository rather than
/// speed it up.
pub const MAX_JOBS: usize = 8;

/// Worker threads for the scan: `barber.jobs`, else the machine's
/// parallelism, capped at [`MAX_JOBS`] and never below 1.
pub fn jobs(git: &dyn Git) -> usize {
    let configured = get(git, "barber.jobs").and_then(|v| v.parse::<usize>().ok());
    let jobs = match configured {
        Some(n) if n > 0 => n,
        // Values that cannot mean anything sensible (0, negative, garbage)
        // fall back rather than failing the scan over a setting.
        _ => std::thread::available_parallelism().map_or(1, |n| n.get()),
    };
    jobs.clamp(1, MAX_JOBS)
}

/// Whether the TUI opens with the commit-preview panel showing:
/// `barber.preview`, on unless explicitly disabled.
pub fn preview(git: &dyn Git) -> bool {
    get(git, "barber.preview").is_none_or(|v| bool_value(&v).unwrap_or(true))
}

/// git's own boolean spelling, parsed here rather than via
/// `git config --type=bool` so that a git too old for `--type` still honours
/// the setting instead of silently ignoring it.
fn bool_value(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// A single-valued config key, or None when unset or empty.
fn get(git: &dyn Git, key: &str) -> Option<String> {
    match git.try_run(&["config", "--get", key]) {
        Ok((true, out)) => Some(out.trim().to_string()).filter(|v| !v.is_empty()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fake::FakeGit;

    #[test]
    fn the_preview_panel_is_on_until_config_turns_it_off() {
        // Seeing what you are about to delete is the whole point of the
        // panel, so hiding it takes an explicit opt-out.
        let git = FakeGit::default().on(&["config", "--get", "barber.preview"], Err(""));
        assert!(preview(&git), "unset must leave the panel on");

        // git's own boolean spelling, all of it — not just "false".
        for off in ["false\n", "0\n", "no\n", "off\n", "FALSE\n"] {
            let git = FakeGit::default().on(&["config", "--get", "barber.preview"], Ok(off));
            assert!(!preview(&git), "{off:?} must switch the panel off");
        }
        for on in ["true\n", "1\n", "yes\n", "on\n", "\n"] {
            let git = FakeGit::default().on(&["config", "--get", "barber.preview"], Ok(on));
            assert!(preview(&git), "{on:?} must leave the panel on");
        }
        // A value that means nothing must not silently disable a feature.
        let git = FakeGit::default().on(&["config", "--get", "barber.preview"], Ok("banana\n"));
        assert!(preview(&git), "an unparseable value must fall back to on");
    }

    #[test]
    fn jobs_is_read_from_config_and_always_usable() {
        let git = FakeGit::default().on(&["config", "--get", "barber.jobs"], Ok("4\n"));
        assert_eq!(jobs(&git), 4);
    }

    #[test]
    fn unset_or_unusable_jobs_falls_back_to_a_sane_default() {
        // Never zero: a zero would mean "scan nothing".
        for response in [Err(""), Ok("\n"), Ok("nonsense\n"), Ok("0\n"), Ok("-3\n")] {
            let git = FakeGit::default().on(&["config", "--get", "barber.jobs"], response);
            let jobs = jobs(&git);
            assert!(
                (1..=MAX_JOBS).contains(&jobs),
                "got {jobs} for {response:?}"
            );
        }
    }

    #[test]
    fn jobs_is_capped_so_one_repo_cannot_fork_a_thousand_gits() {
        let git = FakeGit::default().on(&["config", "--get", "barber.jobs"], Ok("999\n"));
        assert_eq!(jobs(&git), MAX_JOBS);
    }
}
