use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::cache::{Cache, Key};
use crate::git::Git;
use crate::parallel;
use crate::progress::Reporter;

/// How we concluded a branch is safe to trim, ordered by confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeKind {
    /// Tip is an ancestor of the base branch (ancestry check).
    Merged,
    /// The branch squashed into one diff matches a patch already in base
    /// (GitHub "Squash and merge").
    Squash,
    /// Every branch commit's patch already exists in base individually
    /// (GitHub "Rebase and merge").
    Rebase,
    /// The tracked upstream was deleted; the branch itself may be unmerged.
    Gone,
}

/// Where a deletion candidate lives. Remote-only candidates are acted on
/// exclusively through the TUI confirmation: `--yes --remote` must not turn
/// into a broad remote-pruning command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateScope {
    Local,
    RemoteOnly,
}

/// The branch everything is compared against.
#[derive(Debug, Clone)]
pub struct Base {
    /// User-facing name (e.g. "origin/main").
    pub name: String,
    /// Full refname when the base is a ref (e.g. "refs/remotes/origin/main").
    /// None when --base was given a raw commit.
    pub refname: Option<String>,
    /// Resolved tip. Part of every cache key: everything the base gained
    /// since a fork point is exactly what squash/rebase detection compares
    /// against, so a moved base must invalidate cached verdicts.
    pub sha: String,
}

impl Base {
    /// The rev to feed to git commands. The FULL name whenever we have one:
    /// a local branch literally named "origin/main" would shadow the
    /// remote-tracking ref under gitrevisions short-name resolution.
    pub fn rev(&self) -> &str {
        self.refname.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub name: String,
    /// Full refname ("refs/heads/<name>"); unambiguous, unlike short names
    /// that can collide with tags.
    pub refname: String,
    /// Tip OID at scan time; deletion re-verifies against it (undo anchor).
    pub sha: String,
    pub kind: MergeKind,
    pub scope: CandidateScope,
    pub upstream: Option<String>,
    pub remote_name: Option<String>,
    pub upstream_gone: bool,
    pub last_commit_unix: i64,
    pub subject: String,
    /// Full upstream refname, e.g. "refs/remotes/origin/feat".
    #[serde(skip)]
    pub upstream_ref: Option<String>,
}

impl Candidate {
    /// Merged/squash/rebase are verified; gone branches need a human opt-in.
    pub fn selected_by_default(&self) -> bool {
        self.kind != MergeKind::Gone
    }

    /// Whether deletion requires `git branch -D` rather than `-d`.
    pub fn needs_force(&self) -> bool {
        self.kind != MergeKind::Merged
    }
}

pub struct Scan {
    pub base: Base,
    pub candidates: Vec<Candidate>,
    pub warnings: Vec<String>,
}

const FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(upstream)%00%(upstream:short)%00%(upstream:track)%00%(upstream:remotename)%00%(committerdate:unix)%00%(contents:subject)";

pub const DEFAULT_PROTECTED: [&str; 3] = ["main", "master", "develop"];

/// What the caller wants scanned, as opposed to how progress is reported or
/// where verdicts are cached.
pub struct Options<'a> {
    /// `--base`; None auto-detects.
    pub base: Option<&'a str>,
    /// Extra protected names or globs, on top of config and the defaults.
    pub protect: &'a [String],
    /// Scan worker threads.
    pub jobs: usize,
    /// Also scan remote branches that are merged into the base but have no
    /// local twin. Off by default so library-style callers and tests get the
    /// plain local scan.
    pub include_remote_only: bool,
}

