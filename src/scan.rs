use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::git::Git;

/// How we concluded a branch is safe to trim, ordered by confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeKind {
    /// Tip is an ancestor of the base branch (`git branch --merged`).
    Merged,
    /// The branch squashed into one commit matches a patch already in base.
    Squash,
    /// The tracked upstream was deleted; the branch itself may be unmerged.
    Gone,
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub name: String,
    /// Tip OID at scan time; used for undo hints after deletion.
    pub sha: String,
    pub kind: MergeKind,
    pub upstream: Option<String>,
    pub remote_name: Option<String>,
    pub upstream_gone: bool,
    pub last_commit_unix: i64,
    pub subject: String,
}

impl Candidate {
    /// Merged/squash are high-confidence; gone branches need a human opt-in.
    pub fn selected_by_default(&self) -> bool {
        matches!(self.kind, MergeKind::Merged | MergeKind::Squash)
    }

    /// Whether deletion requires `git branch -D` rather than `-d`.
    pub fn needs_force(&self) -> bool {
        self.kind != MergeKind::Merged
    }
}

pub struct Scan {
    pub base: String,
    pub candidates: Vec<Candidate>,
    pub warnings: Vec<String>,
}

const FORMAT: &str = "--format=%(refname:short)%00%(objectname)%00%(upstream:short)%00%(upstream:track)%00%(upstream:remotename)%00%(committerdate:unix)%00%(contents:subject)";

pub const DEFAULT_PROTECTED: [&str; 3] = ["main", "master", "develop"];

pub fn scan(git: &dyn Git, base_flag: Option<&str>, extra_protect: &[String]) -> Result<Scan> {
    let base = resolve_base(git, base_flag)?;
    let current = current_branch(git)?;
    let protected = protected_patterns(git, extra_protect)?;
    let shallow = git.run(&["rev-parse", "--is-shallow-repository"])?.trim() == "true";

    let mut warnings = Vec::new();
    if shallow {
        warnings.push("shallow repository: squash-merge detection is disabled".to_string());
    }

    let merged: HashSet<String> = git
        .run(&[
            "for-each-ref",
            "refs/heads",
            "--merged",
            &base,
            "--format=%(refname:short)",
        ])?
        .lines()
        .map(str::to_string)
        .collect();

    // Branch names that must never become candidates: the checked-out branch,
    // the base itself, and its local counterpart (base `origin/main` → `main`).
    let base_local = base.strip_prefix("origin/");

    let mut candidates = Vec::new();
    for line in git.run(&["for-each-ref", "refs/heads", FORMAT])?.lines() {
        let Some(b) = RawBranch::parse(line) else {
            continue;
        };
        if current.as_deref() == Some(b.name.as_str())
            || b.name == base
            || base_local == Some(b.name.as_str())
            || b.upstream.as_deref() == Some(base.as_str())
            || is_protected(&b.name, &protected)
        {
            continue;
        }

        let kind = if merged.contains(&b.name) {
            MergeKind::Merged
        } else if !shallow && is_squash_merged(git, &base, &b.name)? {
            MergeKind::Squash
        } else if b.upstream_gone {
            MergeKind::Gone
        } else {
            continue;
        };

        candidates.push(Candidate {
            name: b.name,
            sha: b.sha,
            kind,
            upstream: b.upstream,
            remote_name: b.remote_name,
            upstream_gone: b.upstream_gone,
            last_commit_unix: b.last_commit_unix,
            subject: b.subject,
        });
    }

    candidates.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    Ok(Scan {
        base,
        candidates,
        warnings,
    })
}

fn resolve_base(git: &dyn Git, flag: Option<&str>) -> Result<String> {
    if let Some(base) = flag {
        let (ok, _) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{base}^{{commit}}"),
        ])?;
        if !ok {
            bail!("--base {base} does not resolve to a commit");
        }
        return Ok(base.to_string());
    }
    let (ok, out) = git.try_run(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
    if ok && let Some(rest) = out.trim().strip_prefix("refs/remotes/") {
        return Ok(rest.to_string());
    }
    for cand in ["origin/main", "origin/master", "main", "master"] {
        let (ok, _) = git.try_run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{cand}^{{commit}}"),
        ])?;
        if ok {
            return Ok(cand.to_string());
        }
    }
    bail!(
        "could not determine the base branch; pass --base <branch>, or run `git remote set-head origin --auto`"
    )
}

