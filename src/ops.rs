use serde::Serialize;

use crate::git::Git;
use crate::scan::{Base, Candidate};

pub struct PlannedDeletion {
    pub candidate: Candidate,
    pub delete_remote: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum LocalOutcome {
    Deleted,
    /// `-D` was needed; we only force after verifying the merge ourselves.
    ForceDeleted,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum RemoteOutcome {
    /// `target` is the ref actually deleted (e.g. "origin/feat") — it can
    /// differ from the local branch name.
    Deleted {
        target: String,
        sha: String,
    },
    /// Not requested, or no live upstream to delete.
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct DeletionResult {
    pub name: String,
    pub sha: String,
    pub local: LocalOutcome,
    pub remote: RemoteOutcome,
    /// Copy-pasteable commands that restore what was deleted.
    pub undo: Vec<String>,
}

impl DeletionResult {
    pub fn failed(&self) -> bool {
        matches!(self.local, LocalOutcome::Failed(_))
            || matches!(self.remote, RemoteOutcome::Failed(_))
    }
}

/// Delete every planned branch, local first, then (optionally) its remote
/// counterpart. Never aborts mid-batch: each branch reports its own outcome.
pub fn execute(git: &dyn Git, base: &Base, plans: &[PlannedDeletion]) -> Vec<DeletionResult> {
    let current = current_branch(git);
    plans
        .iter()
        .map(|p| delete_one(git, base, current.as_deref(), p))
        .collect()
}

/// Delete a single planned branch. `current` is the checked-out branch,
/// re-read at execution time as defense in depth against scan-time bugs.
pub fn delete_one(
    git: &dyn Git,
    base: &Base,
    current: Option<&str>,
    plan: &PlannedDeletion,
) -> DeletionResult {
    let c = &plan.candidate;
    let mut undo = Vec::new();

    // Invariants scan.rs already enforces, re-asserted before destruction.
    let local = if Some(c.name.as_str()) == current {
        LocalOutcome::Failed("refusing to delete the checked-out branch".to_string())
    } else if c.name == base.name || Some(&c.refname) == base.refname.as_ref() {
        LocalOutcome::Failed("refusing to delete the base branch".to_string())
    } else {
        delete_local(git, base, c)
    };

    if matches!(local, LocalOutcome::Deleted | LocalOutcome::ForceDeleted) {
        undo.push(format!("git branch {} {}", c.name, short(&c.sha)));
    }

    let remote = if !plan.delete_remote {
        RemoteOutcome::Skipped
    } else if matches!(local, LocalOutcome::Failed(_)) {
        // Keep the remote branch when the local deletion failed; deleting
        // only the remote half would surprise more than it helps.
        RemoteOutcome::Skipped
    } else {
        delete_remote(git, c)
    };

    if let RemoteOutcome::Deleted { sha, .. } = &remote
        && let Some((r, b)) = remote_branch(c)
    {
        undo.push(format!("git push {r} {}:refs/heads/{b}", short(sha)));
    }

    DeletionResult {
        name: c.name.clone(),
        sha: c.sha.clone(),
        local,
        remote,
        undo,
    }
}

fn delete_local(git: &dyn Git, base: &Base, c: &Candidate) -> LocalOutcome {
    // Compare-then-delete: the tip must still be the sha the user consented
    // to. A branch advanced from another terminal between scan and delete
    // must survive. (The remaining check-to-delete window is milliseconds.)
    let tip = match git.try_run(&["rev-parse", "--verify", "--quiet", &c.refname]) {
        Ok((true, out)) => out.trim().to_string(),
        Ok((false, _)) => return LocalOutcome::Failed("branch no longer exists".to_string()),
        Err(e) => return LocalOutcome::Failed(e.to_string()),
    };
    if tip != c.sha {
        return LocalOutcome::Failed(format!(
            "tip moved since scan ({} → {}); rescan before deleting",
            short(&c.sha),
            short(&tip)
        ));
    }

    if c.needs_force() {
        // Squash/rebase/gone are never "merged" in git's ancestry sense; the
        // user consented to force via selection defaults or flags.
        return force_delete(git, c);
    }
    match git.try_run(&["branch", "-d", "--", &c.name]) {
        Ok((true, _)) => LocalOutcome::Deleted,
        Ok((false, first_err)) => {
            // `-d` judges merged-ness against HEAD/upstream, not our base.
            // If the verified tip is an ancestor of base, force with a clear
            // conscience; otherwise surface git's refusal.
            match git.try_run(&["merge-base", "--is-ancestor", &c.sha, &base.name]) {
                Ok((true, _)) => force_delete(git, c),
                _ => LocalOutcome::Failed(first_err.trim().to_string()),
            }
        }
        Err(e) => LocalOutcome::Failed(e.to_string()),
    }
}

fn force_delete(git: &dyn Git, c: &Candidate) -> LocalOutcome {
    match git.try_run(&["branch", "-D", "--", &c.name]) {
        Ok((true, _)) => LocalOutcome::ForceDeleted,
        Ok((false, err)) => LocalOutcome::Failed(err.trim().to_string()),
        Err(e) => LocalOutcome::Failed(e.to_string()),
    }
}

fn delete_remote(git: &dyn Git, c: &Candidate) -> RemoteOutcome {
    let (Some((remote, branch)), Some(upstream_ref)) =
        (remote_branch(c), c.upstream_ref.as_deref())
    else {
        return RemoteOutcome::Skipped; // gone or never tracked → nothing to delete
    };
    let target = format!("{remote}/{branch}");
    // Lease: delete only if the remote still points where our tracking ref
    // says. A colleague's push after our last fetch aborts the deletion.
    let expected = match git.try_run(&["rev-parse", "--verify", "--quiet", upstream_ref]) {
        Ok((true, out)) => out.trim().to_string(),
        _ => {
            return RemoteOutcome::Failed(format!(
                "cannot resolve {upstream_ref} for a leased delete"
            ));
        }
    };
    match git.try_run(&[
        "push",
        &format!("--force-with-lease=refs/heads/{branch}:{expected}"),
        &remote,
        &format!(":refs/heads/{branch}"),
    ]) {
        Ok((true, _)) => RemoteOutcome::Deleted {
            target,
            sha: expected,
        },
        Ok((false, err)) if err.contains("stale info") => RemoteOutcome::Failed(format!(
            "{target} moved since last fetch; run --fetch and rescan"
        )),
        Ok((false, err)) => RemoteOutcome::Failed(err.trim().to_string()),
        Err(e) => RemoteOutcome::Failed(e.to_string()),
    }
}

/// The live remote counterpart as (remote, branch), e.g. ("origin", "feat/x").
/// The branch part can differ from the local name — always show this to the
/// user before deleting remotely.
pub fn remote_branch(c: &Candidate) -> Option<(String, String)> {
    if c.upstream_gone {
        return None;
    }
    let remote = c.remote_name.clone()?;
    let branch = c
        .upstream
        .as_ref()?
        .strip_prefix(&format!("{remote}/"))?
        .to_string();
    Some((remote, branch))
}

pub(crate) fn current_branch(git: &dyn Git) -> Option<String> {
    match git.try_run(&["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok((true, out)) => Some(out.trim().to_string()),
        _ => None,
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fake::FakeGit;
    use crate::scan::MergeKind;

    const SHA: &str = "0123456789abcdef0123";

    fn base() -> Base {
        Base {
            name: "origin/main".into(),
            refname: Some("refs/remotes/origin/main".into()),
        }
    }

    fn candidate(name: &str, kind: MergeKind) -> Candidate {
        Candidate {
            name: name.into(),
            refname: format!("refs/heads/{name}"),
            sha: SHA.into(),
            kind,
            upstream: Some(format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: String::new(),
            upstream_ref: Some(format!("refs/remotes/origin/{name}")),
        }
    }

    fn plan(name: &str, kind: MergeKind, delete_remote: bool) -> PlannedDeletion {
        PlannedDeletion {
            candidate: candidate(name, kind),
            delete_remote,
        }
    }

    /// Cans the CAS pre-check as "tip unchanged".
    fn with_tip(git: FakeGit, name: &str) -> FakeGit {
        git.on(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ],
            Ok(&format!("{SHA}\n")),
        )
    }

    #[test]
    fn merged_branch_uses_gentle_delete() {
        let git = with_tip(FakeGit::default(), "feat").on(&["branch", "-d", "--", "feat"], Ok(""));
        let r = delete_one(
            &git,
            &base(),
            Some("main"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert_eq!(r.local, LocalOutcome::Deleted);
        assert_eq!(r.remote, RemoteOutcome::Skipped);
        assert_eq!(r.undo, vec!["git branch feat 0123456789ab"]);
    }

    #[test]
    fn moved_tip_refuses_deletion() {
        // The branch advanced between scan and delete: nothing may be run.
        let git = FakeGit::default().on(
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"],
            Ok("fedcba9876543210fedc\n"),
        );
        for kind in [MergeKind::Merged, MergeKind::Squash, MergeKind::Gone] {
            let r = delete_one(&git, &base(), None, &plan("feat", kind, true));
            let LocalOutcome::Failed(msg) = &r.local else {
                panic!("expected failure, got {:?}", r.local);
            };
            assert!(msg.contains("moved since scan"), "bad message: {msg}");
            assert_eq!(
                r.remote,
                RemoteOutcome::Skipped,
                "remote must stay untouched"
            );
            assert!(r.undo.is_empty());
        }
    }

    #[test]
    fn vanished_branch_reports_cleanly() {
        let git = FakeGit::default().on(
            &["rev-parse", "--verify", "--quiet", "refs/heads/feat"],
            Err(""),
        );
        let r = delete_one(&git, &base(), None, &plan("feat", MergeKind::Merged, false));
        assert!(matches!(r.local, LocalOutcome::Failed(ref m) if m.contains("no longer exists")));
    }

    #[test]
    fn gentle_delete_falls_back_to_force_only_when_verified() {
        let git = with_tip(FakeGit::default(), "feat")
            .on(
                &["branch", "-d", "--", "feat"],
                Err("error: not fully merged"),
            )
            .on(&["merge-base", "--is-ancestor", SHA, "origin/main"], Ok(""))
            .on(&["branch", "-D", "--", "feat"], Ok(""));
        let r = delete_one(
            &git,
            &base(),
            Some("other"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert_eq!(r.local, LocalOutcome::ForceDeleted);

        let git = with_tip(FakeGit::default(), "feat")
            .on(
                &["branch", "-d", "--", "feat"],
                Err("error: not fully merged"),
            )
            .on(
                &["merge-base", "--is-ancestor", SHA, "origin/main"],
                Err(""),
            );
        let r = delete_one(
            &git,
            &base(),
            Some("other"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert!(matches!(r.local, LocalOutcome::Failed(_)));
        assert!(r.undo.is_empty());
    }

    #[test]
    fn squash_rebase_and_gone_require_force() {
        for kind in [MergeKind::Squash, MergeKind::Rebase, MergeKind::Gone] {
            let git =
                with_tip(FakeGit::default(), "feat").on(&["branch", "-D", "--", "feat"], Ok(""));
            let r = delete_one(&git, &base(), None, &plan("feat", kind, false));
            assert_eq!(r.local, LocalOutcome::ForceDeleted);
        }
    }

    #[test]
    fn never_deletes_current_or_base() {
        let git = FakeGit::default(); // any git call would panic the fake
        let r = delete_one(
            &git,
            &base(),
            Some("feat"),
            &plan("feat", MergeKind::Merged, true),
        );
        assert!(matches!(r.local, LocalOutcome::Failed(_)));
        assert_eq!(
            r.remote,
            RemoteOutcome::Skipped,
            "remote must be kept when local failed"
        );

        let mut base_is_feat = base();
        base_is_feat.refname = Some("refs/heads/feat".into());
        let r = delete_one(
            &git,
            &base_is_feat,
            Some("main"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert!(matches!(r.local, LocalOutcome::Failed(_)));
    }

    #[test]
    fn remote_deletion_uses_a_lease_and_reports_the_target() {
        let git = with_tip(FakeGit::default(), "feat")
            .on(&["branch", "-d", "--", "feat"], Ok(""))
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/feat",
                ],
                Ok("aaaabbbbccccddddeeee\n"),
            )
            .on(
                &[
                    "push",
                    "--force-with-lease=refs/heads/feat:aaaabbbbccccddddeeee",
                    "origin",
                    ":refs/heads/feat",
                ],
                Ok(""),
            );
        let r = delete_one(&git, &base(), None, &plan("feat", MergeKind::Merged, true));
        assert_eq!(
            r.remote,
            RemoteOutcome::Deleted {
                target: "origin/feat".into(),
                sha: "aaaabbbbccccddddeeee".into()
            }
        );
        // The remote undo restores the REMOTE tip, not the local one.
        assert_eq!(
            r.undo,
            vec![
                "git branch feat 0123456789ab",
                "git push origin aaaabbbbcccc:refs/heads/feat",
            ]
        );
    }

    #[test]
    fn stale_remote_lease_fails_with_guidance() {
        let git = with_tip(FakeGit::default(), "feat")
            .on(&["branch", "-d", "--", "feat"], Ok(""))
            .on(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/feat",
                ],
                Ok("aaaabbbbccccddddeeee\n"),
            )
            .on(
                &[
                    "push",
                    "--force-with-lease=refs/heads/feat:aaaabbbbccccddddeeee",
                    "origin",
                    ":refs/heads/feat",
                ],
                Err("! [rejected] feat (stale info)"),
            );
        let r = delete_one(&git, &base(), None, &plan("feat", MergeKind::Merged, true));
        assert!(matches!(r.remote, RemoteOutcome::Failed(ref m) if m.contains("--fetch")));
    }

    #[test]
    fn gone_upstream_is_never_pushed_to() {
        let git = with_tip(FakeGit::default(), "feat").on(&["branch", "-D", "--", "feat"], Ok(""));
        let r = delete_one(&git, &base(), None, &plan("feat", MergeKind::Gone, true));
        assert_eq!(r.remote, RemoteOutcome::Skipped);
    }

    #[test]
    fn remote_target_can_differ_from_local_name() {
        // git checkout -b local-copy origin/shared → deleting remotely
        // targets "shared", and the user must see that name.
        let mut c = candidate("local-copy", MergeKind::Merged);
        c.upstream = Some("origin/shared".into());
        c.upstream_ref = Some("refs/remotes/origin/shared".into());
        assert_eq!(remote_branch(&c), Some(("origin".into(), "shared".into())));
    }
}