pub fn scan(
    git: &dyn Git,
    opts: &Options,
    reporter: &dyn Reporter,
    cache: &mut Cache,
) -> Result<Scan> {
    let (base_flag, extra_protect, jobs) = (opts.base, opts.protect, opts.jobs);
    let base = resolve_base(git, base_flag)?;
    let current = current_branch(git)?;
    let protected = protected_patterns(git, extra_protect)?;
    // try_run: ancient git without --is-shallow-repository predates shallow
    // clones being common; treat failure as "not shallow".
    let shallow = matches!(
        git.try_run(&["rev-parse", "--is-shallow-repository"])?,
        (true, out) if out.trim() == "true"
    );
    let held = worktree_branches(git)?;

    let mut warnings = Vec::new();
    if shallow {
        warnings.push("shallow repository: squash/rebase detection is disabled".to_string());
    }

    // Every query below is pinned to the sha resolved at scan start, not to
    // the base REF, which git would re-resolve per call. A fetch landing
    // mid-scan would otherwise judge early branches against one base tip and
    // later ones against another — and, worse, cache a verdict under a key
    // naming a base it was not computed against.
    let mut merged_args = vec!["for-each-ref", "refs/heads"];
    if opts.include_remote_only {
        merged_args.push("refs/remotes");
    }
    merged_args.extend(["--merged", &base.sha, "--format=%(refname)"]);
    let merged: HashSet<String> = git.run(&merged_args)?.lines().map(str::to_string).collect();

    // The local twin of a remote base: base `origin/main` → `refs/heads/main`.
    let base_local = base_local_counterpart(git, &base)?;

    let raw = git.run(&["for-each-ref", "refs/heads", FORMAT])?;
    let branches: Vec<RawBranch> = raw.lines().filter_map(RawBranch::parse).collect();

    // Exclusions are pure predicates over data already in hand — no git calls
    // at all — so they run first and shrink the expensive phases below.
    // Never candidates: branches checked out in any worktree, the base itself,
    // its local twin, protected names. (Merely TRACKING the base is not an
    // exclusion: `git switch -c feat origin/main` sets exactly that upstream
    // on perfectly ordinary feature branches.)
    let mut examinable: Vec<RawBranch> = branches
        .into_iter()
        .filter(|b| {
            !(held.contains(&b.refname)
                || current.as_deref() == Some(b.name.as_str())
                || Some(&b.refname) == base.refname.as_ref()
                || Some(&b.refname) == base_local.as_ref()
                || is_protected(&b.name, &protected))
        })
        .collect();

    // Remote branches merged into the base whose local twin is already gone.
    // They join `examinable` rather than getting a probe loop of their own, so
    // they share the same cache, the same parallel walk, and the same progress
    // total as everything else.
    if opts.include_remote_only {
        // Every local branch, not just the examinable ones: a remote whose
        // twin is merely protected or checked out is not "remote-only".
        let all_local: HashSet<String> = git
            .run(&["for-each-ref", "refs/heads", "--format=%(refname:short)"])?
            .lines()
            .map(str::to_string)
            .collect();
        let raw_remote = git.run(&["for-each-ref", "refs/remotes", FORMAT])?;
        examinable.extend(
            raw_remote
                .lines()
                .filter_map(RawBranch::parse_remote)
                .filter(|b| {
                    // `origin/HEAD` is a symref onto the default branch, the base
                    // itself is never a candidate, and anything that still has a
                    // local branch belongs to the normal local flow above.
                    !(b.short_remote_name() == "HEAD"
                        || Some(&b.refname) == base.refname.as_ref()
                        || is_protected(b.short_remote_name(), &protected)
                        || is_protected(&b.name, &protected)
                        || all_local.contains(b.short_remote_name()))
                }),
        );
    }

    let total = examinable.len();
    let done = AtomicUsize::new(0);
    let tick = || reporter.tick(done.fetch_add(1, Ordering::Relaxed) + 1, total);

    // Ancestry already answered these; they cost nothing more.
    for _ in examinable.iter().filter(|b| merged.contains(&b.refname)) {
        tick();
    }

    // Everything else needs patch-id probing, which is where the time goes —
    // except on a shallow clone, where there is no history to probe. Those
    // branches still tick: a counter that stops short of its total reads as
    // an aborted scan.
    let unmerged = examinable.iter().filter(|b| !merged.contains(&b.refname));
    let to_probe: Vec<&RawBranch> = if shallow {
        unmerged.for_each(|_| tick());
        Vec::new()
    } else {
        unmerged.collect()
    };

    let mut probed: HashMap<&str, MergeKind> = HashMap::new();

    let mut fresh: Vec<(Key, Option<MergeKind>)> = Vec::new();

    if !to_probe.is_empty() {
        // Phase 1: fork points. One cheap call per branch, run concurrently.
        // Needed even for cache hits, because the fork point is part of the
        // key that decides whether a hit is still valid.
        let forks = parallel::map(&to_probe, jobs, |b| merge_base(git, &base, &b.sha));

        // Anything already resolved by an earlier run with all three shas
        // unchanged is taken as-is; only the rest costs anything.
        let mut to_walk: Vec<(&&RawBranch, &Result<Option<String>>)> = Vec::new();
        for (b, fork) in to_probe.iter().zip(forks.iter()) {
            match fork {
                Ok(Some(mb)) => match cache.get(&Key::new(&base.sha, mb, &b.sha)) {
                    Some(verdict) => {
                        if let Some(kind) = verdict {
                            probed.insert(b.refname.as_str(), kind);
                        }
                        tick();
                    }
                    None => to_walk.push((b, fork)),
                },
                // No common history, or the call failed: nothing to cache.
                _ => to_walk.push((b, fork)),
            }
        }

        // Phase 2: the expensive upstream walk, once per DISTINCT fork point,
        // and only for fork points a cache miss actually needs. Branches cut
        // from the same commit share one walk — the common case by far, and
        // the reason this is not simply folded into phase 3.
        let mut distinct: Vec<String> = to_walk
            .iter()
            .filter_map(|(_, fork)| fork.as_ref().ok().and_then(|f| f.clone()))
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        let sets = parallel::map(&distinct, jobs, |mb| upstream_patch_ids(git, &base, mb));
        let mut walks: HashMap<&str, HashSet<String>> = HashMap::new();
        for (mb, set) in distinct.iter().zip(sets) {
            match set {
                Ok(ids) => {
                    walks.insert(mb.as_str(), ids);
                }
                // One unwalkable fork point must not abort the whole scan.
                // The fork point is a bare sha and means nothing to the user,
                // so the warning names every branch it actually cost.
                Err(e) => warnings.extend(
                    to_walk
                        .iter()
                        .filter(|(_, fork)| matches!(fork, Ok(Some(f)) if f == mb))
                        .map(|(b, _)| format!("{}: merge probe failed: {e:#}", b.name)),
                ),
            }
        }

        // Phase 3: the per-branch probe, against a now read-only walk table.
        let results = parallel::map(&to_walk, jobs, |(b, fork)| {
            let outcome = match fork.as_ref() {
                Ok(Some(mb)) => match walks.get(mb.as_str()) {
                    Some(ids) => probe_branch(git, &b.sha, mb, ids),
                    None => Ok(None), // its walk failed; already warned above
                },
                Ok(None) => Ok(None), // no common history
                Err(e) => Err(anyhow::anyhow!("{e:#}")),
            };
            tick();
            outcome
        });

        for ((b, fork), result) in to_walk.iter().zip(results) {
            match result {
                Ok(verdict) => {
                    if let Some(kind) = verdict {
                        probed.insert(b.refname.as_str(), kind);
                    }
                    // Only a verdict computed against a known fork point can
                    // be keyed, and only a successful walk produced one.
                    if let Ok(Some(mb)) = fork
                        && walks.contains_key(mb.as_str())
                    {
                        fresh.push((Key::new(&base.sha, mb, &b.sha), verdict));
                    }
                }
                // One odd branch must not abort the whole scan.
                Err(e) => warnings.push(format!("{}: merge probe failed: {e:#}", b.name)),
            }
        }
    }

    for (key, verdict) in fresh {
        cache.insert(key, verdict);
    }

    let mut candidates = Vec::new();
    for b in &examinable {
        let kind = if merged.contains(&b.refname) {
            MergeKind::Merged
        } else {
            match probed.get(b.refname.as_str()) {
                Some(kind) => *kind,
                None if b.upstream_gone => MergeKind::Gone,
                None => continue,
            }
        };
        candidates.push(Candidate {
            name: b.name.clone(),
            refname: b.refname.clone(),
            sha: b.sha.clone(),
            kind,
            scope: b.scope,
            upstream: b.upstream.clone(),
            remote_name: b.remote_name.clone(),
            upstream_gone: b.upstream_gone,
            last_commit_unix: b.last_commit_unix,
            subject: b.subject.clone(),
            upstream_ref: b.upstream_ref.clone(),
        });
    }

    reporter.finish();
    candidates.sort_by(|a, b| {
        a.scope
            .cmp(&b.scope)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Scan {
        base,
        candidates,
        warnings,
    })
}

fn resolve_base(git: &dyn Git, flag: Option<&str>) -> Result<Base> {
    if let Some(name) = flag {
        let (ok, sha) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ])?;
        if !ok {
            bail!("--base {name} does not resolve to a commit");
        }
        let sha = sha.trim().to_string();
        // --symbolic-full-name resolves symrefs to the terminal ref, so
        // `--base origin/HEAD` already yields e.g. refs/remotes/origin/main
        // and the exclusion logic protects the right branch.
        let (ok, full) = git.try_run(&["rev-parse", "--symbolic-full-name", name])?;
        let full = full.trim().to_string();
        return Ok(Base {
            name: name.to_string(),
            refname: (ok && !full.is_empty()).then_some(full),
            sha,
        });
    }
    let (ok, out) = git.try_run(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
    if ok && let Some(rest) = out.trim().strip_prefix("refs/remotes/") {
        // The symref can dangle (e.g. a clone from before a master→main
        // rename); only trust it when the target still resolves.
        let full = out.trim().to_string();
        let (ok, sha) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{full}^{{commit}}"),
        ])?;
        if ok {
            return Ok(Base {
                name: rest.to_string(),
                refname: Some(full),
                sha: sha.trim().to_string(),
            });
        }
    }
    for (name, full) in [
        ("origin/main", "refs/remotes/origin/main"),
        ("origin/master", "refs/remotes/origin/master"),
        ("main", "refs/heads/main"),
        ("master", "refs/heads/master"),
    ] {
        let (ok, sha) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{full}^{{commit}}"),
        ])?;
        if ok {
            return Ok(Base {
                name: name.to_string(),
                refname: Some(full.to_string()),
                sha: sha.trim().to_string(),
            });
        }
    }
    bail!(
        "could not determine the base branch; pass --base <branch>, or run `git remote set-head origin --auto`"
    )
}

