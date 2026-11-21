//! Encrypted off-site backups.
//!
//! `backup::backup` takes a consistent SQLite snapshot onto the local disk. That is a copy, not a
//! recovery plan: it sits on the same machine as the repository, so one disk failure loses both. This
//! seals that snapshot and puts it somewhere else.
//!
//! Three decisions:
//!
//! 1. **Encrypted before it leaves the process.** A backup carries every object, every branch and
//!    every blame range the repository has. Uploading that in the clear to an object store means the
//!    store's access control is the only thing between a stranger and the whole history. The seal is
//!    ChaCha20-Poly1305 with a key derived by HMAC-SHA256 from the passphrase, the same primitives the
//!    secret store already uses, so there is one implementation of "seal bytes" in the tree.
//!
//! 2. **The key never travels with the file.** The sealed format carries salt and nonce, never the
//!    passphrase. A backup and its passphrase stored in the same place is not encryption.
//!
//! 3. **Pruned by count, not by age.** A backup schedule that a person forgets to run leaves an old
//!    file; a backup schedule that runs every hour and never prunes fills a bucket. Keeping the last N
//!    is the rule that survives both.

use crate::transport::s3::S3Config;
use crate::{bad, ZResult};

const MAGIC: &str = "gesta-backup-v1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;

/// What a push did.
pub struct Pushed {
    pub key: String,
    pub plain_bytes: u64,
    pub sealed_bytes: u64,
    pub changesets: i64,
    pub objects: i64,
}

/// Seal arbitrary bytes with a passphrase.
///
/// The layout is `magic salt nonce ciphertext`, all hex after the magic, which is what makes the file
/// a text file: it can be pasted, diffed and stored in a place that only takes strings. A binary
/// format would be smaller and would be a second thing to parse on restore.
pub fn seal(plain: &[u8], passphrase: &str) -> ZResult<Vec<u8>> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
    use ring::rand::{SecureRandom, SystemRandom};

    if passphrase.is_empty() {
        return Err(bad(
            "a backup cannot be sealed without a passphrase: an unencrypted copy of the whole \
             history in an object store is not a backup, it is a disclosure"
                .to_string(),
        ));
    }

    let rng = SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut salt)
        .map_err(|_| bad("the system random source failed".to_string()))?;
    rng.fill(&mut nonce_bytes)
        .map_err(|_| bad("the system random source failed".to_string()))?;

    let key_bytes = crate::support::secrets::derive_key(passphrase, &salt);
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| bad("cannot build the cipher key".to_string()))?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut buf = plain.to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
        .map_err(|_| bad("sealing the backup failed".to_string()))?;

    let mut out = Vec::new();
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(hex::encode(salt).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(hex::encode(nonce_bytes).as_bytes());
    out.push(b'\n');
    out.extend_from_slice(hex::encode(&buf).as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// Open a sealed backup.
///
/// The tag is checked before the plaintext is returned, so a file that was truncated by a failed
/// upload fails here rather than during a restore into a live repository.
pub fn unseal(sealed: &[u8], passphrase: &str) -> ZResult<Vec<u8>> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};

    let text = std::str::from_utf8(sealed).map_err(|_| {
        bad("the sealed backup is not text, so it is not a gesta backup".to_string())
    })?;
    let mut lines = text.lines();
    let magic = lines.next().unwrap_or("");
    if magic != MAGIC {
        return Err(bad(format!(
            "the file does not start with {MAGIC}, so it is not a sealed gesta backup"
        )));
    }
    let salt = hex::decode(lines.next().unwrap_or(""))
        .map_err(|_| bad("the salt is not valid hex".to_string()))?;
    let nonce_bytes = hex::decode(lines.next().unwrap_or(""))
        .map_err(|_| bad("the nonce is not valid hex".to_string()))?;
    let mut ct = hex::decode(lines.next().unwrap_or(""))
        .map_err(|_| bad("the ciphertext is not valid hex".to_string()))?;

    if nonce_bytes.len() != NONCE_LEN {
        return Err(bad("the nonce has the wrong length".to_string()));
    }
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(&nonce_bytes);

    let key_bytes = crate::support::secrets::derive_key(passphrase, &salt);
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| bad("cannot build the cipher key".to_string()))?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_arr);

    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut ct)
        .map_err(|_| {
            bad(
                "cannot open the backup: wrong passphrase, or the file was truncated or edited"
                    .to_string(),
            )
        })?;
    Ok(plain.to_vec())
}

/// Take a snapshot, seal it, and upload it under `prefix/gesta-<timestamp>.bak`.
///
/// The timestamp is UTC and zero padded, so listing the prefix sorts chronologically without reading
/// the files. That is what makes pruning by count possible with one listing.
pub fn push(
    conn: &rusqlite::Connection,
    s3: &S3Config,
    prefix: &str,
    passphrase: &str,
    keep: usize,
) -> ZResult<Pushed> {
    let tmp = std::env::temp_dir().join(format!(
        "gesta-backup-{}-{}.db",
        std::process::id(),
        crate::support::clock::unix_now()
    ));

    let report = match crate::support::backup::backup(conn, &tmp) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };

    let plain = match std::fs::read(&tmp) {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    };
    let _ = std::fs::remove_file(&tmp);

    let sealed = seal(&plain, passphrase)?;
    let stamp = utc_stamp(std::time::SystemTime::now());
    let key = format!("{}/gesta-{}.bak", prefix.trim_end_matches('/'), stamp);

    s3.put(&key, &sealed)?;

    // Pruned after the upload, not before: a failed upload must not remove the last good backup. A
    // prune that fails is not a reason to report the backup as failed, because the backup succeeded
    // and the next run prunes again.
    let _ = prune(s3, prefix, keep);

    Ok(Pushed {
        key,
        plain_bytes: plain.len() as u64,
        sealed_bytes: sealed.len() as u64,
        changesets: report.changesets,
        objects: report.objects,
    })
}

/// List the prefix and delete everything past the newest `keep`.
///
/// Returns how many were removed. Not an error to have fewer than `keep`: a fresh schedule has one.
pub fn prune(s3: &S3Config, prefix: &str, keep: usize) -> ZResult<usize> {
    let mut keys = s3.list_keys(prefix)?;
    keys.sort();
    if keys.len() <= keep {
        return Ok(0);
    }
    let doomed = &keys[..keys.len() - keep];
    let mut removed = 0;
    for k in doomed {
        s3.delete(k)?;
        removed += 1;
    }
    Ok(removed)
}

/// `YYYYMMDDTHHMMSSmmmZ`, which sorts as text in the same order as time.
///
/// Milliseconds, because a second is not fine enough to name a backup uniquely: an agent loop can
/// push twice inside one, and two uploads to one key leave the bucket holding whichever finished
/// last. A counter would be simpler and would not sort.
fn utc_stamp(now: std::time::SystemTime) -> String {
    let unix = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // Days since the epoch to a civil date, the standard algorithm. A dependency on chrono for this
    // one conversion would be a second date implementation in a tree that already has one in
    // `support::clock`.
    let millis = unix.rem_euclid(1000);
    let secs_total = unix.div_euclid(1000);
    let days = secs_total.div_euclid(86_400);
    let secs = secs_total.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}{:03}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        millis
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
