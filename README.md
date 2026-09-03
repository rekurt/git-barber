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

From 0.2.0 onwards, Homebrew (macOS and Linux):

```bash
brew install rekurt/tap/git-barber
```

Or a prebuilt binary, no Rust toolchain needed — grab the archive for your
platform from the
[latest release](https://github.com/rekurt/git-barber/releases/latest) and put
`git-barber` on your `PATH`. Archives also ship the completions and the man
page.

With an existing Rust setup, `cargo binstall` downloads the same prebuilt
binary instead of compiling:

```bash
cargo binstall git-barber
```

Or build from source:

```bash
cargo install git-barber
```

The binary is named `git-barber`, so git picks it up as a subcommand: run it
as `git barber`.

### Shell completions

Homebrew installs completions and the man page for you. Otherwise generate
them yourself — for zsh:

```bash
mkdir -p ~/.zfunc && git-barber --completions zsh > ~/.zfunc/_git-barber
```

then, in `.zshrc` before `compinit` runs:

```bash
fpath+=~/.zfunc
autoload -U compinit && compinit
```

`bash`, `zsh`, `fish`, `powershell` and `elvish` are supported. Completions
register against the `git-barber` executable — as `git barber`, git drives its
own completion machinery instead.

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
`p` show/hide the preview panel ·
`enter` confirm · in the dialog `y` executes, `n`/`q`/`esc` cancel ·
`q`/`Ctrl+C` quit.

The preview panel shows the commits the highlighted branch carries that the
base does not, plus its diffstat — so you can see what a deletion would
actually cost before confirming it. It sits beside the list on terminals 100
columns or wider and underneath on narrower ones, loads lazily as the cursor
moves, and remembers what it has already fetched. Turn it off with
`git config barber.preview false`.

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
| `--remote`       | also delete remote counterparts of local branches; pre-arms the `r` toggle |
| `--protect <P>`  | extra protected names/globs, e.g. `--protect 'release/*'`           |
| `--fetch`        | run `git fetch --prune` first (non-fatal when offline)              |
| `-C <PATH>`      | operate on a repository at PATH                                     |
| `--no-cache`     | ignore and do not update the verdict cache; re-verify everything     |
| `--completions <SHELL>` | print a shell completion script and exit; needs no repository |

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

## Speed

Detection is dominated by the patch-id probes, which are independent per
branch, so the scan runs them across several workers and shows a counter on
stderr while it works (a terminal only — piped output stays byte-for-byte
clean). Verdicts are then cached in `.git/barber/cache.json`, keyed on the
base tip, the fork point and the branch tip together: if any of the three
moved, the branch is probed again rather than trusted. Only entries the last
run actually used are written back, so the file stays the size of your branch
list.

Measured on a synthetic 200-branch repository (30 distinct fork points, every
branch squash-merged), on an 18-core machine with git 2.50.1 — treat the
ratios as the point, not the absolute seconds:

| workers | cold  | warm cache |
|---------|-------|------------|
| 1       | 11.1s | —          |
| 4       | 4.6s  | —          |
| 8       | 3.5s  | 1.3s       |

```bash
git config barber.jobs 4   # default: CPU count, capped at 8
```

A repository you cannot write to simply pays full price every scan — the
cache is an optimisation and never an error. To force a full re-verification,
pass `--no-cache` or delete `.git/barber`.

The cache pays off most for repeated runs against an unchanged base. When the
base moves — after any `git fetch` that brings in new commits — every verdict
is recomputed, because what the base gained since the fork point is exactly
what squash and rebase detection compares against.

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
- **Remote-only cleanup is manual.** Branches that are merged into the base
  but have no local twin always appear as preselected `remote` candidates.
  Press `Enter` and answer `y` to delete, or `n` to leave them intact. They
  are never removed by `--yes`.
- Every deletion prints an undo command
  (`git branch <name> <sha>` / `git push origin <sha>:refs/heads/<name>`),
  and the report survives the TUI (reprinted after exit).
- **The verdict cache never widens what gets deleted.** It is keyed on the
  base tip, the fork point and the branch tip together, and every probe is
  pinned to those exact shas rather than to a ref that could move mid-scan.
  Any of the three moving is a miss. `--no-cache`, or deleting `.git/barber`,
  forces a full re-verification.
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
interactive credential prompts are disabled for every git call except
`--fetch` — remote deletion needs a non-interactive credential helper
configured, and fails fast instead of hanging when there is none.

## Development

```bash
cargo test          # unit + e2e (e2e builds real throwaway repos in tempdirs)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The codebase is deliberately small: `scan.rs` (detection), `ops.rs`
(deletion), `tui/` (pure state machine + rendering), `git.rs` (the
subprocess seam, faked in tests), `cache.rs` (cross-run verdict cache),
`parallel.rs` (ordered parallel map), `progress.rs` (the scan counter) and
`config.rs` (`git config barber.*`). Start reading at `src/main.rs`.

## License

MIT or Apache-2.0, at your option.