/// For a remote base, the local branch of the same name (refs/heads/<x>).
fn base_local_counterpart(git: &dyn Git, base: &Base) -> Result<Option<String>> {
    let Some(rest) = base
        .refname
        .as_deref()
        .and_then(|r| r.strip_prefix("refs/remotes/"))
    else {
        return Ok(None);
    };
    for remote in git.run(&["remote"])?.lines() {
        if let Some(branch) = rest.strip_prefix(&format!("{remote}/")) {
            return Ok(Some(format!("refs/heads/{branch}")));
        }
    }
    Ok(None)
}

/// The checked-out branch, or None on a detached HEAD.
fn current_branch(git: &dyn Git) -> Result<Option<String>> {
    let (ok, out) = git.try_run(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    Ok(ok.then(|| out.trim().to_string()))
}

/// Branches checked out in ANY worktree — git refuses to delete them, so
/// they must never be offered as candidates.
fn worktree_branches(git: &dyn Git) -> Result<HashSet<String>> {
    let (ok, out) = git.try_run(&["worktree", "list", "--porcelain"])?;
    if !ok {
        return Ok(HashSet::new()); // very old git; the current-branch check still guards
    }
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("branch "))
        .map(str::to_string)
        .collect())
}

fn protected_patterns(git: &dyn Git, extra: &[String]) -> Result<Vec<String>> {
    let mut patterns: Vec<String> = DEFAULT_PROTECTED.iter().map(|s| s.to_string()).collect();
    let (ok, out) = git.try_run(&["config", "--get-all", "barber.protect"])?;
    if ok {
        patterns.extend(
            out.lines()
                .flat_map(|l| l.split(','))
                .map(|p| p.trim().to_string()),
        );
    }
    patterns.extend(extra.iter().map(|p| p.trim().to_string()));
    patterns.retain(|p| !p.is_empty());
    Ok(patterns)
}

fn is_protected(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// Glob with any number of `*` wildcards (each matches any substring).
fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let Some(mut rest) = name.strip_prefix(parts[0]) else {
        return false;
    };
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(parts[parts.len() - 1])
}

/// Diff rendering must be identical on both sides of a patch-id comparison,
/// and immune to user config: `git log -p` is porcelain (honours
/// diff.renames/algorithm/context/noprefix and color), `git diff-tree` is
/// plumbing (honours none of them). Pin every knob on both.
const DIFF_FLAGS: [&str; 13] = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--no-relative", // diff.relative + a subdir cwd would drop hunks from log -p
    "--full-index",
    "--diff-algorithm=myers",
    "-U3",
    "--inter-hunk-context=0",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--submodule=short",
    "--ignore-submodules=none",
];

fn with_diff_flags(prefix: &[&str], range: &str) -> Vec<String> {
    prefix
        .iter()
        .map(|s| s.to_string())
        .chain(DIFF_FLAGS.iter().map(|s| s.to_string()))
        .chain(std::iter::once(range.to_string()))
        .collect()
}

