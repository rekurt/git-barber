# Automatic branch cleanup after a merge

The reusable workflow verifies classic, squash and rebase merges with Git Barber
v0.3.0. It removes a remote branch only when its tip commit is **older than seven
days** and its changes are verified in the repository's default branch. The age
is measured from the tip's committer timestamp, not the merge date.

Default branch, `main`, `master`, `dev`, `develop`, `release/*`, `hotfix/*`, GitHub
protected branches and branches of open pull requests in this repository are
excluded. Every API page is read; API failures, missing history and verification
warnings stop deletion. Eligibility is checked again before deleting, and Git
Barber uses a lease against the scanned SHA to refuse concurrent remote changes.

Add the following file as `.github/workflows/git-barber.yml`. Replace
`WORKFLOW_COMMIT_SHA` with a reviewed commit containing `branch-cleanup.yml`.
SHA-pinned callers stay on that implementation until explicitly updated.

```yaml
name: Git Barber
on:
  pull_request:
    types: [closed]
  workflow_dispatch:
    inputs:
      dry-run:
        description: Report only; do not delete branches
        type: boolean
        default: true
permissions:
  contents: write
  pull-requests: read
jobs:
  cleanup:
    if: >-
      github.event_name == 'workflow_dispatch' ||
      (github.event.pull_request.merged == true &&
       github.event.pull_request.base.ref == github.event.repository.default_branch)
    uses: rekurt/git-barber/.github/workflows/branch-cleanup.yml@WORKFLOW_COMMIT_SHA
    with:
      dry-run: ${{ github.event_name == 'workflow_dispatch' && inputs.dry-run }}
      min-age-days: 7
```

There is no schedule. Closing an unmerged PR or merging into another branch skips
cleanup. The only additional entry point is a manual run, which defaults to a
dry-run. Additional protected globs can be supplied through the newline-separated
`protect` input. The minimum age cannot be reduced below seven days.

The reusable workflow checks out only the current default branch and pinned
tooling; it never executes PR code. It uses `GITHUB_TOKEN` with `contents: write`
and `pull-requests: read`. GitHub may restrict that token to read-only for fork PR
events; such runs can report candidates but remote deletion will fail safely.
Repository Actions policies or branch rules may also refuse deletion; they are
not bypassed.

Each run writes a Job Summary and uploads `report.json`, `summary.md` and
`undo.txt` for 30 days, including successful and partial deletions. Recovery
commands require the old Git object to remain available, for example in a local
clone that fetched the branch before deletion. Serialize cleanup runs using the
workflow's repository-specific concurrency group.

For local verification, point `GIT_BARBER_BINARY` at the v0.3.0 binary and run:

```sh
python3 -m unittest discover -s .github/scripts -p 'test_*.py' -v
```

The cleanup script deliberately refuses checkouts containing local branches.
Use a disposable detached clone; this prevents a local dry-run from modifying
your working repository's branch layout.
