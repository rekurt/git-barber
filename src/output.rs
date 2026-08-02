use anyhow::Result;
use serde::Serialize;

use crate::ops;
use crate::scan::{Candidate, MergeKind, Scan};

pub fn human_list(scan: &Scan, now_unix: i64) -> String {
    let mut out = String::new();
    for w in &scan.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    if scan.candidates.is_empty() {
        out.push_str(&format!("Nothing to trim (base: {}).\n", scan.base.name));
        return out;
    }
    out.push_str(&format!("base: {}\n", scan.base.name));

    let name_w = scan
        .candidates
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    for c in &scan.candidates {
        let upstream = match (&c.upstream, c.upstream_gone) {
            (Some(u), true) => format!("  ↑ {u} (gone)"),
            (Some(u), false) => format!("  ↑ {u}"),
            (None, _) => String::new(),
        };
        out.push_str(&format!(
            "  {:name_w$}  {:6}  {:>4} ago{}\n",
            c.name,
            kind_label(c.kind),
            age(now_unix, c.last_commit_unix),
            upstream,
        ));
    }

    let selected = scan
        .candidates
        .iter()
        .filter(|c| c.selected_by_default())
        .count();
    out.push_str(&format!(
        "\n{} candidate(s), {selected} selected by default. Run `git barber` for the TUI or `git barber --yes` to delete.\n",
        scan.candidates.len(),
    ));
    if scan.candidates.iter().any(|c| c.kind == MergeKind::Gone) {
        out.push_str("tip: gone branches are never deleted without explicit consent (--include-gone or a manual TUI check).\n");
    }
    out
}

pub fn kind_label(kind: MergeKind) -> &'static str {
    match kind {
        MergeKind::Merged => "merged",
        MergeKind::Squash => "squash",
        MergeKind::Rebase => "rebase",
        MergeKind::Gone => "gone",
    }
}

#[derive(Serialize)]
struct BranchJson<'a> {
    #[serde(flatten)]
    candidate: &'a Candidate,
    selected_by_default: bool,
}

#[derive(Serialize)]
struct ListJson<'a> {
    base: &'a str,
    warnings: &'a [String],
    branches: Vec<BranchJson<'a>>,
}

fn branches_json(scan: &Scan) -> Vec<BranchJson<'_>> {
    scan.candidates
        .iter()
        .map(|c| BranchJson {
            candidate: c,
            selected_by_default: c.selected_by_default(),
        })
        .collect()
}