/// The fork point of `branch_ref` from the base, or None when they share no
/// history at all.
fn merge_base(git: &dyn Git, base: &Base, branch_sha: &str) -> Result<Option<String>> {
    let (ok, out) = git.try_run(&["merge-base", &base.sha, branch_sha])?;
    Ok(ok.then(|| out.trim().to_string()))
}

/// Patch-ids of everything the base gained since `merge_base`. This is the
/// expensive half of squash/rebase detection, and every branch cut from the
/// same fork point shares one walk — hence computing it per distinct fork
/// point rather than per branch.
fn upstream_patch_ids(git: &dyn Git, base: &Base, merge_base: &str) -> Result<HashSet<String>> {
    let args = with_diff_flags(
        &["log", "-p", "--no-merges"],
        &format!("{merge_base}..{}", base.sha),
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(patch_ids(git, &git.run(&refs)?)?.into_iter().collect())
}

/// Patch-id detection for merges that rewrite history. Pure reads — no
/// objects are written, so listing works on read-only repositories.
///
/// Takes the branch's SHA, never its ref: the verdict is cached under that
/// sha, so measuring anything else would store an answer about a commit the
/// key does not name. A branch moved by another terminal mid-scan and then
/// restored would otherwise be force-deleted on the strength of a verdict
/// computed for different commits.
fn probe_branch(
    git: &dyn Git,
    branch_sha: &str,
    merge_base: &str,
    upstream_ids: &HashSet<String>,
) -> Result<Option<MergeKind>> {
    if upstream_ids.is_empty() {
        return Ok(None);
    }

    // Squash: the whole branch collapsed into one diff — exactly what
    // GitHub's "Squash and merge" lands on base.
    let mut args = vec!["diff-tree".to_string(), "-p".to_string(), "-r".to_string()];
    args.extend(DIFF_FLAGS.iter().map(|s| s.to_string()));
    args.extend([merge_base.to_string(), branch_sha.to_string()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let combined = git.run(&refs)?;
    if let Some(id) = patch_ids(git, &combined)?.first()
        && upstream_ids.contains(id)
    {
        return Ok(Some(MergeKind::Squash));
    }

    // Rebase: every branch commit's patch exists in base individually.
    // Merge commits carry conflict resolutions that patch-ids skip, so
    // their presence disables this check (safe direction: branch survives).
    let merges = git.run(&[
        "rev-list",
        "--min-parents=2",
        "--count",
        &format!("{merge_base}..{branch_sha}"),
    ])?;
    if merges.trim() != "0" {
        return Ok(None);
    }
    let args = with_diff_flags(
        &["log", "-p", "--no-merges"],
        &format!("{merge_base}..{branch_sha}"),
    );
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let branch_ids = patch_ids(git, &git.run(&refs)?)?;
    // Empty-diff commits emit no patch-id at all; requiring the counts to
    // match keeps a branch with e.g. an --allow-empty release marker from
    // being classified (and force-deleted) as fully rebase-merged.
    let commits: usize = git
        .run(&[
            "rev-list",
            "--no-merges",
            "--count",
            &format!("{merge_base}..{branch_sha}"),
        ])?
        .trim()
        .parse()
        .unwrap_or(0);
    if commits > 0
        && branch_ids.len() == commits
        && branch_ids.iter().all(|id| upstream_ids.contains(id))
    {
        return Ok(Some(MergeKind::Rebase));
    }
    Ok(None)
}

/// Feed `git log -p` / `git diff-tree -p` output through `git patch-id`.
fn patch_ids(git: &dyn Git, patches: &str) -> Result<Vec<String>> {
    if patches.trim().is_empty() {
        return Ok(Vec::new());
    }
    let out = git.run_with_input(&["patch-id", "--stable"], patches)?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(str::to_string))
        .collect())
}

struct RawBranch {
    name: String,
    refname: String,
    sha: String,
    scope: CandidateScope,
    upstream: Option<String>,
    upstream_ref: Option<String>,
    remote_name: Option<String>,
    upstream_gone: bool,
    last_commit_unix: i64,
    subject: String,
}

impl RawBranch {
    /// One NUL-separated line of our `for-each-ref` format.
    fn parse(line: &str) -> Option<Self> {
        let mut f = line.split('\0');
        let refname = f.next()?.to_string();
        let name = refname.strip_prefix("refs/heads/")?.to_string();
        let sha = f.next()?.to_string();
        let upstream_ref = f.next()?;
        let upstream = f.next()?;
        let track = f.next()?;
        let remote_name = f.next()?;
        let last_commit_unix = f.next()?.parse().ok()?;
        let subject = f.next().unwrap_or_default().to_string();
        Some(Self {
            name,
            refname,
            sha,
            scope: CandidateScope::Local,
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
            upstream_ref: (!upstream_ref.is_empty()).then(|| upstream_ref.to_string()),
            remote_name: (!remote_name.is_empty()).then(|| remote_name.to_string()),
            upstream_gone: track == "[gone]",
            last_commit_unix,
            subject,
        })
    }

    /// The same `for-each-ref` line, read as a remote-tracking ref. A remote
    /// ref has no upstream of its own, so the fields that describe one are
    /// synthesised to point at the ref itself: that is what `ops::remote_branch`
    /// and the confirmation dialog need in order to name the deletion target.
    fn parse_remote(line: &str) -> Option<Self> {
        let mut f = line.split('\0');
        let refname = f.next()?.to_string();
        let name = refname.strip_prefix("refs/remotes/")?.to_string();
        // A bare "refs/remotes/origin" (no branch part) is not a branch.
        let remote = name.split_once('/')?.0.to_string();
        let sha = f.next()?.to_string();
        // upstream, upstream:short, upstream:track and upstream:remotename
        // are all empty for a remote-tracking ref; step past them.
        for _ in 0..4 {
            f.next()?;
        }
        let last_commit_unix = f.next()?.parse().ok()?;
        let subject = f.next().unwrap_or_default().to_string();
        Some(Self {
            name: name.clone(),
            refname: refname.clone(),
            sha,
            scope: CandidateScope::RemoteOnly,
            upstream: Some(name),
            upstream_ref: Some(refname),
            remote_name: Some(remote),
            upstream_gone: false,
            last_commit_unix,
            subject,
        })
    }

    /// For a remote-tracking ref, the branch name on the remote
    /// ("origin/feat" → "feat"). Meaningless for a local branch.
    fn short_remote_name(&self) -> &str {
        self.name.split_once('/').map_or(&self.name, |(_, b)| b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fake::FakeGit;
    use crate::progress::Reporter;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct RecordingReporter {
        ticks: Mutex<Vec<(usize, usize)>>,
        finished: AtomicBool,
    }

    impl Reporter for RecordingReporter {
        fn tick(&self, done: usize, total: usize) {
            self.ticks.lock().unwrap().push((done, total));
        }
        fn finish(&self) {
            self.finished.store(true, Ordering::SeqCst);
        }
    }

    /// A repository with `main` as origin/HEAD and two ancestry-merged
    /// branches, so the scan never reaches the patch-id probes.
    fn two_merged_branches() -> FakeGit {
        let format = FORMAT;
        FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("base000\n"),
            )
            .on(&["symbolic-ref", "--quiet", "--short", "HEAD"], Ok("main\n"))
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("false\n"))
            .on(&["worktree", "list", "--porcelain"], Ok("branch refs/heads/main\n"))
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    "base000",
                    "--format=%(refname)",
                ],
                Ok("refs/heads/feat-a\nrefs/heads/feat-b\n"),
            )
            .on(&["remote"], Ok("origin\n"))
            .on(
                &["for-each-ref", "refs/heads", format],
                Ok("refs/heads/feat-a\u{0}aaa111\u{0}refs/remotes/origin/feat-a\u{0}origin/feat-a\u{0}\u{0}origin\u{0}1700000000\u{0}add a\n\
                    refs/heads/feat-b\u{0}bbb222\u{0}\u{0}\u{0}\u{0}\u{0}1700000100\u{0}add b\n"),
            )
    }

    /// Everything a scan needs up to and including the fork point, for one
    /// unmerged branch. Deliberately cans NO probe commands: `FakeGit` bails
    /// on unexpected argv, so any test using this fails loudly the moment a
    /// probe runs.
    fn up_to_the_fork_point() -> FakeGit {
        FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("basesha0\n"),
            )
            .on(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                Ok("main\n"),
            )
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("false\n"))
            .on(
                &["worktree", "list", "--porcelain"],
                Ok("branch refs/heads/main\n"),
            )
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    "basesha0",
                    "--format=%(refname)",
                ],
                Ok("\n"),
            )
            .on(&["remote"], Ok("origin\n"))
            .on(
                &["for-each-ref", "refs/heads", FORMAT],
                Ok("refs/heads/feat\u{0}feat111\u{0}\u{0}\u{0}\u{0}\u{0}1700000000\u{0}work\n"),
            )
            .on(&["merge-base", "basesha0", "feat111"], Ok("mb0\n"))
    }

    fn one_branch_options() -> Options<'static> {
        Options {
            base: None,
            protect: &[],
            jobs: 1,
            include_remote_only: false,
        }
    }

    #[test]
    fn a_cached_verdict_is_used_instead_of_probing_again() {
        // The whole point of the cache, and the only test that proves it is
        // READ at all: no probe command is canned here, so if the scan falls
        // through to the prober FakeGit refuses the call and the branch stops
        // being a squash candidate.
        let git = up_to_the_fork_point();
        let mut cache = Cache::default();
        cache.insert(
            Key::new("basesha0", "mb0", "feat111"),
            Some(MergeKind::Squash),
        );

        let scan = scan(
            &git,
            &one_branch_options(),
            &crate::progress::NullReporter,
            &mut cache,
        )
        .unwrap();

        assert!(scan.warnings.is_empty(), "unexpected: {:?}", scan.warnings);
        assert_eq!(
            scan.candidates
                .iter()
                .map(|c| (c.name.as_str(), c.kind))
                .collect::<Vec<_>>(),
            vec![("feat", MergeKind::Squash)],
        );
    }

    #[test]
    fn a_verdict_cached_for_a_different_tip_is_never_reused() {
        // Same branch name, same fork point, different branch tip. Serving
        // the old verdict here would force-delete commits nothing verified,
        // so the scan must go back to the prober — which FakeGit refuses,
        // turning the reuse into a loud failure rather than a silent delete.
        let git = up_to_the_fork_point();
        let mut cache = Cache::default();
        cache.insert(
            Key::new("basesha0", "mb0", "OLD-TIP"),
            Some(MergeKind::Squash),
        );

        let scan = scan(
            &git,
            &one_branch_options(),
            &crate::progress::NullReporter,
            &mut cache,
        )
        .unwrap();

        assert!(
            scan.candidates.is_empty(),
            "a stale verdict was reused: {:?}",
            scan.candidates
                .iter()
                .map(|c| (c.name.as_str(), c.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            scan.warnings.iter().any(|w| w.contains("feat")),
            "the prober should have been consulted, got {:?}",
            scan.warnings
        );
    }

    #[test]
    fn every_probe_is_pinned_to_the_shas_resolved_at_scan_start() {
        // The cache key records the base tip and the branch tip captured at
        // scan start. If any probe asked git for a REF instead, whatever the
        // ref pointed at seconds later would be judged, and the verdict
        // stored under the sha that was NOT measured:
        //
        //   * base side — a fetch lands mid-scan, then a later rewind serves
        //     the verdict as a hit;
        //   * branch side — a rebase in another terminal moves the branch,
        //     then `git rebase --abort` puts it back.
        //
        // Either way compare-then-delete cannot catch it: by deletion time
        // the tip does equal the sha, so the branch is force-deleted with
        // work nothing ever verified. Pinning every command to a sha closes
        // both, and removes the intra-run inconsistency the parallel phases
        // widened.
        //
        // FakeGit rejects any argv it was not given, so canning ONLY the sha
        // form is what pins this.
        const SHA: &str = "basesha0";
        const BRANCH: &str = "feat111";
        fn log_args(range: &str) -> Vec<&str> {
            let mut v = vec!["log", "-p", "--no-merges"];
            v.extend_from_slice(&DIFF_FLAGS);
            v.push(range);
            v
        }
        fn diff_tree_args<'a>(mb: &'a str, branch_ref: &'a str) -> Vec<&'a str> {
            let mut v = vec!["diff-tree", "-p", "-r"];
            v.extend_from_slice(&DIFF_FLAGS);
            v.extend([mb, branch_ref]);
            v
        }

        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("basesha0\n"),
            )
            .on(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                Ok("main\n"),
            )
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("false\n"))
            .on(
                &["worktree", "list", "--porcelain"],
                Ok("branch refs/heads/main\n"),
            )
            // Ancestry, fork point and upstream walk: all keyed to the sha.
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    SHA,
                    "--format=%(refname)",
                ],
                Ok("\n"),
            )
            .on(&["remote"], Ok("origin\n"))
            .on(
                &["for-each-ref", "refs/heads", FORMAT],
                Ok("refs/heads/feat\u{0}feat111\u{0}\u{0}\u{0}\u{0}\u{0}1700000000\u{0}work\n"),
            )
            .on(&["merge-base", SHA, BRANCH], Ok("mb0\n"))
            .on(&log_args(&format!("mb0..{SHA}")), Ok("UPLOG"))
            .on_input(&["patch-id", "--stable"], "UPLOG", Ok("shared1 c1\n"))
            .on(&diff_tree_args("mb0", BRANCH), Ok("DIFF"))
            .on_input(&["patch-id", "--stable"], "DIFF", Ok("shared1 000\n"));

        let scan = scan(
            &git,
            &Options {
                base: None,
                protect: &[],
                jobs: 1,
                include_remote_only: false,
            },
            &crate::progress::NullReporter,
            &mut Cache::default(),
        )
        .unwrap();
        assert_eq!(scan.base.sha, SHA);
        assert_eq!(
            scan.candidates
                .iter()
                .map(|c| (c.name.as_str(), c.kind))
                .collect::<Vec<_>>(),
            vec![("feat", MergeKind::Squash)]
        );
    }

    #[test]
    fn a_shallow_repository_still_counts_every_branch_it_examined() {
        // Shallow clones disable the patch-id probes entirely, so nothing
        // reaches phase 3. The counter must not stop short of the total and
        // leave the scan looking aborted.
        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("base000\n"),
            )
            .on(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                Ok("main\n"),
            )
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("true\n"))
            .on(
                &["worktree", "list", "--porcelain"],
                Ok("branch refs/heads/main\n"),
            )
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    "base000",
                    "--format=%(refname)",
                ],
                Ok("refs/heads/feat-a\n"),
            )
            .on(&["remote"], Ok("origin\n"))
            .on(
                &["for-each-ref", "refs/heads", FORMAT],
                Ok(
                    "refs/heads/feat-a\u{0}aaa111\u{0}\u{0}\u{0}\u{0}\u{0}1700000000\u{0}a\n\
                    refs/heads/feat-b\u{0}bbb222\u{0}\u{0}\u{0}\u{0}\u{0}1700000100\u{0}b\n",
                ),
            );

        let reporter = RecordingReporter::default();
        let scan = scan(
            &git,
            &Options {
                base: None,
                protect: &[],
                jobs: 1,
                include_remote_only: false,
            },
            &reporter,
            &mut Cache::default(),
        )
        .unwrap();

        assert!(
            scan.warnings.iter().any(|w| w.contains("shallow")),
            "expected the shallow warning, got {:?}",
            scan.warnings
        );
        let ticks = reporter.ticks.lock().unwrap().clone();
        assert_eq!(
            ticks.last(),
            Some(&(2, 2)),
            "the counter must reach the total, got {ticks:?}"
        );
    }

    #[test]
    fn scan_reports_progress_for_every_branch_and_clears_the_line() {
        // Silence during a long scan reads as a hang: every branch examined
        // has to move the counter, and the line must be wiped afterwards so
        // it cannot collide with the listing.
        let git = two_merged_branches();
        let reporter = RecordingReporter::default();
        let scan = scan(
            &git,
            &Options {
                base: None,
                protect: &[],
                jobs: 1,
                include_remote_only: false,
            },
            &reporter,
            &mut Cache::default(),
        )
        .unwrap();

        assert_eq!(scan.candidates.len(), 2);
        let ticks = reporter.ticks.lock().unwrap().clone();
        assert_eq!(ticks.len(), 2, "one tick per branch, got {ticks:?}");
        assert!(
            ticks.iter().all(|&(_, total)| total == 2),
            "total must be the branch count, got {ticks:?}"
        );
        assert!(
            reporter.finished.load(Ordering::SeqCst),
            "finish not called"
        );
    }

    #[test]
    fn base_flag_is_verified_and_normalized() {
        let git = FakeGit::default()
            .on(
                &["rev-parse", "--verify", "--quiet", "topic^{commit}"],
                Ok("abc\n"),
            )
            .on(
                &["rev-parse", "--symbolic-full-name", "topic"],
                Ok("refs/heads/topic\n"),
            );
        let base = resolve_base(&git, Some("topic")).unwrap();
        assert_eq!(base.name, "topic");
        assert_eq!(base.refname.as_deref(), Some("refs/heads/topic"));
        assert_eq!(base.rev(), "refs/heads/topic");

        // A raw SHA base has no refname but still works.
        let git = FakeGit::default()
            .on(
                &["rev-parse", "--verify", "--quiet", "abc123^{commit}"],
                Ok("abc123\n"),
            )
            .on(&["rev-parse", "--symbolic-full-name", "abc123"], Ok("\n"));
        let base = resolve_base(&git, Some("abc123")).unwrap();
        assert_eq!(base.refname, None);

        let git = FakeGit::default().on(
            &["rev-parse", "--verify", "--quiet", "nope^{commit}"],
            Err(""),
        );
        assert!(resolve_base(&git, Some("nope")).is_err());
    }

    #[test]
    fn base_prefers_origin_head_symref() {
        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/trunk\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/trunk^{commit}",
                ],
                Ok("abc\n"),
            );
        let base = resolve_base(&git, None).unwrap();
        assert_eq!(base.name, "origin/trunk");
        assert_eq!(base.refname.as_deref(), Some("refs/remotes/origin/trunk"));
    }

    #[test]
    fn dangling_origin_head_falls_through_to_probes() {
        // Classic post-rename state: origin/HEAD still points at a deleted
        // origin/master. Must not error; must fall through to origin/main.
        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/master\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/master^{commit}",
                ],
                Err(""),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("abc\n"),
            );
        assert_eq!(resolve_base(&git, None).unwrap().name, "origin/main");
    }

    #[test]
    fn base_resolution_failure_is_actionable() {
        let mut git = FakeGit::default().on(
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            Err(""),
        );
        for full in [
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
            "refs/heads/main",
            "refs/heads/master",
        ] {
            git = git.on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{full}^{{commit}}"),
                ],
                Err(""),
            );
        }
        let err = resolve_base(&git, None).unwrap_err().to_string();
        assert!(err.contains("--base"), "unhelpful error: {err}");
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "main2"));
        assert!(glob_match("release/*", "release/1.0"));
        assert!(!glob_match("release/*", "released"));
        assert!(glob_match("*-wip", "feature-wip"));
        assert!(!glob_match("*-wip", "-wi"));
        assert!(glob_match("*", "anything"));
        // multiple wildcards
        assert!(glob_match("*-keep-*", "wip-keep-alice"));
        assert!(!glob_match("*-keep-*", "wip-kept-alice"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(glob_match("feature/*/keep", "feature/x/keep"));
        assert!(!glob_match("a*a", "a")); // parts must not overlap
    }

    #[test]
    fn parse_ref_line() {
        let b = RawBranch::parse(
            "refs/heads/feat/x\0abc123\0refs/remotes/origin/feat/x\0origin/feat/x\0[gone]\0origin\x001700000000\0Add x\0extra",
        )
        .unwrap();
        assert_eq!(b.name, "feat/x");
        assert_eq!(b.refname, "refs/heads/feat/x");
        assert_eq!(b.sha, "abc123");
        assert_eq!(b.upstream.as_deref(), Some("origin/feat/x"));
        assert_eq!(
            b.upstream_ref.as_deref(),
            Some("refs/remotes/origin/feat/x")
        );
        assert!(b.upstream_gone);
        assert_eq!(b.remote_name.as_deref(), Some("origin"));
        assert_eq!(b.last_commit_unix, 1_700_000_000);
        assert_eq!(b.subject, "Add x");

        let b = RawBranch::parse("refs/heads/local\0abc\0\0\0\0\x001700000000\0msg").unwrap();
        assert!(b.upstream.is_none());
        assert!(b.upstream_ref.is_none());
        assert!(!b.upstream_gone);
    }

    fn line(name: &str, sha: &str, upstream_short: &str, track: &str) -> String {
        let upstream_full = if upstream_short.is_empty() {
            String::new()
        } else {
            format!("refs/remotes/{upstream_short}")
        };
        format!(
            "refs/heads/{name}\0{sha}\0{upstream_full}\0{upstream_short}\0{track}\0origin\x001700000000\0subject"
        )
    }

    /// Can the full scan classify Merged/Squash/Rebase/Gone and apply every
    /// exclusion? All git calls are canned.
    #[test]
    fn scan_classifies_and_excludes() {
        let enumeration = [
            line("main", "s0", "origin/main", ""),
            line("feature-merged", "s1", "", ""),
            line("feature-squash", "s2", "", ""),
            line("feature-rebase", "s3", "", ""),
            line("feature-gone", "s4", "origin/feature-gone", "[gone]"),
            line("feature-active", "s5", "origin/feature-active", ""),
            line("feature-wt", "s6", "", ""),
            line("release/1.0", "s7", "", ""),
        ]
        .join("\n");

        // Argv builders mirroring the production DIFF_FLAGS plumbing.
        fn log_args(range: &str) -> Vec<&str> {
            let mut v = vec!["log", "-p", "--no-merges"];
            v.extend_from_slice(&super::DIFF_FLAGS);
            v.push(range);
            v
        }
        fn diff_tree_args<'a>(mb: &'a str, branch_ref: &'a str) -> Vec<&'a str> {
            let mut v = vec!["diff-tree", "-p", "-r"];
            v.extend_from_slice(&super::DIFF_FLAGS);
            v.extend([mb, branch_ref]);
            v
        }
        // Scan queries are pinned to the resolved base SHA, not the ref —
        // see `every_probe_is_pinned_to_the_shas_resolved_at_scan_start`.
        const BASE_REV: &str = "abc";

        // Every probe argument is a SHA, never a ref — both the base and the
        // branch. That is what the cache keys name.
        let probe = |git: FakeGit,
                     branch: &str,
                     branch_sha: &str,
                     mb: &str,
                     combined_id: &str,
                     branch_ids: &str,
                     branch_commits: &str| {
            git.on(
                &["merge-base", BASE_REV, branch_sha],
                Ok(&format!("{mb}\n")),
            )
            .on(
                &log_args(&format!("{mb}..{BASE_REV}")),
                Ok(&format!("UPLOG-{mb}")),
            )
            .on_input(
                &["patch-id", "--stable"],
                &format!("UPLOG-{mb}"),
                Ok("up1 c1\nup2 c2\n"),
            )
            .on(
                &diff_tree_args(mb, branch_sha),
                Ok(&format!("DIFF-{branch}")),
            )
            .on_input(
                &["patch-id", "--stable"],
                &format!("DIFF-{branch}"),
                Ok(&format!("{combined_id} 000\n")),
            )
            .on(
                &[
                    "rev-list",
                    "--min-parents=2",
                    "--count",
                    &format!("{mb}..{branch_sha}"),
                ],
                Ok("0\n"),
            )
            .on(
                &log_args(&format!("{mb}..{branch_sha}")),
                Ok(&format!("BLOG-{branch}")),
            )
            .on_input(
                &["patch-id", "--stable"],
                &format!("BLOG-{branch}"),
                Ok(branch_ids),
            )
            .on(
                &[
                    "rev-list",
                    "--no-merges",
                    "--count",
                    &format!("{mb}..{branch_sha}"),
                ],
                Ok(branch_commits),
            )
        };

        let mut git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("abc\n"),
            )
            .on(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                Ok("feature-active\n"),
            )
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("false\n"))
            .on(
                &["worktree", "list", "--porcelain"],
                Ok("worktree /w\nHEAD s6\nbranch refs/heads/feature-wt\n"),
            )
            .on(&["remote"], Ok("origin\n"))
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    BASE_REV,
                    "--format=%(refname)",
                ],
                // feature-wt is merged but held by a worktree → must be excluded
                Ok("refs/heads/main\nrefs/heads/feature-merged\nrefs/heads/feature-wt\n"),
            )
            .on(
                &["for-each-ref", "refs/heads", super::FORMAT],
                Ok(&enumeration),
            );
        // squash: combined diff id matches upstream; rebase check not reached
        git = probe(
            git,
            "feature-squash",
            "s2",
            "mb2",
            "up1",
            "irrelevant 0\n",
            "9",
        );
        // rebase: combined misses, but each commit's id is upstream and the
        // id count equals the commit count (no empty commits hiding)
        git = probe(
            git,
            "feature-rebase",
            "s3",
            "mb3",
            "zzz",
            "up1 a\nup2 b\n",
            "2\n",
        );
        // gone: nothing matches
        git = probe(git, "feature-gone", "s4", "mb4", "zzz", "own1 a\n", "1\n");

        let scan = scan(
            &git,
            &Options {
                base: None,
                protect: &["release/*".to_string()],
                jobs: 1,
                include_remote_only: false,
            },
            &crate::progress::NullReporter,
            &mut Cache::default(),
        )
        .unwrap();
        assert_eq!(scan.base.name, "origin/main");
        let kinds: Vec<(&str, MergeKind)> = scan
            .candidates
            .iter()
            .map(|c| (c.name.as_str(), c.kind))
            .collect();
        // Excluded: main (base twin), feature-active (checked out, never
        // probed), feature-wt (worktree), release/1.0 (protected glob).
        assert_eq!(
            kinds,
            vec![
                ("feature-merged", MergeKind::Merged),
                ("feature-squash", MergeKind::Squash),
                ("feature-rebase", MergeKind::Rebase),
                ("feature-gone", MergeKind::Gone),
            ]
        );
        assert!(
            scan.candidates
                .iter()
                .take(3)
                .all(|c| c.selected_by_default())
        );
        assert!(!scan.candidates[0].needs_force());
        assert!(scan.candidates[1].needs_force());
        assert!(scan.candidates[2].needs_force());
        assert!(!scan.candidates[3].selected_by_default());
    }

    #[test]
    fn probe_failure_is_a_warning_not_an_abort() {
        let enumeration = line("feature-odd", "s1", "", "");
        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/main^{commit}",
                ],
                Ok("abc\n"),
            )
            .on(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                Ok("main\n"),
            )
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("false\n"))
            .on(&["worktree", "list", "--porcelain"], Ok(""))
            .on(&["remote"], Ok("origin\n"))
            // Pinned to the resolved base SHA, not the ref — see
            // `every_probe_is_pinned_to_the_base_sha_resolved_at_scan_start`.
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    "abc",
                    "--format=%(refname)",
                ],
                Ok(""),
            )
            .on(
                &["for-each-ref", "refs/heads", super::FORMAT],
                Ok(&enumeration),
            )
            .on(&["merge-base", "abc", "refs/heads/feature-odd"], Ok("mb\n"))
            // upstream log blows up (e.g. corrupt object) — scan must survive
            .on(
                &{
                    let mut v = vec!["log", "-p", "--no-merges"];
                    v.extend_from_slice(&super::DIFF_FLAGS);
                    v.push("mb..abc");
                    v
                },
                Err("boom"),
            );

        let scan = scan(
            &git,
            &Options {
                base: None,
                protect: &[],
                jobs: 1,
                include_remote_only: false,
            },
            &crate::progress::NullReporter,
            &mut Cache::default(),
        )
        .unwrap();
        assert!(scan.candidates.is_empty());
        assert!(
            scan.warnings.iter().any(|w| w.contains("feature-odd")),
            "expected a probe warning, got {:?}",
            scan.warnings
        );
    }

    #[test]
    fn detached_head_excludes_nothing() {
        let git = FakeGit::default().on(&["symbolic-ref", "--quiet", "--short", "HEAD"], Err(""));
        assert_eq!(current_branch(&git).unwrap(), None);
    }
}
