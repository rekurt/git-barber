#!/usr/bin/env python3
"""Apply GitHub branch policy, then delegate merge verification/deletion to Git Barber."""

import argparse
import fnmatch
import html
import json
import os
from pathlib import Path
import shlex
import subprocess
import time
import urllib.request


PROTECTED = ("main", "master", "dev", "develop", "release/*", "hotfix/*")
VERIFIED = {"merged", "squash", "rebase"}


class GitHub:
    def __init__(self, repository, token):
        if not token:
            raise RuntimeError("GH_TOKEN is required")
        self.repository = repository
        self.token = token

    def get(self, path):
        request = urllib.request.Request(
            "https://api.github.com/repos/" + self.repository + path,
            headers={
                "Authorization": "Bearer " + self.token,
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)

    def pages(self, path):
        result = []
        for page in range(1, 10001):
            separator = "&" if "?" in path else "?"
            batch = self.get(f"{path}{separator}per_page=100&page={page}")
            if not isinstance(batch, list):
                raise RuntimeError("Incomplete GitHub list response")
            result.extend(batch)
            if len(batch) < 100:
                return result
        raise RuntimeError("GitHub pagination did not finish")

    def snapshot(self):
        metadata = self.get("")
        branches = self.pages("/branches")
        pulls = self.pages("/pulls?state=open")
        # Missing policy data must never be interpreted as permission to delete.
        default = metadata["default_branch"]
        branch_map = {}
        for branch in branches:
            if not isinstance(branch["protected"], bool):
                raise RuntimeError("Missing branch protection data")
            branch_map[branch["name"]] = {
                "sha": branch["commit"]["sha"], "protected": branch["protected"]
            }
        if default not in branch_map:
            raise RuntimeError("Default branch is missing from GitHub response")
        open_heads = set()
        for pull in pulls:
            head = pull["head"]
            if head["repo"] and head["repo"]["full_name"] == self.repository:
                open_heads.add(head["ref"])
        return default, branch_map, open_heads


def run(repo, *args):
    return subprocess.run(args, cwd=repo, text=True, capture_output=True)


def git(repo, *args):
    result = run(repo, "git", *args)
    if result.returncode:
        raise RuntimeError(f"git {args[0]} failed: {result.stderr.strip()}")
    return result.stdout.strip()


def exclusion(name, branch, default, open_heads, timestamp, cutoff, patterns):
    if name == default or any(fnmatch.fnmatchcase(name, p) for p in patterns):
        return "protected-name"
    if branch["protected"]:
        return "github-protected"
    if name in open_heads:
        return "open-pull-request"
    if timestamp >= cutoff:
        return "younger-than-minimum-age"
    return None


def barber(repo, binary, base, delete=False):
    args = [binary, "--base", base, "--json", "--no-cache"]
    args += ["--yes", "--remote"] if delete else ["--list"]
    result = run(repo, *args)
    if not result.stdout.strip():
        raise RuntimeError("Git Barber returned no JSON: " + result.stderr.strip())
    report = json.loads(result.stdout)
    if result.returncode not in (0, 1) or (not delete and result.returncode):
        raise RuntimeError("Git Barber failed: " + result.stderr.strip())
    if report.get("warnings") and not delete:
        raise RuntimeError("Git Barber reported incomplete verification: " +
                           "; ".join(report["warnings"]))
    if not isinstance(report.get("branches"), list):
        raise RuntimeError("Git Barber returned an invalid branch report")
    return (1 if report.get("warnings") else result.returncode), report


def clean(repo, api, binary, dry_run, min_age_days, patterns, report, now=None):
    now = time.time() if now is None else now
    cutoff = now - min_age_days * 86400
    default, branches, open_heads = api.snapshot()
    report.update(default_branch=default, dry_run=dry_run, min_age_days=min_age_days,
                  age_basis="tip committer timestamp", branches=[], results=[])
    if git(repo, "rev-parse", "--is-shallow-repository") != "false":
        raise RuntimeError("Full Git history is required")
    # Refuse developer checkouts: this script creates/removes disposable local refs.
    if git(repo, "for-each-ref", "--format=%(refname)", "refs/heads"):
        raise RuntimeError("Use a fresh detached checkout with no local branches")
    base = "refs/remotes/origin/" + default
    if git(repo, "rev-parse", base) != branches[default]["sha"]:
        raise RuntimeError("Default branch changed since fetch; retry with a fresh checkout")
    git(repo, "checkout", "--detach", base)
    eligible = {}
    for name, branch in sorted(branches.items()):
        ref = "refs/remotes/origin/" + name
        sha = git(repo, "rev-parse", "--verify", ref)
        if sha != branch["sha"]:
            raise RuntimeError("Branch changed since fetch: " + name)
        timestamp = int(git(repo, "show", "-s", "--format=%ct", sha))
        reason = exclusion(name, branch, default, open_heads, timestamp, cutoff, patterns)
        row = {"name": name, "sha": sha, "last_commit_unix": timestamp,
               "status": "skipped", "reason": reason}
        report["branches"].append(row)
        if reason is None:
            git(repo, "branch", "--track", "--", name, ref)
            eligible[name] = row
    _, scan = barber(repo, binary, base)
    report["scan"] = scan
    candidates = {b["name"]: b for b in scan["branches"]
                  if b["name"] in eligible and b.get("kind") in VERIFIED
                  and b.get("selected_by_default") is True
                  and b["sha"] == eligible[b["name"]]["sha"]}
    for name, row in eligible.items():
        if name in candidates:
            row.update(status="candidate", reason=None, kind=candidates[name]["kind"])
        else:
            row["reason"] = "not-verified-merged"
            git(repo, "branch", "-D", "--", name)
    if dry_run or not candidates:
        report["status"] = "dry-run" if dry_run else "nothing-to-delete"
        return 0
    # Re-read every page before any remote mutation. Never fetch new tips here:
    # Git Barber's lease must retain the original, verified remote SHA.
    current_default, current, current_pulls = api.snapshot()
    if current_default != default or current[default]["sha"] != branches[default]["sha"]:
        raise RuntimeError("Default branch changed during scan; retry")
    for name in candidates:
        row = eligible[name]
        branch = current.get(name)
        reason = "branch-moved-or-gone" if not branch or branch["sha"] != row["sha"] else (
            exclusion(name, branch, default, current_pulls, row["last_commit_unix"],
                      cutoff, patterns))
        if reason:
            row.update(status="skipped", reason=reason)
            git(repo, "branch", "-D", "--", name)
    code, executed = barber(repo, binary, base, delete=True)
    report["execution"] = executed
    report["results"] = executed["results"]
    for result in report["results"]:
        eligible[result["name"]]["status"] = result["remote"]["status"]
    report["status"] = "failed" if code else "complete"
    return code


def write_report(directory, report):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    undo = []
    for result in report.get("results", []):
        # Preserve the full remote SHA, rather than relying on an abbreviated
        # object ID being present in a later fresh clone.
        remote = result.get("remote", {})
        if remote.get("status") == "deleted":
            detail = remote["detail"]
            command = "git push origin " + shlex.quote(detail["sha"] + ":refs/heads/" + result["name"])
            undo.append(command)
        undo.extend(result.get("undo", []))
    (directory / "undo.txt").write_text("\n".join(undo) + ("\n" if undo else ""))
    summary = ["## Git Barber branch cleanup", "", "Status: " + report["status"],
               "", "| Branch | Result | Reason |", "| --- | --- | --- |"]
    for row in report.get("branches", [])[:100]:
        cells = [html.escape(str(row.get(k) or "")).replace("|", "&#124;")
                 for k in ("name", "status", "reason")]
        summary.append("| " + " | ".join(cells) + " |")
    if report.get("error"):
        summary += ["", "Error: " + html.escape(report["error"])]
    summary += ["", "Full JSON and recovery commands are in the 30-day artifact."]
    content = "\n".join(summary) + "\n"
    (directory / "summary.md").write_text(content)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as target:
            target.write(content)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--report-dir", required=True)
    parser.add_argument("--dry-run", choices=("true", "false"), default="true")
    parser.add_argument("--min-age-days", type=int, default=7)
    parser.add_argument("--protect", default="")
    args = parser.parse_args()
    report = {"repository": args.repository, "status": "failed", "results": []}
    code = 1
    try:
        if args.min_age_days < 7:
            raise RuntimeError("Minimum age must be at least 7 days")
        api = GitHub(args.repository, os.environ.get("GH_TOKEN"))
        patterns = PROTECTED + tuple(p.strip() for p in args.protect.splitlines() if p.strip())
        code = clean(Path(args.repo), api, str(Path(args.binary).resolve()),
                     args.dry_run == "true", args.min_age_days, patterns, report)
    except Exception as error:
        report.update(status="failed", error=str(error))
        print("Branch cleanup failed: " + str(error))
    finally:
        write_report(args.report_dir, report)
    return code


if __name__ == "__main__":
    raise SystemExit(main())
