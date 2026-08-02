# git-barber

[![CI](https://github.com/rekurt/git-barber/actions/workflows/ci.yml/badge.svg)](https://github.com/rekurt/git-barber/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/git-barber.svg)](https://crates.io/crates/git-barber)
![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)

**git-barber trims your stale branches.** It finds local branches that are already
merged — including GitHub-style **squash merges** that `git branch --merged`
can't see — shows them in a TUI with safe defaults, and deletes the ones you
confirm. Optionally it deletes their remote counterparts too.

![demo](assets/demo.gif)

## Install

```bash
cargo install git-barber
```

The binary is named `git-barber`, so git picks it up as a subcommand: run it
as `git barber`.

## Usage

```bash
git barber              # TUI: pick branches, confirm, done
git barber --list       # dry-run listing (also the default outside a TTY)
git barber --json       # machine-readable listing
git barber --yes        # delete all high-confidence candidates, no questions
git barber --yes --json # ...with a JSON report of what happened
```

### How branches are classified

| badge    | meaning                                                                | confidence | default   |
|----------|------------------------------------------------------------------------|------------|-----------|
| `merged` | tip is an ancestor of the base branch                                  | high       | selected  |
| `squash` | the branch, squashed into one commit, matches a patch already in base  | high       | selected  |
| `gone`   | the tracked upstream was deleted; the branch itself may be unmerged    | low        | **not** selected |

Squash detection uses the patch-id technique: the whole branch is collapsed
into a synthetic commit on top of the merge-base — exactly the diff a GitHub
*Squash and merge* lands — and `git cherry` checks whether an equivalent patch
already exists in base.

The base branch is `origin/HEAD` when set, then `origin/main`, `origin/master`,
`main`, `master`; override with `--base <ref>`.

### Flags

| flag             | effect                                                             |
|------------------|--------------------------------------------------------------------|
| `--base <REF>`   | compare against this ref instead of the auto-detected base         |
| `--list`         | print candidates and exit (never deletes); alias `--dry-run`       |
| `--json`         | JSON output, for scripts                                           |
| `--yes`          | non-interactive deletion of merged + squash candidates             |
| `--include-gone` | with `--yes`: also delete `gone` branches                          |
| `--remote`       | also delete remote counterparts (`git push origin --delete ...`)   |
| `--protect <P>`  | extra protected names/globs, e.g. `--protect 'release/*'`          |
| `--fetch`        | run `git fetch --prune origin` before scanning                     |
| `-C <PATH>`      | operate on a repository at PATH                                    |

Exit codes: `0` ok / nothing to do · `1` some deletions failed · `2` usage or
environment error.

### Protected branches

`main`, `master` and `develop` are never touched, along with the current
branch and the base. Add your own patterns per repository:

```bash
git config --add barber.protect 'release/*'
```

or per invocation with `--protect`. Patterns support a single `*` wildcard.

## Safety

- Merged branches are deleted with plain `git branch -d`.
- `git branch -d` judges "merged" against *HEAD or the upstream*, not against
  the base. When it refuses but the tip **is** an ancestor of the base,
  git-barber re-verifies that itself and retries with `-D`, reporting
  `force-deleted (verified merged)`.
- Squash and gone branches require `-D` by nature. Squash candidates are
  selected by default (high confidence); **gone candidates never are** — they
  need a manual check in the TUI or an explicit `--include-gone`.
- Remote deletion is always opt-in (`--remote` or the per-branch `r` toggle)
  and shown in red in the confirm dialog.
- Every deletion prints an undo command
  (`git branch <name> <sha>` / `git push origin <sha>:refs/heads/<name>`).
- No network access unless you pass `--fetch`. Stale remote refs only make
  detection more conservative, never more aggressive.

Known limitation: a squash merge whose conflicts were resolved during the
merge produces a different patch and is not detected as `squash` — the branch
survives (usually surfacing as `gone` instead, since the PR branch was
deleted). Shallow clones disable squash detection.

## Comparison

- `git branch --merged | xargs git branch -d` — misses squash merges, no
  confirmation step.
- [git-trim](https://crates.io/crates/git-trim) — similar detection, more
  configuration surface, no TUI, libgit2-based.
- [git-broom](https://crates.io/crates/git-broom) — TUI for local branches,
  no squash detection, no remote cleanup, no JSON.

git-barber shells out to your system `git` (like `gh` does), so hooks,
config and credential helpers behave exactly as they do on your command line.

## Development

```bash
cargo test          # unit + e2e (e2e builds real throwaway repos in tempdirs)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The codebase is deliberately small (~1500 LOC): `scan.rs` (detection),
`ops.rs` (deletion), `tui/` (app state machine + rendering), `git.rs` (the
subprocess seam, faked in tests). Start reading at `src/main.rs`.

## License

MIT or Apache-2.0, at your option.
