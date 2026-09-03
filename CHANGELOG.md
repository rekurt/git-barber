# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-09-03

### Added

- Remote branches merged into the base that have no local twin are now
  scanned and listed as `remote` candidates. They are preselected in the TUI
  but always need an explicit `Enter` then `y`, and `--yes` never touches
  them — `--yes --remote` must not become a broad remote-pruning command.
  Squash- and rebase-merged remotes are detected like any other branch.
  Remote refs share the existing parallel probe, verdict cache and progress
  total rather than running a scan pass of their own.

### Changed

- `--list` marks remote-only rows `[remote-only]` and leaves them out of the
  "selected by default" count, which describes what `--yes` will delete.
- CI and release workflows moved to `actions/checkout@v7`,
  `actions/upload-artifact@v7` and `actions/download-artifact@v8`.

## [0.2.0] - 2026-08-26

### Added

- Prebuilt binaries for macOS (arm64, x86_64), Linux (arm64, x86_64, musl) and
  Windows on every tagged release, plus a Homebrew tap and `cargo binstall`
  support — installing no longer requires a Rust toolchain.
- `--completions <SHELL>` generates bash/zsh/fish/powershell completion
  scripts, and a man page is shipped in the release archives. Both work
  outside a repository.
- Parallel scanning across `barber.jobs` workers (default: CPU count, capped
  at 8) and a cross-run verdict cache in `.git/barber/cache.json`, keyed on
  the base tip, fork point and branch tip so a moved branch is always probed
  again. A 200-branch repository scans in ~5s instead of ~25s, or ~1.6s warm.
- A progress counter on stderr while scanning, shown only when stderr is a
  terminal so piped output is unaffected.
- `--no-cache` ignores and does not update the verdict cache, forcing every
  branch to be re-verified from scratch.
- A commit-preview panel in the TUI: the commits the highlighted branch has
  that the base does not, plus its diffstat. Beside the list at 100 columns or
  wider, underneath below that, lazily loaded and cached per branch. Toggle
  with `p`; disable with `git config barber.preview false`.

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

[Unreleased]: https://github.com/rekurt/git-barber/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/rekurt/git-barber/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/rekurt/git-barber/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rekurt/git-barber/releases/tag/v0.1.0
