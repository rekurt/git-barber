//! End-to-end tests against real throwaway git repositories.
//!
//! Every repo lives in a tempdir and is fully hermetic: no system or user
//! git config is read, identity comes from environment variables.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::Command as BarberCommand;
use tempfile::TempDir;

/// Run git in `dir`, panicking on failure (test setup must not fail silently).
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@localhost")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@localhost")
        .output()
        .expect("failed to run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn commit_file(dir: &Path, name: &str, content: &str, message: &str) {
    std::fs::write(dir.join(name), content).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-m", message]);
}

/// A repo with an initial commit on `main`.
fn repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    git(&dir, &["init", "-b", "main"]);
    commit_file(&dir, "README.md", "hello", "initial");
    (tmp, dir)
}

/// The binary under test, pointed at `dir`, with hermetic git env.
fn barber(dir: &Path) -> BarberCommand {
    let mut cmd = BarberCommand::cargo_bin("git-barber").unwrap();
    cmd.arg("-C")
        .arg(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir);
    cmd
}

fn list_json(dir: &Path) -> serde_json::Value {
    let out = barber(dir).arg("--json").assert().success();
    serde_json::from_slice(&out.get_output().stdout).expect("--json must emit valid JSON")
}

fn branch_kinds(json: &serde_json::Value) -> Vec<(String, String)> {
    json["branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["name"].as_str().unwrap().to_string(),
                b["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn merge_commit_branch_is_detected_as_merged() {
    let (_tmp, dir) = repo();
    git(&dir, &["checkout", "-b", "feature"]);
    commit_file(&dir, "f.txt", "feature", "add feature");
    git(&dir, &["checkout", "main"]);
    git(
        &dir,
        &["merge", "--no-ff", "feature", "-m", "merge feature"],
    );

    assert_eq!(
        branch_kinds(&list_json(&dir)),
        vec![("feature".into(), "merged".into())]
    );
}

#[test]
fn squash_merged_branch_is_detected_as_squash() {
    let (_tmp, dir) = repo();
    git(&dir, &["checkout", "-b", "feature"]);
    commit_file(&dir, "a.txt", "one", "step one");
    commit_file(&dir, "b.txt", "two", "step two");
    git(&dir, &["checkout", "main"]);
    git(&dir, &["merge", "--squash", "feature"]);
    git(&dir, &["commit", "-m", "feature (squashed)"]);

    assert_eq!(
        branch_kinds(&list_json(&dir)),
        vec![("feature".into(), "squash".into())]
    );
}

#[test]
fn unmerged_branch_is_not_a_candidate() {
    let (_tmp, dir) = repo();
    git(&dir, &["checkout", "-b", "active"]);
    commit_file(&dir, "wip.txt", "wip", "work in progress");
    git(&dir, &["checkout", "main"]);

    let json = list_json(&dir);
    assert!(
        branch_kinds(&json).is_empty(),
        "unexpected candidates: {json}"
    );
}

/// Bare "origin" + clone. Returns (guard, origin_path, clone_path).
fn repo_with_origin() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let origin = tmp.path().join("origin.git");
    let clone = tmp.path().join("clone");
    std::fs::create_dir(&origin).unwrap();
    git(&origin, &["init", "--bare", "-b", "main"]);
    let seed = tmp.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    commit_file(&seed, "README.md", "hello", "initial");
    git(
        &seed,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&seed, &["push", "-u", "origin", "main"]);
    git(
        tmp.path(),
        &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()],
    );
    (tmp, origin, clone)
}

#[test]
fn gone_upstream_branch_needs_explicit_consent() {
    let (_tmp, origin, dir) = repo_with_origin();
    git(&dir, &["checkout", "-b", "was-merged-remotely"]);
    commit_file(&dir, "f.txt", "feature", "add feature");
    git(&dir, &["push", "-u", "origin", "was-merged-remotely"]);
    git(&dir, &["checkout", "main"]);
    // Simulate GitHub's "delete branch after merge" without merging the
    // commits (e.g. squash with conflict resolution we cannot patch-id match).
    git(&origin, &["branch", "-D", "was-merged-remotely"]);
    git(&dir, &["fetch", "--prune"]);

    assert_eq!(
        branch_kinds(&list_json(&dir)),
        vec![("was-merged-remotely".into(), "gone".into())]
    );
}

#[test]
fn protected_branches_are_excluded() {
    let (_tmp, dir) = repo();
    for name in ["develop", "release/1.0", "qa-env"] {
        git(&dir, &["branch", name]); // same tip as main → all "merged"
    }
    git(&dir, &["config", "barber.protect", "qa-*"]);

    let out = barber(&dir)
        .args(["--protect", "release/*", "--json"])
        .assert()
        .success();
    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert!(
        branch_kinds(&json).is_empty(),
        "develop (default), release/* (flag) and qa-* (config) must all be protected: {json}"
    );
}

#[test]
fn current_branch_is_excluded() {
    let (_tmp, dir) = repo();
    git(&dir, &["checkout", "-b", "same-as-main"]); // merged by definition, but checked out

    let json = list_json(&dir);
    assert!(
        branch_kinds(&json).is_empty(),
        "unexpected candidates: {json}"
    );
}

#[test]
fn list_is_default_outside_a_tty_and_never_deletes() {
    let (_tmp, dir) = repo();
    git(&dir, &["branch", "merged-branch"]);

    // stdin/stdout are pipes here, so no flags must still mean "list".
    barber(&dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("merged-branch"));
    git(&dir, &["rev-parse", "--verify", "merged-branch"]); // still exists
}

