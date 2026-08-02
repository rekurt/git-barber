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
        out.push_str(&format!("Nothing to trim (base: {}).\n", scan.base));
        return out;
    }
    out.push_str(&format!("base: {}\n", scan.base));

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

pub fn json_list(scan: &Scan) -> Result<String> {
    let report = ListJson {
        base: &scan.base,
        warnings: &scan.warnings,
        branches: scan
            .candidates
            .iter()
            .map(|c| BranchJson {
                candidate: c,
                selected_by_default: c.selected_by_default(),
            })
            .collect(),
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

pub fn human_execute(base: &str, results: &[ops::DeletionResult]) -> String {
    let mut out = format!("base: {base}\n");
    let name_w = results.iter().map(|r| r.name.len()).max().unwrap_or(0);
    for r in results {
        let local = match &r.local {
            ops::LocalOutcome::Deleted => "deleted".to_string(),
            ops::LocalOutcome::ForceDeleted => "force-deleted (verified merged)".to_string(),
            ops::LocalOutcome::Failed(msg) => format!("FAILED: {msg}"),
        };
        let remote = match &r.remote {
            ops::RemoteOutcome::Deleted { remote } => format!(", deleted on {remote}"),
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
    }
    out
}

#[derive(Serialize)]
struct ExecuteJson<'a> {
    base: &'a str,
    results: &'a [ops::DeletionResult],
}

pub fn json_execute(base: &str, results: &[ops::DeletionResult]) -> Result<String> {
    Ok(serde_json::to_string_pretty(&ExecuteJson {
        base,
        results,
    })?)
}

/// Compact human age: 5m, 3h, 5d, 2w, 3mo, 1y.
fn age(now: i64, then: i64) -> String {
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

    #[test]
    fn ages() {
        assert_eq!(age(1000, 1000), "1m");
        assert_eq!(age(7200, 0), "2h");
        assert_eq!(age(86_400 * 3, 0), "3d");
        assert_eq!(age(86_400 * 10, 0), "1w");
        assert_eq!(age(86_400 * 45, 0), "1mo");
        assert_eq!(age(86_400 * 800, 0), "2y");
    }

    fn candidate(name: &str, kind: MergeKind) -> Candidate {
        Candidate {
            name: name.into(),
            sha: "abc123".into(),
            kind,
            upstream: Some(format!("origin/{name}")),
            remote_name: Some("origin".into()),
            upstream_gone: kind == MergeKind::Gone,
            last_commit_unix: 0,
            subject: "subject".into(),
        }
    }

    #[test]
    fn human_list_empty_and_full() {
        let scan = Scan {
            base: "origin/main".into(),
            candidates: vec![],
            warnings: vec![],
        };
        assert!(human_list(&scan, 0).contains("Nothing to trim"));

        let scan = Scan {
            base: "origin/main".into(),
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
    fn json_shape() {
        let scan = Scan {
            base: "origin/main".into(),
            candidates: vec![candidate("a", MergeKind::Squash)],
            warnings: vec![],
        };
        let value: serde_json::Value = serde_json::from_str(&json_list(&scan).unwrap()).unwrap();
        assert_eq!(value["base"], "origin/main");
        assert_eq!(value["branches"][0]["kind"], "squash");
        assert_eq!(value["branches"][0]["selected_by_default"], true);
    }
}
