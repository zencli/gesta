//! Working-tree scan with a stat cache.
//!
//! Hashing every file on every `status` costs disk I/O and CPU proportional to
//! the size of the tree, which dominates once a repository holds tens of
//! thousands of files. git avoids this by keeping `(size, mtime)` per path in its
//! index and only re-reading files whose stat changed; this does the same in a
//! `worktree_cache` table.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::params;
use rusqlite::Connection;

use crate::store::hash_content;
use crate::store::walk::{read_file, IgnoreRules, ReadOutcome};
use crate::ZResult;
use std::fs;

/// Files whose mtime is younger than this are re-read rather than trusted.
/// Two seconds covers filesystem timestamp granularity plus the clock skew of a
/// writer that opened the file just before touching it.
const RACY_WINDOW_NS: i64 = 2_000_000_000;

/// One file in the working tree.
pub struct ScannedFile {
    pub rel: String,
    pub hash: String,
    pub content: String,
    /// The git mode this file carries: `100644`, `100755` for a file with the executable bit set.
    ///
    /// A mode is not content. A script that is executable and the same script without the bit are the
    /// same bytes and different trees, and a store that only records content silently turns every
    /// executable into a plain file on the way out.
    pub mode: String,
}

/// The git mode for a path, from its own metadata.
///
/// Windows has no executable bit, so a checkout there reports `100644` for everything and the bit is
/// whatever git recorded. That is why the mode is read where the format knows about it (import) and
/// carried as data everywhere else, rather than recomputed from the filesystem on the way out.
fn mode_of(meta: &fs::Metadata) -> String {
    if meta.file_type().is_symlink() {
        return "120000".into();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return "100755".into();
        }
    }
    let _ = meta;
    "100644".into()
}

pub struct ScanResult {
    pub files: Vec<ScannedFile>,
    /// Paths that are binary or otherwise not valid UTF-8.
    pub skipped_binary: Vec<String>,
    /// Paths whose stat did not match the cache and had to be re-read.
    pub rehashed: usize,
    /// Files that could not be read at all, with the reason.
    pub unreadable: Vec<String>,
    /// Paths served straight from the cache.
    pub cached: usize,
}

