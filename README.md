# git-barber

[![CI](https://github.com/rekurt/git-barber/actions/workflows/ci.yml/badge.svg)](https://github.com/rekurt/git-barber/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/git-barber.svg)](https://crates.io/crates/git-barber)
![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)

**git-barber trims your stale branches.** It finds local branches that are
already merged — including GitHub-style **squash** and **rebase** merges that
`git branch --merged` can't see — shows them in a TUI with safe defaults, and
deletes the ones you confirm. Optionally it deletes their remote counterparts
too, with a lease so nobody's fresh commits are ever swept away.

![demo](https://raw.githubusercontent.com/rekurt/git-barber/master/assets/demo.gif)

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
git barber --yes        # delete all verified candidates, no questions
git barber --yes --json # ...with a JSON report of what happened
```

### How branches are classified

| badge    | meaning                                                               | confidence | default   |
|----------|-----------------------------------------------------------------------|------------|-----------|
| `merged` | tip is an ancestor of the base branch                                 | high       | selected  |
| `squash` | the branch, squashed into one diff, matches a patch already in base   | high       | selected  |
| `rebase` | every branch commit's patch already exists in base individually       | high       | selected  |
| `gone`   | the tracked upstream was deleted; the branch itself may be unmerged   | low        | **not** selected |

Squash and rebase detection use `git patch-id --stable`: the whole-branch
diff (what a GitHub *Squash and merge* lands) and the per-commit patches
(what *Rebase and merge* lands) are compared against everything the base
gained since the fork point. Detection is pure reads — no objects are
written, so `--list` works even on read-only repositories.

The base branch is `origin/HEAD` when set, then `origin/main`,
`origin/master`, `main`, `master`; override with `--base <ref>`.

### TUI keys

`j`/`k`/arrows move · `g`/`G` top/bottom · `space` toggle · `a` all ·
`n` none · `r` arm remote deletion for the highlighted branch ·
`enter` confirm · in the dialog `y` executes, `n`/`q`/`esc` cancel ·
`q`/`Ctrl+C` quit.
On the results screen `j`/`k` scroll; the full report (with undo commands)
is reprinted to the normal terminal after exit, so nothing is lost with the
alternate screen. Deletions run synchronously; the title shows the branch
in flight.

### Flags

| flag             | effect                                                              |
|------------------|---------------------------------------------------------------------|
| `--base <REF>`   | compare against this ref instead of the auto-detected base          |
| `--list`         | print candidates and exit; alias `--dry-run`; conflicts with `--yes`|
| `--json`         | JSON output, for scripts                                            |
| `--yes`          | non-interactive deletion of merged + squash + rebase candidates     |
| `--include-gone` | with `--yes`: also delete `gone` branches (requires `--yes`)        |
| `--remote`       | also delete remote counterparts; in the TUI pre-arms the `r` toggle |
| `--protect <P>`  | extra protected names/globs, e.g. `--protect 'release/*'`           |
| `--fetch`        | run `git fetch --prune` first (non-fatal when offline)              |
| `-C <PATH>`      | operate on a repository at PATH                                     |

Exit codes: `0` ok / nothing to do · `1` some deletions failed · `2` usage or
environment error.

### Protected branches

`main`, `master` and `develop` are never touched, along with the current
branch, the base, its local twin, and branches checked out in any worktree.
Add your own patterns per repository:

```bash
git config --add barber.protect 'release/*'
```

or per invocation with `--protect`. Globs support any number of `*`
wildcards (`release/*`, `*-keep-*`).

## Safety

- **Compare-then-delete.** Deletion verifies the branch tip still equals the
  sha you saw at scan time; a branch that advanced meanwhile (another
  terminal, an editor's git integration) is refused with a "rescan" hint.
- Merged branches use plain `git branch -d`. When git refuses only because
  it judges merged-ness against HEAD rather than the base, git-barber
  re-verifies the ancestry itself and retries with `-D`. Every forced
  deletion is labeled with its justification: `verified merged into base`,
  `patch-id verified`, or `upstream was gone`.
- Squash/rebase candidates are verified by patch-id and selected by default;
  **gone candidates never are** — they need a manual check in the TUI or an
  explicit `--include-gone`.
- **Remote deletion is leased.** `--remote` (or the per-branch `r` toggle)
  deletes with `--force-with-lease` against your tracking ref: if anyone
  pushed after your last fetch, the deletion is rejected. The confirm dialog
  shows the *actual* remote ref being deleted, which can differ from the
  local branch name.
- Every deletion prints an undo command
  (`git branch <name> <sha>` / `git push origin <sha>:refs/heads/<name>`),
  and the report survives the TUI (reprinted after exit).
- Scanning never touches the network unless you pass `--fetch` (remote
  *deletion*, when you ask for it, obviously pushes). Stale remote refs make
  detection more conservative and make leased remote deletion refuse —
  never the other way around.

Known limitations: a squash merge whose conflicts were resolved during the
merge produces a different patch and is not detected (the branch usually
surfaces as `gone` instead); shallow clones disable squash/rebase detection;
`Ctrl+Z` suspend is not handled; SIGKILL during the TUI can leave the
terminal in raw mode (`reset` fixes it).

## Comparison

- `git branch --merged | xargs git branch -d` — misses squash and rebase
  merges, no confirmation step, no undo hints.
- [git-trim](https://crates.io/crates/git-trim) — similar detection ideas,
  more configuration surface, no TUI, libgit2-based; last released in 2022.

git-barber shells out to your system `git` (like `gh` does), so hooks and
config behave the way your git does. Two deliberate exceptions: repo-selecting
environment variables (`GIT_DIR` & co) are cleared so `-C` always wins, and
interactive credential prompts are disabled while the TUI owns the terminal —
use `--fetch` (which may prompt) or a credential helper for remotes that
need auth.

## Development

```bash
cargo test          # unit + e2e (e2e builds real throwaway repos in tempdirs)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The codebase is deliberately small: `scan.rs` (detection), `ops.rs`
(deletion), `tui/` (pure state machine + rendering), `git.rs` (the
subprocess seam, faked in tests). Start reading at `src/main.rs`.

## License

MIT or Apache-2.0, at your option.
