# Recognizing squash and replayed Git patches with Rust

**Disclosure:** This draft was written by an AI assistant on behalf of rekurt, the maintainer of git-barber. Its examples have passed automated checks and AI-assisted technical review; human editorial review has not been performed. It describes that affiliated project and does not attribute authorship or personal experience to the maintainer.

Git already exposes ancestry-based tools such as `git branch --merged` and `git branch -d`. A squash merge or a rebase can leave the original branch tip outside the base branch's ancestry, even when equivalent changes were integrated. This walkthrough builds a read-only Rust classifier that adds patch evidence to the ancestry check. Its output is evidence for review, not permission to delete a branch.

The example follows [git-barber's scanner at commit bb59cdf](https://github.com/rekurt/git-barber/blob/bb59cdfaf3a069b20ad04a33887f721550c31407/src/scan.rs), version 0.3.0. It isolates the classifier and additionally pins log presentation independently of Git configuration; the production source is unchanged. Production scanning also handles protected branches, worktrees, candidate selection and caching. Those policies and deletion operations are outside this example.

The inspection-only claim assumes a trusted, fully materialized repository with every required object already local. In a partial clone, even inspection commands can lazily fetch missing objects and write packs. This example does not prevent that behavior and must not be used to infer network isolation or unchanged repository files in a partial clone. The disposable lab uses local, fully materialized repositories; its shallow clone truncates history but is not a partial clone.

## Choose the evidence before implementing the subprocesses

Resolve both ref names to commit IDs once. An ancestry check comes first: `git merge-base --is-ancestor BRANCH BASE` returns 0 for an ancestor, 1 for a negative result, and another status for an error. An error must not become “not merged.” If the repository is shallow, stop before patch comparisons because its available history is incomplete. If there is no common ancestor, return `Unknown`.

For a full history, find the merge base and collect patch IDs from non-merge commits in `FORK..BASE`. [Git's stable patch ID](https://git-scm.com/docs/git-patch-id) ignores whitespace and line numbers and is insensitive to file-diff order. It identifies likely duplicate patches; it does not establish semantic equivalence of programs. Use the same explicit diff options on both sides: configuration-dependent rename detection, diff algorithms or context could otherwise alter the input.

Log presentation is input to the parser too. `format.pretty`, `log.date`, `log.abbrevCommit` and signature display can change that stream. Medium output contains a Date field; a custom strftime date can include literal newlines and diff-like text, so selecting medium alone is insufficient. Set `--format=medium --date=default --no-abbrev-commit --no-show-signature` explicitly for both log queries: full commit headers delimit patches, and the configured pretty/date text is excluded from the stream. These options address the tested presentation overrides; this is not a claim about every possible Git configuration. The lab separately tests a deliberately diff-shaped pretty format and a custom date containing literal newlines and a synthetic patch on the existing replay fixture.

Compare the combined `diff-tree FORK BRANCH` patch against the base's individual patches. A match is squash-style evidence. If it misses, compare each non-merge branch commit with that set. Require at least one commit, no merge commits, one patch ID per commit, and membership for every ID. Empty commits produce no patch ID, so the count check prevents silently dropping them. Merge commits can contain conflict-resolution changes that `log --no-merges` omits. These guards apply to the replay stage; the combined-patch check ran earlier.

A `HashSet` gives membership, not sequence or multiplicity equivalence. Neither stage compares commit messages or authors. A patch can appear in the base history and later be reverted: the match remains even though the current tree no longer contains the change. Review the current tree and the branch's purpose before acting on this evidence.

## Implement a read-only classifier

The complete program below uses only the standard library. The subprocess wrapper passes argument vectors, propagates failures, and clears Git's repository-selection environment variables. A writer thread feeds `patch-id` while `wait_with_output` drains its output; writing all input before draining output can deadlock on large patches. The example buffers patches in memory and makes no scalability claim.

Save this as `docs/examples/patch_equivalence.rs` (the [companion source](examples/patch_equivalence.rs) is identical):

```rust
//! Inspection example for a trusted repository with all required objects local.
//! Not a branch deletion policy; partial clones may lazily fetch objects.
use std::{
    collections::HashSet,
    error::Error,
    io::Write,
    process::{Command, Output, Stdio},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const FLAGS: &[&str] = &[
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--no-relative",
    "--full-index",
    "--diff-algorithm=myers",
    "-U3",
    "--inter-hunk-context=0",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--submodule=short",
    "--ignore-submodules=none",
];
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

struct Git<'a>(&'a str);
impl Git<'_> {
    fn output(&self, args: &[&str], input: Option<Vec<u8>>) -> Result<Output> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(self.0).args(args);
        for key in REPO_ENV {
            cmd.env_remove(key);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn()?;
        // Drain stdout while writing: a large patch must not deadlock the pipes.
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            std::thread::spawn(move || stdin.write_all(&bytes))
        });
        let output = child.wait_with_output()?;
        if let Some(writer) = writer {
            writer.join().map_err(|_| "stdin writer panicked")??;
        }
        Ok(output)
    }
    fn run(&self, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
        let output = self.output(args, input)?;
        if !output.status.success() {
            return Err(
                format!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into(),
            );
        }
        Ok(output.stdout)
    }
    fn text(&self, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8(self.run(args, None)?)?.trim().to_owned())
    }
    fn oid(&self, name: &str) -> Result<String> {
        self.text(&["rev-parse", "--verify", &format!("{name}^{{commit}}")])
    }
    fn patches(&self, args: &[&str]) -> Result<Vec<String>> {
        let patch = self.run(args, None)?;
        let ids = self.run(&["patch-id", "--stable"], Some(patch))?;
        Ok(String::from_utf8(ids)?
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect())
    }
    fn log_ids(&self, range: &str) -> Result<Vec<String>> {
        // Pin headers as well as diff options: patch-id parses this stream.
        let mut args = vec![
            "log",
            "-p",
            "--no-merges",
            "--format=medium",
            "--date=default",
            "--no-abbrev-commit",
            "--no-show-signature",
        ];
        args.extend_from_slice(FLAGS);
        args.push(range);
        self.patches(&args)
    }
}

#[derive(Debug, PartialEq)]
enum Evidence {
    Ancestor,
    SquashPatch,
    ReplayedPatches,
    ShallowHistory,
    Unknown,
}

fn classify(git: &Git<'_>, base: &str, branch: &str) -> Result<Evidence> {
    // Resolve names once; every later query uses the same tips.
    let base = git.oid(base)?;
    let branch = git.oid(branch)?;
    let ancestry = git.output(&["merge-base", "--is-ancestor", &branch, &base], None)?;
    match ancestry.status.code() {
        Some(0) => return Ok(Evidence::Ancestor),
        Some(1) => {}
        _ => {
            return Err(format!(
                "ancestry query failed: {}",
                String::from_utf8_lossy(&ancestry.stderr)
            )
            .into());
        }
    }
    if git.text(&["rev-parse", "--is-shallow-repository"])? == "true" {
        return Ok(Evidence::ShallowHistory);
    }
    let fork = git.output(&["merge-base", &base, &branch], None)?;
    match fork.status.code() {
        Some(0) => {}
        Some(1) => return Ok(Evidence::Unknown),
        _ => {
            return Err(format!(
                "merge-base failed: {}",
                String::from_utf8_lossy(&fork.stderr)
            )
            .into());
        }
    }
    let fork = String::from_utf8(fork.stdout)?.trim().to_owned();
    let upstream: HashSet<_> = git
        .log_ids(&format!("{fork}..{base}"))?
        .into_iter()
        .collect();
    if upstream.is_empty() {
        return Ok(Evidence::Unknown);
    }

    let mut args = vec!["diff-tree", "-p", "-r"];
    args.extend_from_slice(FLAGS);
    args.extend([fork.as_str(), branch.as_str()]);
    if git
        .patches(&args)?
        .first()
        .is_some_and(|id| upstream.contains(id))
    {
        return Ok(Evidence::SquashPatch);
    }

    let range = format!("{fork}..{branch}");
    let merges: usize = git
        .text(&["rev-list", "--min-parents=2", "--count", &range])?
        .parse()?;
    if merges != 0 {
        return Ok(Evidence::Unknown);
    }
    let ids = git.log_ids(&range)?;
    let commits: usize = git
        .text(&["rev-list", "--no-merges", "--count", &range])?
        .parse()?;
    // Empty commits produce no patch-id. Do not silently lose them here.
    if commits > 0 && ids.len() == commits && ids.iter().all(|id| upstream.contains(id)) {
        return Ok(Evidence::ReplayedPatches);
    }
    Ok(Evidence::Unknown)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: patch-equivalence REPO BASE BRANCH".into());
    }
    println!("{:?}", classify(&Git(&args[1]), &args[2], &args[3])?);
    Ok(())
}
```

`Unknown` means that these checks found no sufficient evidence. It is not proof that a branch was never integrated. For example, conflict resolution may change a squash patch so that it no longer matches the original branch patch. Integration spread across unrelated commits may also evade both comparisons.

## Run the disposable lab

Requirements: Git, Python 3, and a Rust 2024-capable toolchain. Building git-barber itself requires Rust 1.88 or newer and its locked Cargo dependencies. From the root of a checkout containing this article:

```sh
cargo build --locked
lab_bin_dir=$(mktemp -d)
rustc --edition=2024 docs/examples/patch_equivalence.rs -o "$lab_bin_dir/patch-equivalence"
python3 docs/examples/patch_equivalence_lab.py "$lab_bin_dir/patch-equivalence" target/debug/git-barber
```

The [lab script](examples/patch_equivalence_lab.py) creates fresh temporary repositories, an empty local bare remote, synthetic commits and a shallow file-URL clone. It isolates global/system Git configuration and disables fixture hooks. It performs no network fetch or branch deletion and retains the temporary directory for inspection. Its repository writes create the examples; classification and git-barber listings are checked separately for changes by hashing every fixture file before and after, including `.git`.

The lab also sets `format.pretty` to `format:diff --git a/%h b/%h`, enables abbreviated commits and signature display, then asserts that the teaching classifier still reports the existing `rebase` fixture as `ReplayedPatches`. That regression fails with `Unknown` before log formatting is pinned. It then restores medium formatting and sets `log.date` to a literal multiline strftime format containing the combined replay patch between commit headers. Without `--date=default`, that date makes the teaching classifier incorrectly return `SquashPatch`; with the option it remains `ReplayedPatches`. Percent signs in the synthetic patch are escaped as `%%` for strftime. These two regressions run only against the teaching classifier, while the pinned product source is compared under the baseline configuration. Fixture files are hashed again around each query.

These are the asserted classifier results:

| Fixture | Evidence | Reason |
| --- | --- | --- |
| `classic` | `Ancestor` | Ordinary merge retains the original tip in base ancestry |
| `squash` | `SquashPatch` | Two branch commits equal one combined base patch |
| `rebase` | `ReplayedPatches` | Two patches were cherry-picked onto a different parent |
| `conflict` | `Unknown` | The resolved squash differs from the branch patch |
| `empty` | `Unknown` | Replayed changes do not account for the empty branch commit |
| `reverted` | `SquashPatch` | Matching integration patch exists in history, then was reverted |
| `gone` | `Unknown` | A missing tracking ref supplies no patch evidence |
| shallow clone's `squash` | `ShallowHistory` | Patch detection stops on incomplete history |

The lab also invokes the current binary with `--list --no-cache` and `--json --no-cache`. Its JSON reports `classic` as merged, `squash` and `reverted` as squash, `rebase` as rebase, and `gone` as gone. The `gone` row has `selected_by_default: false`; a missing upstream is a separate status, not evidence of integration. The shallow scan warns that squash/rebase detection is disabled. Relative ages in the human listing depend on when the lab runs.

## Verification scope

The commands and assertions above were checked with Rust 1.98.1, Git 2.50.1 and git-barber built from the pinned source commit on macOS arm64. Both classifier and production JSON results agreed on the fixture cases. Full fixture-file hashes were unchanged by the read-only commands. These checks demonstrate the stated examples, not universal branch-cleanup safety or a performance guarantee.

For the underlying semantics, see Git's documentation for [merge-base](https://git-scm.com/docs/git-merge-base), [patch-id](https://git-scm.com/docs/git-patch-id) and [branch](https://git-scm.com/docs/git-branch). The Rust design keeps ancestry, patch evidence, incomplete history and command failures distinct so that a caller can apply a separate review policy.
