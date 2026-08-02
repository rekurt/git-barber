# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-02

### Added

- Four-tier detection of stale branches: classic merges (ancestry),
  GitHub-style squash merges and rebase merges (`git patch-id --stable`
  probes, pure reads — works on read-only repositories), and branches whose
  upstream is gone.
- Interactive ratatui TUI with checkboxes, confidence badges, per-branch
  remote-deletion toggle, severity-ordered confirm modal, scrollable results
  screen; the full report with undo commands is reprinted to the normal
  terminal after exit.
- Non-interactive modes: `--list`/`--dry-run`, `--json`, `--yes`,
  `--include-gone`, `--remote`.
- Base auto-detection (`origin/HEAD` → `origin/main` → `origin/master` →
  `main` → `master`) with `--base` override and full-refname safety checks.
- Protected branches: `main`/`master`/`develop` defaults, multi-star
  `--protect` globs, `git config barber.protect`; the current branch, the
  base, its local twin and worktree-held branches are always excluded.
- Safe deletion semantics: compare-then-delete (refuses when the tip moved
  since scan), `-d` for merged branches with a verified `-D` fallback, `-D`
  only with consent for squash/rebase/gone, leased remote deletion
  (`--force-with-lease`) strictly opt-in, undo command printed for every
  deletion.

[Unreleased]: https://github.com/rekurt/git-barber/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rekurt/git-barber/releases/tag/v0.1.0
