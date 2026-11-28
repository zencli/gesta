//! Large objects kept outside the database.
//!
//! SQLite stores a blob inline, and an inline blob is copied by every operation that touches the
//! page: a backup, a VACUUM, a read of one row. A 200 MB binary in a table makes every one of those
//! pay for it. git's answer is the same shape: the big thing lives beside the history and the history
//! records a pointer.
//!
//! What this is not: the git-lfs HTTP protocol. A client that speaks `lfs` expects a separate
//! endpoint, a batch API and pointer files in the tree. This is the storage half, chosen because it
//! is the half that fixes a real cost, and it stays invisible to everything above it: the hash is
//! still the object's name, and a reader that asks for one gets its bytes.
//!
//! Layout: `.gesta/objects/<first two hex>/<rest>` beside the database, which is the shape git uses
//! for loose objects and is what makes a look by eye possible.

use std::path::{Path, PathBuf};

use crate::{bad, ZResult};

/// Objects at or above this size leave the database.
///
/// Measured rather than guessed: a SQLite row of a few megabytes is still cheaper to read than a file
/// open plus a read, and the crossover on this machine was between 1 and 4 MB. Below it a row wins;
/// above it the row is a copy of a file that could just be the file.
pub const INLINE_LIMIT: usize = 4 * 1024 * 1024;

/// Where the sidecar directory for one repository lives.
pub fn dir(root: &Path) -> PathBuf {
    crate::repo_dir(root).join("objects")
}

/// Path for one object, two levels deep so a directory with a hundred thousand entries does not
/// become one directory with a hundred thousand entries.
pub fn path_for(root: &Path, hash: &str) -> ZResult<PathBuf> {
    if hash.len() < 4 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad(format!(
            "{} is not an object name: a sidecar path built from it could leave the store",
            hash
        )));
    }
    Ok(dir(root).join(&hash[..2]).join(&hash[2..]))
}

/// Should this object live outside the database?
pub fn is_large(len: usize) -> bool {
    len >= INLINE_LIMIT
}

/// Write one object's bytes beside the database, atomically.
///
/// Written to a temporary name and renamed into place. A reader that finds a file at the final path
/// therefore has the whole object: a half-written file with an object's name is a corrupt object that
/// looks present, which is the one outcome a store must not produce.
pub fn write(root: &Path, hash: &str, bytes: &[u8]) -> ZResult<PathBuf> {
    let to = path_for(root, hash)?;
    if to.exists() {
        return Ok(to);
    }
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = to.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &to)?;
    Ok(to)
}

/// Write an object from a file beside the database, copying in blocks.
///
/// The in-memory `write` above needs the whole body, which is exactly what a large object cannot
/// afford: a 256 MB asset cost 256 MB to store on top of the 256 MB the hash already needed. This
/// copies through a fixed buffer, so storing a large file costs the buffer and nothing per byte.
/// The rename still happens, so a reader never sees a partial object.
pub fn write_from_file(root: &Path, hash: &str, from: &Path) -> ZResult<PathBuf> {
    use std::io::{Read, Write};
    let to = path_for(root, hash)?;
    if to.exists() {
        return Ok(to);
    }
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = to.with_extension("tmp");
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
    }
    dst.sync_all()?;
    drop(dst);
    std::fs::rename(&tmp, &to)?;
    Ok(to)
}

/// Read one object's bytes, or `None` when no sidecar holds it.
pub fn read(root: &Path, hash: &str) -> Option<Vec<u8>> {
    let p = path_for(root, hash).ok()?;
    std::fs::read(p).ok()
}

/// Whether a sidecar holds this object.
pub fn holds(root: &Path, hash: &str) -> bool {
    path_for(root, hash).map(|p| p.exists()).unwrap_or(false)
}

/// Total bytes held outside the database, and the count.
pub fn stats(root: &Path) -> (usize, u64) {
    let base = dir(root);
    let mut count = 0usize;
    let mut bytes = 0u64;
    let Ok(level1) = std::fs::read_dir(&base) else {
        return (0, 0);
    };
    for d1 in level1.flatten() {
        let Ok(level2) = std::fs::read_dir(d1.path()) else {
            continue;
        };
        for f in level2.flatten() {
            if let Ok(m) = f.metadata() {
                if m.is_file() {
                    count += 1;
                    bytes += m.len();
                }
            }
        }
    }
    (count, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let d =
            crate::support::scratch::scratch("gesta-sidecar").expect("a test scratch directory");
        std::fs::create_dir_all(d.join(crate::ZF_DIR)).unwrap();
        d
    }

    #[test]
    fn a_written_object_round_trips() {
        let root = repo();
        let body = vec![7u8; 64];
        let p = write(&root, "abcdef0123456789", &body).unwrap();
        assert!(p.exists());
        assert_eq!(read(&root, "abcdef0123456789").unwrap(), body);
        assert!(holds(&root, "abcdef0123456789"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writing_twice_keeps_one_file() {
        let root = repo();
        write(&root, "aabbccdd", b"one").unwrap();
        write(&root, "aabbccdd", b"one").unwrap();
        let (count, _) = stats(&root);
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_name_that_could_leave_the_store_is_refused() {
        let root = repo();
        for bad_hash in ["../../etc/passwd", "abc", "abcdefg", ""] {
            assert!(
                path_for(&root, bad_hash).is_err(),
                "{} should not name a path",
                bad_hash
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_object_reads_as_none_rather_than_failing() {
        let root = repo();
        assert!(read(&root, "deadbeef").is_none());
        assert!(!holds(&root, "deadbeef"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_threshold_is_a_size_and_not_a_guess() {
        assert!(!is_large(INLINE_LIMIT - 1));
        assert!(is_large(INLINE_LIMIT));
    }
}
