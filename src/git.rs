use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Minimal seam over the `git` binary. Everything the tool learns about a
/// repository flows through this trait, so tests can substitute a fake.
///
/// `Sync` because the scan probes branches from several worker threads; every
/// implementation is read-only shared state, so this costs nothing.
pub trait Git: Sync {
    /// Run git with `args`; returns stdout on exit code 0, error otherwise.
    fn run(&self, args: &[&str]) -> Result<String>;

    /// Run git where a non-zero exit is an expected answer (e.g.
    /// `rev-parse --verify`). Returns `(success, stdout-or-stderr)`.
    fn try_run(&self, args: &[&str]) -> Result<(bool, String)>;

    /// Run git with `input` piped to stdin (e.g. `git patch-id`).
    fn run_with_input(&self, args: &[&str], input: &str) -> Result<String>;
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
        // An inherited GIT_DIR would silently override `-C` and point every
        // command at the ambient repository (hooks, `rebase -x`, IDE tasks).
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
        ] {
            cmd.env_remove(var);
        }
        // Never hang on credential prompts while the TUI owns the terminal.
        // `fetch` is the exception: it runs before the TUI starts, and the
        // user explicitly asked for network access, so prompting is fine.
        if args.first() != Some(&"fetch") {
            cmd.env("GIT_TERMINAL_PROMPT", "0");
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

    fn run_with_input(&self, args: &[&str], input: &str) -> Result<String> {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to launch `git {}` — is git on PATH?",
                    args.join(" ")
                )
            })?;
        // Feed stdin from a thread so a filled stdout pipe can't deadlock us.
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let payload = input.to_string();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes()); // EPIPE = child exited early; fine
        });
        let out = child.wait_with_output()?;
        let _ = writer.join();
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
}

#[cfg(test)]
pub mod fake {
    use super::Git;
    use anyhow::{Result, bail};
    use std::collections::HashMap;

    /// Canned-response fake: maps a full argv (and optionally stdin) to
    /// Ok(stdout) or Err(stderr).
    #[derive(Default)]
    pub struct FakeGit {
        responses: HashMap<Vec<String>, Result<String, String>>,
        input_responses: HashMap<(Vec<String>, String), Result<String, String>>,
    }

    impl FakeGit {
        pub fn on(mut self, args: &[&str], response: Result<&str, &str>) -> Self {
            self.responses.insert(
                args.iter().map(|s| s.to_string()).collect(),
                response.map(str::to_string).map_err(str::to_string),
            );
            self
        }

        /// Canned response for `run_with_input` matched on (args, exact stdin).
        pub fn on_input(
            mut self,
            args: &[&str],
            input: &str,
            response: Result<&str, &str>,
        ) -> Self {
            self.input_responses.insert(
                (
                    args.iter().map(|s| s.to_string()).collect(),
                    input.to_string(),
                ),
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

        fn run_with_input(&self, args: &[&str], input: &str) -> Result<String> {
            let key = (
                args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                input.to_string(),
            );
            match self.input_responses.get(&key) {
                Some(Ok(out)) => Ok(out.clone()),
                Some(Err(err)) => bail!("`git {}` failed: {err}", args.join(" ")),
                None => bail!(
                    "FakeGit: unexpected call `git {}` with input {:?}",
                    args.join(" "),
                    input
                ),
            }
        }
    }
}
