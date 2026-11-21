//! Rotating and recording signing keys.
//!
//! A signing key that cannot be rotated is a key that gets shared, kept in a home directory for
//! years, and copied between machines. The thing that makes rotation safe is that an old signature
//! does not need the old private key: a changeset records the public key it was signed with, so a
//! verifier checks it against that and never asks which key is current.
//!
//! What rotation has to do, then, is not re-sign anything. It has to:
//!
//! 1. Keep the old private key somewhere it can still be found if a signature is disputed, rather
//!    than overwriting it. Overwriting is the one outcome that makes history unverifiable.
//! 2. Record which public keys were ever active for this repository, so "was this key ever ours"
//!    has an answer that does not depend on somebody remembering.
//! 3. Make the new key the one `changeset create --sign` uses, which is the default path.
//!
//! The record is a file in the repository directory, not in the store. It is about this checkout's
//! keys, and a clone on another machine may sign with a different one.

use std::path::{Path, PathBuf};

use crate::{bad, ZResult};

/// One key this repository has used, as recorded at the time it was active.
#[derive(Debug, Clone)]
pub struct KeyRecord {
    pub public_hex: String,
    /// When it became the active key, unix seconds.
    pub since: i64,
    /// When it was retired, or `None` while it is active.
    pub retired_at: Option<i64>,
    /// Where the private half lives, so a disputed signature can be traced to a file.
    pub path: String,
}

fn record_path(root: &Path) -> PathBuf {
    crate::repo_dir(root).join("keys")
}

/// The keys this repository has used, oldest first.
pub fn read_records(root: &Path) -> ZResult<Vec<KeyRecord>> {
    let p = record_path(root);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&p)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // public : since : retired : path. An empty retired field means the key is active.
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 4 {
            continue;
        }
        let retired_at = if f[2].is_empty() {
            None
        } else {
            f[2].parse::<i64>().ok()
        };
        out.push(KeyRecord {
            public_hex: f[0].to_string(),
            since: f[1].parse::<i64>().unwrap_or(0),
            retired_at,
            path: f[3..].join(":"),
        });
    }
    Ok(out)
}

fn write_records(root: &Path, records: &[KeyRecord]) -> ZResult<()> {
    let mut text = String::from(
        "# Signing keys this repository has used. One line per key:\n\
         # public:since:retired:path. An empty retired field means the key is active.\n",
    );
    for r in records {
        let retired = r.retired_at.map(|t| t.to_string()).unwrap_or_default();
        text.push_str(&format!(
            "{}:{}:{}:{}\n",
            r.public_hex, r.since, retired, r.path
        ));
    }
    let p = record_path(root);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, text)?;
    Ok(())
}

/// Record the key at `path` as active, retiring whatever was active before.
///
/// Called by `key generate` and `key rotate`, so the first key a repository ever makes also gets a
/// record. Without that, a repository that never rotated has no record at all and the answer to
/// "which keys were ours" starts at the first rotation, which is the wrong half of the history.
pub fn record_active(root: &Path, public_hex: &str, path: &Path, now: i64) -> ZResult<()> {
    let mut records = read_records(root)?;
    for r in &mut records {
        if r.retired_at.is_none() {
            r.retired_at = Some(now);
        }
    }
    records.push(KeyRecord {
        public_hex: public_hex.to_string(),
        since: now,
        retired_at: None,
        path: path.display().to_string(),
    });
    write_records(root, &records)
}

/// The key that is active now, if the record names one whose file is still there.
pub fn active(root: &Path) -> ZResult<Option<KeyRecord>> {
    Ok(read_records(root)?
        .into_iter()
        .find(|r| r.retired_at.is_none()))
}

/// Was `public_hex` ever a key of this repository?
///
/// The answer is what makes an old signature auditable: a verifier that cannot find the key in this
/// list is looking at a changeset signed by something else, which is a different question from
/// whether the maths checks out.
pub fn was_ours(root: &Path, public_hex: &str) -> ZResult<bool> {
    Ok(read_records(root)?
        .iter()
        .any(|r| r.public_hex == public_hex.trim()))
}

/// Move `from` aside to a name that keeps the date, so nothing is lost and the new key can take the
/// place that every command expects.
///
/// The name carries the public key rather than a counter: two rotations on one day would collide on
/// a date-only name, and the public key is the one string that cannot repeat.
pub fn archive(from: &Path, public_hex: &str) -> ZResult<PathBuf> {
    if !from.exists() {
        return Err(bad(format!(
            "there is no key at {} to archive",
            from.display()
        )));
    }
    let short = &public_hex[..public_hex.len().min(16)];
    // The archive keeps the name the key actually had. `with_file_name("signing.key...")` renamed a
    // `--path` key called something else, which then looked like a stray file rather than an archive
    // of the key in use.
    let stem = from
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "signing.key".to_string());
    let to = from.with_file_name(format!("{}.{}.retired", stem, short));
    if to.exists() {
        return Err(bad(format!(
            "{} is already there: a second rotation of the same key would overwrite the archive",
            to.display()
        )));
    }
    std::fs::rename(from, &to)?;
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gesta-keys-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(d.join(crate::ZF_DIR)).unwrap();
        d
    }

    #[test]
    fn a_first_key_is_recorded_as_active() {
        let root = repo();
        record_active(&root, "aaaa", Path::new("/k1"), 100).unwrap();
        let recs = read_records(&root).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].retired_at, None);
        assert_eq!(active(&root).unwrap().unwrap().public_hex, "aaaa");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_key_retires_the_first_and_both_are_ours() {
        let root = repo();
        record_active(&root, "aaaa", Path::new("/k1"), 100).unwrap();
        record_active(&root, "bbbb", Path::new("/k2"), 200).unwrap();
        let recs = read_records(&root).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].retired_at, Some(200));
        assert_eq!(recs[1].retired_at, None);
        assert_eq!(active(&root).unwrap().unwrap().public_hex, "bbbb");
        // An old signature still has an answer to "was this key ever ours".
        assert!(was_ours(&root, "aaaa").unwrap());
        assert!(!was_ours(&root, "cccc").unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn archiving_keeps_the_old_file_under_a_name_that_names_the_key() {
        let root = repo();
        let key = root.join("signing.key");
        std::fs::write(&key, b"secret").unwrap();
        let moved = archive(&key, "abcdef0123456789ffff").unwrap();
        assert!(!key.exists());
        assert!(moved.exists());
        assert!(moved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("abcdef0123456789"));
        // The archive keeps the original filename, so it is recognisable beside the new key.
        assert!(moved
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("signing.key."));
        // Doing it twice for the same key must not overwrite the archive.
        std::fs::write(&key, b"secret2").unwrap();
        assert!(archive(&key, "abcdef0123456789ffff").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_repository_with_no_record_has_no_active_key() {
        let root = repo();
        assert!(read_records(&root).unwrap().is_empty());
        assert!(active(&root).unwrap().is_none());
        assert!(!was_ours(&root, "aaaa").unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}
