# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-08-02

### Added

- Three-tier detection of stale branches: classic merges (ancestry),
  GitHub-style squash merges (patch-id probe via `git cherry`), and branches
  whose upstream is gone.
- Interactive ratatui TUI with checkboxes, confidence badges, per-branch
  remote-deletion toggle, confirm modal and a results screen with undo hints.
- Non-interactive modes: `--list`/`--dry-run`, `--json`, `--yes`,
  `--include-gone`, `--remote`.
- Base auto-detection (`origin/HEAD` → `origin/main` → `origin/master` →
  `main` → `master`) with `--base` override.
- Protected branches: `main`/`master`/`develop` defaults, `--protect` globs,
  `git config barber.protect`.
- Safe deletion semantics: `-d` for merged branches with a verified `-D`
  fallback, `-D` only with consent for squash/gone, remote deletion strictly
  opt-in, undo command printed for every deletion.

[Unreleased]: https://github.com/rekurt/git-barber/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rekurt/git-barber/releases/tag/v0.1.0