#[test]
fn not_a_repo_exits_2() {
    let tmp = TempDir::new().unwrap();
    barber(tmp.path()).assert().code(2);
}

#[test]
fn yes_deletes_merged_and_squash_but_not_gone_or_active() {
    let (_tmp, origin, dir) = repo_with_origin();
    // merged via merge commit
    git(&dir, &["checkout", "-b", "merged-one"]);
    commit_file(&dir, "m.txt", "m", "merged work");
    git(&dir, &["checkout", "main"]);
    git(&dir, &["merge", "--no-ff", "merged-one", "-m", "merge"]);
    // squash-merged
    git(&dir, &["checkout", "-b", "squashed-one"]);
    commit_file(&dir, "s.txt", "s", "squashed work");
    git(&dir, &["checkout", "main"]);
    git(&dir, &["merge", "--squash", "squashed-one"]);
    git(&dir, &["commit", "-m", "squash"]);
    git(&dir, &["push", "origin", "main"]);
    // gone upstream, unmerged content
    git(&dir, &["checkout", "-b", "gone-one"]);
    commit_file(&dir, "g.txt", "g", "gone work");
    git(&dir, &["push", "-u", "origin", "gone-one"]);
    git(&dir, &["checkout", "main"]);
    git(&origin, &["branch", "-D", "gone-one"]);
    git(&dir, &["fetch", "--prune"]);
    // active, untouched
    git(&dir, &["checkout", "-b", "active-one"]);
    commit_file(&dir, "a.txt", "a", "active work");
    git(&dir, &["checkout", "main"]);

    barber(&dir)
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicates::str::contains("merged-one"))
        .stdout(predicates::str::contains("undo:"));

    let left = git(&dir, &["branch", "--format=%(refname:short)"]);
    let left: Vec<&str> = left.lines().collect();
    assert_eq!(
        left,
        vec!["active-one", "gone-one", "main"],
        "only merged+squash must go"
    );
}

#[test]
fn include_gone_extends_yes_to_gone_branches() {
    let (_tmp, origin, dir) = repo_with_origin();
    git(&dir, &["checkout", "-b", "gone-one"]);
    commit_file(&dir, "g.txt", "g", "gone work");
    git(&dir, &["push", "-u", "origin", "gone-one"]);
    git(&dir, &["checkout", "main"]);
    git(&origin, &["branch", "-D", "gone-one"]);
    git(&dir, &["fetch", "--prune"]);

    barber(&dir)
        .args(["--yes", "--include-gone"])
        .assert()
        .success();
    let left = git(&dir, &["branch", "--format=%(refname:short)"]);
    assert_eq!(left.lines().collect::<Vec<_>>(), vec!["main"]);
}

#[test]
fn remote_flag_deletes_the_remote_counterpart_too() {
    let (_tmp, origin, dir) = repo_with_origin();
    git(&dir, &["checkout", "-b", "feature"]);
    commit_file(&dir, "f.txt", "f", "feature work");
    git(&dir, &["push", "-u", "origin", "feature"]);
    git(&dir, &["checkout", "main"]);
    git(&dir, &["merge", "--no-ff", "feature", "-m", "merge"]);
    git(&dir, &["push", "origin", "main"]);

    let out = barber(&dir)
        .args(["--yes", "--remote", "--json"])
        .assert()
        .success();
    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(json["results"][0]["remote"]["status"], "deleted");

    let remote_branches = git(&origin, &["branch", "--format=%(refname:short)"]);
    assert_eq!(
        remote_branches.lines().collect::<Vec<_>>(),
        vec!["main"],
        "gone from origin too"
    );
    let local = git(&dir, &["branch", "--format=%(refname:short)"]);
    assert_eq!(local.lines().collect::<Vec<_>>(), vec!["main"]);
}

#[test]
fn gentle_delete_falls_back_to_verified_force_from_another_branch() {
    let (_tmp, _origin, dir) = repo_with_origin();
    // `other` diverges from the initial commit and never sees the merge.
    git(&dir, &["branch", "other", "main"]);
    git(&dir, &["checkout", "-b", "feature"]);
    commit_file(&dir, "f.txt", "f", "feature work");
    git(&dir, &["checkout", "main"]);
    git(&dir, &["merge", "--no-ff", "feature", "-m", "merge"]);
    git(&dir, &["push", "origin", "main"]);
    // From `other`, plain `git branch -d feature` refuses (not merged into
    // HEAD); the tool must verify against origin/main and force.
    git(&dir, &["checkout", "other"]);

    barber(&dir)
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicates::str::contains("force-deleted (verified merged)"));
    let left = git(&dir, &["branch", "--format=%(refname:short)"]);
    assert_eq!(left.lines().collect::<Vec<_>>(), vec!["main", "other"]);
}

#[test]
fn undo_hint_actually_restores_the_branch() {
    let (_tmp, dir) = repo();
    git(&dir, &["checkout", "-b", "feature"]);
    commit_file(&dir, "f.txt", "f", "feature work");
    git(&dir, &["checkout", "main"]);
    git(&dir, &["merge", "--no-ff", "feature", "-m", "merge"]);
    let sha = git(&dir, &["rev-parse", "feature"]).trim().to_string();

    let out = barber(&dir).args(["--yes", "--json"]).assert().success();
    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let undo = json["results"][0]["undo"][0].as_str().unwrap().to_string();
    let undo_args: Vec<&str> = undo.split_whitespace().skip(1).collect();

    git(&dir, &undo_args);
    assert_eq!(git(&dir, &["rev-parse", "feature"]).trim(), sha);
}
