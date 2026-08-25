//! Ordered parallel map over a slice.
//!
//! The scan spends nearly all of its time in independent `git` subprocesses,
//! one batch per branch, so it parallelises almost perfectly. This is the
//! whole threading story of the tool — deliberately a dozen lines of `std`
//! rather than a work-stealing runtime.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Apply `f` to every item across `jobs` threads, returning results in INPUT
/// order. Order matters: it is what candidate listings are sorted from, and
/// it must not depend on which worker finished first.
pub fn map<T, R, F>(items: &[T], jobs: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let jobs = jobs.max(1).min(items.len());
    if jobs <= 1 {
        return items.iter().map(&f).collect();
    }

    let cursor = AtomicUsize::new(0);
    let mut collected: Vec<(usize, R)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..jobs)
            .map(|_| {
                let cursor = &cursor;
                let f = &f;
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        match items.get(i) {
                            Some(item) => mine.push((i, f(item))),
                            None => return mine,
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            // A worker panic is a bug in `f`; propagating it here keeps the
            // failure loud instead of silently losing that branch's result.
            .flat_map(|h| h.join().expect("scan worker panicked"))
            .collect()
    });
    collected.sort_unstable_by_key(|(i, _)| *i);
    collected.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[test]
    fn results_keep_input_order_regardless_of_completion_order() {
        // Candidate ordering is user-visible and must not depend on which
        // worker happened to finish first.
        let items: Vec<usize> = (0..200).collect();
        let out = map(&items, 8, |&i| {
            // Invert the natural finishing order: late items finish first.
            std::thread::sleep(std::time::Duration::from_micros((200 - i) as u64));
            i * 2
        });
        assert_eq!(out, items.iter().map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn work_is_spread_over_several_threads() {
        let seen: Mutex<HashSet<std::thread::ThreadId>> = Mutex::new(HashSet::new());
        let items: Vec<usize> = (0..64).collect();
        map(&items, 4, |_| {
            seen.lock().unwrap().insert(std::thread::current().id());
            std::thread::sleep(std::time::Duration::from_millis(1));
        });
        assert!(
            seen.lock().unwrap().len() > 1,
            "expected more than one worker thread"
        );
    }

    #[test]
    fn empty_input_and_zero_jobs_are_handled() {
        let empty: Vec<usize> = Vec::new();
        assert!(map(&empty, 4, |&i: &usize| i).is_empty());
        // jobs=0 must still do the work rather than silently dropping it.
        assert_eq!(map(&[1usize, 2, 3], 0, |&i| i), vec![1, 2, 3]);
    }
}