/// The checked-out branch, or None on a detached HEAD.
fn current_branch(git: &dyn Git) -> Result<Option<String>> {
    let (ok, out) = git.try_run(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    Ok(ok.then(|| out.trim().to_string()))
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
    patterns.extend(extra.iter().cloned());
    patterns.retain(|p| !p.is_empty());
    Ok(patterns)
}

fn is_protected(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// Exact match, or a glob with a single `*` wildcard (e.g. `release/*`).
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => {
            name.len() >= pre.len() + post.len() && name.starts_with(pre) && name.ends_with(post)
        }
    }
}

/// The git-trim technique: squash the whole branch into one synthetic commit
/// on top of the merge-base — exactly the diff a GitHub "Squash and merge"
/// lands on base — then let `git cherry` look for an equivalent patch-id.
/// The synthetic commit is a dangling object; gc reaps it.
fn is_squash_merged(git: &dyn Git, base: &str, branch: &str) -> Result<bool> {
    let (ok, merge_base) = git.try_run(&["merge-base", base, branch])?;
    if !ok {
        return Ok(false); // no common history
    }
    let merge_base = merge_base.trim().to_string();
    let tree = git.run(&["rev-parse", &format!("{branch}^{{tree}}")])?;
    let probe = git.run(&[
        "commit-tree",
        tree.trim(),
        "-p",
        &merge_base,
        "-m",
        "git-barber squash probe",
    ])?;
    let cherry = git.run(&["cherry", base, probe.trim()])?;
    Ok(cherry.lines().next().is_some_and(|l| l.starts_with('-')))
}

struct RawBranch {
    name: String,
    sha: String,
    upstream: Option<String>,
    remote_name: Option<String>,
    upstream_gone: bool,
    last_commit_unix: i64,
    subject: String,
}

