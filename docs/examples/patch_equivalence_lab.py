#!/usr/bin/env python3
"""Disposable, local-only lab. Never deletes branches or calls the cleanup mode."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

if len(sys.argv) != 3:
    raise SystemExit("usage: python3 patch_equivalence_lab.py CLASSIFIER GIT_BARBER")
classifier, barber = (str(Path(p).resolve(strict=True)) for p in sys.argv[1:])
lab = Path(tempfile.mkdtemp(prefix="barber-patch-lab-"))
repo = lab / "repo"
env = os.environ.copy()
for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE",
            "GIT_CONFIG_PARAMETERS"):
    env.pop(key, None)
env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
           GIT_CONFIG_COUNT="0", GIT_OPTIONAL_LOCKS="0",
           GIT_AUTHOR_NAME="Fixture", GIT_COMMITTER_NAME="Fixture",
           GIT_AUTHOR_EMAIL="fixture@example.invalid", GIT_COMMITTER_EMAIL="fixture@example.invalid",
           GIT_AUTHOR_DATE="2026-01-01T00:00:00+00:00", GIT_COMMITTER_DATE="2026-01-01T00:00:00+00:00")

def run(*args, expected=0):
    result = subprocess.run(args, env=env, text=True, capture_output=True)
    if result.returncode != expected:
        raise RuntimeError(f"{args!r}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip()

def git(*args, expected=0):
    return run("git", "-C", str(repo), *args, expected=expected)

def change(file, text, message):
    (repo / file).write_text(text + "\n")
    git("add", "--", file)
    git("commit", "-m", message)

empty_template = lab / "empty-template"
empty_template.mkdir()
run("git", "init", "--template=" + str(empty_template), "-b", "main", str(repo))
git("config", "core.hooksPath", os.devnull)
change("shared.txt", "base", "base")

git("switch", "-c", "squash")
change("sq-a.txt", "a", "squash a")
change("sq-b.txt", "b", "squash b")
git("switch", "main")
git("merge", "--squash", "squash")
git("commit", "-m", "integrate squash")

git("switch", "-c", "rebase")
change("rb-a.txt", "a", "rebase a")
first = git("rev-parse", "HEAD")
change("rb-b.txt", "b", "rebase b")
second = git("rev-parse", "HEAD")
git("switch", "main")
change("context.txt", "main context", "advance base")
git("cherry-pick", first, second)
assert git("rev-parse", "HEAD") != second

git("switch", "-c", "classic")
change("classic.txt", "ordinary", "classic")
git("switch", "main")
git("merge", "--no-ff", "classic", "-m", "ordinary merge")

git("switch", "-c", "conflict")
change("shared.txt", "branch value", "branch edit")
git("switch", "main")
change("shared.txt", "main value", "main edit")
git("merge", "--squash", "conflict", expected=1)
change("shared.txt", "resolved value", "resolve squash conflict differently")

git("switch", "-c", "empty")
change("empty-a.txt", "a", "empty a")
ea = git("rev-parse", "HEAD")
change("empty-b.txt", "b", "empty b")
eb = git("rev-parse", "HEAD")
git("commit", "--allow-empty", "-m", "metadata-only checkpoint")
git("switch", "main")
change("context.txt", "more context", "advance base again")
git("cherry-pick", ea, eb)

git("switch", "-c", "reverted")
change("reverted.txt", "later removed", "reverted topic")
git("switch", "main")
git("merge", "--squash", "reverted")
git("commit", "-m", "integrate reverted topic")
git("revert", "--no-edit", "HEAD")
assert not (repo / "reverted.txt").exists()

git("switch", "-c", "gone")
change("gone.txt", "not integrated", "unmerged topic")
git("switch", "main")
# A missing tracking ref is simulated locally; no network or branch deletion.
origin = lab / "origin.git"
run("git", "init", "--bare", "--template=" + str(empty_template), str(origin))
git("remote", "add", "origin", str(origin))
git("config", "branch.gone.remote", "origin")
git("config", "branch.gone.merge", "refs/heads/gone")

def snapshot(directory):
    return {str(p.relative_to(directory)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in sorted(directory.rglob("*")) if p.is_file()}

before = snapshot(repo)
expected = {"classic": "Ancestor", "squash": "SquashPatch", "rebase": "ReplayedPatches",
            "conflict": "Unknown", "empty": "Unknown", "reverted": "SquashPatch", "gone": "Unknown"}
observed = {branch: run(classifier, str(repo), "refs/heads/main", "refs/heads/" + branch)
            for branch in expected}
assert observed == expected, observed
listing = run(barber, "-C", str(repo), "--base", "refs/heads/main", "--list", "--no-cache")
report = json.loads(run(barber, "-C", str(repo), "--base", "refs/heads/main", "--json", "--no-cache"))
rows = {row["name"]: row for row in report["branches"]}
assert {name: row["kind"] for name, row in rows.items()} == {
    "classic": "merged", "squash": "squash", "rebase": "rebase", "reverted": "squash", "gone": "gone"}, rows
assert rows["gone"]["selected_by_default"] is False
assert all(rows[name]["selected_by_default"] for name in ("classic", "squash", "rebase", "reverted"))
assert snapshot(repo) == before, "read-only commands changed fixture files"

shallow = lab / "shallow"
run("git", "clone", "--template=" + str(empty_template), "--no-local", "--depth", "1",
    "--no-single-branch", repo.as_uri(), str(shallow))
run("git", "-C", str(shallow), "config", "core.hooksPath", os.devnull)
run("git", "-C", str(shallow), "branch", "squash", "refs/remotes/origin/squash")
shallow_before = snapshot(shallow)
assert run(classifier, str(shallow), "refs/heads/main", "refs/heads/squash") == "ShallowHistory"
shallow_report = json.loads(run(barber, "-C", str(shallow), "--base", "refs/heads/main", "--json", "--no-cache"))
assert any("shallow" in warning for warning in shallow_report["warnings"])
assert not any(row["kind"] in ("squash", "rebase") for row in shallow_report["branches"])
assert snapshot(shallow) == shallow_before
(lab / "list.txt").write_text(listing + "\n")
(lab / "report.json").write_text(json.dumps(report, indent=2) + "\n")
(lab / "verification.json").write_text(json.dumps({"classifier": observed, "shallow": "ShallowHistory",
    "all_fixture_files_unchanged_by_read_commands": True}, indent=2) + "\n")
print(json.dumps(observed, indent=2))
print(listing)
print("ShallowHistory; shallow warning verified; all fixture files unchanged by read commands.")
print("Retained disposable fixture and reports:", lab)
