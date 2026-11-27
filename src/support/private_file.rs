//! Writing a file that holds a credential.
//!
//! Three places in this crate store something a local user must not read: the secret store, the TLS
//! private key, and `.gesta/auth`, which holds serve tokens in clear unless they were hashed. Each
//! of them answered the question separately, and one of them answered it not at all: `fs::write`
//! creates a file 0644 under the usual umask, so `.gesta/auth` was readable by every user on the
//! machine while the other two were 0600.
//!
//! The mode is set before the content is written, not after. Setting it afterwards leaves the file
//! readable for the width of the write, and on a machine where another user is already watching the
//! directory that is the whole window.
//!
//! On Windows there is no mode to set: the file inherits the directory ACL, and the directories
//! these live in are per-user. The function exists on both so callers do not branch.

use crate::ZResult;
use std::io::Write;
use std::path::Path;

/// A temp name no two calls in one process can pick at the same time (#292).
///
/// The name was `path.tmp-<pid>`, identical for every call in a process. Two threads replacing the
/// same credential raced, and the loser's `create_new` failed with `AlreadyExists` for an operation
/// that was perfectly doable. Worse, a call that died between creating the temp and the rename left
/// the file behind, and every later replace failed the same way until somebody deleted it by hand -
/// a credential store that cannot rewrite itself.
///
/// A counter closes it inside one process, which is where the collision was: the pid was already in
/// the name for the cross-process case, and `create_new` still refuses a second process. A thread id
/// would not be enough, because one thread can call twice and the second call may start before the
/// first renames.
fn temp_path_for(path: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut tmp = path.to_path_buf();
    tmp.set_extension(format!("tmp-{}-{}", std::process::id(), n));
    tmp
}