pub fn json_list(scan: &Scan) -> Result<String> {
    let report = ListJson {
        base: &scan.base.name,
        warnings: &scan.warnings,
        branches: branches_json(scan),
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

/// Human label for a local outcome; the force wording depends on WHY the
/// force was justified (a gone branch is consented-to, not verified).
pub(crate) fn local_label(r: &ops::DeletionResult) -> String {
    match (&r.local, r.kind) {
        (ops::LocalOutcome::Deleted, _) => "deleted".to_string(),
        (ops::LocalOutcome::ForceDeleted, MergeKind::Merged) => {
            "force-deleted (verified merged into base)".to_string()
        }
        (ops::LocalOutcome::ForceDeleted, MergeKind::Squash | MergeKind::Rebase) => {
            "force-deleted (patch-id verified)".to_string()
        }
        (ops::LocalOutcome::ForceDeleted, MergeKind::Gone) => {
            "force-deleted (upstream was gone)".to_string()
        }
        (ops::LocalOutcome::Failed(msg), _) => format!("FAILED: {msg}"),
    }
}

pub fn human_execute(base: &str, warnings: &[String], results: &[ops::DeletionResult]) -> String {
    let mut out = String::new();
    // Warnings explain WHY branches may be missing (shallow repo, failed
    // probes, failed fetch) — the execute path needs them as much as --list.
    for w in warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out.push_str(&format!("base: {base}\n"));
    let name_w = results.iter().map(|r| r.name.len()).max().unwrap_or(0);
    for r in results {
        let local = local_label(r);
        let remote = match &r.remote {
            ops::RemoteOutcome::Deleted { target, .. } => format!(", deleted {target} on remote"),
            ops::RemoteOutcome::Skipped => String::new(),
            ops::RemoteOutcome::Failed(msg) => format!(", remote FAILED: {msg}"),
        };
        out.push_str(&format!("  {:name_w$}  {local}{remote}\n", r.name));
    }

    let undo: Vec<&String> = results.iter().flat_map(|r| &r.undo).collect();
    if !undo.is_empty() {
        out.push_str("\nundo:\n");
        for u in undo {
            out.push_str(&format!("  {u}\n"));
        }
        out.push_str("(recent tips also linger in `git reflog` for a while)\n");
    }
    out
}

/// The --yes --json report always carries the full candidate list (same
/// `branches` key as the listing, so scripts can share a parser) — a CI job
/// must be able to tell "clean repo" from "gone branches awaiting consent".
#[derive(Serialize)]
struct ExecuteJson<'a> {
    base: &'a str,
    warnings: &'a [String],
    branches: Vec<BranchJson<'a>>,
    results: &'a [ops::DeletionResult],
}

pub fn json_execute(scan: &Scan, results: &[ops::DeletionResult]) -> Result<String> {
    Ok(serde_json::to_string_pretty(&ExecuteJson {
        base: &scan.base.name,
        warnings: &scan.warnings,
        branches: branches_json(scan),
        results,
    })?)
}

/// Compact human age: 5m, 3h, 5d, 2w, 3mo, 1y.
pub(crate) fn age(now: i64, then: i64) -> String {
    let days = (now - then).max(0) / 86_400;
    let hours = (now - then).max(0) / 3_600;
    match days {
        d if d >= 365 => format!("{}y", d / 365),
        d if d >= 30 => format!("{}mo", d / 30),
        d if d >= 7 => format!("{}w", d / 7),
        d if d >= 1 => format!("{d}d"),
        _ if hours >= 1 => format!("{hours}h"),
        _ => format!("{}m", (((now - then).max(60)) / 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Base;

    #[test]
    fn ages() {
        assert_eq!(age(1000, 1000), "1m");
        assert_eq!(age(7200, 0), "2h");
        assert_eq!(age(86_400 * 3, 0), "3d");
        assert_eq!(age(86_400 * 10, 0), "1w");
        assert_eq!(age(86_400 * 45, 0), "1mo");
        assert_eq!(age(86_400 * 800, 0), "2y");
    }

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
            sha: "abc123".into(),
            kind,
            upstream: Some(format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: "subject".into(),
            upstream_ref: Some(format!("refs/remotes/origin/{name}")),
        }
    }

    #[test]
    fn human_list_empty_and_full() {
        let scan = Scan {
            base: base(),
            candidates: vec![],
            warnings: vec![],
        };
        assert!(human_list(&scan, 0).contains("Nothing to trim"));

        let scan = Scan {
            base: base(),
            candidates: vec![
                candidate("a", MergeKind::Merged),
                candidate("b", MergeKind::Gone),
            ],
            warnings: vec!["shallow".into()],
        };
        let text = human_list(&scan, 86_400);
        assert!(text.contains("warning: shallow"));
        assert!(text.contains("merged"));
        assert!(text.contains("(gone)"));
        assert!(text.contains("2 candidate(s), 1 selected"));
    }

    #[test]
    fn json_shapes() {
        let scan = Scan {
            base: base(),
            candidates: vec![candidate("a", MergeKind::Rebase)],
            warnings: vec![],
        };
        let value: serde_json::Value = serde_json::from_str(&json_list(&scan).unwrap()).unwrap();
        assert_eq!(value["base"], "origin/main");
        assert_eq!(value["branches"][0]["kind"], "rebase");
        assert_eq!(value["branches"][0]["selected_by_default"], true);

        // --yes --json with nothing deleted still reports the candidates,
        // under the same `branches` key as the listing.
        let value: serde_json::Value =
            serde_json::from_str(&json_execute(&scan, &[]).unwrap()).unwrap();
        assert_eq!(value["branches"][0]["name"], "a");
        assert_eq!(value["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn execute_report_shows_remote_target_and_undo() {
        let results = vec![ops::DeletionResult {
            name: "local-copy".into(),
            sha: "abc123".into(),
            kind: MergeKind::Merged,
            local: ops::LocalOutcome::Deleted,
            remote: ops::RemoteOutcome::Deleted {
                target: "origin/shared".into(),
                sha: "def456".into(),
            },
            undo: vec![
                "git branch local-copy abc123".into(),
                "git push origin def456:refs/heads/shared".into(),
            ],
        }];
        let text = human_execute("origin/main", &["late fetch".to_string()], &results);
        assert!(text.contains("warning: late fetch"), "{text}");
        assert!(text.contains("deleted origin/shared on remote"), "{text}");
        assert!(text.contains("undo:"));
        assert!(text.contains("git reflog"));
    }
}
