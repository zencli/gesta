//! A temporary directory whose name cannot repeat within one process.
//!
//! Temporary directories used to be named from the clock:
//!
//! ```text
//! std::env::temp_dir().join(format!("gesta-audit-{}-{}", pid, nanos))
//! ```
//!
//! A nanosecond timestamp looks unique and is not. Measured on a Windows host, two back to back
//! `SystemTime::now()` calls in the same process returned the same nanosecond in 9836 of 19999
//! pairs, 49.2% of the time: the system clock granularity is coarser than the unit it reports.
//! Two callers that drew the same name then shared one directory, and the `remove_dir_all` at the
//! end of whichever finished first deleted the other one's files while it was still writing them.
//!
//! The symptom is here to recognise, because it looks nothing like the cause: a `std::fs::write`
//! or `File::create` fails with `NotFound` immediately after a `create_dir_all` that succeeded.
//! That is what failed the Windows CI leg in `a_limited_read_does_not_load_the_whole_file`.
//!
//! A counter that is incremented per process makes the name unique, and the process id keeps it
//! unique across runs. Nothing here depends on the clock.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ZResult;

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under the system temp directory, named `<prefix>-<pid>-<seq>-<random>`.
///
/// The directory is created, so it is ready to be written into. The caller owns it and is
/// responsible for removing it.
///
/// The random suffix is what makes the name unique across RUNS, not just within one (#305). The
/// counter resets at every start, so `<prefix>-<pid>-<seq>` repeated exactly whenever the system
/// handed out the same pid twice - after a crash the directory is still there, `create_dir_all`
/// succeeds on it because it exists, and the caller inherits whatever the dead process left behind:
/// a half-written pack, a stale lock file, an audit log with somebody else's entries. A pid is
/// reused constantly on a container or a laptop that restarts things.
///
/// It returns an error rather than panicking. `expect` here took the whole process down when the
/// temp directory was full, read-only, or missing, at call sites where the caller had a perfectly
/// good error path available - a push that could fail with a message instead of aborting mid-write.
pub fn scratch(prefix: &str) -> ZResult<PathBuf> {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{seq}-{:08x}",
        std::process::id(),
        nonce()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| {
        crate::bad(format!(
            "cannot create scratch directory {}: {}",
            dir.display(),
            e
        ))
    })?;
    Ok(dir)
}

/// A number that differs between runs of the same process id.
///
/// `RandomState` is seeded from the OS on first use and, per std's contract, gives different values
/// in different processes and different runs; it is the entropy std exposes without a dependency.
/// A counter alone cannot cover pid reuse, and the clock was measured to repeat within microseconds,
/// so this is the one source left that does not need a crate added to the build.
pub(crate) fn nonce() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(NEXT.load(Ordering::Relaxed));
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_calls_never_return_the_same_path() {
        // This is the property the clock-based name did not have. A thousand calls is far more
        // than any single caller asks for, and a duplicate anywhere in that run would have two
        // callers share a directory and delete each other's files.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(scratch("gesta-scratch-probe").unwrap()),
                "a repeated path means two callers would share one directory"
            );
        }
        for path in &seen {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn the_directory_exists_when_it_is_handed_back() {
        let dir = scratch("gesta-scratch-exists").unwrap();
        assert!(dir.is_dir(), "the caller should not have to create it");
        std::fs::write(dir.join("f.txt"), "ok\n").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "ok\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_counts_as_a_fresh_directory_even_when_the_pid_repeats() {
        // #305: the name was `<prefix>-<pid>-<seq>` and `seq` resets at every start, so a pid reused
        // after a crash landed on the directory the dead process left behind. `create_dir_all`
        // succeeds on a directory that exists, so the caller inherited its contents - a half-written
        // pack, a stale lock, an audit log with somebody else's entries.
        //
        // A reused pid cannot be arranged in a test, so the property is stated on the name itself:
        // the path `scratch()` returns must NOT be the path the old format would have produced for
        // the same prefix, pid and counter. That is exactly what the nonce contributes, and it fails
        // if the nonce is dropped from the name - which is the check a direct test of `nonce()` was
        // too weak to make.
        let dir = scratch("gesta-scratch-pid").unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let pid = std::process::id().to_string();

        // The old name was `<prefix>-<pid>-<seq>`: three dash-separated fields after the prefix.
        let after_prefix = name
            .strip_prefix("gesta-scratch-pid-")
            .expect("the prefix is part of the name");
        let fields: Vec<&str> = after_prefix.split('-').collect();
        assert!(
            fields.len() >= 3,
            "the name needs a field beyond prefix, pid and counter, or a reset counter on a reused \
             pid collides; got {name:?}"
        );
        assert_eq!(fields[0], pid, "the pid stays in the name: {name:?}");
        assert!(
            fields.last().map(|f| f.len()).unwrap_or(0) >= 8,
            "the last field is the nonce and must be wide enough to differ; got {name:?}"
        );

        // And it is not the clock: two calls in the same tick differ, which the module header measures
        // as failing 49.2% of the time for a nanosecond stamp.
        let first = scratch("gesta-scratch-same-tick").unwrap();
        let second = scratch("gesta-scratch-same-tick").unwrap();
        assert_ne!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }
}
