//! Cross-run cache of patch-id verdicts.
//!
//! Squash and rebase detection is the expensive part of a scan, and its answer
//! is a pure function of three shas: the base tip, the fork point, and the
//! branch tip. Nothing else can change it, so a run that sees all three
//! unchanged can reuse the previous verdict outright.
//!
//! The cache is strictly an optimisation. Every failure — missing file,
//! corrupt JSON, an unwritable `.git` — degrades to "compute it again", never
//! to an error: `--list` is documented to work on read-only repositories.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::scan::MergeKind;

/// Identity of one cached verdict. All three shas participate: a stale hit
/// would mean force-deleting a branch whose newer commits were never checked.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key(String);

impl Key {
    pub fn new(base_sha: &str, fork_sha: &str, branch_sha: &str) -> Self {
        Self(format!("{base_sha}:{fork_sha}:{branch_sha}"))
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    /// Bumped whenever the detection logic changes: old verdicts computed by
    /// different rules must not be trusted by a newer binary.
    version: u32,
    entries: HashMap<String, Option<MergeKind>>,
}

const VERSION: u32 = 1;

#[derive(Default)]
pub struct Cache {
    entries: HashMap<String, Option<MergeKind>>,
    /// Keys this run actually looked up or computed. Only these are written
    /// back, so the file stays the size of the current branch list instead of
    /// accumulating every tip the repository has ever had.
    live: HashSet<String>,
}

impl Cache {
    /// Read the cache for the repository at `git_dir`. Never fails.
    pub fn load(git_dir: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path(git_dir)) else {
            return Self::default();
        };
        match serde_json::from_str::<Stored>(&text) {
            Ok(stored) if stored.version == VERSION => Self {
                // A verdict the prober cannot produce did not come from a
                // scan. Since a cached Squash/Rebase authorises `branch -D`
                // with no re-verification, anything else is dropped rather
                // than obeyed.
                entries: stored
                    .entries
                    .into_iter()
                    .filter(|(_, v)| {
                        matches!(v, None | Some(MergeKind::Squash) | Some(MergeKind::Rebase))
                    })
                    .collect(),
                live: HashSet::new(),
            },
            // Corrupt, or written by a build with different detection rules.
            _ => Self::default(),
        }
    }

    /// The cached verdict, if this exact triple was resolved before. The outer
    /// Option is "was it cached", the inner one is the verdict itself —
    /// "definitely not merged" is worth remembering too.
    /// Takes `&mut self` because a lookup also marks the entry as still in
    /// use — that is what keeps the file from growing without bound.
    pub fn get(&mut self, key: &Key) -> Option<Option<MergeKind>> {
        let verdict = self.entries.get(&key.0).copied();
        if verdict.is_some() {
            self.live.insert(key.0.clone());
        }
        verdict
    }

    pub fn insert(&mut self, key: Key, verdict: Option<MergeKind>) {
        self.live.insert(key.0.clone());
        self.entries.insert(key.0, verdict);
    }

    /// Persist, best effort. A repository we cannot write to simply pays the
    /// full scan cost every time.
    pub fn save(&self, git_dir: &Path) {
        let file = path(git_dir);
        let Some(dir) = file.parent() else { return };
        // `create_dir_all` is satisfied by a SYMLINK to a directory and then
        // both the temp file and the rename land wherever it points — which
        // would undo the leaf-level protection below. This directory is ours
        // to create, so anything already in its place that is not a real
        // directory is refused rather than followed. `symlink_metadata` is
        // the load-bearing part: it does not follow links.
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return,
            Err(_) if std::fs::create_dir(dir).is_ok() => {}
            Err(_) => return,
        }
        let entries = self
            .entries
            .iter()
            .filter(|(k, _)| self.live.contains(*k))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let Ok(json) = serde_json::to_string(&Stored {
            version: VERSION,
            entries,
        }) else {
            return;
        };

