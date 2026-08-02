use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Minimal seam over the `git` binary. Everything the tool learns about a
/// repository flows through this trait, so tests can substitute a fake.
pub trait Git {
    /// Run git with `args`; returns stdout on exit code 0, error otherwise.
    fn run(&self, args: &[&str]) -> Result<String>;

    /// Run git where a non-zero exit is an expected answer (e.g.
    /// `rev-parse --verify`). Returns `(success, stdout-or-stderr)`.
    fn try_run(&self, args: &[&str]) -> Result<(bool, String)>;
}

/// Real implementation shelling out to the system `git`.
pub struct SystemGit {
    dir: Option<PathBuf>,
}

impl SystemGit {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        if let Some(dir) = &self.dir {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args);
        // Never hang on interactive credential prompts; fail fast instead.
        // Critical while the TUI owns the terminal.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if args.first() == Some(&"commit-tree") {
            // The squash probe creates a dangling commit; a fixed identity
            // keeps it working in repos with no user.name/email configured
            // and makes probe OIDs deterministic.
            cmd.env("GIT_AUTHOR_NAME", "git-barber");
            cmd.env("GIT_AUTHOR_EMAIL", "barber@localhost");
            cmd.env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z");
            cmd.env("GIT_COMMITTER_NAME", "git-barber");
            cmd.env("GIT_COMMITTER_EMAIL", "barber@localhost");
            cmd.env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z");
        }
        cmd
    }

    fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        self.command(args).output().with_context(|| {
            format!(
                "failed to launch `git {}` — is git on PATH?",
                args.join(" ")
            )
        })
    }
}

impl Git for SystemGit {
    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            bail!(
                "`git {}` failed ({}): {}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn try_run(&self, args: &[&str]) -> Result<(bool, String)> {
        let out = self.output(args)?;
        let text = if out.status.success() {
            String::from_utf8_lossy(&out.stdout).into_owned()
        } else {
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        Ok((out.status.success(), text))
    }
}

#[cfg(test)]
pub mod fake {
    use super::Git;
    use anyhow::{Result, bail};
    use std::collections::HashMap;

    /// Canned-response fake: maps a full argv to Ok(stdout) or Err(stderr).
    #[derive(Default)]
    pub struct FakeGit {
        responses: HashMap<Vec<String>, Result<String, String>>,
    }

    impl FakeGit {
        pub fn on(mut self, args: &[&str], response: Result<&str, &str>) -> Self {
            self.responses.insert(
                args.iter().map(|s| s.to_string()).collect(),
                response.map(str::to_string).map_err(str::to_string),
            );
            self
        }
    }

    impl Git for FakeGit {
        fn run(&self, args: &[&str]) -> Result<String> {
            match self.try_run(args)? {
                (true, out) => Ok(out),
                (false, err) => bail!("`git {}` failed: {err}", args.join(" ")),
            }
        }

        fn try_run(&self, args: &[&str]) -> Result<(bool, String)> {
            match self
                .responses
                .get(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            {
                Some(Ok(out)) => Ok((true, out.clone())),
                Some(Err(err)) => Ok((false, err.clone())),
                None => bail!("FakeGit: unexpected call `git {}`", args.join(" ")),
            }
        }
    }
}