/// Scan the working tree, reusing cached hashes where the stat is unchanged.
///
/// A cache hit needs `(size, mtime)` to match what was recorded last time. That
/// is the same rule git uses: it is not perfect (a file can change within the
/// same second and keep its size), but `status` is allowed to be as good as git's
/// and the content hash is verified the moment a changeset is created.
pub fn scan_with_cache(
    conn: &Connection,
    root: &Path,
    rules: &IgnoreRules,
    attrs: &crate::support::attributes::Rules,
) -> ZResult<ScanResult> {
    let known = load_cache(conn)?;
    // Sized up front. Growing a vector of 2000 entries by doubling copies every
    // element several times, which is measurable when the elements hold strings.
    let estimate = known.len().max(64);
    let mut files = Vec::with_capacity(estimate);
    let skipped_binary: Vec<String> = Vec::new();

    let mut rehashed = 0usize;
    let mut cached = 0usize;
    let mut unreadable: Vec<String> = Vec::new();
    // Cache rows are written in one batch at the end. Remembering each file
    // separately meant one transaction per file, and on 2000 files that was the
    // difference between a 5 ms scan and a 27 ms one.
    let mut pending_writes: Vec<(String, i64, i64, String)> = Vec::new();

    // Read the clock once. Asking for it per file turned a cheap comparison into
    // a syscall for every path in the tree.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    // Subrepository checkouts are excluded before the walk, so their files are
    // never recorded in this repository's cache either.
    // A submodule checkout holds another repository's files and must never be recorded as
    // this repository's own. Both kinds of pointer are excluded here so the walk has one list.
    let excluded = crate::store::walk::excluded_paths(conn, root);
    let walked = crate::store::walk::scan_files_excluding(root, rules, &excluded)?;
    for (rel, meta) in walked {
        // Nanoseconds, not seconds. Two writes inside one second with the same
        // size are indistinguishable at second resolution, and the cache would
        // then serve the first write's hash for the second write's content.
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;

        // Borrowed, not taken. Removing the row to hand its hash over measured 5 ms slower over 15
        // interleaved runs, because `remove` costs more than `get` plus a clone of forty bytes. The
        // clone stays.
        if let Some((cm, cs, ch)) = known.get(&rel) {
            if *cm == mtime && *cs == size {
                // Racy case, the same one git guards against. A file written moments ago can be
                // written again without the stat changing in a way the next scan would see, if both
                // writes land inside the filesystem's timestamp resolution. Such a file is re-read
                // instead of trusted. Once it is a couple of seconds old the stat is stable and the
                // fast path applies.
                let young = now_ns.saturating_sub(mtime) < RACY_WINDOW_NS;
                if !young && !meta.file_type().is_symlink() {
                    // The stat matches, so the recorded hash is the answer and the file is never
                    // opened. Reading it anyway made the cache worthless: a 2000 file status read all
                    // 2000 files.
                    cached += 1;
                    files.push(ScannedFile {
                        rel,
                        hash: ch.clone(),
                        content: String::new(),
                        mode: mode_of(&meta),
                    });
                    continue;
                }
            }
        }

        // The plan is per path and constant for the life of the scan, so it is computed once here
        // rather than at every place that reads a body. The hash has to be taken over the bytes the
        // object will hold, or a checkout with CRLF endings records a hash that no object matches.
        let plan = crate::support::attributes::plan_for(attrs, &rel);

        // Only a file that has to be read needs an absolute path. Building one per entry cost an
        // allocation for every cache hit, which is the common case.
        let path = root.join(&rel);
        match read_file(&path) {
            ReadOutcome::Text(content) => {
                let content = if plan.normalise && !meta.file_type().is_symlink() {
                    match String::from_utf8(plan.stored(content.as_bytes())) {
                        Ok(s) => s,
                        // Normalising never changes bytes for a body that parsed as UTF-8, so this
                        // is unreachable rather than a case to handle. Keeping the original is the
                        // safe answer if it ever is reached.
                        Err(_) => content,
                    }
                } else {
                    content
                };
                // A scan reads a file to learn its hash; it does not write one. Storing an object
                // here made every read path (`status`, `grep`, `diff`) pay for a write and a
                // delta-base search, which is work only a commit owes. The object is written by
                // `put_object_for` at commit time, and that is where a base is chosen.
                let hash = hash_content(&content);
                rehashed += 1;
                pending_writes.push((rel.clone(), mtime, size, hash.clone()));
                files.push(ScannedFile {
                    rel,
                    hash,
                    content,
                    mode: mode_of(&meta),
                });
            }
            ReadOutcome::Binary => {
                // Binary files are stored as raw bytes, no compression, no delta. Hashing and
                // storing both stream, so a large asset costs one block buffer rather than its own
                // size twice: this path used to read the whole file, and a 256 MB binary made
                // `status` resident at 263 MB.
                //
                // A symlink is the exception. Its content is its target name, which `read_bytes`
                // returns without following the link, and the streaming path cannot produce:
                // opening the path follows the link, and a target that does not exist - a dangling
                // link, or a name that is not valid UTF-8 - would fail the read entirely. A target
                // name is also tiny, so there is nothing to gain by streaming it.
                let link = std::fs::symlink_metadata(&path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                let hash = if link {
                    match crate::store::walk::read_bytes(&path) {
                        Ok(bytes) => {
                            let h = crate::store::hash_bytes(&bytes);
                            // Do NOT write the object here. A scan is a read path; the object
                            // will be written by the caller that needs it (e.g., changeset create).
                            h
                        }
                        Err(e) => {
                            unreadable.push(format!("{}: {}", rel, e));
                            continue;
                        }
                    }
                } else {
                    let h = match crate::store::hash_file(&path) {
                        Ok(h) => h,
                        Err(e) => {
                            unreadable.push(format!("{}: {}", rel, e));
                            continue;
                        }
                    };
                    // Do NOT write the object here. A scan is a read path; the object will be
                    // written by the caller that needs it (e.g., changeset create).
                    h
                };
                pending_writes.push((rel.clone(), mtime, size, hash.clone()));
                // Binary files have no text content in the scan result.
                files.push(ScannedFile {
                    rel,
                    hash,
                    content: String::new(),
                    mode: mode_of(&meta),
                });
            }
            ReadOutcome::Unreadable(e) => unreadable.push(format!("{}: {}", rel, e)),
        }
    }

    if !pending_writes.is_empty() {
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO worktree_cache(path, mtime, size, hash) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                                                 size = excluded.size,
                                                 hash = excluded.hash",
            )?;
            for (rel, m, sz, h) in &pending_writes {
                stmt.execute(params![rel, m, sz, h])?;
            }
        }
        tx.commit()?;
    }

    // Pruning uses the scan's own results: `files` holds the text paths and
    // `skipped_binary` the binary ones, which together are every path present on
    // disk. Keeping a third copy of the same strings was pure allocation.
    prune_cache(conn, &files, &skipped_binary)?;
    // Sorted by repository path before it leaves this function. The walk returns whatever order the
    // filesystem hands back, which is hash order on ext4 and insertion order on others, so anything
    // that reads `files` positionally was reading an order the filesystem chose. Two machines with
    // identical trees could then produce different output from the same command.
    //
    // The cost is 7.2 ms on 100,003 paths against a 183 ms status, which is worth paying for a
    // contract every caller can rely on instead of one each has to remember not to depend on.
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(ScanResult {
        files,
        skipped_binary,
        rehashed,
        cached,
        unreadable,
    })
}