        // Write to a fresh temp, then rename over the target. Two properties
        // come out of this, both of which a plain `fs::write` loses:
        //
        //  * `create_new` (O_EXCL) refuses an existing path, so a symlink
        //    planted here by a repository you did not author is never
        //    followed — and `rename` replaces the target's link itself
        //    rather than writing through it. Scanning stays safe to point
        //    at an untrusted checkout, which is true of everything else the
        //    scan does.
        //  * The replacement is atomic, so a second `git barber` reading
        //    concurrently sees the old file or the new one, never a
        //    truncated one.
        //
        // Every failure just leaves the previous cache in place.
        let tmp = dir.join(format!("cache.json.{}.tmp", std::process::id()));
        let Ok(mut handle) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        else {
            return;
        };
        if handle.write_all(json.as_bytes()).is_ok()
            && handle.flush().is_ok()
            && std::fs::rename(&tmp, &file).is_ok()
        {
            return;
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

fn path(git_dir: &Path) -> PathBuf {
    git_dir.join("barber").join("cache.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::MergeKind;
    use tempfile::TempDir;

    fn key(base: &str, fork: &str, branch: &str) -> Key {
        Key::new(base, fork, branch)
    }

    #[test]
    fn a_stored_verdict_is_found_again_by_the_next_run() {
        let tmp = TempDir::new().unwrap();
        let mut cache = Cache::load(tmp.path());
        cache.insert(key("b1", "f1", "x1"), Some(MergeKind::Squash));
        cache.insert(key("b1", "f1", "x2"), None);
        cache.save(tmp.path());

        let mut reloaded = Cache::load(tmp.path());
        assert_eq!(
            reloaded.get(&key("b1", "f1", "x1")),
            Some(Some(MergeKind::Squash))
        );
        // A verified "not merged" is worth caching too — it is just as
        // expensive to compute as a hit.
        assert_eq!(reloaded.get(&key("b1", "f1", "x2")), Some(None));
    }

    #[test]
    fn moving_any_of_the_three_shas_invalidates_the_entry() {
        // The verdict depends on the branch tip, the fork point, and what the
        // base gained since. A stale hit here would delete a branch whose new
        // commits were never checked.
        let tmp = TempDir::new().unwrap();
        let mut cache = Cache::load(tmp.path());
        cache.insert(key("b1", "f1", "x1"), Some(MergeKind::Squash));
        cache.save(tmp.path());

        let mut cache = Cache::load(tmp.path());
        assert_eq!(cache.get(&key("MOVED", "f1", "x1")), None);
        assert_eq!(cache.get(&key("b1", "MOVED", "x1")), None);
        assert_eq!(cache.get(&key("b1", "f1", "MOVED")), None);
    }

    #[test]
    fn only_entries_the_latest_run_used_are_kept() {
        // Otherwise the file grows without bound: every branch tip ever seen
        // would be remembered forever, long after the branch was deleted.
        let tmp = TempDir::new().unwrap();
        let mut cache = Cache::load(tmp.path());
        cache.insert(key("b1", "f1", "stale"), Some(MergeKind::Squash));
        cache.insert(key("b1", "f1", "live"), Some(MergeKind::Rebase));
        cache.save(tmp.path());

        // A later run looks up only one of the two.
        let mut cache = Cache::load(tmp.path());
        assert_eq!(
            cache.get(&key("b1", "f1", "live")),
            Some(Some(MergeKind::Rebase))
        );
        cache.save(tmp.path());

        let mut reloaded = Cache::load(tmp.path());
        assert_eq!(
            reloaded.get(&key("b1", "f1", "live")),
            Some(Some(MergeKind::Rebase))
        );
        assert_eq!(
            reloaded.get(&key("b1", "f1", "stale")),
            None,
            "an entry no run touched must not be carried forward"
        );
    }

    #[test]
    fn only_verdicts_the_prober_can_actually_produce_are_trusted() {
        // The cache authorises `git branch -D` with no re-verification, so a
        // file claiming "merged" or "gone" — kinds the patch-id prober never
        // returns — must be rejected rather than obeyed. Anyone able to write
        // here can already write .git/hooks, so this is defence in depth, not
        // a privilege boundary; it costs two lines.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("barber")).unwrap();
        std::fs::write(
            tmp.path().join("barber/cache.json"),
            r#"{"version":1,"entries":{"b:f:x":"merged","b:f:y":"gone","b:f:z":"squash"}}"#,
        )
        .unwrap();

        let mut cache = Cache::load(tmp.path());
        assert_eq!(
            cache.get(&key("b", "f", "x")),
            None,
            "\"merged\" must be rejected"
        );
        assert_eq!(
            cache.get(&key("b", "f", "y")),
            None,
            "\"gone\" must be rejected"
        );
        assert_eq!(
            cache.get(&key("b", "f", "z")),
            Some(Some(MergeKind::Squash)),
            "a producible verdict must survive"
        );
    }

    #[test]
    fn saving_leaves_no_partial_file_behind() {
        // The file is written via a temp and renamed so a concurrent reader
        // sees either the old file or the new one, never a truncated one.
        // Atomicity itself is not unit-testable without a race; this pins the
        // observable half — the temp must not be left behind.
        let tmp = TempDir::new().unwrap();
        let mut cache = Cache::load(tmp.path());
        cache.insert(key("b", "f", "x"), Some(MergeKind::Squash));
        cache.save(tmp.path());

        let files: Vec<String> = std::fs::read_dir(tmp.path().join("barber"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            files,
            vec!["cache.json".to_string()],
            "stray files: {files:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_never_becomes_a_file_clobber() {
        // git-barber used to be safe to point at a repository you did not
        // author: its scan runs no hooks. Writing a cache into .git brings
        // that back only if the write follows symlinks — a repo unpacked
        // from a tarball preserves them, so its author could aim either the
        // target or the temp path at a file outside the repo.
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path();
        std::fs::create_dir_all(git_dir.join("barber")).unwrap();

        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, "PRECIOUS DATA").unwrap();

        // Every path the write could plausibly touch, planted in advance.
        for name in [
            "barber/cache.json".to_string(),
            "barber/cache.json.tmp".to_string(),
            format!("barber/cache.json.{}.tmp", std::process::id()),
        ] {
            std::os::unix::fs::symlink(&victim, git_dir.join(name)).unwrap();
        }

        let mut cache = Cache::default();
        cache.insert(key("b", "f", "x"), Some(MergeKind::Squash));
        cache.save(git_dir);

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "PRECIOUS DATA",
            "the cache write escaped the repository"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_cache_directory_is_refused() {
        // Protecting the file names is not enough: if `.git/barber` itself is
        // a symlink, `create_dir_all` follows it and both the temp file and
        // the rename land in the linked directory instead. A repository you
        // did not author could point it anywhere the user can write.
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join("gitdir");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, git_dir.join("barber")).unwrap();

        let mut cache = Cache::default();
        cache.insert(key("b", "f", "x"), Some(MergeKind::Squash));
        cache.save(&git_dir);

        let leaked: Vec<String> = std::fs::read_dir(&outside)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leaked.is_empty(),
            "the cache write escaped through a symlinked directory: {leaked:?}"
        );
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_simply_empty() {
        // The cache is an optimisation; a damaged one must never be an error.
        let tmp = TempDir::new().unwrap();
        assert_eq!(Cache::load(tmp.path()).get(&key("b", "f", "x")), None);

        std::fs::create_dir_all(tmp.path().join("barber")).unwrap();
        std::fs::write(tmp.path().join("barber/cache.json"), "{not json").unwrap();
        assert_eq!(Cache::load(tmp.path()).get(&key("b", "f", "x")), None);
    }

    #[test]
    fn saving_into_a_read_only_repository_is_a_silent_no_op() {
        // `--list` is documented to work on read-only repositories; failing
        // to persist the cache must not change that.
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join("ro");
        std::fs::create_dir_all(&git_dir).unwrap();
        let mut perms = std::fs::metadata(&git_dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&git_dir, perms).unwrap();

        let mut cache = Cache::load(&git_dir);
        cache.insert(key("b", "f", "x"), Some(MergeKind::Rebase));
        cache.save(&git_dir); // must not panic and must not propagate
    }
}
