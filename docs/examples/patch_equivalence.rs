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
