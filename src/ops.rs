use serde::Serialize;

use crate::git::Git;
use crate::scan::{Candidate, MergeKind};

pub struct PlannedDeletion {
    pub candidate: Candidate,
    pub delete_remote: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum LocalOutcome {
    Deleted,
    /// `-D` was needed; we only force after re-verifying the merge ourselves.
    ForceDeleted,
    Failed(String),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum RemoteOutcome {
    Deleted {
        remote: String,
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
pub fn execute(git: &dyn Git, base: &str, plans: &[PlannedDeletion]) -> Vec<DeletionResult> {
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
    base: &str,
    current: Option<&str>,
    plan: &PlannedDeletion,
) -> DeletionResult {
    let c = &plan.candidate;
    let mut undo = Vec::new();

    // Invariants scan.rs already enforces, re-asserted before destruction.
    let local = if Some(c.name.as_str()) == current {
        LocalOutcome::Failed("refusing to delete the checked-out branch".to_string())
    } else if c.name == base {
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

    if let RemoteOutcome::Deleted { remote: r } = &remote {
        let branch = remote_branch(c)
            .map(|(_, b)| b)
            .unwrap_or_else(|| c.name.clone());
        undo.push(format!(
            "git push {r} {}:refs/heads/{branch}",
            short(&c.sha)
        ));
    }

    DeletionResult {
        name: c.name.clone(),
        sha: c.sha.clone(),
        local,
        remote,
        undo,
    }
}

fn delete_local(git: &dyn Git, base: &str, c: &Candidate) -> LocalOutcome {
    if c.needs_force() {
        // Squash/gone branches are never "merged" in git's ancestry sense;
        // the user consented to force via selection defaults or flags.
        return match git.try_run(&["branch", "-D", &c.name]) {
            Ok((true, _)) => LocalOutcome::ForceDeleted,
            Ok((false, err)) => LocalOutcome::Failed(err.trim().to_string()),
            Err(e) => LocalOutcome::Failed(e.to_string()),
        };
    }
    match git.try_run(&["branch", "-d", &c.name]) {
        Ok((true, _)) => LocalOutcome::Deleted,
        Ok((false, first_err)) => {
            // `-d` judges merged-ness against HEAD/upstream, not our base.
            // If the tip is still an ancestor of base, force with a clear
            // conscience; otherwise surface git's refusal.
            match git.try_run(&["merge-base", "--is-ancestor", &c.name, base]) {
                Ok((true, _)) => match git.try_run(&["branch", "-D", &c.name]) {
                    Ok((true, _)) => LocalOutcome::ForceDeleted,
                    Ok((false, err)) => LocalOutcome::Failed(err.trim().to_string()),
                    Err(e) => LocalOutcome::Failed(e.to_string()),
                },
                _ => LocalOutcome::Failed(first_err.trim().to_string()),
            }
        }
        Err(e) => LocalOutcome::Failed(e.to_string()),
    }
}

fn delete_remote(git: &dyn Git, c: &Candidate) -> RemoteOutcome {
    let Some((remote, branch)) = remote_branch(c) else {
        return RemoteOutcome::Skipped; // gone or never tracked → nothing to delete
    };
    match git.try_run(&["push", &remote, "--delete", &branch]) {
        Ok((true, _)) => RemoteOutcome::Deleted { remote },
        Ok((false, err)) => RemoteOutcome::Failed(err.trim().to_string()),
        Err(e) => RemoteOutcome::Failed(e.to_string()),
    }
}

/// The live remote counterpart as (remote, branch), e.g. ("origin", "feat/x").
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

fn current_branch(git: &dyn Git) -> Option<String> {
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

    fn candidate(name: &str, kind: MergeKind) -> Candidate {
        Candidate {
            name: name.into(),
            sha: "0123456789abcdef0123".into(),
            kind,
            upstream: Some(format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: String::new(),
        }
    }

    fn plan(name: &str, kind: MergeKind, delete_remote: bool) -> PlannedDeletion {
        PlannedDeletion {
            candidate: candidate(name, kind),
            delete_remote,
        }
    }

    #[test]
    fn merged_branch_uses_gentle_delete() {
        let git = FakeGit::default().on(&["branch", "-d", "feat"], Ok("Deleted branch feat\n"));
        let r = delete_one(
            &git,
            "origin/main",
            Some("main"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert_eq!(r.local, LocalOutcome::Deleted);
        assert_eq!(r.remote, RemoteOutcome::Skipped);
        assert_eq!(r.undo, vec!["git branch feat 0123456789ab"]);
    }

    #[test]
    fn gentle_delete_falls_back_to_force_only_when_verified() {
        let git = FakeGit::default()
            .on(&["branch", "-d", "feat"], Err("error: not fully merged"))
            .on(
                &["merge-base", "--is-ancestor", "feat", "origin/main"],
                Ok(""),
            )
            .on(&["branch", "-D", "feat"], Ok(""));
        let r = delete_one(
            &git,
            "origin/main",
            Some("other"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert_eq!(r.local, LocalOutcome::ForceDeleted);

        let git = FakeGit::default()
            .on(&["branch", "-d", "feat"], Err("error: not fully merged"))
            .on(
                &["merge-base", "--is-ancestor", "feat", "origin/main"],
                Err(""),
            );
        let r = delete_one(
            &git,
            "origin/main",
            Some("other"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert!(matches!(r.local, LocalOutcome::Failed(_)));
        assert!(r.undo.is_empty());
    }

    #[test]
    fn squash_and_gone_require_force() {
        for kind in [MergeKind::Squash, MergeKind::Gone] {
            let git = FakeGit::default().on(&["branch", "-D", "feat"], Ok(""));
            let r = delete_one(&git, "origin/main", None, &plan("feat", kind, false));
            assert_eq!(r.local, LocalOutcome::ForceDeleted);
        }
    }

    #[test]
    fn never_deletes_current_or_base() {
        let git = FakeGit::default(); // any git call would panic the fake
        let r = delete_one(
            &git,
            "origin/main",
            Some("feat"),
            &plan("feat", MergeKind::Merged, true),
        );
        assert!(matches!(r.local, LocalOutcome::Failed(_)));
        assert_eq!(
            r.remote,
            RemoteOutcome::Skipped,
            "remote must be kept when local failed"
        );

        let r = delete_one(
            &git,
            "feat",
            Some("main"),
            &plan("feat", MergeKind::Merged, false),
        );
        assert!(matches!(r.local, LocalOutcome::Failed(_)));
    }

    #[test]
    fn remote_deletion_and_undo() {
        let git = FakeGit::default()
            .on(&["branch", "-d", "feat"], Ok(""))
            .on(&["push", "origin", "--delete", "feat"], Ok(""));
        let r = delete_one(
            &git,
            "origin/main",
            None,
            &plan("feat", MergeKind::Merged, true),
        );
        assert_eq!(
            r.remote,
            RemoteOutcome::Deleted {
                remote: "origin".into()
            }
        );
        assert_eq!(
            r.undo,
            vec![
                "git branch feat 0123456789ab",
                "git push origin 0123456789ab:refs/heads/feat",
            ]
        );
    }

    #[test]
    fn gone_upstream_is_never_pushed_to() {
        let git = FakeGit::default().on(&["branch", "-D", "feat"], Ok(""));
        let r = delete_one(
            &git,
            "origin/main",
            None,
            &plan("feat", MergeKind::Gone, true),
        );
        assert_eq!(r.remote, RemoteOutcome::Skipped);
    }
}
