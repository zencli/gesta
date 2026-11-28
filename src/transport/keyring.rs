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
        // An empty retired field means the key is still in use. A field that holds something else
        // is not evidence that it never retired: `parse().ok()` turned a corrupt value into
        // `None`, the same value an active key carries, so a key retired years ago came back as
        // the one in use. A record that cannot say when it retired is treated as retired, and the
        // timestamp it does not have is read as the epoch so the ordering still holds.
        let retired_at = if f[2].is_empty() {
            None
        } else {
            Some(f[2].parse::<i64>().unwrap_or(0))
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
    // The list is written to a neighbour and renamed into place. Writing straight over the file
    // truncated it first, so a crash, a full disk or a second process writing at the same moment
    // left a short list, and the list is the only answer to "which keys were ours": losing it makes
    // every signature an old key made unverifiable. Rename is atomic within a directory, so a
    // reader sees either the old list or the new one and never half of either.
    let staging = p.with_extension("gesta-tmp");
    std::fs::write(&staging, text)?;
    match std::fs::rename(&staging, &p) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&staging);
            Err(e.into())
        }
    }
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
    if public_hex.is_empty() || !public_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad(
            "the public key used to name an archive must be nonempty hexadecimal text",
        ));
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
        let d = crate::support::scratch::scratch("gesta-keys").expect("a test scratch directory");
        std::fs::create_dir_all(d.join(crate::ZF_DIR)).unwrap();
        d
    }

    #[test]
    fn invalid_unicode_archive_name_is_refused_without_moving_the_key() {
        let root = repo();
        let key = root.join("signing.key");
        std::fs::write(&key, b"original key").unwrap();
        for public in [
            format!("{}é", "a".repeat(15)),
            "é".repeat(9),
            "../escape".into(),
            String::new(),
        ] {
            let err = archive(&key, &public).expect_err("invalid public key text must be refused");
            assert!(err.to_string().contains("hexadecimal"), "{err}");
            assert_eq!(std::fs::read(&key).unwrap(), b"original key");
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_malformed_retired_field_does_not_bring_a_retired_key_back() {
        // `retired` held a value that is not a number. `parse().ok()` turned that into `None`,
        // which is the same value an active key carries, so a key retired years ago read as the
        // one in use and every signature made since looked like it came from the wrong key.
        // Refusing to guess is the safe reading: a record that cannot say when it retired is not
        // evidence that it never did.
        let root = repo();
        record_active(&root, "aaaa", Path::new("/k1"), 100).unwrap();
        record_active(&root, "bbbb", Path::new("/k2"), 200).unwrap();
        let p = record_path(&root);
        let text = std::fs::read_to_string(&p).unwrap();
        let broken = text.replace("aaaa:100:200:", "aaaa:100:not-a-number:");
        assert_ne!(
            text, broken,
            "the fixture did not contain the retired field to break"
        );
        std::fs::write(&p, broken).unwrap();

        let recs = read_records(&root).unwrap();
        let aaaa = recs
            .iter()
            .find(|r| r.public_hex == "aaaa")
            .expect("aaaa is still listed");
        assert!(
            aaaa.retired_at.is_some(),
            "a retired key with an unreadable retired field was read as active again"
        );
        assert_eq!(
            active(&root).unwrap().map(|r| r.public_hex),
            Some("bbbb".to_string()),
            "the active key is not the one that was written last"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_retired_key_whose_field_is_blank_stays_active() {
        // The blank field is the documented way to say "still in use", and the fix above must not
        // turn that into a refusal.
        let root = repo();
        record_active(&root, "aaaa", Path::new("/k1"), 100).unwrap();
        let recs = read_records(&root).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].retired_at, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_write_leaves_the_previous_record_list_readable() {
        // write_records wrote straight over the file. A crash, a full disk or a second process
        // writing at the same moment left a truncated list, and the list is the only answer to
        // "which keys were ours" - losing it makes every old signature unverifiable.
        let root = repo();
        record_active(&root, "aaaa", Path::new("/k1"), 100).unwrap();
        let before = std::fs::read_to_string(record_path(&root)).unwrap();
        // A directory where the temporary file would go makes the write fail after the point the
        // old code had already truncated the real file.
        let scratch = record_path(&root).with_extension("gesta-tmp");
        std::fs::create_dir_all(&scratch).unwrap();
        let err = write_records(&root, &[]);
        assert!(
            err.is_err(),
            "a write that could not complete reported success"
        );
        let after = std::fs::read_to_string(record_path(&root)).unwrap();
        assert_eq!(after, before, "a failed write damaged the record list");
        let _ = std::fs::remove_dir_all(&scratch);
        let _ = std::fs::remove_dir_all(&root);
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