impl RawBranch {
    /// One NUL-separated line of our `for-each-ref` format.
    fn parse(line: &str) -> Option<Self> {
        let mut f = line.split('\0');
        let name = f.next()?.to_string();
        let sha = f.next()?.to_string();
        let upstream = f.next()?;
        let track = f.next()?;
        let remote_name = f.next()?;
        let last_commit_unix = f.next()?.parse().ok()?;
        let subject = f.next().unwrap_or_default().to_string();
        Some(Self {
            name,
            sha,
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
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
    fn base_flag_is_verified() {
        let git = FakeGit::default().on(
            &["rev-parse", "--verify", "--quiet", "topic^{commit}"],
            Ok("abc\n"),
        );
        assert_eq!(resolve_base(&git, Some("topic")).unwrap(), "topic");

        let git = FakeGit::default().on(
            &["rev-parse", "--verify", "--quiet", "nope^{commit}"],
            Err(""),
        );
        assert!(resolve_base(&git, Some("nope")).is_err());
    }

    #[test]
    fn base_prefers_origin_head_symref() {
        let git = FakeGit::default().on(
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            Ok("refs/remotes/origin/trunk\n"),
        );
        assert_eq!(resolve_base(&git, None).unwrap(), "origin/trunk");
    }

    #[test]
    fn base_probes_fallbacks_in_order() {
        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Err(""),
            )
            .on(
                &["rev-parse", "--verify", "--quiet", "origin/main^{commit}"],
                Err(""),
            )
            .on(
                &["rev-parse", "--verify", "--quiet", "origin/master^{commit}"],
                Err(""),
            )
            .on(
                &["rev-parse", "--verify", "--quiet", "main^{commit}"],
                Ok("abc\n"),
            );
        assert_eq!(resolve_base(&git, None).unwrap(), "main");
    }

    #[test]
    fn base_resolution_failure_is_actionable() {
        let mut git = FakeGit::default().on(
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            Err(""),
        );
        for cand in ["origin/main", "origin/master", "main", "master"] {
            git = git.on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{cand}^{{commit}}"),
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
    }

    #[test]
    fn parse_ref_line() {
        let b = RawBranch::parse(
            "feat/x\0abc123\0origin/feat/x\0[gone]\0origin\x001700000000\0Add x\0extra",
        )
        .unwrap();
        assert_eq!(b.name, "feat/x");
        assert_eq!(b.sha, "abc123");
        assert_eq!(b.upstream.as_deref(), Some("origin/feat/x"));
        assert!(b.upstream_gone);
        assert_eq!(b.remote_name.as_deref(), Some("origin"));
        assert_eq!(b.last_commit_unix, 1_700_000_000);
        assert_eq!(b.subject, "Add x");

        let b = RawBranch::parse("local\0abc\0\0\0\x001700000000\0msg").unwrap();
        assert!(b.upstream.is_none());
        assert!(b.remote_name.is_none());
        assert!(!b.upstream_gone);
    }

    fn line(name: &str, sha: &str, upstream: &str, track: &str) -> String {
        format!("{name}\0{sha}\0{upstream}\0{track}\0origin\x001700000000\0subject")
    }

    #[test]
    fn scan_classifies_and_excludes() {
        let enumeration = [
            line("main", "s0", "origin/main", ""),
            line("feature-merged", "s1", "", ""),
            line("feature-squash", "s2", "", ""),
            line("feature-gone", "s3", "origin/feature-gone", "[gone]"),
            line("feature-active", "s4", "origin/feature-active", ""),
            line("release/1.0", "s5", "", ""),
        ]
        .join("\n");

        let git = FakeGit::default()
            .on(
                &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
                Ok("refs/remotes/origin/main\n"),
            )
            .on(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                Ok("feature-active\n"),
            )
            .on(&["config", "--get-all", "barber.protect"], Err(""))
            .on(&["rev-parse", "--is-shallow-repository"], Ok("false\n"))
            .on(
                &[
                    "for-each-ref",
                    "refs/heads",
                    "--merged",
                    "origin/main",
                    "--format=%(refname:short)",
                ],
                Ok("main\nfeature-merged\n"),
            )
            .on(
                &["for-each-ref", "refs/heads", super::FORMAT],
                Ok(&enumeration),
            )
            // squash probe for feature-squash → patch found upstream
            .on(
                &["merge-base", "origin/main", "feature-squash"],
                Ok("mb2\n"),
            )
            .on(&["rev-parse", "feature-squash^{tree}"], Ok("t2\n"))
            .on(
                &[
                    "commit-tree",
                    "t2",
                    "-p",
                    "mb2",
                    "-m",
                    "git-barber squash probe",
                ],
                Ok("p2\n"),
            )
            .on(&["cherry", "origin/main", "p2"], Ok("- p2\n"))
            // squash probe for feature-gone → no equivalent patch
            .on(&["merge-base", "origin/main", "feature-gone"], Ok("mb3\n"))
            .on(&["rev-parse", "feature-gone^{tree}"], Ok("t3\n"))
            .on(
                &[
                    "commit-tree",
                    "t3",
                    "-p",
                    "mb3",
                    "-m",
                    "git-barber squash probe",
                ],
                Ok("p3\n"),
            )
            .on(&["cherry", "origin/main", "p3"], Ok("+ p3\n"));

        let scan = scan(&git, None, &["release/*".to_string()]).unwrap();
        assert_eq!(scan.base, "origin/main");
        let kinds: Vec<(&str, MergeKind)> = scan
            .candidates
            .iter()
            .map(|c| (c.name.as_str(), c.kind))
            .collect();
        // main excluded (base counterpart), feature-active excluded (checked out,
        // and never probed), release/1.0 excluded (protected glob).
        assert_eq!(
            kinds,
            vec![
                ("feature-merged", MergeKind::Merged),
                ("feature-squash", MergeKind::Squash),
                ("feature-gone", MergeKind::Gone),
            ]
        );
        assert!(scan.candidates[0].selected_by_default());
        assert!(!scan.candidates[0].needs_force());
        assert!(scan.candidates[1].selected_by_default());
        assert!(scan.candidates[1].needs_force());
        assert!(!scan.candidates[2].selected_by_default());
    }

    #[test]
    fn detached_head_excludes_nothing() {
        let git = FakeGit::default().on(&["symbolic-ref", "--quiet", "--short", "HEAD"], Err(""));
        assert_eq!(current_branch(&git).unwrap(), None);
    }
}
