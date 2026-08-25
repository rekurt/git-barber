//! Scan progress reporting.
//!
//! A scan of a large repository takes tens of seconds during which the tool
//! is otherwise silent, which reads as a hang. The counter goes to stderr and
//! only when stderr is a terminal, so `--json` piped into a script stays
//! byte-for-byte clean.

use std::io::Write;
use std::sync::Mutex;

/// Progress sink for the scan. Implemented by a real terminal reporter and a
/// no-op one, so callers never branch on "is there a terminal".
pub trait Reporter: Sync {
    fn tick(&self, done: usize, total: usize);
    /// Wipe the counter line so the report that follows starts clean.
    fn finish(&self);
}

/// Used whenever stderr is not a terminal.
pub struct NullReporter;

impl Reporter for NullReporter {
    fn tick(&self, _done: usize, _total: usize) {}
    fn finish(&self) {}
}

/// Redraws a single counter line in place. The mutex makes it `Sync`: scan
/// worker threads report concurrently.
struct Inner<W: Write + Send> {
    out: W,
    /// Highest count printed so far. Workers claim their number and then
    /// queue for the lock, so they can arrive out of order; without this the
    /// displayed count visibly jitters backwards mid-scan. It lives INSIDE
    /// the mutex on purpose: compared outside, two workers could both pass
    /// the check and still write in the wrong order.
    high_water: usize,
}

pub struct WriteReporter<W: Write + Send> {
    inner: Mutex<Inner<W>>,
}

impl<W: Write + Send> WriteReporter<W> {
    pub fn new(out: W) -> Self {
        Self {
            inner: Mutex::new(Inner { out, high_water: 0 }),
        }
    }

    #[cfg(test)]
    fn into_inner(self) -> W {
        self.inner
            .into_inner()
            .expect("reporter mutex was poisoned")
            .out
    }
}

impl<W: Write + Send> Reporter for WriteReporter<W> {
    fn tick(&self, done: usize, total: usize) {
        if let Ok(mut inner) = self.inner.lock() {
            if done <= inner.high_water {
                return;
            }
            inner.high_water = done;
            // Write errors are swallowed deliberately: a broken progress
            // line must never abort a scan that is otherwise fine.
            let _ = write!(inner.out, "\r\x1b[Kscanning {done}/{total} branches");
            let _ = inner.out.flush();
        }
    }

    fn finish(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let _ = write!(inner.out, "\r\x1b[K");
            let _ = inner.out.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_redraws_one_line_and_clears_it_when_done() {
        // A scan can take tens of seconds; the counter must overwrite itself
        // in place rather than scrolling the terminal, and must leave no
        // residue behind for the report that follows.
        let reporter = WriteReporter::new(Vec::new());
        reporter.tick(3, 10);
        reporter.tick(4, 10);
        reporter.finish();
        let out = String::from_utf8(reporter.into_inner()).unwrap();
        assert!(out.contains("3/10"), "missing first tick: {out:?}");
        assert!(out.contains("4/10"), "missing second tick: {out:?}");
        assert_eq!(out.matches('\n').count(), 0, "must not scroll: {out:?}");
        assert!(out.ends_with("\r\x1b[K"), "line not cleared: {out:?}");
    }

    #[test]
    fn the_counter_never_goes_backwards() {
        // Workers increment a shared counter and then queue for the output
        // lock, so a later branch can reach the writer first. Printing that
        // verbatim makes the count visibly jitter downwards mid-scan.
        let reporter = WriteReporter::new(Vec::new());
        reporter.tick(5, 10);
        reporter.tick(3, 10);
        reporter.tick(6, 10);
        reporter.finish();
        let out = String::from_utf8(reporter.into_inner()).unwrap();
        assert!(out.contains("5/10"), "{out:?}");
        assert!(
            !out.contains("3/10"),
            "a lower count must be dropped: {out:?}"
        );
        assert!(out.contains("6/10"), "{out:?}");
    }

    #[test]
    fn ticks_arriving_from_many_threads_still_read_as_one_rising_counter() {
        // Workers claim their number and then queue for the writer, so they
        // reach it out of order. The transcript still has to be a single
        // counter climbing to the total — not interleaved garbage, and not a
        // count that visibly falls back.
        const TOTAL: usize = 64;
        let reporter = WriteReporter::new(Vec::new());
        std::thread::scope(|scope| {
            for i in 1..=TOTAL {
                let reporter = &reporter;
                scope.spawn(move || reporter.tick(i, TOTAL));
            }
        });
        reporter.finish();

        let out = String::from_utf8(reporter.into_inner()).unwrap();
        let counts: Vec<usize> = out
            .split("scanning ")
            .skip(1)
            .filter_map(|s| s.split('/').next()?.parse().ok())
            .collect();
        assert!(!counts.is_empty(), "nothing was reported: {out:?}");
        assert!(
            counts.windows(2).all(|w| w[0] < w[1]),
            "counter went backwards: {counts:?}"
        );
        assert_eq!(counts.last(), Some(&TOTAL), "never reached the total");
    }
}