/// Write `contents` to `path`, readable only by the current user.
///
/// Creates the file with mode 0600 when it does not exist, and tightens an existing one to 0600 as
/// well (#293).
///
/// The old doc said an existing file keeps the mode it had, on the reasoning that tightening a file
/// the caller did not create is a different decision. The effect was a `.gesta/auth` or TLS key left
/// at 0644 by an older build staying readable by every user on the machine while being rewritten by
/// this program - the one file whose whole purpose is to be private, and `mode()` does nothing once
/// the file exists. Refusing instead would strand those stores; tightening is the outcome the caller
/// asked for by calling a function named `write_private`.
#[cfg(unix)]
pub fn write_private(path: &Path, contents: &[u8]) -> ZResult<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` above only applies at creation. Set it after opening, on the descriptor's file, so a
    // pre-existing 0644 file is fixed rather than left alone.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(contents)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub fn write_private(path: &Path, contents: &[u8]) -> ZResult<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(contents)?;
    f.sync_all()?;
    Ok(())
}

/// Write a credential that must not already exist, and refuse if it does - in one step.
///
/// `write_private` opens with `create(true) + truncate(true)`, so it overwrites by design (a token
/// rotation rewrites a file that is already there). `replace_private` overwrites too, atomically.
/// A signing key needs the opposite: the second writer must lose, and it must lose *because the file
/// is there*, not because a separate `exists()` check happened to see it first (#161).
///
/// `create_new` is what turns the check and the write into one operation. The kernel answers both
/// questions at the moment the file is created, so two callers racing cannot both win - and the mode
/// is applied at creation rather than after it, so the key is never readable by anyone else even for
/// the width of the write.
///
/// Returns the OS error untouched, so the caller can say "already exists" without guessing: a
/// `AlreadyExists` here is the normal way to lose the race, and a permission error is not.
#[cfg(unix)]
pub fn write_new_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents)?;
    f.sync_all()
}

#[cfg(not(unix))]
pub fn write_new_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // No mode to set: the file inherits the directory ACL, and the directories these live in are
    // per-user. `create_new` still does the "must not exist" half, which is the half the race was.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(contents)?;
    f.sync_all()
}

/// Replace `path` with `contents` in one step: a reader sees the old file or the new one, never a
/// half-written one.
///
/// `write_private` truncates in place, which is fine for a file that is read by nothing else at the
/// moment. The secret store is not: `gesta secret get` can read it while `secret put` is writing it,
/// and the write is not atomic - the reader gets a truncated file that does not decode, and the
/// whole credential store looks gone. Worse, a write that dies partway leaves it that way, so the
/// loss is permanent.
///
/// The temporary file is created in the same directory, so the rename is on one filesystem and is
/// atomic there; a temp file in the system directory would make the rename a copy and lose the
/// guarantee it exists for.
#[cfg(unix)]
pub fn replace_private(path: &Path, contents: &[u8]) -> ZResult<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = temp_path_for(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let wrote = f.write_all(contents).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = wrote {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn replace_private(path: &Path, contents: &[u8]) -> ZResult<()> {
    let tmp = temp_path_for(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let wrote = f.write_all(contents).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = wrote {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    // Older Windows refuses a rename over an existing file; retrying after removing loses the
    // atomicity but not the finished write, which is the better of the two outcomes.
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(path);
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn two_replaces_of_one_path_do_not_collide_on_the_temp_name() {
        // #292: the temp name was `path.tmp-<pid>`, identical for every call in a process. Two
        // concurrent replaces of one credential raced and the loser failed `AlreadyExists` for an
        // operation that was perfectly doable. A temp left behind by a call that died before renaming
        // made every later replace fail the same way - a credential store that could not rewrite
        // itself until somebody deleted the file by hand.
        //
        // The stale case needs no threads and is the one that hurts most, so it is what this asserts.
        let dir = std::env::temp_dir().join(format!("gesta-private-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth");

        // The exact name the pid-only scheme used for this path.
        let stale = dir.join(format!("auth.tmp-{}", std::process::id()));
        std::fs::write(&stale, b"leftover-from-a-dead-call").unwrap();

        replace_private(&path, b"first")
            .expect("a leftover temp from a dead call must not stop a later replace");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");

        // The operation is repeatable, which the pid-only name also forbade once the first call had
        // not yet renamed.
        replace_private(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn write_private_tightens_a_file_that_already_exists() {
        // #293: `mode(0o600)` only applies when the file is created, so a `.gesta/auth` written by an
        // older build at 0644 stayed 0644 while this program kept rewriting it - the one file whose
        // whole purpose is to be private.
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("gesta-private-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth");

        // An existing file at 0644, as an older build left it.
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "the fixture must start at 0644"
        );

        write_private(&path, b"new").unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "an existing file must be tightened to 0600, not left readable by others"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        // A file created fresh is 0600 too, which the old code already did.
        let fresh = dir.join("fresh");
        write_private(&fresh, b"x").unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_file_is_readable_only_by_its_owner() {
        let dir = std::env::temp_dir().join(format!("gesta-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth");

        write_private(&path, b"token\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"token\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "a file holding a credential must not be readable by other users, mode was {mode:o}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_mode_is_set_before_the_content_lands_not_after() {
        // The defect this guards: `fs::write` then `set_permissions` leaves the file 0644 for the
        // width of the write. Reading the mode of a file this function created and then written to
        // proves nothing on its own, so this checks the mode at the moment the file exists empty:
        // `OpenOptions::mode` applies at creation, before `write_all` runs.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let dir = std::env::temp_dir().join(format!("gesta-mode-order-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let probe = dir.join("probe");
            // Same call the helper makes, stopped before any content is written.
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&probe)
                .unwrap();
            let mode = std::fs::metadata(&probe).unwrap().permissions().mode() & 0o777;
            drop(f);
            assert_eq!(mode, 0o600, "the mode must be applied at creation");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_replaced_file_is_never_seen_half_written() {
        // #71: `save` truncated in place. A reader that opened the file during the write got a
        // truncated store, which does not decode, so a concurrent `secret get` saw the whole
        // credential store as lost. A write that died partway left it that way for good.
        let dir = std::env::temp_dir().join(format!("gesta-pf-repl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store");

        replace_private(&path, b"one").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        // No temporary file is left behind after a successful replace.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "store")
            .collect();
        assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");

        replace_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_replace_leaves_the_old_content() {
        // The point of writing beside the target and renaming: a failure before the rename cannot
        // damage what was there.
        let dir = std::env::temp_dir().join(format!("gesta-pf-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store");
        replace_private(&path, b"keep").unwrap();

        // A directory where the temp file wants to be makes the open fail.
        //
        // The name now carries a counter (#292), so a fixture cannot guess it. Blocking a RANGE of
        // upcoming names is the honest version: the counter is global and other tests advance it, so
        // the next value is not knowable from here. The blocker that matches is the one the call
        // tried; the rest are removed with the directory.
        let mut blocked = 0usize;
        for n in 0..256usize {
            let blocker = dir.join(format!("store.tmp-{}-{}", std::process::id(), n));
            if std::fs::create_dir_all(&blocker).is_ok() {
                blocked += 1;
            }
        }
        assert!(blocked > 0, "the fixture must create at least one blocker");
        let e = replace_private(&path, b"new").unwrap_err();
        let _ = e;
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"keep",
            "the old content must survive a failed replace"
        );
        // The failed call removed its own temp, so no directory was created where a file belongs.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
