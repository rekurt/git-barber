use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::git::Git;

/// How we concluded a branch is safe to trim, ordered by confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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

/// The branch everything is compared against.
#[derive(Debug, Clone)]
pub struct Base {
    /// User-facing name (e.g. "origin/main").
    pub name: String,
    /// Full refname when the base is a ref (e.g. "refs/remotes/origin/main").
    /// None when --base was given a raw commit.
    pub refname: Option<String>,
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

pub fn scan(git: &dyn Git, base_flag: Option<&str>, extra_protect: &[String]) -> Result<Scan> {
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

    let merged: HashSet<String> = git
        .run(&[
            "for-each-ref",
            "refs/heads",
            "--merged",
            base.rev(),
            "--format=%(refname)",
        ])?
        .lines()
        .map(str::to_string)
        .collect();

    // The local twin of a remote base: base `origin/main` → `refs/heads/main`.
    let base_local = base_local_counterpart(git, &base)?;

    let mut upstream_cache = HashMap::new();
    let mut candidates = Vec::new();
    for line in git.run(&["for-each-ref", "refs/heads", FORMAT])?.lines() {
        let Some(b) = RawBranch::parse(line) else {
            continue;
        };
        // Never candidates: branches checked out in any worktree, the base
        // itself, its local twin, protected names. (Merely TRACKING the base
        // is not an exclusion: `git switch -c feat origin/main` sets exactly
        // that upstream on perfectly ordinary feature branches.)
        if held.contains(&b.refname)
            || current.as_deref() == Some(b.name.as_str())
            || Some(&b.refname) == base.refname.as_ref()
            || Some(&b.refname) == base_local.as_ref()
            || is_protected(&b.name, &protected)
        {
            continue;
        }

        let kind = if merged.contains(&b.refname) {
            MergeKind::Merged
        } else {
            let probed = if shallow {
                None
            } else {
                // One odd branch must not abort the whole scan.
                match merged_by_patch_id(git, &base, &b.refname, &mut upstream_cache) {
                    Ok(kind) => kind,
                    Err(e) => {
                        warnings.push(format!("{}: merge probe failed: {e:#}", b.name));
                        None
                    }
                }
            };
            match probed {
                Some(kind) => kind,
                None if b.upstream_gone => MergeKind::Gone,
                None => continue,
            }
        };

        candidates.push(Candidate {
            name: b.name,
            refname: b.refname,
            sha: b.sha,
            kind,
            upstream: b.upstream,
            remote_name: b.remote_name,
            upstream_gone: b.upstream_gone,
            last_commit_unix: b.last_commit_unix,
            subject: b.subject,
            upstream_ref: b.upstream_ref,
        });
    }

    candidates.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    Ok(Scan {
        base,
        candidates,
        warnings,
    })
}

fn resolve_base(git: &dyn Git, flag: Option<&str>) -> Result<Base> {
    if let Some(name) = flag {
        let (ok, _) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ])?;
        if !ok {
            bail!("--base {name} does not resolve to a commit");
        }
        // --symbolic-full-name resolves symrefs to the terminal ref, so
        // `--base origin/HEAD` already yields e.g. refs/remotes/origin/main
        // and the exclusion logic protects the right branch.
        let (ok, full) = git.try_run(&["rev-parse", "--symbolic-full-name", name])?;
        let full = full.trim().to_string();
        return Ok(Base {
            name: name.to_string(),
            refname: (ok && !full.is_empty()).then_some(full),
        });
    }
    let (ok, out) = git.try_run(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
    if ok && let Some(rest) = out.trim().strip_prefix("refs/remotes/") {
        // The symref can dangle (e.g. a clone from before a master→main
        // rename); only trust it when the target still resolves.
        let full = out.trim().to_string();
        let (ok, _) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{full}^{{commit}}"),
        ])?;
        if ok {
            return Ok(Base {
                name: rest.to_string(),
                refname: Some(full),
            });
        }
    }
    for (name, full) in [
        ("origin/main", "refs/remotes/origin/main"),
        ("origin/master", "refs/remotes/origin/master"),
        ("main", "refs/heads/main"),
        ("master", "refs/heads/master"),
    ] {
        let (ok, _) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{full}^{{commit}}"),
        ])?;
        if ok {
            return Ok(Base {
                name: name.to_string(),
                refname: Some(full.to_string()),
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

/// Patch-id detection for merges that rewrite history. Pure reads — no
/// objects are written, so listing works on read-only repositories.
/// `upstream_cache` memoizes the (expensive) upstream patch-id walk per
/// merge-base: branches forked from the same point share one walk.
fn merged_by_patch_id(
    git: &dyn Git,
    base: &Base,
    branch_ref: &str,
    upstream_cache: &mut HashMap<String, HashSet<String>>,
) -> Result<Option<MergeKind>> {
    let (ok, merge_base) = git.try_run(&["merge-base", base.rev(), branch_ref])?;
    if !ok {
        return Ok(None); // no common history
    }
    let merge_base = merge_base.trim().to_string();

    // Patches base gained since the fork point.
    if !upstream_cache.contains_key(&merge_base) {
        let args = with_diff_flags(
            &["log", "-p", "--no-merges"],
            &format!("{merge_base}..{}", base.rev()),
        );
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let ids = patch_ids(git, &git.run(&refs)?)?.into_iter().collect();
        upstream_cache.insert(merge_base.clone(), ids);
    }
    let upstream_ids = &upstream_cache[&merge_base];
    if upstream_ids.is_empty() {
        return Ok(None);
    }

    // Squash: the whole branch collapsed into one diff — exactly what
    // GitHub's "Squash and merge" lands on base.
    let mut args = vec!["diff-tree".to_string(), "-p".to_string(), "-r".to_string()];
    args.extend(DIFF_FLAGS.iter().map(|s| s.to_string()));
    args.extend([merge_base.clone(), branch_ref.to_string()]);
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
        &format!("{merge_base}..{branch_ref}"),
    ])?;
    if merges.trim() != "0" {
        return Ok(None);
    }
    let args = with_diff_flags(
        &["log", "-p", "--no-merges"],
        &format!("{merge_base}..{branch_ref}"),
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
            &format!("{merge_base}..{branch_ref}"),
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
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
            upstream_ref: (!upstream_ref.is_empty()).then(|| upstream_ref.to_string()),
            remote_name: (!remote_name.is_empty()).then(|| remote_name.to_string()),
            upstream_gone: track == "[gone]",
            last_commit_unix,
            subject,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fake::FakeGit;

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
        const BASE_REV: &str = "refs/remotes/origin/main";

        let probe = |git: FakeGit,
                     branch: &str,
                     mb: &str,
                     combined_id: &str,
                     branch_ids: &str,
                     branch_commits: &str| {
            let branch_ref = format!("refs/heads/{branch}");
            git.on(
                &["merge-base", BASE_REV, &branch_ref],
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
                &diff_tree_args(mb, &branch_ref),
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
                    &format!("{mb}..{branch_ref}"),
                ],
                Ok("0\n"),
            )
            .on(
                &log_args(&format!("{mb}..{branch_ref}")),
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
                    &format!("{mb}..{branch_ref}"),
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
        git = probe(git, "feature-squash", "mb2", "up1", "irrelevant 0\n", "9");
        // rebase: combined misses, but each commit's id is upstream and the
        // id count equals the commit count (no empty commits hiding)
        git = probe(git, "feature-rebase", "mb3", "zzz", "up1 a\nup2 b\n", "2\n");
        // gone: nothing matches
        git = probe(git, "feature-gone", "mb4", "zzz", "own1 a\n", "1\n");

        let scan = scan(&git, None, &["release/*".to_string()]).unwrap();
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
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    "refs/remotes/origin/main",
                    "--format=%(refname)",
                ],
                Ok(""),
            )
            .on(
                &["for-each-ref", "refs/heads", super::FORMAT],
                Ok(&enumeration),
            )
            .on(
                &[
                    "merge-base",
                    "refs/remotes/origin/main",
                    "refs/heads/feature-odd",
                ],
                Ok("mb\n"),
            )
            // upstream log blows up (e.g. corrupt object) — scan must survive
            .on(
                &{
                    let mut v = vec!["log", "-p", "--no-merges"];
                    v.extend_from_slice(&super::DIFF_FLAGS);
                    v.push("mb..refs/remotes/origin/main");
                    v
                },
                Err("boom"),
            );

        let scan = scan(&git, None, &[]).unwrap();
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