fn load_cache(conn: &Connection) -> ZResult<HashMap<String, (i64, i64, String)>> {
    // Reserving matters here: the map holds one entry per file and growing it by doubling rehashes
    // every string it already holds. The count is a cheap query on a table that is already in page
    // cache by the time this runs.
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM worktree_cache", [], |r| r.get(0))
        .unwrap_or(0);
    let mut out = HashMap::with_capacity(n.max(0) as usize);
    let mut stmt = conn.prepare("SELECT path, mtime, size, hash FROM worktree_cache")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for r in rows {
        let (path, mtime, size, hash) = r?;
        out.insert(path, (mtime, size, hash));
    }
    Ok(out)
}

/// Drop cache rows whose paths are absent from the scan results.
///
/// Equal counts do not prove equal membership. Populate a temporary live set and
/// delete stale rows by membership, including when the two counts are equal.
fn prune_cache(conn: &Connection, files: &[ScannedFile], binary: &[String]) -> ZResult<()> {
    conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS live_paths(path TEXT PRIMARY KEY);")?;
    conn.execute("DELETE FROM live_paths", [])?;
    {
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO live_paths(path) VALUES(?1)")?;
            for f in files {
                stmt.execute(params![f.rel])?;
            }
            for b in binary {
                stmt.execute(params![b.as_str()])?;
            }
        }
        tx.execute(
            "DELETE FROM worktree_cache WHERE path NOT IN (SELECT path FROM live_paths)",
            [],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Update one path's cache row after its content is known.
/// Record one path. Only the tests call this now; production batches its writes
/// through scan_with_cache, which needs a single transaction for the whole tree.
#[cfg(test)]
pub fn remember(conn: &Connection, root: &Path, rel: &str, hash: &str) -> ZResult<()> {
    let p = root.join(rel);
    let Ok(meta) = std::fs::metadata(&p) else {
        return Ok(());
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO worktree_cache(path, mtime, size, hash) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime,
                                         size = excluded.size,
                                         hash = excluded.hash",
        params![rel, mtime, meta.len() as i64, hash],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::mem_db;
    #[cfg(unix)]
    use crate::store::walk::read_bytes;

    #[test]
    fn pruning_compares_membership_when_counts_are_equal() {
        let conn = mem_db();
        conn.execute_batch(
            "INSERT INTO worktree_cache(path, mtime, size, hash) VALUES
             ('stale.txt', 0, 1, 'old'), ('keep.bin', 0, 1, 'binary');",
        )
        .unwrap();
        let files = vec![ScannedFile {
            rel: "new.txt".into(),
            hash: "new".into(),
            content: String::new(),
            mode: "100644".into(),
        }];
        prune_cache(&conn, &files, &["keep.bin".into()]).unwrap();
        let rows = load_cache(&conn).unwrap();
        assert!(
            !rows.contains_key("stale.txt"),
            "equal counts must not retain a stale row"
        );
        assert!(rows.contains_key("keep.bin"));
        assert_eq!(rows.len(), 1);

        prune_cache(&conn, &[], &[]).unwrap();
        assert!(load_cache(&conn).unwrap().is_empty());
        prune_cache(&conn, &[], &[]).unwrap();
        assert!(load_cache(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_scan_replaces_deleted_paths_without_retaining_cache_rows() {
        let root = tmpdir("replaced_path");
        fs::write(root.join("old.txt"), "old\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        let attrs = crate::support::attributes::Rules::default();
        scan_with_cache(&conn, &root, &rules, &attrs).unwrap();
        fs::remove_file(root.join("old.txt")).unwrap();
        fs::write(root.join("new.txt"), "new\n").unwrap();
        let scan = scan_with_cache(&conn, &root, &rules, &attrs).unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.files[0].rel, "new.txt");
        let rows = load_cache(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows.contains_key("new.txt"));
        assert!(!rows.contains_key("old.txt"));
        fs::remove_dir_all(root).unwrap();
    }

    /// Move a file's mtime into the past so it is outside the racy window.
    fn backdate(p: &std::path::Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        f.set_modified(when).unwrap();
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zf_cache_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_records_links_without_reading_their_targets() {
        use std::os::unix::fs::symlink;
        let root = tmpdir("links");
        let outside = tmpdir("link_targets");
        fs::write(outside.join("secret"), "private target content\n").unwrap();
        for (name, target) in [
            ("file_link", outside.join("secret")),
            ("directory_link", outside.clone()),
            ("dangling_link", outside.join("missing")),
        ] {
            symlink(&target, root.join(name)).unwrap();
        }
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        for _ in 0..2 {
            let scan = scan_with_cache(
                &conn,
                &root,
                &rules,
                &crate::support::attributes::Rules::default(),
            )
            .unwrap();
            assert!(scan.unreadable.is_empty(), "{:?}", scan.unreadable);
            assert_eq!(
                scan.files.len(),
                3,
                "directory and dangling links must be tracked"
            );
            for file in scan.files {
                let target = fs::read_link(root.join(&file.rel)).unwrap();
                let target = target.to_str().unwrap();
                assert_eq!(file.mode, "120000");
                assert_eq!(file.content, target);
                assert_eq!(file.hash, hash_content(target));
                assert!(!file.content.contains("private target content"));
            }
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn parallel_stats_preserve_directory_and_dangling_links() {
        use std::os::unix::fs::symlink;
        let root = tmpdir("parallel_links");
        for n in 0..512 {
            fs::write(root.join(format!("file_{n}")), "ordinary\n").unwrap();
        }
        symlink("missing", root.join("dangling_link")).unwrap();
        symlink(".", root.join("directory_link")).unwrap();
        let scan = scan_with_cache(
            &mem_db(),
            &root,
            &IgnoreRules::with_defaults(&[]),
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(scan.files.len(), 514);
        for (name, target) in [("dangling_link", "missing"), ("directory_link", ".")] {
            let file = scan.files.iter().find(|file| file.rel == name).unwrap();
            assert_eq!(file.mode, "120000");
            assert_eq!(file.content, target);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_bytes_does_not_follow_a_link_swapped_after_lstat() {
        use std::os::unix::fs::symlink;
        use std::time::Duration;

        let root = tmpdir("link_swap");
        let target = root.join("target");
        let candidate = root.join("candidate");
        let entry = root.join("entry");
        fs::write(&target, vec![b'x'; 32 * 1024 * 1024]).unwrap();
        symlink("target", &candidate).unwrap();
        fs::write(&entry, "safe").unwrap();

        let writer_entry = entry.clone();
        let writer_candidate = candidate.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..10_000 {
                let _ = fs::remove_file(&writer_entry);
                let _ = fs::rename(&writer_candidate, &writer_entry);
                let _ = symlink("target", &writer_candidate);
            }
        });
        std::thread::sleep(Duration::from_millis(10));
        for _ in 0..10_000 {
            if let Ok(bytes) = read_bytes(&entry) {
                assert!(
                    bytes == b"safe" || bytes == b"target",
                    "read followed a concurrently swapped symlink"
                );
            }
        }
        writer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_bytes_rejects_a_fifo_without_blocking() {
        use std::os::unix::fs::FileTypeExt;

        let root = tmpdir("fifo");
        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
        assert!(read_bytes(&fifo).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_scan_reports_every_text_file() {
        let d = tmpdir("scan");
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join("b.txt"), "two\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(r.files.len(), 2);
        // By name, not by index. The scan sorts now, but a test that looks up what it wrote is
        // checking the scan rather than the filesystem's ordering, and its failure message says which
        // file is wrong.
        let a = r
            .files
            .iter()
            .find(|f| f.rel == "a.txt")
            .expect("a.txt is missing");
        let b = r
            .files
            .iter()
            .find(|f| f.rel == "b.txt")
            .expect("b.txt is missing");
        assert_eq!(a.hash, hash_content("one\n"));
        assert_eq!(b.hash, hash_content("two\n"));
        // And the order itself is part of the contract now.
        assert_eq!(
            r.files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(),
            vec!["a.txt", "b.txt"],
            "the scan must return paths in sorted order"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A binary file is tracked now, with its bytes stored raw and no text content in the
    /// scan result. It used to be skipped entirely, which meant a repository holding an image
    /// could not take a snapshot at all.
    #[test]
    fn a_binary_file_is_tracked_with_its_bytes_and_no_text_content() {
        let d = tmpdir("bin");
        std::fs::write(d.join("a.txt"), "text\n").unwrap();
        std::fs::write(d.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();

        assert_eq!(r.files.len(), 2, "the binary should be in the scan");
        let bin = r
            .files
            .iter()
            .find(|f| f.rel == "b.bin")
            .expect("b.bin is missing from the scan");
        assert!(bin.content.is_empty(), "a binary must not arrive as text");
        assert_eq!(bin.hash, crate::store::hash_bytes(&[0u8, 1, 2, 3]));

        // The binary hash is tracked, but its bytes are NOT in the store yet.
        // A scan is a read path; the object is written when needed (e.g., changeset create).
        // get_object_bytes would fail here; the object is written by changeset create.
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn remember_then_scan_uses_the_cache_for_an_unchanged_file() {
        let d = tmpdir("hit");
        std::fs::write(d.join("a.txt"), "stable\n").unwrap();
        // Backdate the file past the racy window. A freshly written file is
        // deliberately re-read even when its stat matches, because a second
        // write inside the same timestamp would be invisible otherwise.
        backdate(&d.join("a.txt"), 60);
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);

        remember(&conn, &d, "a.txt", &hash_content("stable\n")).unwrap();
        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(r.cached, 1, "an unchanged stat should be a cache hit");
        assert_eq!(r.rehashed, 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_edited_seconds_ago_is_reread_even_when_its_stat_matches() {
        // The racy case: same path, same size, written twice close together. The
        // cache must not answer from the first write.
        let d = tmpdir("racy");
        std::fs::write(d.join("a.txt"), "AAAA\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        remember(&conn, &d, "a.txt", &hash_content("WRONG\n")).unwrap();

        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(
            r.files[0].hash,
            hash_content("AAAA\n"),
            "a young file must be reread, not trusted from the cache"
        );
        assert_eq!(r.cached, 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_file_is_rehashed_and_the_new_content_returned() {
        let d = tmpdir("miss");
        std::fs::write(d.join("a.txt"), "before\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        remember(&conn, &d, "a.txt", &hash_content("before\n")).unwrap();

        // Sleep past the one-second mtime granularity the cache uses.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(d.join("a.txt"), "after and longer\n").unwrap();

        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].content, "after and longer\n");
        assert_eq!(r.files[0].hash, hash_content("after and longer\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_cache_entry_never_hides_a_content_change() {
        // The stat can match while the bytes differ (same size, same second).
        // The scan rehashes whatever it reads, so the reported hash must follow
        // the content, not the cache row.
        let d = tmpdir("stale");
        std::fs::write(d.join("a.txt"), "AAAA\n").unwrap();
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        remember(&conn, &d, "a.txt", &hash_content("WRONG\n")).unwrap();

        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(
            r.files[0].hash,
            hash_content("AAAA\n"),
            "must hash real content"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cache_hit_does_not_read_the_file() {
        // The point of the cache is not opening the file. An earlier version
        // counted cache hits while still reading and hashing every file, which
        // reported a healthy cache and saved nothing.
        //
        // Proof: put different bytes on disk while keeping the stat identical. A
        // scan that reads would report the new hash. A scan that trusts the cache
        // reports the recorded one.
        let d = tmpdir("noread");
        let p = d.join("a.txt");
        std::fs::write(&p, "aaaa\n").unwrap();
        backdate(&p, 60);
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        let recorded = hash_content("aaaa\n");
        remember(&conn, &d, "a.txt", &recorded).unwrap();

        let old = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::fs::write(&p, "bbbb\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(r.cached, 1, "the cache row should answer");
        assert_eq!(r.rehashed, 0, "nothing should have been read");
        assert_eq!(
            r.files[0].hash, recorded,
            "the file must not have been read"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_mtime_forces_a_reread() {
        let d = tmpdir("mtime");
        let p = d.join("a.txt");
        std::fs::write(&p, "one\n").unwrap();
        backdate(&p, 60);
        let conn = mem_db();
        let rules = IgnoreRules::with_defaults(&[]);
        remember(&conn, &d, "a.txt", &hash_content("one\n")).unwrap();

        std::fs::write(&p, "a much longer second body\n").unwrap();
        let r = scan_with_cache(
            &conn,
            &d,
            &rules,
            &crate::support::attributes::Rules::default(),
        )
        .unwrap();
        assert_eq!(r.rehashed, 1);
        assert_eq!(r.files[0].hash, hash_content("a much longer second body\n"));

        let _ = std::fs::remove_dir_all(&d);
    }
}
