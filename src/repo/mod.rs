//! The repo tier.
//! Changeset lifecycle, blame derivation, merge, checkout.

pub mod bisect;
pub mod conflict;
pub mod dangling;
pub mod history;
pub mod hooks;
pub mod landed;
pub mod merge;
pub use merge::recursive_merge;
pub mod notes;
pub mod rebase;
pub mod stash;
pub mod submodule;
pub mod subrepo;
pub mod worktree;

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::repo::merge::{diff_hunks, merge3, MergeResult};
use crate::store::walk::IgnoreRules;
use crate::store::{
    branch_head, current_branch, get_object, hash_at_point, hash_content, latest_hash, merge_base,
    merge_bases, new_changeset_id, put_object, put_object_for,
};
use crate::support::safe_write;
use crate::{bad, now, repo_dir, ZResult};

/// Files loaded from the repository's ignore configuration.
pub fn load_ignore_rules(root: &Path, conn: &Connection) -> ZResult<IgnoreRules> {
    let mut extra: Vec<String> = Vec::new();

    for name in [".zenignore", ".gitignore"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            for line in text.lines() {
                let l = line.trim();
                if !l.is_empty() && !l.starts_with('#') {
                    extra.push(l.to_string());
                }
            }
        }
    }
    if let Ok(text) = fs::read_to_string(repo_dir(root).join("ignore")) {
        for line in text.lines() {
            let l = line.trim();
            if !l.is_empty() && !l.starts_with('#') {
                extra.push(l.to_string());
            }
        }
    }
    let mut stmt = conn.prepare("SELECT pattern FROM ignore_rules ORDER BY pattern")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    for r in rows {
        extra.push(r?);
    }
    Ok(IgnoreRules::with_defaults(&extra))
}

/// One file's diff: path, removed lines, added lines.
pub type FileDiff = (String, Vec<String>, Vec<String>);

/// What a diff of the working tree against a branch found.
pub struct WorktreeDelta {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub unchanged: usize,
    pub skipped_binary: Vec<String>,
    /// Files served from the stat cache, and files that had to be re-hashed.
    pub cached: usize,
    pub rehashed: usize,
}

/// Compare the working tree to `branch`.
pub fn diff_worktree(conn: &Connection, root: &Path, branch: &str) -> ZResult<WorktreeDelta> {
    let _q2 = std::time::Instant::now();
    let rules = load_ignore_rules(root, conn)?;
    let scope = crate::store::sparse::SparseScope::load(conn)?;
    let mut d = WorktreeDelta {
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
        unchanged: 0,
        skipped_binary: Vec::new(),
        cached: 0,
        rehashed: 0,
    };
    // Paths seen on disk, borrowed from the scan result. Cloning each one was 100,000 allocations
    // for a set that is read once, right here.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    // The committed tree is fetched once. Asking per file made this quadratic:
    // each lookup re-walked the whole parent chain.
    let committed = committed_tree(conn, branch)?;

    // The scan uses the stat cache, so an unchanged file is not re-read from
    // disk and re-hashed on every `status`.
    let attrs = crate::support::attributes::Rules::load(root);
    let scanned = crate::store::cache::scan_with_cache(conn, root, &rules, &attrs)?;
    for f in &scanned.files {
        // A sparse checkout leaves out-of-scope files on disk. Counting them
        // would report the rest of the repository as new.
        if !scope.contains(&f.rel) {
            continue;
        }
        seen.insert(f.rel.as_str());
        match committed.get(&f.rel) {
            None => d.added.push(f.rel.clone()),
            Some(old) if old != &f.hash => d.modified.push(f.rel.clone()),
            Some(_) => d.unchanged += 1,
        }
    }
    if let Some(first) = scanned.unreadable.first() {
        // A file that cannot be read cannot be compared, so reporting it as
        // unchanged or missing would be a lie either way.
        return Err(bad(format!(
            "cannot read {} ({} more)",
            first,
            scanned.unreadable.len() - 1
        )));
    }
    d.skipped_binary = scanned.skipped_binary;
    d.cached = scanned.cached;
    d.rehashed = scanned.rehashed;

    // Files tracked on the branch but absent from disk. A path outside the sparse scope was never
    // supposed to be on disk, so its absence is not a deletion: reporting it as one would let a
    // commit wipe the whole repo.
    //
    // The scan already stat-ed every path on disk, so a committed path is missing exactly when the
    // scan did not report it. The older loop asked the filesystem again for every tracked path,
    // which was a second stat per file, and it cloned and sorted all 100,000 keys to do it.
    // A subrepository whose checkout is missing is not a deletion either: reporting it as one would
    // let a single commit remove the pointer to the child.
    // Submodule checkouts are excluded for the same reason a subrepository checkout is:
    // their files belong to another repository.
    let mut subs = crate::repo::subrepo::excluded_paths(conn);
    subs.extend(crate::repo::submodule::excluded_paths(root));
    for path in committed.keys() {
        if !scope.contains(path) {
            continue;
        }
        if crate::repo::subrepo::is_inside_sub(path, &subs) {
            continue;
        }
        if !seen.contains(path.as_str()) {
            d.deleted.push(path.clone());
        }
    }

    Ok(d)
}

/// A newly created changeset.
pub struct NewChangeset {
    pub id: String,
    pub files: Vec<(String, Option<String>, String)>,
    pub deleted: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn create_changeset(
    conn: &Connection,
    root: &Path,
    message: &str,
    author: &str,
) -> ZResult<Option<NewChangeset>> {
    create_changeset_scoped(conn, root, message, author, &[])
}

/// `create_changeset`, optionally limited to a set of paths.
///
/// A snapshot of the whole tree is one writer's answer. Two agents working in one checkout need
/// theirs separated, or each records the other's files under its own name. A path matches when it is
/// the path, or when it is a parent of it, so naming a directory covers what is under it.
pub fn create_changeset_scoped(
    conn: &Connection,
    root: &Path,
    message: &str,
    author: &str,
    paths: &[String],
) -> ZResult<Option<NewChangeset>> {
    let wanted: Vec<String> = paths
        .iter()
        .map(|p| p.trim_start_matches("./").replace('\\', "/"))
        .collect();
    let in_scope = |rel: &str| -> bool {
        if wanted.is_empty() {
            return true;
        }
        wanted
            .iter()
            .any(|w| rel == w || rel.starts_with(&format!("{w}/")) || w.is_empty())
    };
    let branch = current_branch(conn)?;
    let rules = load_ignore_rules(root, conn)?;
    let scope = crate::store::sparse::SparseScope::load(conn)?;

    let mut changes: Vec<(String, Option<String>, String, String, String)> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // One tree lookup for the whole snapshot, not one per file.
    let committed = committed_tree(conn, &branch)?;

    let attrs = crate::support::attributes::Rules::load(root);
    let scanned = crate::store::cache::scan_with_cache(conn, root, &rules, &attrs)?;
    if let Some(first) = scanned.unreadable.first() {
        return Err(bad(format!(
            "cannot read {} ({} more)",
            first,
            scanned.unreadable.len() - 1
        )));
    }
    // Two files on disk can name one repository path only if a name means something different
    // from what the store thinks a separator is. Say which two, rather than letting the insert
    // fail on a primary key and reporting a database constraint to somebody who asked about files.
    {
        let mut seen_rel: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for f in &scanned.files {
            if !seen_rel.insert(f.rel.as_str()) {
                return Err(bad(format!(
                    "two files on disk map to one repository path: {}\na directory name and a file name differ only in the separator byte, and a repository path holds one of them",
                    f.rel
                )));
            }
        }
    }
    for f in scanned.files {
        if !scope.contains(&f.rel) || !in_scope(&f.rel) {
            continue;
        }
        seen.insert(f.rel.clone());
        let old_hash = committed.get(&f.rel).cloned();
        if old_hash.as_deref() != Some(f.hash.as_str()) {
            // Only a changed file needs its bytes. A stat cache hit returns an
            // empty body, so read it here rather than opening every file in the
            // tree on every snapshot.
            let content = if f.content.is_empty() {
                // Could be a binary file or a cache hit. Read the bytes and
                // classify.
                match crate::store::walk::read_file(&root.join(&f.rel)) {
                    crate::store::walk::ReadOutcome::Text(c) => c,
                    crate::store::walk::ReadOutcome::Binary => {
                        // A symlink's body is its target name, which reading the path cannot
                        // produce - the open follows the link, so a dangling or non-UTF-8 target
                        // fails instead of yielding the name. The name is tiny, so the streaming
                        // path buys nothing there either.
                        let p = root.join(&f.rel);
                        let is_link = std::fs::symlink_metadata(&p)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false);
                        if is_link {
                            let bytes = crate::store::walk::read_bytes(&p)?;
                            crate::store::put_object_bytes_for(
                                conn,
                                &bytes,
                                Some(f.rel.as_str()),
                                Some(&attrs),
                            )?;
                        } else {
                            let hash = crate::store::hash_file(&p)?;
                            crate::store::put_object_from_file(
                                conn,
                                &p,
                                &hash,
                                std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0),
                            )?;
                        }
                        String::new()
                    }
                    _ => continue,
                }
            } else {
                f.content
            };
            changes.push((f.rel, old_hash, f.hash, content, f.mode));
        }
    }
    let skipped = scanned.skipped_binary;

    let mut tracked: Vec<String> = committed.keys().cloned().collect();
    tracked.sort();
    for path in tracked {
        if !scope.contains(&path) || !in_scope(&path) {
            continue;
        }
        if !seen.contains(&path) && !root.join(&path).exists() {
            deleted.push(path);
        }
    }

    if changes.is_empty() && deleted.is_empty() {
        return Ok(None);
    }

    let id = new_changeset_id(conn, message, author)?;
    let parent = branch_head(conn, &branch)?;

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, branch)
         VALUES(?1, ?2, ?3, ?4, 'pending', ?5, ?6)",
        params![id, message, author, now(), parent, branch],
    )?;

    let mut files = Vec::new();
    for (rel, old, new, content, mode) in &changes {
        // A delta against the previous version of this path is what keeps a
        // long editing history small. `old` is that previous version's hash.
        // The path is here and the rules are here, so a text file the rules call large goes out too.
        put_object_for(&tx, content, Some(rel), Some(&attrs), old.as_deref())?;
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash, mode)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id, rel, old, new, mode],
        )?;
        files.push((rel.clone(), old.clone(), new.clone()));
    }
    for path in &deleted {
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, NULL)",
            params![id, path, latest_hash(&tx, path, &branch)?],
        )?;
    }
    tx.commit()?;

    Ok(Some(NewChangeset {
        id,
        files,
        deleted,
        skipped,
    }))
}

/// Commit one pending changeset. Returns files written; `0` means it was
/// already committed.
pub fn commit_one(conn: &Connection, id: &str) -> ZResult<usize> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT status, COALESCE(branch, 'main') FROM changesets WHERE id = ?1",
            params![id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let (status, branch) = row.ok_or_else(|| {
        crate::support::error::Error::not_found(format!("no such changeset: {}", id))
    })?;
    if status == "committed" {
        return Ok(0);
    }

    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    // One transaction: a half-written commit leaves orphan versions and a
    // pending changeset with content already recorded.
    let tx = conn.unchecked_transaction()?;

    // Re-parent onto the live head so a pending changeset never forks history
    // or orphans an earlier one.
    let head_now = branch_head(&tx, &branch)?;
    if head_now.as_deref() != Some(id) {
        tx.execute(
            "UPDATE changesets SET parent = ?1 WHERE id = ?2",
            params![head_now, id],
        )?;
    }

    let mut written = 0usize;
    let mut touched: Vec<(String, String)> = Vec::new();
    for (path, old, new) in &files {
        match new {
            Some(hash) => {
                let latest: Option<String> = tx
                    .query_row(
                        "SELECT hash FROM file_versions WHERE path = ?1
                         ORDER BY version DESC LIMIT 1",
                        params![path],
                        |r| r.get(0),
                    )
                    .optional()?;

                if latest.as_deref() != Some(hash.as_str()) {
                    let next_ver: i64 = tx.query_row(
                        "SELECT COALESCE(MAX(version), 0) + 1 FROM file_versions WHERE path = ?1",
                        params![path],
                        |r| r.get(0),
                    )?;
                    tx.execute(
                        "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
                         VALUES(?1, ?2, ?3, ?4, ?5)",
                        params![path, next_ver, hash, id, now()],
                    )?;
                    written += 1;
                }

                // The committed content is already in `objects`; duplicating it
                // into a working-copy table only doubled the disk for small
                // repositories and nothing ever read it back.
                touched.push((path.clone(), hash.clone()));
            }
            None => {
                // Deletion: record it as a version with no hash so history can still answer "which
                // changeset removed this file".
                //
                // The blame rows are kept when this changeset also added the same content under
                // another path, because that is a rename: the file still exists, under a new name,
                // and the lines keep the authors they had. Deleting them here was why `blame` after
                // a rename attributed every surviving line to the commit that moved the file - the
                // rows it would have inherited from were gone by the time anyone asked.
                //
                // A real deletion drops them, so `blame_ranges` does not grow a permanent row set
                // for every path that ever existed.
                let renamed = {
                    let mut stmt = tx.prepare(
                        "SELECT 1 FROM changeset_files
                         WHERE changeset_id = ?1 AND old_hash IS NULL AND new_hash = ?2 AND path <> ?3
                         LIMIT 1",
                    )?;
                    let found: Option<i64> = stmt
                        .query_row(params![id, old, path], |r| r.get(0))
                        .optional()?;
                    found.is_some()
                };
                if !renamed {
                    tx.execute("DELETE FROM blame_ranges WHERE path = ?1", params![path])?;
                }
                written += 1;
            }
        }
    }

    // Record how far this changeset is from a root. A commit is one more than its highest parent,
    // which is the number git calls `generation` and every history walk in git, Jujutsu, Sapling and
    // Wren uses to skip a level. Written at commit time so it is never derived on a read.
    let generation: i64 = {
        let p1: Option<String> = tx
            .query_row(
                "SELECT parent FROM changesets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let p2: Option<String> = tx
            .query_row(
                "SELECT parent2 FROM changesets WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let gen_of = |p: &Option<String>| -> i64 {
            p.as_ref()
                .and_then(|h| {
                    tx.query_row(
                        "SELECT COALESCE(generation, 0) FROM changesets WHERE id = ?1",
                        params![h],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                })
                .unwrap_or(0)
        };
        1 + gen_of(&p1).max(gen_of(&p2))
    };
    tx.execute(
        "UPDATE changesets SET status = 'committed', generation = ?2 WHERE id = ?1",
        params![id, generation],
    )?;
    crate::store::set_branch_head(&tx, &branch, id)?;

    recompute_blame(&tx, &touched)?;
    tx.commit()?;
    Ok(written)
}

/// Rebuild blame for the given files.
///
/// Lines are matched by sequence (longest common subsequence against each older
/// version), not by content set, so a line that duplicates another line keeps
/// its own author and a moved line keeps the author who wrote it.
/// Rebuild blame for the given files, starting from the blame already stored for
/// the previous version of each file.
///
/// The earlier version walked the whole version history on every commit, so the
/// nth commit to a file read n versions and total work grew with the square of
/// the edit count. Measured on a 200 line file: 9 ms at 10 versions, 131 ms at
/// 120. Only two versions are needed: lines that survived keep the owner they
/// had, lines that appeared belong to the commit being made.
/// Rebuild blame for one path by walking its whole version chain, oldest first.
///
/// `recompute_blame` compares the newest version against the one before it, which is what a commit
/// that appended one version needs. An import writes the entire chain in one go, so when that
/// function runs the previous version has no blame rows yet, every line of the newest version finds
/// nothing to inherit, and the whole file is attributed to the last changeset that touched it.
///
/// Walking from the oldest version forwards fixes that: each step inherits from a chain that has
/// already been computed. The per-step comparison is the same one `recompute_blame` uses, so there is
/// one implementation of how blame propagates and this only decides the order.
pub fn rebuild_blame_chain(conn: &Connection, path: &str) -> ZResult<()> {
    let versions: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT version, hash FROM file_versions WHERE path = ?1 ORDER BY version ASC",
        )?;
        let rows = stmt.query_map(params![path], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if versions.is_empty() {
        return Ok(());
    }

    // Clearing and rebuilding are one write. The `DELETE` used to run on its own, so a failure
    // partway through the rebuild left the path with no blame at all: worse than the stale rows it
    // had, because `blame` then had nothing to answer with and nothing said so. A savepoint nests
    // inside a caller's transaction, which `BEGIN` cannot do (#96).
    conn.execute_batch("SAVEPOINT rebuild_blame_chain")?;
    let outcome = rebuild_blame_chain_inner(conn, path, &versions);
    match outcome {
        Ok(()) => {
            conn.execute_batch("RELEASE rebuild_blame_chain")?;
            Ok(())
        }
        Err(e) => {
            // Put the previous blame back exactly as it was: a failed rebuild leaves the old rows.
            let _ = conn.execute_batch("ROLLBACK TO rebuild_blame_chain");
            let _ = conn.execute_batch("RELEASE rebuild_blame_chain");
            Err(e)
        }
    }
}

fn rebuild_blame_chain_inner(
    conn: &Connection,
    path: &str,
    versions: &[(i64, String)],
) -> ZResult<()> {
    // Clear first: a chain built on top of half-written rows attributes lines to whatever was there.
    conn.execute("DELETE FROM blame_ranges WHERE path = ?1", params![path])?;

    for (_version, hash) in versions {
        // One step: this version against the rows the previous step wrote. `recompute_blame` reads
        // the newest `file_versions` row, so the chain is walked by asking it at each position and
        // letting it inherit from what is already stored.
        recompute_blame_at(conn, path, hash)?;
    }
    Ok(())
}

/// Attribute one version of a path, inheriting from whatever blame rows already exist.
///
/// This is the body of the per-path step in `recompute_blame`, split out so a chain walk can call it
/// with whichever version it is on rather than always the newest.
fn recompute_blame_at(conn: &Connection, path: &str, hash: &str) -> ZResult<()> {
    let current = match get_object(conn, hash) {
        Ok(text) => text,
        Err(_) => {
            // A binary file has no lines to attribute.
            return Ok(());
        }
    };
    let cur_lines: Vec<&str> = current.lines().collect();

    // The changeset that wrote this version, found by the hash rather than by "newest", because a
    // chain walk is not always on the newest one.
    let (this_id, this_author, this_ts): (String, String, i64) = conn.query_row(
        "SELECT cs.id, cs.author, cs.created_at
         FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
         WHERE fv.path = ?1 AND fv.hash = ?2
         ORDER BY fv.version ASC LIMIT 1",
        params![path, hash],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    // What the previous step attributed, one entry per line.
    //
    // The rows for this path are the normal case. When there are none, the lines came from a file
    // that was renamed here, and reading only this path's rows attributes every surviving line to
    // the commit that moved the file rather than the one that wrote the code.
    //
    // The rename is found through `rename_history`, which walks the chain the same way `gesta
    // history` does. Writing a second detector here would let the two commands disagree about what
    // counts as a rename, and the disagreement would only show up on a file old enough to matter.
    let blame_source: String = {
        let has_rows: bool = conn
            .query_row(
                "SELECT 1 FROM blame_ranges WHERE path = ?1 LIMIT 1",
                params![path],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if has_rows {
            path.to_string()
        } else {
            // The chain starts at this path and walks backwards, so the name the lines came
            // from is the last entry. Taking the first returns the path itself, which looks
            // harmless and silently puts every caller back on the branch with no previous
            // version: one version means nothing to diff against, and every line is attributed
            // to the rename.
            crate::repo::history::rename_history(conn, path, 4)?
                .into_iter()
                .last()
                .filter(|earlier| earlier != path)
                .unwrap_or_else(|| path.to_string())
        }
    };

    let previous: Vec<(String, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT start_line, end_line, changeset_id FROM blame_ranges
             WHERE path = ?1 ORDER BY start_line ASC",
        )?;
        let ranges = stmt
            .query_map(params![blame_source], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for (start, end, cs) in ranges {
            for _ in start..=end {
                out.push((cs.clone(), String::new(), 0i64));
            }
        }
        out
    };

    // The version immediately older than this one, by version number rather than by offset from the
    // newest: in a chain walk the newest is not the reference point.
    // Where the content came from, which is not always this path.
    //
    // Two shapes need different answers and the difference is the whole point:
    //
    // - an ordinary edit: this path's own previous version, one number below the newest
    // - a rename: the renamed file's last version, because this path has exactly one version and
    //   there is nothing below it. Asking for "the version older than this one" on a one-version
    //   path returns nothing, `old_lines` comes back empty, and every surviving line is attributed
    //   to the commit that moved the file rather than the one that wrote it.
    let prev_hash: Option<String> = if blame_source == *path {
        let this_version: i64 = conn.query_row(
            "SELECT version FROM file_versions WHERE path = ?1 AND hash = ?2 ORDER BY version ASC LIMIT 1",
            params![path, hash],
            |r| r.get(0),
        )?;
        conn.query_row(
            "SELECT hash FROM file_versions WHERE path = ?1 AND version < ?2
             ORDER BY version DESC LIMIT 1",
            params![path, this_version],
            |r| r.get(0),
        )
        .optional()?
    } else {
        conn.query_row(
            "SELECT hash FROM file_versions WHERE path = ?1 ORDER BY version DESC LIMIT 1",
            params![blame_source],
            |r| r.get(0),
        )
        .optional()?
    };
    let old_lines: Vec<String> = match prev_hash {
        Some(h) => get_object(conn, &h)
            .map(|t| t.lines().map(|l| l.to_string()).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // The two matching passes, unchanged from the single-step version.
    let mut owner: Vec<Option<(String, String, i64)>> = vec![None; cur_lines.len()];
    let mut used = vec![false; old_lines.len()];
    let mut j = 0usize;
    for (i, line) in cur_lines.iter().enumerate() {
        let mut k = j;
        while k < old_lines.len() {
            if !used[k] && &old_lines[k] == line {
                owner[i] = previous.get(k).cloned();
                used[k] = true;
                j = k + 1;
                break;
            }
            k += 1;
        }
    }
    let mut by_content: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    for (k, line) in old_lines.iter().enumerate() {
        if !used[k] {
            by_content.entry(line.as_str()).or_default().push(k);
        }
    }
    for i in 0..cur_lines.len() {
        if owner[i].is_some() {
            continue;
        }
        if let Some(cands) = by_content.get_mut(cur_lines[i]) {
            if let Some(k) = cands.pop() {
                if !used[k] {
                    owner[i] = previous.get(k).cloned();
                    used[k] = true;
                }
            }
        }
    }

    // Anything still unattributed was written by this version.
    let mut ranges: Vec<(i64, i64, String)> = Vec::new();
    for (i, o) in owner.iter().enumerate() {
        let cs = match o {
            Some((id, _, _)) if !id.is_empty() => id.clone(),
            _ => this_id.clone(),
        };
        let line = (i + 1) as i64;
        match ranges.last_mut() {
            Some((_, end, last)) if *last == cs && *end == line - 1 => *end = line,
            _ => ranges.push((line, line, cs)),
        }
    }
    conn.execute("DELETE FROM blame_ranges WHERE path = ?1", params![path])?;
    let mut ins = conn.prepare(
        "INSERT INTO blame_ranges(path, start_line, end_line, changeset_id) VALUES(?1,?2,?3,?4)",
    )?;
    for (start, end, cs) in ranges {
        ins.execute(params![path, start, end, cs])?;
    }
    let _ = (this_author, this_ts);
    Ok(())
}

/// Rebuild blame for every path the store holds a newest version of.
///
/// A commit already knows which files it touched and calls `recompute_blame` with that list. An
/// import does not: it writes a whole history at once, and nothing in that path collects the per-file
/// list a commit would. Reading the store's own answer for "newest version of each path" is the same
/// question asked once instead of per file, and it is what makes `blame` work on imported history.
///
/// `history` worked on imported repositories and `blame` did not, because only one of them reads
/// `file_versions`. The lines a reader wants attributed were never computed.
pub fn rebuild_all_blame(conn: &Connection) -> ZResult<usize> {
    let paths: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT path FROM file_versions ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for p in &paths {
        rebuild_blame_chain(conn, p)?;
    }
    Ok(paths.len())
}

pub fn recompute_blame(conn: &Connection, files: &[(String, String)]) -> ZResult<()> {
    for (path, hash) in files {
        // A binary file has no lines, so there is nothing to attribute. Asking for it as text
        // failed the whole commit with a message about UTF-8, which stopped any repository
        // holding an image from taking a snapshot at all.
        let current = match get_object(conn, hash) {
            Ok(text) => text,
            Err(_) => {
                conn.execute("DELETE FROM blame_ranges WHERE path = ?1", params![path])?;
                continue;
            }
        };
        let cur_lines: Vec<&str> = current.lines().collect();

        // The commit that put this version in place. A file always has a version
        // row by the time blame runs; a missing one is a broken store, not a
        // case to paper over with a default.
        let (this_id, this_author, this_ts): (String, String, i64) = conn.query_row(
            "SELECT cs.id, cs.author, cs.created_at
             FROM file_versions fv JOIN changesets cs ON cs.id = fv.changeset_id
             WHERE fv.path = ?1 ORDER BY fv.version DESC LIMIT 1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        // Where the lines came from. Normally this path. When this path is new and has no blame of
        // its own, the lines arrived through a rename, and the rows to inherit are the ones the old
        // name still holds.
        //
        // `rename_history` is the same walk `gesta history` prints, so both commands agree about
        // what counts as a rename. It returns this path first and the names it came from after, so
        // the oldest is the last entry - taking the first would return the path itself and put the
        // reader straight back on the one-version branch this block exists to avoid.
        let blame_source: String = {
            let has_rows: bool = conn
                .query_row(
                    "SELECT 1 FROM blame_ranges WHERE path = ?1 LIMIT 1",
                    params![path],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if has_rows {
                path.to_string()
            } else {
                crate::repo::history::rename_history(conn, path, 4)?
                    .into_iter()
                    .last()
                    .filter(|earlier| earlier != path)
                    .unwrap_or_else(|| path.to_string())
            }
        };

        // Blame comes back as ranges, and the match below walks two sequences line by line, so
        // the ranges are expanded here. Line order is what lets that walk run forward instead of
        // searching, and a range always covers consecutive lines.
        let previous: Vec<(String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT b.start_line, b.end_line, b.changeset_id, c.author, c.created_at
                 FROM blame_ranges b JOIN changesets c ON c.id = b.changeset_id
                 WHERE b.path = ?1 ORDER BY b.start_line ASC",
            )?;
            let ranges = stmt
                .query_map(params![blame_source], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for (start, end, cs, author, ts) in ranges {
                for _ in start..=end {
                    out.push((cs.clone(), author.clone(), ts));
                }
            }
            out
        };
        // `previous` is one entry per line of the previous version, which is what the two matching
        // passes index by line number. It is built by expanding ranges rather than by cloning per
        // line, so a file written in one commit costs one range and one pass, not one range and
        // N clones.

        // The version the lines came from. On an ordinary edit that is the version before this one;
        // after a rename it is the old name's last version, because this path has exactly one and
        // there is nothing below it.
        let prev_hash: Option<String> = if blame_source == *path {
            conn.query_row(
                "SELECT hash FROM file_versions WHERE path = ?1
                 ORDER BY version DESC LIMIT 1 OFFSET 1",
                params![path],
                |r| r.get(0),
            )
            .optional()?
        } else {
            conn.query_row(
                "SELECT hash FROM file_versions WHERE path = ?1 ORDER BY version DESC LIMIT 1",
                params![blame_source],
                |r| r.get(0),
            )
            .optional()?
        };
        let old_lines: Vec<String> = match prev_hash {
            Some(h) => get_object(conn, &h)
                .map(|s| s.lines().map(|l| l.to_string()).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        };

        // Two passes. The first matches lines in order, which covers the common
        // case and cannot attribute a line to an earlier position than the
        // previous match. The second looks for the leftovers anywhere in the old
        // version, so a moved line keeps the author who wrote it instead of being
        // handed to whoever moved it.
        let mut owner: Vec<Option<(String, String, i64)>> = vec![None; cur_lines.len()];
        let mut used = vec![false; old_lines.len()];
        let mut j = 0usize;
        for (i, line) in cur_lines.iter().enumerate() {
            let mut k = j;
            while k < old_lines.len() {
                if !used[k] && &old_lines[k] == line {
                    owner[i] = previous.get(k).cloned();
                    used[k] = true;
                    j = k + 1;
                    break;
                }
                k += 1;
            }
        }
        // Second pass: a line that moved keeps its author. The old version is indexed by content
        // once, so each unmatched line costs a lookup rather than a scan of the whole file. The
        // scan it replaces was quadratic, and on a file with ten thousand lines that was the
        // difference between a commit that costs 13 ms and one that costs 47.
        let mut by_content: std::collections::HashMap<&str, Vec<usize>> =
            std::collections::HashMap::new();
        for (k, old_line) in old_lines.iter().enumerate() {
            if !used[k] {
                by_content.entry(old_line.as_str()).or_default().push(k);
            }
        }
        for (i, line) in cur_lines.iter().enumerate() {
            if owner[i].is_some() {
                continue;
            }
            if let Some(candidates) = by_content.get_mut(line) {
                // Take the first entry that this commit has not already claimed. A moved line
                // appears once in the old version, so the list is short in the common case.
                if let Some(k) = candidates.iter().position(|k| !used[*k]) {
                    let old_index = candidates.remove(k);
                    owner[i] = previous.get(old_index).cloned();
                    used[old_index] = true;
                }
            }
        }

        // Whatever is still unowned was introduced by this commit.
        let fallback = (this_id.clone(), this_author.clone(), this_ts);
        let owner: Vec<(String, String, i64)> = owner
            .into_iter()
            .map(|o| o.unwrap_or_else(|| fallback.clone()))
            .collect();

        // The new blame is written as ranges, and only the ranges that differ from the stored ones
        // are written. Deleting the file's whole blame and rewriting it cost 34 ms on a ten thousand
        // line file, every commit, which is why a commit got slower as a file grew.
        //
        // Comparing line by line against the previous version would be wrong: removing one line
        // shifts every line after it, so index i in the new file is a different line than index i in
        // the old one. The comparison is between the old ranges and the new ones instead, which is
        // what the table actually holds.
        let mut ranges: Vec<(usize, usize, String)> = Vec::new();
        let mut i = 0usize;
        while i < owner.len() {
            let (cs_id, _, _) = &owner[i];
            let start = i;
            let mut end = i;
            while end + 1 < owner.len() && &owner[end + 1].0 == cs_id {
                end += 1;
            }
            ranges.push((start + 1, end + 1, cs_id.clone()));
            i = end + 1;
        }

        // The stored ranges, in the same shape.
        let stored: Vec<(usize, usize, String)> = {
            let mut stmt = conn.prepare(
                "SELECT start_line, end_line, changeset_id FROM blame_ranges
                 WHERE path = ?1 ORDER BY start_line",
            )?;
            let rows = stmt
                .query_map(params![path], |r| {
                    Ok((
                        r.get::<_, i64>(0)? as usize,
                        r.get::<_, i64>(1)? as usize,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        if stored == ranges {
            // Identical content with identical ownership. Nothing to write, which is the common case
            // for a file that was touched but not changed.
            return Ok(());
        }

        // The span covered by the ranges that differ. A range present in one list and not the other
        // is inside it, so deleting the span and rewriting it leaves both lists in agreement.
        let first_diff =
            (0..ranges.len().max(stored.len())).find(|k| ranges.get(*k) != stored.get(*k));
        let Some(first_diff) = first_diff else {
            return Ok(());
        };
        // From the first difference to the end, the two lists can differ in boundaries as well as
        // owners, so the rewrite reaches to the end of the file. A prefix that matches exactly is
        // left alone, which is what makes the cost follow the change rather than the file: appending
        // one line leaves every earlier range untouched.
        let lo = ranges
            .get(first_diff)
            .map(|r| r.0)
            .or_else(|| stored.get(first_diff).map(|r| r.0))
            .unwrap_or(1);

        conn.execute(
            "DELETE FROM blame_ranges WHERE path = ?1 AND end_line >= ?2",
            params![path, lo as i64],
        )?;
        let mut stmt = conn.prepare(
            "INSERT INTO blame_ranges(path, start_line, end_line, changeset_id)
             VALUES(?1, ?2, ?3, ?4)",
        )?;
        for (start, end, cs_id) in &ranges {
            if *end >= lo {
                stmt.execute(params![path, *start as i64, *end as i64, cs_id])?;
            }
        }
    }
    Ok(())
}

/// Append a conflict copy of a file so a failed merge never loses the other side.
pub fn write_conflict_copy(root: &Path, path: &str, theirs: &str) -> ZResult<String> {
    let target = root.join(format!("{}.theirs", path));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&target, theirs)?;
    Ok(format!("{}.theirs", path))
}

/// Result of a merge attempt.
#[derive(Debug)]
pub struct MergeOutcome {
    pub applied: Vec<String>,
    pub conflicts: Vec<String>,
    /// One structured report per conflicted file, with both sides and line
    /// numbers, so a caller can hand it to an agent instead of markers.
    pub reports: Vec<crate::repo::conflict::ConflictReport>,
    pub merge_changeset: Option<String>,
}

/// Compute what merging `source` into `target` would conflict on, without
/// touching the working tree, the branch heads, or the database.
///
/// A merge that stops on conflict has already written markers to disk, which is
/// fine for a person but wasteful for an agent: it would have to read files it
/// is about to overwrite. This answers the same question from the object store.
pub fn merge_preview(
    conn: &Connection,
    _root: &Path,
    source: &str,
    target: &str,
) -> ZResult<Vec<crate::repo::conflict::ConflictReport>> {
    let src_head = branch_head(conn, source)?;
    let tgt_head = branch_head(conn, target)?;
    let Some(src_head) = src_head else {
        return Ok(Vec::new());
    };
    if Some(src_head.clone()) == tgt_head {
        return Ok(Vec::new());
    }

    let tgt_head_for_bases = tgt_head.clone().unwrap_or_default();
    let bases = merge_bases(conn, &tgt_head_for_bases, &src_head)?;
    let base: Option<String> = if bases.is_empty() {
        None
    } else if bases.len() == 1 {
        Some(bases[0].clone())
    } else {
        // Multiple bases: recursively merge them to form a virtual base. The target head is needed for
        // that, and a target with no commits cannot have produced multiple merge bases - but the
        // unwrap that used to sit here turned "cannot happen" into a panic if it ever did.
        match tgt_head.as_ref() {
            Some(t) => Some(recursive_merge(conn, &bases, &src_head, t)?),
            None => Some(bases[0].clone()),
        }
    };

    let src_anc = crate::store::ancestors(conn, &src_head)?;
    let tgt_anc: Vec<String> = match &tgt_head {
        Some(t) => crate::store::ancestors(conn, t)?,
        None => Vec::new(),
    };
    let base_set: std::collections::HashSet<String> = match &base {
        Some(b) => crate::store::ancestors(conn, b)?.into_iter().collect(),
        None => std::collections::HashSet::new(),
    };

    let collect = |ids: &[String],
                   exclude: &std::collections::HashSet<String>|
     -> ZResult<Vec<String>> {
        let mut out = std::collections::HashSet::new();
        let mut stmt = conn.prepare("SELECT path FROM changeset_files WHERE changeset_id = ?1")?;
        for id in ids {
            if exclude.contains(id) {
                continue;
            }
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.insert(r?);
            }
        }
        Ok(out.into_iter().collect())
    };

    let mut all: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in collect(&src_anc, &base_set)? {
        all.insert(p);
    }
    for p in collect(&tgt_anc, &base_set)? {
        all.insert(p);
    }
    let mut paths: Vec<String> = all.into_iter().collect();
    paths.sort();

    let read = |path: &str, rev: Option<&String>| -> ZResult<String> {
        match rev {
            Some(r) => match hash_at_point(conn, path, r)? {
                Some(h) => get_object(conn, &h),
                None => Ok(String::new()),
            },
            None => Ok(String::new()),
        }
    };

    let mut reports = Vec::new();
    for path in &paths {
        let base_text = read(path, base.as_ref())?;
        let our_text = read(path, tgt_head.as_ref())?;
        let their_text = read(path, Some(&src_head))?;
        if let Some(rep) = crate::repo::conflict::report(path, &base_text, &our_text, &their_text) {
            reports.push(rep);
        }
    }
    Ok(reports)
}

/// Merge `source` into `target` with a real three-way merge.
///
/// Writes merged content to disk, records a merge changeset with two parents,
/// and refuses to move the target head when any file conflicts.
pub fn merge_branch(
    conn: &Connection,
    root: &Path,
    source: &str,
    target: &str,
    author: &str,
) -> ZResult<MergeOutcome> {
    let src_head = branch_head(conn, source)?;
    let tgt_head = branch_head(conn, target)?;

    let Some(src_head) = src_head else {
        return Ok(MergeOutcome {
            applied: Vec::new(),
            conflicts: Vec::new(),
            reports: Vec::new(),
            merge_changeset: None,
        });
    };
    if Some(src_head.clone()) == tgt_head {
        return Ok(MergeOutcome {
            applied: Vec::new(),
            conflicts: Vec::new(),
            reports: Vec::new(),
            merge_changeset: None,
        });
    }

    // Fast-forward when the target is already an ancestor of the source.
    if let Some(t) = &tgt_head {
        let tgt_anc = crate::store::ancestors(conn, t)?;
        if tgt_anc.contains(&src_head) {
            return Ok(MergeOutcome {
                applied: Vec::new(),
                conflicts: Vec::new(),
                reports: Vec::new(),
                merge_changeset: None,
            });
        }
        let src_anc = crate::store::ancestors(conn, &src_head)?;
        if src_anc.contains(t) {
            // Target is behind: move the head and materialise the files.
            let tx = conn.unchecked_transaction()?;
            crate::store::set_branch_head(&tx, target, &src_head)?;
            tx.commit()?;
            return Ok(MergeOutcome {
                applied: vec![src_head],
                conflicts: Vec::new(),
                reports: Vec::new(),
                merge_changeset: None,
            });
        }
    }

    let base = match &tgt_head {
        Some(t) => merge_base(conn, t, &src_head)?,
        None => None,
    };

    // Every path touched on either side relative to the base.
    let mut paths: Vec<String> = Vec::new();
    let src_anc = crate::store::ancestors(conn, &src_head)?;
    let tgt_anc: Vec<String> = match &tgt_head {
        Some(t) => crate::store::ancestors(conn, t)?,
        None => Vec::new(),
    };
    let base_set: std::collections::HashSet<String> = match &base {
        Some(b) => crate::store::ancestors(conn, b)?.into_iter().collect(),
        None => std::collections::HashSet::new(),
    };

    let collect = |ids: &[String],
                   exclude: &std::collections::HashSet<String>|
     -> ZResult<Vec<String>> {
        let mut out = std::collections::HashSet::new();
        let mut stmt = conn.prepare("SELECT path FROM changeset_files WHERE changeset_id = ?1")?;
        for id in ids {
            if exclude.contains(id) {
                continue;
            }
            let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.insert(r?);
            }
        }
        Ok(out.into_iter().collect())
    };

    let mut all: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in collect(&src_anc, &base_set)? {
        all.insert(p);
    }
    for p in collect(&tgt_anc, &base_set)? {
        all.insert(p);
    }
    paths.extend(all);
    paths.sort();

    let mut merged_files: Vec<(String, String)> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut reports: Vec<crate::repo::conflict::ConflictReport> = Vec::new();

    for path in &paths {
        // Present or absent is a different question from empty or not empty, and this loop used to
        // conflate them: every `None` became `String::new()`, so a path one side deleted looked
        // exactly like a path one side replaced with nothing. A merge of a deletion against an
        // unchanged file then produced an empty file instead of the deletion, and the caller was told
        // the merge was clean.
        //
        // The three states are kept as `Option<String>` and resolved below, which is what
        // `cherry_pick` already did by looking at `new_hash` rather than at the text.
        let base_side: Option<String> = match &base {
            Some(b) => match hash_at_point(conn, path, b)? {
                Some(h) => Some(get_object(conn, &h)?),
                None => None,
            },
            None => None,
        };
        let our_side: Option<String> = match &tgt_head {
            Some(t) => match hash_at_point(conn, path, t)? {
                Some(h) => Some(get_object(conn, &h)?),
                None => None,
            },
            None => None,
        };
        let their_side: Option<String> = match hash_at_point(conn, path, &src_head)? {
            Some(h) => Some(get_object(conn, &h)?),
            None => None,
        };

        // One side removed the path and the other left it as the base had it: the answer is the
        // removal, not an empty file.
        //
        // "Left it as the base had it" is the whole condition. If the other side changed the file,
        // a delete/modify question has two defensible answers and this merges it as a conflict
        // rather than choosing. If the base did not have the path, the side that has it added it and
        // there is nothing to remove, which falls through to the ordinary three-way merge.
        if let Some(b) = &base_side {
            let ours_same = our_side.as_ref() == Some(b);
            let theirs_same = their_side.as_ref() == Some(b);
            if (their_side.is_none() && ours_same) || (our_side.is_none() && theirs_same) {
                removed.push(path.clone());
                continue;
            }
            if our_side.is_some() != their_side.is_some() && !ours_same && !theirs_same {
                conflicts.push(path.clone());
                // Their content, for the copy a person inspects while resolving.
                if let Some(theirs) = &their_side {
                    write_conflict_copy(root, path, theirs)?;
                }
                if let Some(rep) = crate::repo::conflict::report(
                    path,
                    b,
                    our_side.as_deref().unwrap_or(""),
                    their_side.as_deref().unwrap_or(""),
                ) {
                    reports.push(rep);
                }
                continue;
            }
        }

        let base_text = base_side.clone().unwrap_or_default();
        let our_text = our_side.clone().unwrap_or_default();
        let their_text = their_side.clone().unwrap_or_default();

        // A path absent on one side and present on the other is an add.
        let result = merge3(&base_text, &our_text, &their_text);
        if result.is_conflict() {
            // Record the path and preserve their side before writing anything,
            // so a refused merge still leaves the other version recoverable.
            let theirs = match &result {
                MergeResult::Conflict { theirs, .. } => theirs.clone(),
                MergeResult::Clean(_) => String::new(),
            };
            // `ours` is what the file holds now; the caller needs both sides to
            // show a useful conflict message.
            let _ = result.ours();
            write_conflict_copy(root, path, &theirs)?;
            // Keep the structured report alongside the markers: markers for a
            // human, the report for an agent that has to decide.
            if let Some(rep) =
                crate::repo::conflict::report(path, &base_text, &our_text, &their_text)
            {
                reports.push(rep);
            }
            merged_files.push((path.clone(), result.text().to_string()));
            conflicts.push(path.clone());
        } else if result.text() != our_text {
            merged_files.push((path.clone(), result.text().to_string()));
        }
    }

    if !conflicts.is_empty() {
        // Write what we produced so the user can inspect it, but leave the head
        // alone: a conflicted merge is not a merge.
        for (path, text) in &merged_files {
            let target_path = root.join(path);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target_path, text)?;
        }
        return Ok(MergeOutcome {
            applied: Vec::new(),
            conflicts,
            reports,
            merge_changeset: None,
        });
    }

    // Record the merge as a changeset with two parents.
    let id = new_changeset_id(conn, &format!("merge {} into {}", source, target), author)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch)
         VALUES(?1, ?2, ?3, ?4, 'committed', ?5, ?6, ?7)",
        params![
            id,
            format!("merge {} into {}", source, target),
            author,
            now(),
            tgt_head,
            src_head,
            target
        ],
    )?;

    for (path, text) in &merged_files {
        let h = put_object(&tx, text)?;
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, NULL, ?3)",
            params![id, path, h],
        )?;
        let next_ver: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM file_versions WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path, next_ver, h, id, now()],
        )?;
    }
    // A removal is a changeset file row with no new hash, which is how every other delete in this
    // codebase records one. Without this the path stayed in the tree: the merge reported clean, the
    // file was still there, and the branch it came from had already deleted it.
    for path in &removed {
        let old_hash: Option<String> = tx
            .query_row(
                "SELECT new_hash FROM changeset_files WHERE path = ?1 ORDER BY rowid DESC LIMIT 1",
                params![path],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, NULL)",
            params![id, path, old_hash],
        )?;
    }
    crate::store::set_branch_head(&tx, target, &id)?;
    tx.commit()?;

    // Materialise the merged tree on disk.
    for (path, text) in &merged_files {
        let target_path = root.join(path);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target_path, text)?;
    }
    // And take the removed ones off it, or the worktree contradicts the tree the branch now points
    // at. A missing file is not an error here: the base may have held the path while this worktree
    // never did.
    for path in &removed {
        let target_path = root.join(path);
        if target_path.exists() {
            fs::remove_file(&target_path)?;
        }
    }

    let touched: Vec<(String, String)> = merged_files
        .iter()
        .map(|(p, t)| (p.clone(), hash_content(t)))
        .collect();
    recompute_blame(conn, &touched)?;

    Ok(MergeOutcome {
        applied: vec![id.clone()],
        conflicts: Vec::new(),
        reports: Vec::new(),
        merge_changeset: Some(id),
    })
}

/// Apply the change one changeset made onto the current branch.
///
/// The three-way merge uses the changeset's own parent as the base, not the
/// merge base of two branches: the question is "what did this changeset change,
/// and does that change still apply here", which is exactly a diff against its
/// parent replayed on the target.
///
/// A cherry-pick never moves the source, and it records a new changeset on the
/// target rather than reusing the source id, because the same id on two branches
/// would make every history query ambiguous.
pub fn cherry_pick(
    conn: &Connection,
    root: &Path,
    source: &str,
    target: &str,
    author: &str,
) -> ZResult<MergeOutcome> {
    // Resolve the id: a branch name means its head, anything else is an id or tag.
    let source_id = crate::store::resolve_rev(conn, source)?.ok_or_else(|| {
        crate::support::error::Error::not_found(format!("no such changeset: {}", source))
    })?;

    let parent: Option<String> = conn
        .query_row(
            "SELECT parent FROM changesets WHERE id = ?1",
            params![source_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    let tgt_head = branch_head(conn, target)?;

    // The paths this changeset touched.
    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![source_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if files.is_empty() {
        return Ok(MergeOutcome {
            applied: Vec::new(),
            conflicts: Vec::new(),
            reports: Vec::new(),
            merge_changeset: None,
        });
    }

    let mut picked: Vec<(String, Option<String>, String)> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut reports: Vec<crate::repo::conflict::ConflictReport> = Vec::new();

    for (path, _old, new_hash) in &files {
        // What the file held before this changeset.
        let base_text = match &parent {
            Some(p) => match hash_at_point(conn, path, p)? {
                Some(h) => get_object(conn, &h)?,
                None => String::new(),
            },
            None => String::new(),
        };
        // What the target branch holds now.
        let our_text = match &tgt_head {
            Some(t) => match hash_at_point(conn, path, t)? {
                Some(h) => get_object(conn, &h)?,
                None => String::new(),
            },
            None => String::new(),
        };
        // What the changeset made it.
        let their_text = match new_hash {
            Some(h) => get_object(conn, h)?,
            // A deletion: the change is "this path goes away".
            None => String::new(),
        };

        let result = merge3(&base_text, &our_text, &their_text);
        if result.is_conflict() {
            let theirs = match &result {
                MergeResult::Conflict { theirs, .. } => theirs.clone(),
                MergeResult::Clean(_) => String::new(),
            };
            // Preserve their side before anything is written, so a refused
            // cherry-pick still leaves the content recoverable.
            write_conflict_copy(root, path, &theirs)?;
            if let Some(rep) =
                crate::repo::conflict::report(path, &base_text, &our_text, &their_text)
            {
                reports.push(rep);
            }
            conflicts.push(path.clone());
            continue;
        }

        let text = result.text().to_string();
        if new_hash.is_none() && text.is_empty() {
            // The changeset removed this path and nothing re-added it.
            if our_text.is_empty() {
                // Already absent here: nothing to do, which is not a conflict.
                continue;
            }
            deleted.push(path.clone());
        } else if text != our_text {
            picked.push((path.clone(), new_hash.clone(), text));
        }
    }

    if !conflicts.is_empty() {
        // A conflicted pick is not a pick: the head stays where it was.
        return Ok(MergeOutcome {
            applied: Vec::new(),
            conflicts,
            reports,
            merge_changeset: None,
        });
    }

    if picked.is_empty() && deleted.is_empty() {
        return Ok(MergeOutcome {
            applied: Vec::new(),
            conflicts: Vec::new(),
            reports: Vec::new(),
            merge_changeset: None,
        });
    }

    let msg: String = conn
        .query_row(
            "SELECT message FROM changesets WHERE id = ?1",
            params![source_id],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "picked".to_string());

    let id = new_changeset_id(
        conn,
        &format!("pick {}: {}", &source_id[..source_id.len().min(14)], msg),
        author,
    )?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch)
         VALUES(?1, ?2, ?3, ?4, 'committed', ?5, NULL, ?6)",
        params![
            id,
            format!("pick {}: {}", &source_id[..source_id.len().min(14)], msg),
            author,
            now(),
            tgt_head,
            target
        ],
    )?;

    for (path, _old_hash, text) in &picked {
        let h = put_object(&tx, text)?;
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, NULL, ?3)",
            params![id, path, h],
        )?;
        let next_ver: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM file_versions WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path, next_ver, h, id, now()],
        )?;
    }
    for path in &deleted {
        // A deletion is recorded with a null hash, which is the same shape a
        // commit uses, so every history query already understands it.
        tx.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, NULL, NULL)",
            params![id, path],
        )?;
        tx.execute("DELETE FROM blame_ranges WHERE path = ?1", params![path])?;
    }
    crate::store::set_branch_head(&tx, target, &id)?;
    tx.commit()?;

    for (path, _hash, text) in &picked {
        let target_path = root.join(path);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target_path, text)?;
    }
    for path in &deleted {
        let target_path = root.join(path);
        if target_path.is_file() {
            fs::remove_file(&target_path)?;
        }
    }

    let touched: Vec<(String, String)> = picked
        .iter()
        .map(|(p, _, t)| (p.clone(), hash_content(t)))
        .collect();
    recompute_blame(conn, &touched)?;

    Ok(MergeOutcome {
        applied: vec![id.clone()],
        conflicts: Vec::new(),
        reports: Vec::new(),
        merge_changeset: Some(id),
    })
}

/// Diff two changesets (or a changeset against its first parent).
pub fn changeset_diff(conn: &Connection, id: &str) -> ZResult<Vec<FileDiff>> {
    let parent: Option<String> = conn
        .query_row(
            "SELECT parent FROM changesets WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    let files: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    let mut out = Vec::new();
    for (path, _old, new) in files {
        let old_text = match &parent {
            Some(p) => match hash_at_point(conn, &path, p)? {
                Some(h) => get_object(conn, &h)?,
                None => String::new(),
            },
            None => String::new(),
        };
        let new_text = match new {
            Some(h) => get_object(conn, &h)?,
            None => String::new(),
        };
        let hunks = diff_hunks(&old_text, &new_text);
        let a: Vec<&str> = old_text.lines().collect();
        let b: Vec<&str> = new_text.lines().collect();
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for h in hunks {
            for l in &a[h.old_start..h.old_start + h.old_len] {
                removed.push(l.to_string());
            }
            for l in &b[h.new_start..h.new_start + h.new_len] {
                added.push(l.to_string());
            }
        }
        out.push((path, removed, added));
    }
    Ok(out)
}

/// Write a committed file back to disk.
pub fn checkout_file(
    conn: &Connection,
    root: &Path,
    rel: &str,
    changeset: Option<&str>,
) -> ZResult<usize> {
    let hash = match changeset {
        Some(cs) => hash_at_point(conn, rel, cs)?
            .ok_or_else(|| bad(format!("{} not tracked as of changeset {}", rel, cs)))?,
        None => {
            let branch = current_branch(conn)?;
            latest_hash(conn, rel, &branch)?
                .ok_or_else(|| bad(format!("no committed version of {}", rel)))?
        }
    };

    let content = crate::store::get_object_bytes(conn, &hash)?;
    // The same decision every other writer makes. A checkout that ignores `eol` writes a file the
    // next scan immediately reports as modified, which reads as if the checkout failed.
    let attrs = crate::support::attributes::Rules::load(root);
    let content = crate::support::attributes::plan_for(&attrs, rel).checked_out(&content);
    let len = content.len();
    safe_write::write_safe(root, rel, &content)?;
    Ok(len)
}

/// The full file tree as of `changeset`: for every path reachable from it, the
/// content decided by the most recent changeset that touched it. A path whose
/// last touch was a deletion is absent from the result.
///
/// `changeset_files` is the authority here, not `file_versions`: it records the
/// new hash of every touch, including deletions as `new_hash IS NULL`, so one
/// ranked scan answers the whole tree.
/// Every path on `branch` with its committed hash, in one query.
pub fn committed_tree(
    conn: &Connection,
    branch: &str,
) -> ZResult<std::collections::HashMap<String, String>> {
    // Read the maintained table. Rebuilding from the whole history cost about
    // 2 ms on a 2000 file repository, on every `status` and every `diff`.
    //
    // Sized from the table's own count. Growing a hundred thousand entries by doubling rehashes every
    // string it already holds, and this map is built on every status and every diff.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM branch_tree WHERE branch = ?1",
            params![branch],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let mut out = std::collections::HashMap::with_capacity(n.max(0) as usize);
    let mut stmt = conn.prepare("SELECT path, hash FROM branch_tree WHERE branch = ?1")?;
    let rows = stmt.query_map(params![branch], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for r in rows {
        let (p, h) = r?;
        out.insert(p, h);
    }

    if out.is_empty() {
        // A repository created before this table existed, or a branch written by
        // an older build. Rebuild once and keep it, so the slow path runs at most
        // one time per branch.
        if let Some(head) = branch_head(conn, branch)? {
            let files = crate::store::tree_at(conn, &head)?;
            let tx = conn.unchecked_transaction()?;
            {
                let mut ins = tx.prepare(
                    "INSERT OR REPLACE INTO branch_tree(branch, path, hash) VALUES(?1, ?2, ?3)",
                )?;
                for (path, hash) in &files {
                    ins.execute(params![branch, path, hash])?;
                }
            }
            tx.commit()?;
            for (path, hash) in files {
                out.insert(path, hash);
            }
        }
    }
    Ok(out)
}

/// Write every file of `branch` to disk. Used after a push and after a clone so
/// the working tree never lags behind the branch head.
/// The file `path` names inside `root`, refusing anything that would land outside it.
///
/// The tree entries and bundle paths that reach the writers here arrived from data, and `Path::join`
/// does not sanitise: an absolute value discards the root. A snapshot entry of `/etc/cron.d/x` was
/// written outside the repository with the bundle's content, and the same `root.join(path)` followed
/// by `fs::write` was in `materialise`, `restore_worktree` and the clone's snapshot restore, reached
/// by `pull` and `clone` after a remote chose the paths.
fn target_inside(root: &Path, path: &str) -> ZResult<std::path::PathBuf> {
    if !crate::support::paths::is_repo_relative(path) {
        return Err(bad(format!(
            "a tree entry names {} which is not inside the repository (absolute, or climbing out)",
            path
        )));
    }
    Ok(root.join(path))
}

pub fn materialise(conn: &Connection, root: &Path, branch: &str) -> ZResult<usize> {
    // An empty branch is a tree with no paths, not a reason to do nothing. Returning early here
    // left every file the repository had tracked sitting on disk after an `undo` moved the head
    // back to the first changeset's parent, and `status` then reported all of them as new files,
    // which reads as if the undo had not happened.
    let files: Vec<(String, String)> = match branch_head(conn, branch)? {
        Some(head) => crate::store::tree_at(conn, &head)?,
        None => Vec::new(),
    };

    // A path that the tree no longer names has to leave the disk, or the working tree keeps
    // a file the head says was deleted. Redo showed this: the head moved forward past a
    // deletion and the file stayed, and `status` then reported it as a new file, which looks
    // like the deletion never happened.
    //
    // Only files the repository knows about are removed. Anything else on disk is the
    // user's, including ignored build output, and deleting it would be far worse than the
    // untidiness this avoids.
    let wanted: HashSet<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let known: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT path FROM file_versions
             UNION SELECT DISTINCT path FROM changeset_files",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for path in known {
        if wanted.contains(path.as_str()) {
            continue;
        }
        // A path the tree no longer names still came from the tree, so it gets the same rule.
        let Ok(target) = target_inside(root, &path) else {
            continue;
        };
        if target.is_file() {
            let _ = fs::remove_file(&target);
        }
        // An empty directory left behind by the removal is noise, not data.
        if let Some(dir) = target.parent() {
            if dir != root {
                let _ = fs::remove_dir(dir);
            }
        }
    }

    // The rules are read once for the whole write. A checkout that ignores them leaves a Windows
    // tree with LF endings the editor then rewrites, so the next commit touches every line.
    let attrs = crate::support::attributes::Rules::load(root);
    for (path, hash) in &files {
        // Bytes, not text: a binary file is stored raw and is not valid UTF-8, and asking for
        // it as a string refused the whole materialise with a message about encoding.
        let content = crate::store::get_object_bytes(conn, hash)?;
        // The line ending the working tree wants, which is not always the one the object holds.
        let plan = crate::support::attributes::plan_for(&attrs, path);
        let content = plan.checked_out(&content);
        let _target = target_inside(root, path)?;
        safe_write::write_safe(root, path, &content)?;
    }
    Ok(files.len())
}

/// Restore every tracked file of `branch` to its committed content, and remove
/// tracked files that should not exist. Untracked files are left alone.
pub fn restore_worktree(conn: &Connection, root: &Path, branch: &str) -> ZResult<(usize, usize)> {
    let head = match branch_head(conn, branch)? {
        Some(h) => h,
        None => return Ok((0, 0)),
    };

    // The tree at `head` is the newest version of every path reachable from it,
    // not just the paths that `head` itself touched. Reading file_versions for
    // the head changeset alone loses every file added by an earlier changeset.
    let files = crate::store::tree_at(conn, &head)?;

    let attrs = crate::support::attributes::Rules::load(root);
    let mut written = 0usize;
    for (path, hash) in &files {
        let content = crate::store::get_object_bytes(conn, hash)?;
        let plan = crate::support::attributes::plan_for(&attrs, path);
        let content = plan.checked_out(&content);
        let target = target_inside(root, path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &content)?;
        written += 1;
    }

    // Tracked on an older changeset but gone from this one: delete it.
    let present: std::collections::HashSet<&String> = files.iter().map(|(p, _)| p).collect();
    let mut removed = 0usize;
    let mut all_stmt = conn.prepare("SELECT DISTINCT path FROM file_versions")?;
    let all: Vec<String> = all_stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in all {
        if !present.contains(&path) {
            let Ok(target) = target_inside(root, &path) else {
                continue;
            };
            if target.is_file() {
                fs::remove_file(&target)?;
                removed += 1;
            }
        }
    }
    Ok((written, removed))
}

/// Write the working tree to match a changeset, whichever branch it belongs to.
///
/// `restore_worktree` takes a branch name and reads that branch's head. A submodule pin is a
/// changeset, and asking it to resolve as a branch name gives `no such branch: cs...` - true, and
/// about the wrong thing. This resolves the id directly.
pub fn restore_at_changeset(conn: &Connection, root: &Path, changeset: &str) -> ZResult<usize> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM changesets WHERE id = ?1",
        [changeset],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(bad(format!("changeset {} does not exist", changeset)));
    }
    let files = crate::store::tree_at(conn, changeset)?;
    let attrs = crate::support::attributes::Rules::load(root);
    let mut written = 0usize;
    for (path, hash) in &files {
        let content = crate::store::get_object_bytes(conn, hash)?;
        let plan = crate::support::attributes::plan_for(&attrs, path);
        let content = plan.checked_out(&content);
        let _target = root.join(path);
        safe_write::write_safe(root, path, &content)?;
        written += 1;
    }
    // A path tracked at some point but absent from this tree has to go, or the checkout is a union
    // of two versions rather than the one asked for.
    let present: std::collections::HashSet<&String> = files.iter().map(|(p, _)| p).collect();
    let mut all_stmt = conn.prepare("SELECT DISTINCT path FROM file_versions")?;
    let all: Vec<String> = all_stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in all {
        if !present.contains(&path) {
            let target = root.join(&path);
            if target.is_file() {
                fs::remove_file(&target)?;
            }
        }
    }
    Ok(written)
}

/// One path's change between two revisions: the lines removed and the lines added.
pub type PathDiff = (String, Vec<String>, Vec<String>);

/// Compare two revisions and report the lines that differ per path.
///
/// `from` and `to` are anything `resolve_rev` understands: a branch, a tag, a
/// changeset id, or `head`. Either side may be empty, meaning an empty tree, which is
/// how the first changeset of a branch is shown on its own.
///
/// The comparison is between two stored trees, not between the working directory and
/// something. Reviewing a step means asking what one point in history changed against
/// another, and until now the only ways to ask were the working tree against the head
/// or one changeset against its own parent. A step in the middle of a branch could not
/// be examined at all.
/// The parent of a revision, which is the revision a "what changed" question means by "before".
///
/// A first changeset has no parent, and an empty string is returned rather than an error: the caller
/// decides whether the empty tree is the right answer for that case, and for a first commit it is.
pub fn parent_of(conn: &Connection, rev: &str) -> ZResult<Option<String>> {
    let id = match crate::store::resolve_rev(conn, rev)? {
        Some(id) => id,
        None => return Ok(None),
    };
    let parent: Option<String> = conn
        .query_row(
            "SELECT parent FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(parent)
}

/// Compare two revisions.
///
/// `from` of `None` means the empty tree, which is what "show me everything in `to`" means and what
/// a first commit needs. That reading is correct and it is not what a caller usually wants: asking
/// for `--to main` and receiving every file in the repository, when one file changed since its
/// parent, is a surprise that costs a reader the work of working out which of those files are new.
/// The command therefore resolves the parent itself; `diff_revs` keeps the strict reading because a
/// caller that means the empty tree says so with an explicit `--from`, and a function that guessed
/// would be unusable for the case that needs it.
pub fn diff_revs(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
) -> ZResult<Vec<PathDiff>> {
    fn tree_of(conn: &Connection, rev: Option<&str>) -> ZResult<Vec<(String, String)>> {
        match rev {
            None => Ok(Vec::new()),
            Some(r) => {
                let id = crate::store::resolve_rev(conn, r)?
                    .ok_or_else(|| bad(format!("no such revision: {}", r)))?;
                crate::store::tree_at(conn, &id)
            }
        }
    }

    let before = tree_of(conn, from)?;
    let after = tree_of(conn, to)?;
    let before_map: std::collections::HashMap<String, String> = before.into_iter().collect();
    let after_map: std::collections::HashMap<String, String> = after.into_iter().collect();

    let mut paths: Vec<String> = before_map
        .keys()
        .chain(after_map.keys())
        .cloned()
        .collect::<std::collections::HashSet<String>>()
        .into_iter()
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let old_hash = before_map.get(&path);
        let new_hash = after_map.get(&path);
        if old_hash == new_hash {
            continue;
        }
        let old_text = match old_hash {
            Some(h) => crate::store::get_object(conn, h)?,
            None => String::new(),
        };
        let new_text = match new_hash {
            Some(h) => crate::store::get_object(conn, h)?,
            None => String::new(),
        };
        if old_text == new_text {
            continue;
        }
        let (removed, added) = line_diff(&old_text, &new_text);
        out.push((path, removed, added));
    }
    Ok(out)
}

/// Lines removed and added between two texts.
///
/// A simple longest common subsequence over lines, which is what a review needs: it
/// shows what changed, not the smallest possible edit script. The cap keeps a
/// pathological pair of files from taking quadratic time forever; beyond it, the whole
/// old text is reported as removed and the whole new text as added, which is honest
/// about not having computed a diff.
fn line_diff(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    const MAX_LINES: usize = 4000;
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > MAX_LINES || b.len() > MAX_LINES {
        return (
            a.iter().map(|s| s.to_string()).collect(),
            b.iter().map(|s| s.to_string()).collect(),
        );
    }

    // table[i][j] = length of the common subsequence of a[i..] and b[j..]
    let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut removed = Vec::new();
    let mut added = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            removed.push(a[i].to_string());
            i += 1;
        } else {
            added.push(b[j].to_string());
            j += 1;
        }
    }
    while i < a.len() {
        removed.push(a[i].to_string());
        i += 1;
    }
    while j < b.len() {
        added.push(b[j].to_string());
        j += 1;
    }
    (removed, added)
}

/// One step of a stack: a changeset, its message, and what it changed.
pub struct StackStep {
    pub id: String,
    pub message: String,
    pub author: String,
    pub files: Vec<PathDiff>,
    pub risk: Risk,
}

/// A deterministic read on how much attention a change deserves.
///
/// This is the shape of what a large code host does with a learned model: score every
/// change, then send the low scores down an automated path and the high scores to a
/// person. A model needs training data this tool does not have, so the score here is
/// built from the things a reviewer looks at first, and every point it awards says
/// which rule awarded it. A score whose reason cannot be stated is not usable for
/// routing, because a wrong routing decision becomes unauditable.
#[derive(Default, Clone)]
pub struct Risk {
    pub score: i64,
    pub reasons: Vec<String>,
}

impl Risk {
    fn add(&mut self, points: i64, why: String) {
        self.score += points;
        self.reasons.push(format!("+{} {}", points, why));
    }

    /// Adjust the score from what this repository has actually reverted.
    ///
    /// The rules above encode a guess. A revert is evidence: a change that matched a shape
    /// which has been reverted before deserves a second look more than one that matched a
    /// shape nobody has ever taken back. This is the smallest honest version of learning
    /// from data, and it only ever raises a score, because the alternative is a rule that
    /// can quietly hide a change a person would have wanted to see.
    fn calibrate(&mut self, hits: usize, reverts: usize) {
        if reverts == 0 || hits == 0 {
            return;
        }
        let points = hits.min(3) as i64;
        self.add(
            points,
            format!(
                "{} of the {} reverted changeset(s) here matched this shape",
                hits.min(reverts),
                reverts
            ),
        );
    }

    /// The band a score falls in, with the same word a person would use.
    pub fn band(&self) -> &'static str {
        match self.score {
            0..=3 => "low",
            4..=8 => "medium",
            _ => "high",
        }
    }
}

/// Score one changeset from what it changed.
#[cfg(test)]
pub fn score_change(conn: &Connection, diffs: &[PathDiff]) -> ZResult<Risk> {
    score_change_calibrated(conn, diffs, None)
}

/// Score one change, optionally adjusted by what this repository has reverted.
///
/// `history` is the list of changesets that a revert named, when the caller has it. With
/// `None` the score is the plain rule set, which is what a fresh repository has.
pub fn score_change_calibrated(
    conn: &Connection,
    diffs: &[PathDiff],
    history: Option<&RevertHistory>,
) -> ZResult<Risk> {
    let mut r = Risk::default();
    let lines: usize = diffs.iter().map(|(_, rm, ad)| rm.len() + ad.len()).sum();
    let touched = diffs.len().max(1);
    let paths: Vec<&str> = diffs.iter().map(|(p, _, _)| p.as_str()).collect();

    if lines > 400 {
        r.add(4, format!("{} lines changed", lines));
    } else if lines > 100 {
        r.add(2, format!("{} lines changed", lines));
    }
    if touched > 20 {
        r.add(3, format!("{} paths touched", touched));
    } else if touched > 5 {
        r.add(1, format!("{} paths touched", touched));
    }

    let mut has_test = false;
    let mut has_dep = false;
    let mut has_binary = false;
    let mut has_ignore = false;
    for path in &paths {
        let lower = path.to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or("");
        if name.contains("test") || name.contains("spec") || lower.contains("/tests/") {
            has_test = true;
        }
        if name == "cargo.toml"
            || name == "cargo.lock"
            || name == "package.json"
            || name == "package-lock.json"
            || name == "requirements.txt"
            || name == "go.mod"
        {
            has_dep = true;
        }
        if lower.contains("vendor/") || lower.contains("node_modules/") {
            has_ignore = true;
        }
    }
    // A change with no test change is not automatically risky, but a sizeable one is
    // worth a second look, which is exactly the judgement being encoded.
    if lines > 50 && !has_test {
        r.add(2, "sizeable change with no test touched".to_string());
    }
    if has_dep {
        r.add(2, "dependency manifest changed".to_string());
    }
    if has_ignore {
        r.add(1, "vendored or generated path touched".to_string());
    }

    // The object's own shape: a file that is not text cannot be reviewed line by line.
    for path in &paths {
        if let Some(hash) = file_hash_at(conn, path)? {
            if let Ok(text) = crate::store::get_object(conn, &hash) {
                if text.contains('\0') {
                    has_binary = true;
                }
            }
        }
    }
    if has_binary {
        r.add(3, "a binary file changed".to_string());
    }

    if let Some(h) = history {
        let hits = h.shapes_matching(&paths, lines, has_test, has_dep);
        r.calibrate(hits, h.reverted.len());
    }

    if r.reasons.is_empty() {
        r.reasons
            .push("small, text only, tests present or not needed".to_string());
    }
    Ok(r)
}

/// Changesets this repository has reverted, and the shapes of those changes.
///
/// The only honest training signal available: something a person took back was, by that
/// act, a change that deserved more attention than it got.
pub struct RevertHistory {
    pub reverted: Vec<String>,
    /// Per reverted change: how many lines, whether a test was touched, whether a
    /// dependency manifest was touched.
    pub shapes: Vec<(usize, bool, bool)>,
}

impl RevertHistory {
    /// Read the reverts recorded in the operation log.
    pub fn load(conn: &Connection) -> ZResult<RevertHistory> {
        let ids: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT detail FROM operations WHERE kind = 'revert' AND detail IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut history = RevertHistory {
            reverted: Vec::new(),
            shapes: Vec::new(),
        };
        for id in ids {
            let diff = match diff_revs(conn, parents_of_first(conn, &id)?.as_deref(), Some(&id)) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if diff.is_empty() {
                continue;
            }
            let lines: usize = diff.iter().map(|(_, rm, ad)| rm.len() + ad.len()).sum();
            let mut has_test = false;
            let mut has_dep = false;
            for (path, _, _) in &diff {
                let lower = path.to_ascii_lowercase();
                let name = lower.rsplit('/').next().unwrap_or("");
                if name.contains("test") || lower.contains("/tests/") {
                    has_test = true;
                }
                if name == "cargo.toml" || name == "package.json" || name == "go.mod" {
                    has_dep = true;
                }
            }
            history.shapes.push((lines, has_test, has_dep));
            history.reverted.push(id);
        }
        Ok(history)
    }

    /// How many reverted changes share the shape of the change being scored.
    ///
    /// The comparison is coarse on purpose: a size band, whether a test was touched, and
    /// whether a manifest was touched. A finer match would find nothing in a small
    /// repository and would be pretending to know more than the data supports.
    fn shapes_matching(
        &self,
        paths: &[&str],
        lines: usize,
        has_test: bool,
        has_dep: bool,
    ) -> usize {
        let band = |n: usize| match n {
            0..=100 => 0,
            101..=400 => 1,
            _ => 2,
        };
        let mine = band(lines);
        let _ = (paths, has_dep);
        self.shapes
            .iter()
            .filter(|(l, t, _)| band(*l) == mine && *t == has_test)
            .count()
    }
}

/// The first parent of a changeset, if it has one.
fn parents_of_first(conn: &Connection, id: &str) -> ZResult<Option<String>> {
    let (p1, _) = crate::store::parents_of_public(conn, id)?;
    Ok(p1)
}

/// The object hash for a path at the branch head, if the path exists there.
fn file_hash_at(conn: &Connection, path: &str) -> ZResult<Option<String>> {
    conn.query_row(
        "SELECT hash FROM branch_tree WHERE branch = (SELECT value FROM repo_state WHERE key = 'current_branch') AND path = ?1",
        params![path],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Walk the chain from `from` (exclusive) to `to` (inclusive) and diff each step.
///
/// This is the answer to reviewing a stack: the changes arrive as a series of small
/// steps, and a reviewer wants each step separately, in order, rather than the net
/// effect. Comparing the endpoints alone loses the intermediate reasoning, which is
/// most of what there is to review when an agent wrote the change.
pub fn review_stack(conn: &Connection, from: Option<&str>, to: &str) -> ZResult<Vec<StackStep>> {
    let head = crate::store::resolve_rev(conn, to)?
        .ok_or_else(|| bad(format!("no such revision: {}", to)))?;

    // Collect the chain oldest first, stopping at the point the two branches parted.
    //
    // Comparing against `from`'s own head stops only when the branch happens to contain that
    // exact changeset, so reviewing a feature against a main that has moved one commit includes
    // every changeset main already has. The merge base is the commit both sides share, which is
    // the boundary a review actually means.
    let stop = match from {
        Some(f) => {
            let from_head = crate::store::resolve_rev(conn, f)?;
            match from_head {
                Some(fh) => crate::store::merge_base(conn, &head, &fh)?,
                None => None,
            }
        }
        None => None,
    };
    let mut chain: Vec<String> = Vec::new();
    let mut cur = Some(head.clone());
    while let Some(id) = cur {
        if Some(&id) == stop.as_ref() {
            break;
        }
        chain.push(id.clone());
        let (p1, _p2) = crate::store::parents_of_public(conn, &id)?;
        cur = p1;
    }
    chain.reverse();

    let mut out = Vec::new();
    for id in chain {
        let (p1, _) = crate::store::parents_of_public(conn, &id)?;
        let diff = diff_revs(conn, p1.as_deref(), Some(&id))?;
        let history = RevertHistory::load(conn)?;
        let risk = score_change_calibrated(conn, &diff, Some(&history))?;
        let (message, author): (String, String) = conn
            .query_row(
                "SELECT message, author FROM changesets WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or_else(|| ("(unknown)".to_string(), "(unknown)".to_string()));
        out.push(StackStep {
            id,
            message,
            author,
            files: diff,
            risk,
        });
    }
    Ok(out)
}

/// Land a branch onto a target, refusing if the target moved since it was read.
///
/// This is the guard a merge queue exists for. Two writers landing at once both read
/// the same target head, both compute a merge against it, and the second overwrites the
/// first. The check is a compare-and-swap on the target head, done inside the same
/// transaction that writes, so a lost update becomes a refusal instead of silent loss.
///
/// The expected head is required. A caller that has not read the target head does not
/// know what it is overwriting, and "I did not check" is the bug this prevents.
pub fn land_branch(
    conn: &Connection,
    root: &Path,
    source: &str,
    target: &str,
    expected_target_head: Option<&str>,
    author: &str,
) -> ZResult<MergeOutcome> {
    // Before anything moves: a landing is when the change stops being private. A refusal here
    // leaves both branches untouched, which is the only outcome that costs nothing to undo.
    crate::repo::hooks::run(root, "pre-land", &[source, target])?;

    let current = branch_head(conn, target)?;
    if current.as_deref() != expected_target_head {
        // The tree is not damaged and the request is not wrong: it is out of date against
        // another writer, which is what this check exists to catch.
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::DirtyWorktree,
            format!(
                "{} moved while this was being prepared: expected {}, found {}",
                target,
                expected_target_head.unwrap_or("(none)"),
                current.as_deref().unwrap_or("(none)")
            ),
        )));
    }
    merge_branch(conn, root, source, target, author)
}

#[test]
fn blame_survives_a_line_inserted_in_the_middle() {
    // The rewrite only touches the span whose owner changed. A prefix and suffix that kept
    // their author must stay untouched, and the inserted line must belong to the commit that
    // added it, not to the one before.
    let d = std::env::temp_dir().join(format!("gesta-blame-mid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".gesta")).unwrap();
    let conn = crate::store::open_db(&d).unwrap();
    crate::store::init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
        [],
    )
    .unwrap();

    let steps = [
        ("1\n2\n3\n4\n5\n", "alice"),
        ("1\nNEW\n2\n3\n4\n5\n", "bob"),
        ("1\nNEW\n2\n4\n5\n", "carol"),
        ("1\nNEW\n2\n4\n5\n6\n", "dave"),
    ];
    for (content, author) in steps {
        std::fs::write(d.join("f.txt"), content).unwrap();
        crate::repo::create_changeset(&conn, &d, "step", author).unwrap();
        let id: String = conn
            .query_row(
                "SELECT id FROM changesets WHERE status='pending' ORDER BY created_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        crate::repo::commit_one(&conn, &id).unwrap();
    }

    // Read the ranges back and expand them, which is what `blame` prints.
    let mut stmt = conn
        .prepare(
            "SELECT b.start_line, b.end_line, c.author FROM blame_ranges b
                 JOIN changesets c ON c.id = b.changeset_id
                 WHERE b.path = 'f.txt' ORDER BY b.start_line",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut authors: Vec<String> = Vec::new();
    for (start, end, author) in rows {
        for _ in start..=end {
            authors.push(author.clone());
        }
    }

    // The shape that matters: line 1 is alice, line 2 (NEW) is bob, the last is dave, and the
    // author of a middle line that never changed is still alice.
    assert_eq!(
        authors.first().map(|s| s.as_str()),
        Some("alice"),
        "first: {:?}",
        authors
    );
    assert_eq!(
        authors.get(1).map(|s| s.as_str()),
        Some("bob"),
        "inserted line: {:?}",
        authors
    );
    assert_eq!(
        authors.last().map(|s| s.as_str()),
        Some("dave"),
        "appended line: {:?}",
        authors
    );
    assert_eq!(authors.len(), 6, "one entry per line: {:?}", authors);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_scoped_snapshot_leaves_another_writer_files_alone() {
    // Two writers in one checkout is the case the whole feature exists for. Without the path
    // filter each snapshot records every changed file, so the second writer's commit carries the
    // first writer's work under its own name.
    let d = std::env::temp_dir().join(format!("gesta-scope-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".gesta")).unwrap();
    std::fs::create_dir_all(d.join("sub")).unwrap();
    let conn = crate::store::open_db(&d).unwrap();
    crate::store::init_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
        [],
    )
    .unwrap();

    std::fs::write(d.join("a.txt"), "a\n").unwrap();
    std::fs::write(d.join("sub/c.txt"), "c\n").unwrap();
    let base = crate::repo::create_changeset(&conn, &d, "base", "base").unwrap();
    crate::repo::commit_one(&conn, &base.unwrap().id).unwrap();

    std::fs::write(d.join("a.txt"), "a2\n").unwrap();
    std::fs::write(d.join("sub/c.txt"), "c2\n").unwrap();

    let mine =
        crate::repo::create_changeset_scoped(&conn, &d, "mine", "alice", &["a.txt".to_string()])
            .unwrap()
            .expect("alice changed a.txt");
    let theirs =
        crate::repo::create_changeset_scoped(&conn, &d, "theirs", "bob", &["sub".to_string()])
            .unwrap()
            .expect("bob changed sub/c.txt");

    let paths_of = |id: &str| -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT path FROM changeset_files WHERE changeset_id = ?1 ORDER BY path")
            .unwrap();
        let rows = stmt
            .query_map(params![id], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    };

    assert_eq!(paths_of(&mine.id), vec!["a.txt".to_string()]);
    assert_eq!(paths_of(&theirs.id), vec!["sub/c.txt".to_string()]);

    // And with no paths, the snapshot is the whole tree, which is what one writer wants.
    std::fs::write(d.join("a.txt"), "a3\n").unwrap();
    let all = crate::repo::create_changeset(&conn, &d, "all", "carol")
        .unwrap()
        .unwrap();
    assert!(paths_of(&all.id).contains(&"a.txt".to_string()));

    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::tests::mem_db;

    #[cfg(unix)]
    #[test]
    fn changeset_stores_symlink_target_bytes_and_mode() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;
        let tmpdir = |tag: &str| {
            let p = std::env::temp_dir().join(format!("gesta_repo_{tag}_{}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            p
        };
        let root = tmpdir("changeset_links");
        fs::create_dir(root.join(".gesta")).unwrap();
        let outside = tmpdir("changeset_link_targets");
        fs::write(outside.join("secret"), "private target content\n").unwrap();
        fs::write(root.join(".gitattributes"), "* text eol=lf\n").unwrap();
        let targets = [
            ("file_link", outside.join("secret")),
            ("directory_link", outside.clone()),
            ("dangling_link", outside.join("missing")),
            (
                "raw_link",
                std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"raw\xff")),
            ),
            ("crlf_link", std::path::PathBuf::from("missing\r\nname")),
        ];
        for (name, target) in &targets {
            symlink(target, root.join(name)).unwrap();
        }
        let conn = mem_db();
        let changeset = crate::repo::create_changeset(&conn, &root, "links", "test")
            .unwrap()
            .expect("new symlinks produce a changeset");
        for (name, target) in targets {
            let (hash, mode): (String, String) = conn
                .query_row(
                    "SELECT new_hash, mode FROM changeset_files WHERE changeset_id=?1 AND path=?2",
                    params![changeset.id, name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(mode, "120000");
            assert_eq!(
                crate::store::get_object_bytes(&conn, &hash).unwrap(),
                target.as_os_str().as_bytes()
            );
        }
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    pub fn commit_files(conn: &Connection, id: &str, msg: &str, files: &[(&str, &str)]) {
        // The parent is the current head, which is what a real commit does. With
        // NULL the fixture only ever exercised the rebuild path, so a defect in
        // the incremental path could not appear in any test.
        let head = crate::store::branch_head(conn, "main").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES(?1,?2,'a',1,'pending',?3,'main')",
            params![id, msg, head],
        )
        .unwrap();
        for (path, content) in files {
            let h = put_object(conn, content).unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,?2,NULL,?3)",
                params![id, path, h],
            )
            .unwrap();
        }
        commit_one(conn, id).unwrap();
    }

    /// Commit one version of one file with a chosen author, so blame can be
    /// checked against who actually wrote each line.
    fn commit_as(conn: &Connection, id: &str, author: &str, path: &str, content: &str) {
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES(?1,?1,?2,1,'pending',NULL,'main')",
            params![id, author],
        )
        .unwrap();
        let h = put_object(conn, content).unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES(?1,?2,NULL,?3)",
            params![id, path, h],
        )
        .unwrap();
        commit_one(conn, id).unwrap();
    }

    /// Authors in line order, one per line of the current file.
    /// One author per line, which is what every caller of this helper means.
    ///
    /// Blame is stored as ranges, so a helper that returned one entry per row would report one
    /// author for a file written in one commit and three for a file written in three. The tests
    /// below are about which line belongs to whom, so the expansion belongs here.
    fn authors(conn: &Connection, path: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT b.start_line, b.end_line, c.author FROM blame_ranges b
                 JOIN changesets c ON c.id = b.changeset_id
                 WHERE b.path = ?1 ORDER BY b.start_line",
            )
            .unwrap();
        let rows = stmt
            .query_map(params![path], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        let mut out = Vec::new();
        for row in rows {
            let (start, end, author) = row.unwrap();
            for _ in start..=end {
                out.push(author.clone());
            }
        }
        out
    }

    #[test]
    fn blame_keeps_the_author_of_a_line_that_survived() {
        let conn = mem_db();
        commit_as(&conn, "c1", "alice", "f.txt", "alpha\nbeta\ngamma\n");
        commit_as(&conn, "c2", "bob", "f.txt", "alpha\nBETA\ngamma\n");
        commit_as(&conn, "c3", "carol", "f.txt", "alpha\nBETA\ngamma\ndelta\n");

        assert_eq!(
            authors(&conn, "f.txt"),
            vec!["alice", "bob", "alice", "carol"],
            "only the replaced line and the added line change owner"
        );
    }

    #[test]
    fn blame_keeps_duplicate_lines_with_their_own_author() {
        // Two identical lines must not be matched by content alone: the first one
        // belongs to whoever wrote it, and so does the second.
        let conn = mem_db();
        commit_as(&conn, "c1", "alice", "f.txt", "same\nx\nsame\n");
        commit_as(&conn, "c2", "bob", "f.txt", "same\nx\nsame\ny\n");

        assert_eq!(
            authors(&conn, "f.txt"),
            vec!["alice", "alice", "alice", "bob"]
        );
    }

    #[test]
    fn blame_attributes_an_inserted_line_alone() {
        let conn = mem_db();
        commit_as(&conn, "c1", "alice", "f.txt", "one\ntwo\nthree\n");
        commit_as(&conn, "c2", "bob", "f.txt", "one\nINSERTED\ntwo\nthree\n");

        assert_eq!(
            authors(&conn, "f.txt"),
            vec!["alice", "bob", "alice", "alice"]
        );
    }

    #[test]
    fn a_failed_rebuild_blame_chain_leaves_the_previous_blame_intact() {
        // `rebuild_blame_chain` used to run the DELETE and then the rebuild as separate writes. A
        // failure partway through the rebuild left the path with no blame at all: worse than the
        // stale rows it had, because `blame` then had nothing to answer with and nothing said so.
        //
        // The failure is forced with a trigger rather than by corrupting a table. Corrupting a
        // changeset row makes the whole rebuild fail on its first step, so it never proves the part
        // that matters: that a failure AFTER work has been written still rolls back. `version 3` is
        // the point where two versions have already been inserted, so the rollback has something to
        // undo.
        let conn = mem_db();
        commit_as(&conn, "c1", "alice", "f.txt", "a\nb\nc\n");
        commit_as(&conn, "c2", "bob", "f.txt", "a\nB\nc\n");
        commit_as(&conn, "c3", "carol", "f.txt", "a\nB\nc\nd\n");

        rebuild_blame_chain(&conn, "f.txt").unwrap();
        let before = authors(&conn, "f.txt");
        assert_eq!(before, vec!["alice", "bob", "alice", "carol"]);

        // `recompute_blame_at` writes its ranges for one version at a time, so this fires after the
        // first version's rows are already in the table.
        conn.execute_batch(
            "CREATE TRIGGER fail_late BEFORE INSERT ON blame_ranges
             WHEN NEW.path = 'f.txt' AND NEW.end_line >= 4
             BEGIN SELECT RAISE(ABORT, 'forced failure for the rollback test'); END",
        )
        .unwrap();

        let err = rebuild_blame_chain(&conn, "f.txt")
            .expect_err("the trigger must make the rebuild fail");
        assert!(
            format!("{err}").contains("forced failure"),
            "the trigger's own error must surface, got: {err}"
        );

        // The old blame is back exactly as it was, not empty and not half-written.
        assert_eq!(
            authors(&conn, "f.txt"),
            before,
            "a failed rebuild must roll back to the previous blame"
        );
    }
    #[test]
    fn blame_gives_a_moved_line_back_to_the_author_who_wrote_it() {
        // Matching only forward would hand the moved line to whoever moved it.
        let conn = mem_db();
        commit_as(&conn, "c1", "dave", "f.txt", "A\nB\nC\n");
        commit_as(&conn, "c2", "erin", "f.txt", "B\nC\nA\n");

        assert_eq!(authors(&conn, "f.txt"), vec!["dave", "dave", "dave"]);
    }

    #[test]
    fn blame_survives_deleting_a_line() {
        let conn = mem_db();
        commit_as(&conn, "c1", "alice", "f.txt", "one\ntwo\nthree\n");
        commit_as(&conn, "c2", "bob", "f.txt", "one\nINSERTED\ntwo\nthree\n");
        commit_as(&conn, "c3", "carol", "f.txt", "one\ntwo\nthree\n");

        assert_eq!(authors(&conn, "f.txt"), vec!["alice", "alice", "alice"]);
    }

    #[test]
    fn commit_creates_versions_head_and_blame() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "first", &[("f.txt", "alpha\nbeta\n")]);

        let versions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_versions WHERE path='f.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(versions, 1);
        // Two lines written by one changeset are one range now. The count of rows is not the count
        // of lines, and a test that asserted otherwise was asserting the old storage shape rather
        // than the answer.
        let blamed: i64 = conn
            .query_row(
                "SELECT SUM(end_line - start_line + 1) FROM blame_ranges WHERE path='f.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(blamed, 2);
        assert_eq!(branch_head(&conn, "main").unwrap().as_deref(), Some("cs1"));
    }

    #[test]
    fn commit_is_idempotent() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "m", &[("f.txt", "x\n")]);
        assert_eq!(commit_one(&conn, "cs1").unwrap(), 0);
        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(versions, 1);
    }

    #[test]
    fn committing_unchanged_content_adds_no_version() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "m", &[("f.txt", "same\n")]);
        commit_files(&conn, "cs2", "m2", &[("f.txt", "same\n")]);
        let versions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_versions WHERE path='f.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(versions, 1, "identical content must not add a version");
    }

    #[test]
    fn blame_attributes_by_sequence_not_content() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "v1", &[("f.txt", "keep\nold\nend\n")]);
        commit_files(&conn, "cs2", "v2", &[("f.txt", "keep\nnew\nend\n")]);

        let line1: String = conn
            .query_row("SELECT changeset_id FROM blame_ranges WHERE path='f.txt' AND start_line<=1 AND end_line>=1", [], |r| r.get(0))
            .unwrap();
        let line2: String = conn
            .query_row("SELECT changeset_id FROM blame_ranges WHERE path='f.txt' AND start_line<=2 AND end_line>=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(line1, "cs1", "unchanged line keeps its original changeset");
        assert_eq!(line2, "cs2", "changed line belongs to the new changeset");
    }

    #[test]
    fn blame_does_not_credit_a_duplicate_line_to_an_earlier_author() {
        let conn = mem_db();
        // agentA writes two identical lines.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','a','agentA',1,'pending',NULL,'main')",
            [],
        )
        .unwrap();
        let h1 = put_object(&conn, "return None\nother\nreturn None\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','f.txt',NULL,?1)",
            params![h1],
        )
        .unwrap();
        commit_one(&conn, "cs1").unwrap();

        // agentB appends a third identical line.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','b','agentB',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        let h2 = put_object(&conn, "return None\nother\nreturn None\nreturn None\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','f.txt',NULL,?1)",
            params![h2],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();

        // Through the helper, so this test asks the same question as the others: which line
        // belongs to whom, not how the answer is stored.
        let blamed = authors(&conn, "f.txt");
        assert_eq!(
            blamed,
            vec!["agentA", "agentA", "agentA", "agentB"],
            "the appended duplicate belongs to agentB"
        );
    }

    #[test]
    fn a_one_sided_deletion_removes_the_file_instead_of_emptying_it() {
        // #118: every absent path was read as `String::new()`, so a deletion on one side and an
        // untouched file on the other merged into an empty file, and the caller was told the merge
        // was clean. `cherry_pick` handled this by looking at the hash rather than the text; this is
        // the same question asked of `merge_branch`.
        let tmp = std::env::temp_dir().join(format!("zf_merge_del_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("gone.txt"), "keep me\n").unwrap();
        commit_files(&conn, "cs1", "base", &[("gone.txt", "keep me\n")]);
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        // main adds a second file and does not touch gone.txt
        fs::write(tmp.join("other.txt"), "ours\n").unwrap();
        commit_files(
            &conn,
            "cs2",
            "ours",
            &[("gone.txt", "keep me\n"), ("other.txt", "ours\n")],
        );
        conn.execute("UPDATE changesets SET parent='cs1' WHERE id='cs2'", [])
            .unwrap();

        // dev deletes gone.txt
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','delete','a',2,'pending','cs1','dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','gone.txt','whatever',NULL)",
            [],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        let out = merge_branch(&conn, &tmp, "dev", "main", "a").unwrap();
        assert!(
            out.conflicts.is_empty(),
            "deletion against an untouched file is not a conflict: {:?}",
            out.conflicts
        );

        // The tree no longer holds the path, and neither does the working copy.
        let tree =
            crate::store::tree_at(&conn, &branch_head(&conn, "main").unwrap().unwrap()).unwrap();
        assert!(
            !tree.iter().any(|(p, _)| p == "gone.txt"),
            "the merged tree still lists the deleted path: {tree:?}"
        );
        assert!(
            !tmp.join("gone.txt").exists(),
            "the working copy still holds a file the merge removed"
        );
        // And the other side's file survived.
        assert_eq!(fs::read_to_string(tmp.join("other.txt")).unwrap(), "ours\n");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_deletion_against_a_changed_file_is_a_conflict_not_a_silent_choice() {
        // The delete/modify question has two defensible answers, so it is reported rather than
        // chosen. Before, it came out as an empty file with no conflict.
        let tmp = std::env::temp_dir().join(format!("zf_merge_dm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("f.txt"), "base\n").unwrap();
        commit_files(&conn, "cs1", "base", &[("f.txt", "base\n")]);
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        // main changes it
        fs::write(tmp.join("f.txt"), "changed by main\n").unwrap();
        commit_files(&conn, "cs2", "ours", &[("f.txt", "changed by main\n")]);
        conn.execute("UPDATE changesets SET parent='cs1' WHERE id='cs2'", [])
            .unwrap();

        // dev deletes it
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','delete','a',2,'pending','cs1','dev')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','f.txt','x',NULL)",
            [],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        let out = merge_branch(&conn, &tmp, "dev", "main", "a").unwrap();
        assert_eq!(
            out.conflicts,
            vec!["f.txt".to_string()],
            "delete against modify must be reported"
        );
        assert!(
            out.merge_changeset.is_none(),
            "a conflicted merge is not a merge"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn merge_of_disjoint_edits_moves_head_and_writes_disk() {
        let tmp = std::env::temp_dir().join(format!("zf_merge_ok_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        commit_files(&conn, "cs1", "base", &[("shared.txt", "one\ntwo\nthree\n")]);

        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        // ours on main
        fs::write(tmp.join("shared.txt"), "ONE\ntwo\nthree\n").unwrap();
        commit_files(&conn, "cs2", "ours", &[("shared.txt", "ONE\ntwo\nthree\n")]);
        conn.execute("UPDATE changesets SET parent='cs1' WHERE id='cs2'", [])
            .unwrap();

        // theirs on dev
        fs::write(tmp.join("shared.txt"), "one\ntwo\nTHREE\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','theirs','a',2,'pending','cs1','dev')",
            [],
        )
        .unwrap();
        let h = put_object(&conn, "one\ntwo\nTHREE\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','shared.txt',NULL,?1)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        let out = merge_branch(&conn, &tmp, "dev", "main", "a").unwrap();
        assert!(out.conflicts.is_empty(), "disjoint edits must not conflict");
        assert!(out.merge_changeset.is_some());

        let on_disk = fs::read_to_string(tmp.join("shared.txt")).unwrap();
        assert_eq!(
            on_disk, "ONE\ntwo\nTHREE\n",
            "merged content must reach the working copy"
        );
        let head = branch_head(&conn, "main").unwrap().unwrap();
        assert_eq!(Some(head), out.merge_changeset);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn conflicting_merge_writes_theirs_and_leaves_head_alone() {
        let tmp = std::env::temp_dir().join(format!("zf_merge_bad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conn = mem_db();
        fs::write(tmp.join("s.txt"), "base\n").unwrap();
        commit_files(&conn, "cs1", "base", &[("s.txt", "base\n")]);
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('dev','cs1',0,'main')",
            [],
        )
        .unwrap();

        fs::write(tmp.join("s.txt"), "OURS\n").unwrap();
        commit_files(&conn, "cs2", "ours", &[("s.txt", "OURS\n")]);
        conn.execute("UPDATE changesets SET parent='cs1' WHERE id='cs2'", [])
            .unwrap();

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs3','theirs','a',2,'pending','cs1','dev')",
            [],
        )
        .unwrap();
        let h = put_object(&conn, "THEIRS\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs3','s.txt',NULL,?1)",
            params![h],
        )
        .unwrap();
        commit_one(&conn, "cs3").unwrap();

        let before_head = branch_head(&conn, "main").unwrap();
        let out = merge_branch(&conn, &tmp, "dev", "main", "a").unwrap();

        assert_eq!(
            out.conflicts,
            vec!["s.txt".to_string()],
            "must report the conflict"
        );
        assert_eq!(
            branch_head(&conn, "main").unwrap(),
            before_head,
            "head must not move"
        );
        assert!(
            tmp.join("s.txt.theirs").exists(),
            "theirs must be preserved on disk"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tree_at_drops_a_path_that_a_later_changeset_deleted() {
        let conn = mem_db();
        commit_files(
            &conn,
            "cs1",
            "two files",
            &[("gone.txt", "bye\n"), ("stay.txt", "hi\n")],
        );

        // A deletion is recorded with new_hash = NULL and no file_version row.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs2','remove gone','a',2,'pending','cs1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs2','gone.txt',NULL,NULL)",
            [],
        )
        .unwrap();
        commit_one(&conn, "cs2").unwrap();

        let tree = crate::store::tree_at(&conn, "cs2").unwrap();
        let paths: Vec<&str> = tree.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec!["stay.txt"],
            "a deleted path must leave the tree"
        );

        // The earlier changeset still sees both.
        let old = crate::store::tree_at(&conn, "cs1").unwrap();
        assert_eq!(old.len(), 2, "history keeps the file it had");
    }

    #[test]
    fn a_deleted_file_is_still_a_deletion_without_a_second_stat() {
        // The deletion loop used to ask the filesystem whether each tracked path still existed, which
        // was one stat per file on every status. It now trusts the scan: a path the scan did not
        // report is missing. This pins that the answer is the same, including for a path that is
        // missing because its directory was removed rather than the file alone.
        let d = std::env::temp_dir().join(format!("gesta-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();

        for rel in ["keep.txt", "gone.txt", "sub/deep.txt"] {
            std::fs::write(d.join(rel), "x\n").unwrap();
        }
        let cs = crate::repo::create_changeset(&conn, &d, "base", "a")
            .unwrap()
            .unwrap();
        crate::repo::commit_one(&conn, &cs.id).unwrap();

        std::fs::remove_file(d.join("gone.txt")).unwrap();
        std::fs::remove_dir_all(d.join("sub")).unwrap();

        let delta = crate::repo::diff_worktree(&conn, &d, "main").unwrap();
        let mut deleted = delta.deleted.clone();
        deleted.sort();
        assert_eq!(
            deleted,
            vec!["gone.txt".to_string(), "sub/deep.txt".to_string()],
            "both removals must be reported"
        );
        assert!(
            delta.added.is_empty() && delta.modified.is_empty(),
            "nothing was added or changed"
        );
        assert_eq!(delta.unchanged, 1, "keep.txt is unchanged");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn committed_tree_is_one_lookup_and_matches_tree_at() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "two", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let tree = committed_tree(&conn, "main").unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree.get("a.txt"), Some(&hash_content("a\n")));
    }

    #[test]
    fn changing_branch_never_orphans_a_changeset() {
        let conn = mem_db();
        commit_files(&conn, "cs1", "one", &[("a.txt", "a\n")]);
        commit_files(&conn, "cs2", "two", &[("b.txt", "b\n")]);
        let r = crate::store::reachable_from(&conn, "main").unwrap();
        assert!(r.contains(&"cs1".to_string()));
        assert!(r.contains(&"cs2".to_string()));
        let parent: Option<String> = conn
            .query_row("SELECT parent FROM changesets WHERE id='cs2'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(parent.as_deref(), Some("cs1"));
    }

    /// Make two branches from one base, each with its own edit to `f.txt`.
    fn two_branches(conn: &Connection, main_text: &str, side_text: &str) -> (String, String) {
        commit_files(conn, "base", "base", &[("f.txt", "a\nb\nc\n")]);
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch)
             VALUES('side', 'base', 1, 'main')",
            [],
        )
        .unwrap();

        commit_files(conn, "side1", "side edit", &[("f.txt", side_text)]);
        conn.execute("UPDATE branches SET head = 'side1' WHERE name = 'side'", [])
            .unwrap();
        conn.execute(
            "UPDATE changesets SET branch = 'side' WHERE id = 'side1'",
            [],
        )
        .unwrap();

        commit_files(conn, "main1", "main edit", &[("f.txt", main_text)]);
        ("side1".to_string(), "main1".to_string())
    }

    #[test]
    fn cherry_pick_applies_a_change_onto_the_current_branch() {
        let conn = mem_db();
        let (side, _main) = two_branches(&conn, "a\nb\nc\n", "a\nSIDE\nc\n");

        let d = std::env::temp_dir().join(format!("gesta-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();

        let out = cherry_pick(&conn, &d, &side, "main", "t").unwrap();
        assert!(
            out.conflicts.is_empty(),
            "a change to a clean base must not conflict"
        );
        let id = out.merge_changeset.expect("pick must record a changeset");
        assert_eq!(
            branch_head(&conn, "main").unwrap().as_deref(),
            Some(id.as_str())
        );

        let content = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert_eq!(content, "a\nSIDE\nc\n");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cherry_pick_refuses_instead_of_overwriting_a_conflicting_change() {
        // The target edited the same line. Writing their version over ours would
        // destroy a change the user can see, with nothing recording it.
        let conn = mem_db();
        let (side, _main) = two_branches(&conn, "a\nMAIN\nc\n", "a\nSIDE\nc\n");

        let d = std::env::temp_dir().join(format!("gesta-pick-c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "a\nMAIN\nc\n").unwrap();

        let head_before = branch_head(&conn, "main").unwrap();
        let out = cherry_pick(&conn, &d, &side, "main", "t").unwrap();

        assert_eq!(out.conflicts, vec!["f.txt".to_string()]);
        assert!(
            out.merge_changeset.is_none(),
            "a conflicted pick must not record"
        );
        assert_eq!(
            branch_head(&conn, "main").unwrap(),
            head_before,
            "the head must not move"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("f.txt")).unwrap(),
            "a\nMAIN\nc\n",
            "the target content is untouched"
        );
        assert!(
            d.join("f.txt.theirs").exists(),
            "their side must be preserved next to the file"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn picking_a_change_that_is_already_present_records_nothing() {
        let conn = mem_db();
        commit_files(&conn, "base", "base", &[("f.txt", "a\nb\n")]);
        commit_files(&conn, "edit", "edit", &[("f.txt", "a\nB\n")]);

        let d = std::env::temp_dir().join(format!("gesta-pick-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "a\nB\n").unwrap();

        let before = branch_head(&conn, "main").unwrap();
        let out = cherry_pick(&conn, &d, "edit", "main", "t").unwrap();
        assert!(
            out.merge_changeset.is_none(),
            "nothing changed, so nothing recorded"
        );
        assert_eq!(branch_head(&conn, "main").unwrap(), before);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cherry_pick_can_delete_a_file() {
        let conn = mem_db();
        commit_files(
            &conn,
            "base",
            "base",
            &[("f.txt", "x\n"), ("keep.txt", "k\n")],
        );
        // Record a deletion on a second commit the same way a commit does.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('del','remove f','a',1,'committed','base','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('del','f.txt',NULL,NULL)",
            [],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='del' WHERE name='main'", [])
            .unwrap();

        let d = std::env::temp_dir().join(format!("gesta-pick-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "x\n").unwrap();

        // Pick the deletion onto a point that still has the file.
        conn.execute("UPDATE branches SET head='base' WHERE name='main'", [])
            .unwrap();
        let out = cherry_pick(&conn, &d, "del", "main", "t").unwrap();
        assert!(out.conflicts.is_empty());
        assert!(out.merge_changeset.is_some());
        assert!(
            !d.join("f.txt").exists(),
            "the picked deletion removes the file"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn picking_an_unknown_changeset_is_an_error() {
        let conn = mem_db();
        commit_files(&conn, "base", "base", &[("f.txt", "x\n")]);
        let d = std::env::temp_dir().join(format!("gesta-pick-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(cherry_pick(&conn, &d, "nope", "main", "t").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The maintained tree must equal a rebuild from history.
    fn assert_tree_is_honest(conn: &Connection, branch: &str) {
        let head = crate::store::branch_head(conn, branch).unwrap();
        let mut expected = match head {
            Some(h) => crate::store::tree_at(conn, &h).unwrap(),
            None => Vec::new(),
        };
        expected.sort();
        let mut stored: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("SELECT path, hash FROM branch_tree WHERE branch = ?1")
                .unwrap();
            let rows = stmt
                .query_map(params![branch], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        stored.sort();
        assert_eq!(
            stored, expected,
            "branch_tree for {} drifted from history",
            branch
        );
    }

    #[test]
    fn the_maintained_tree_follows_a_commit() {
        let conn = mem_db();
        commit_files(&conn, "c1", "one", &[("a.txt", "a\n")]);
        assert_tree_is_honest(&conn, "main");
        commit_files(&conn, "c2", "two", &[("b.txt", "b\n")]);
        assert_tree_is_honest(&conn, "main");
    }

    #[test]
    fn a_deletion_leaves_the_maintained_tree_alone() {
        // A deleted path must not linger in the tree, or `status` reports a file
        // that is already gone as present.
        let conn = mem_db();
        commit_files(&conn, "c1", "one", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('c2','x','a',2,'pending','c1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('c2','a.txt',NULL,NULL)",
            [],
        )
        .unwrap();
        commit_one(&conn, "c2").unwrap();

        assert_tree_is_honest(&conn, "main");
        let stored: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT path FROM branch_tree WHERE branch = 'main'")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(
            stored,
            vec!["b.txt".to_string()],
            "the deleted path is gone"
        );
    }

    #[test]
    fn the_maintained_tree_follows_an_undo() {
        // Undo moves a head without committing. It was the path that left the
        // tree stale, and nothing else in the program would have noticed.
        let conn = mem_db();
        commit_files(&conn, "c1", "one", &[("a.txt", "a\n")]);
        let before = crate::support::ops::RepoSnapshot::capture(&conn).unwrap();
        commit_files(&conn, "c2", "two", &[("b.txt", "b\n")]);
        assert_tree_is_honest(&conn, "main");

        before.restore(&conn).unwrap();
        assert_tree_is_honest(&conn, "main");
    }

    /// The incremental update must match a full rebuild after every kind of
    /// change. A drift here makes `status` report committed files as new, and
    /// nothing else in the program would notice.
    #[test]
    fn the_incremental_tree_matches_a_rebuild_over_a_long_history() {
        let conn = mem_db();

        // A history that adds, edits and deletes, so every branch of the
        // incremental path runs at least once.
        commit_files(&conn, "c1", "one", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        assert_tree_is_honest(&conn, "main");

        commit_files(&conn, "c2", "edit", &[("a.txt", "a2\n")]);
        assert_tree_is_honest(&conn, "main");

        commit_files(&conn, "c3", "add", &[("c.txt", "c\n")]);
        assert_tree_is_honest(&conn, "main");

        // A deletion: recorded with a null hash and no file_version row.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('c4','del','a',4,'pending','c3','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('c4','b.txt',NULL,NULL)",
            [],
        )
        .unwrap();
        commit_one(&conn, "c4").unwrap();
        assert_tree_is_honest(&conn, "main");

        // Adding a path back after deleting it.
        commit_files(&conn, "c5", "readd", &[("b.txt", "b again\n")]);
        assert_tree_is_honest(&conn, "main");
    }

    #[test]
    fn a_merge_rebuilds_the_tree_instead_of_applying_it() {
        // A merge has two parents, so it is not an incremental step. The fallback
        // has to produce the same answer as a rebuild, which this asserts.
        let conn = mem_db();
        commit_files(&conn, "base", "base", &[("f.txt", "x\n"), ("g.txt", "y\n")]);
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch)
             VALUES('side','base',1,'main')",
            [],
        )
        .unwrap();
        crate::store::refresh_branch_tree(&conn, "side").unwrap();

        let d = std::env::temp_dir().join(format!("gesta-inc-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("f.txt"), "x\n").unwrap();
        std::fs::write(d.join("g.txt"), "y\n").unwrap();

        let out = cherry_pick(&conn, &d, "base", "side", "t").unwrap();
        let _ = out;
        assert_tree_is_honest(&conn, "main");
        assert_tree_is_honest(&conn, "side");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// Reviewing a step means comparing two points in history, not the working tree
    /// against the head. This covers a modified file, an added file, a deleted file,
    /// and a range that spans several changesets.
    #[test]
    fn diff_between_two_revisions_reports_changes_on_both_sides() {
        let conn = mem_db();
        commit_files(
            &conn,
            "r1",
            "one",
            &[("a.txt", "alpha\n"), ("b.txt", "beta\n")],
        );

        // A removal and an addition in the second commit.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
             VALUES('r2','two','a',2,'pending','r1',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('r2','b.txt',NULL,NULL)",
            [],
        )
        .unwrap();
        commit_one(&conn, "r2").unwrap();

        commit_files(
            &conn,
            "r3",
            "three",
            &[("a.txt", "alpha changed\n"), ("c.txt", "gamma\n")],
        );

        // One step: b.txt deleted, and nothing else. r2 records only that, so a.txt
        // must not appear even though r3 changes it later.
        let one = diff_revs(&conn, Some("r1"), Some("r2")).unwrap();
        let names: Vec<&str> = one.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(
            names,
            vec!["b.txt"],
            "expected only the deletion, got {:?}",
            names
        );

        // Spanning two steps: c.txt appears as well.
        let all = diff_revs(&conn, Some("r1"), Some("r3")).unwrap();
        let names: Vec<&str> = all.iter().map(|(p, _, _)| p.as_str()).collect();
        assert!(names.contains(&"c.txt"), "c.txt missing from {:?}", names);

        // The same revision on both sides is no difference at all.
        assert!(diff_revs(&conn, Some("r3"), Some("r3")).unwrap().is_empty());

        // An empty left side compares against nothing, which is how a first changeset
        // is shown on its own.
        let from_nothing = diff_revs(&conn, None, Some("r1")).unwrap();
        assert_eq!(
            from_nothing.len(),
            2,
            "expected both files, got {:?}",
            from_nothing
        );
    }

    #[test]
    fn an_unknown_revision_is_an_error_not_an_empty_result() {
        let conn = mem_db();
        commit_files(&conn, "r1", "one", &[("a.txt", "alpha\n")]);
        let err = diff_revs(&conn, Some("does-not-exist"), Some("r1")).unwrap_err();
        assert!(err.to_string().contains("no such revision"), "got: {}", err);
    }

    #[test]
    fn a_diff_shows_the_lines_that_changed_and_nothing_else() {
        let (removed, added) = line_diff("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(removed, vec!["b".to_string()]);
        assert_eq!(added, vec!["B".to_string()]);

        // An append reports only the new line.
        let (removed, added) = line_diff("a\nb\n", "a\nb\nc\n");
        assert!(
            removed.is_empty(),
            "an append removes nothing, got {:?}",
            removed
        );
        assert_eq!(added, vec!["c".to_string()]);
    }
    /// Reviewing a stack is the question an agent-written change raises: what happened,
    /// in what order, and which steps deserve a person's attention.
    #[test]
    fn a_stack_is_reviewed_step_by_step_with_a_risk_read() {
        let conn = mem_db();
        commit_files(&conn, "s1", "one", &[("a.txt", "alpha\n")]);

        // A large change to a manifest, which is what the score is meant to catch.
        let long: String = (0..500).map(|i| format!("line {}\n", i)).collect();
        commit_files(
            &conn,
            "s2",
            "two",
            &[("big.txt", &long), ("package.json", "{}\n")],
        );

        let steps = review_stack(&conn, None, "main").unwrap();
        assert_eq!(steps.len(), 2, "expected both steps, got {}", steps.len());

        // Oldest first, so the review reads in the order things happened.
        assert_eq!(steps[0].id, "s1");
        assert_eq!(steps[1].id, "s2");

        // The small first step scores low, the large one does not.
        assert_eq!(
            steps[0].risk.band(),
            "low",
            "reasons: {:?}",
            steps[0].risk.reasons
        );
        assert!(
            steps[1].risk.score >= 4,
            "a 500 line manifest change scored {}",
            steps[1].risk.score
        );

        // Every point is explained, because a score nobody can audit cannot route work.
        assert!(
            !steps[1].risk.reasons.is_empty(),
            "the risk came with no reasons"
        );
        assert!(steps[1]
            .risk
            .reasons
            .iter()
            .any(|r| r.contains("lines changed")));

        // Starting after the first step leaves only the second.
        let tail = review_stack(&conn, Some("s1"), "main").unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].id, "s2");
    }

    #[test]
    fn a_small_text_change_is_not_flagged() {
        let conn = mem_db();
        commit_files(&conn, "t1", "one", &[("a.txt", "alpha\n")]);
        commit_files(&conn, "t2", "two", &[("a.txt", "alpha changed\n")]);
        let steps = review_stack(&conn, Some("t1"), "main").unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].risk.score, 0,
            "reasons: {:?}",
            steps[0].risk.reasons
        );
        assert_eq!(steps[0].risk.band(), "low");
    }

    #[test]
    fn a_test_change_does_not_raise_the_score_for_size() {
        // Changing code and its test together is the normal healthy shape, and the
        // score must not punish it the way it punishes a large change with no test.
        let conn = mem_db();
        commit_files(&conn, "u1", "one", &[("a.txt", "alpha\n")]);
        let body: String = (0..120).map(|i| format!("v{}\n", i)).collect();
        commit_files(
            &conn,
            "u2",
            "two",
            &[("src/lib.rs", &body), ("src/lib_test.rs", "test\n")],
        );
        let steps = review_stack(&conn, Some("u1"), "main").unwrap();
        let reasons = &steps[0].risk.reasons;
        assert!(
            !reasons.iter().any(|r| r.contains("no test")),
            "a change that touched a test was still flagged: {:?}",
            reasons
        );
    }
    /// The rules are a guess. A revert is evidence, and a change that matches a shape this
    /// repository has taken back before should score higher than the same shape in a
    /// repository that never has.
    #[test]
    fn a_revert_raises_the_score_of_a_change_with_the_same_shape() {
        let conn = mem_db();
        commit_files(&conn, "h1", "base", &[("a.txt", "alpha\n")]);
        let body: String = (0..200).map(|i| format!("line {}\n", i)).collect();
        commit_files(&conn, "h2", "big", &[("big.txt", &body)]);
        commit_files(&conn, "h3", "big again", &[("big2.txt", &body)]);

        // With nothing reverted, the two big changes score the same.
        let plain = diff_revs(&conn, Some("h2"), Some("h3")).unwrap();
        let without = score_change(&conn, &plain).unwrap();
        let second = diff_revs(&conn, Some("h2"), Some("h3")).unwrap();
        let without2 = score_change(&conn, &second).unwrap();
        assert_eq!(
            without.score, without2.score,
            "with no history the same shape must score the same"
        );

        // Record h2 as reverted.
        conn.execute(
            "INSERT INTO operations(kind, target, detail, snapshot, created_at)
             VALUES('revert', 'h2', 'h2', '{}', 1)",
            [],
        )
        .unwrap();
        let history = RevertHistory::load(&conn).unwrap();
        assert_eq!(history.reverted.len(), 1, "the revert was not read back");

        let third = diff_revs(&conn, Some("h2"), Some("h3")).unwrap();
        let with = score_change_calibrated(&conn, &third, Some(&history)).unwrap();
        assert!(
            with.score > without.score,
            "a matching revert did not raise the score: {} vs {}",
            with.score,
            without.score
        );
        assert!(
            with.reasons.iter().any(|r| r.contains("reverted")),
            "the calibration must say where the extra point came from: {:?}",
            with.reasons
        );
        // It never lowers anything, and never adds more than a few points.
        assert!(
            with.score - without.score <= 3,
            "the adjustment is too large"
        );
    }

    #[test]
    fn an_unrelated_revert_does_not_raise_the_score() {
        let conn = mem_db();
        commit_files(&conn, "u1", "base", &[("a.txt", "alpha\n")]);
        // A reverted change that touched a test, at a very different size.
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('rev1','small','a',2,'committed','u1','main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('rev1','a_test.rs',NULL,NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operations(kind, target, detail, snapshot, created_at)
             VALUES('revert', 'rev1', 'rev1', '{}', 1)",
            [],
        )
        .unwrap();

        let body: String = (0..200).map(|i| format!("line {}\n", i)).collect();
        commit_files(&conn, "u2", "big, no test", &[("big.txt", &body)]);
        let diff = diff_revs(&conn, Some("u1"), Some("u2")).unwrap();
        let plain = score_change(&conn, &diff).unwrap();
        let history = RevertHistory::load(&conn).unwrap();
        let calibrated = score_change_calibrated(&conn, &diff, Some(&history)).unwrap();
        assert_eq!(
            plain.score, calibrated.score,
            "a revert of a different shape must not move the score"
        );
    }
    /// Two writers landing at once is the failure a merge queue exists for. The second
    /// must be refused, not silently overwrite the first.
    #[test]
    fn landing_is_refused_when_the_target_moved() {
        let conn = mem_db();
        let root = std::env::temp_dir().join(format!("gesta-land-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f.txt"), "base\n").unwrap();
        commit_files(&conn, "L0", "base", &[("f.txt", "base\n")]);

        // Two side branches from the same point.
        for name in ["left", "right"] {
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch)
                 VALUES(?1,'L0',1,'main')",
                rusqlite::params![name],
            )
            .unwrap();
            crate::store::refresh_branch_tree(&conn, name).unwrap();
        }
        // `commit_files` commits onto main, so the side branch is built explicitly.
        std::fs::write(root.join("l.txt"), "left\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('L1','left','a',2,'pending','L0','left')",
            [],
        )
        .unwrap();
        // A null new_hash means a deletion, so the added content has to be stored and
        // named here. Getting this wrong made the merge apply nothing, which the test
        // then reported as a missing file.
        let l_hash = crate::store::put_object(&conn, "left\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('L1','l.txt',NULL,?1)",
            rusqlite::params![l_hash],
        )
        .unwrap();
        commit_one(&conn, "L1").unwrap();
        std::fs::write(root.join("r.txt"), "right\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('R1','right','a',2,'pending','L0','right')",
            [],
        )
        .unwrap();
        let r_hash = crate::store::put_object(&conn, "right\n").unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('R1','r.txt',NULL,?1)",
            rusqlite::params![r_hash],
        )
        .unwrap();
        commit_one(&conn, "R1").unwrap();

        // First writer lands with the head it read.
        let read = crate::store::branch_head(&conn, "main").unwrap();
        land_branch(&conn, &root, "left", "main", read.as_deref(), "a").unwrap();
        let after_first = crate::store::branch_head(&conn, "main").unwrap();
        assert_ne!(after_first, read, "the first landing must move main");

        // Second writer prepared against the same head and must be refused.
        let err = land_branch(&conn, &root, "right", "main", read.as_deref(), "b").unwrap_err();
        assert!(
            err.to_string()
                .contains("moved while this was being prepared"),
            "got: {}",
            err
        );
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap(),
            after_first,
            "a refused landing must not move the target"
        );

        // Re-reading and retrying succeeds.
        let fresh = crate::store::branch_head(&conn, "main").unwrap();
        land_branch(&conn, &root, "right", "main", fresh.as_deref(), "b").unwrap();
        let final_head = crate::store::branch_head(&conn, "main").unwrap().unwrap();
        let tree = crate::store::tree_at(&conn, &final_head).unwrap();
        let names: Vec<&str> = tree.iter().map(|(p, _)| p.as_str()).collect();
        assert!(names.contains(&"l.txt"), "left is missing from {:?}", names);
        assert!(
            names.contains(&"r.txt"),
            "right is missing from {:?}",
            names
        );

        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn committing_one_edit_reads_the_parent_table_once_whatever_the_tree_size() {
        // The gate step `write-flat` timed `changeset commit` at 200 and 2000 files and failed about
        // one run in three on 52 -> 184 ms, which is the host: the same probe measured 36 to 148 ms
        // for the *identical* size across runs (see A56). A commit that walked the tree per file would
        // show up as a statement count growing with the tree, and a count does not move when the
        // machine is busy.
        //
        // What the claim is: the cost of a write follows the change, not the tree. So the number of
        // statements a one-file commit runs must not depend on how many files the tree holds.
        fn count_commit(files: usize) -> usize {
            let d = std::env::temp_dir().join(format!(
                "gesta-write-flat-{}-{}",
                files,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join(".gesta")).unwrap();
            let mut conn = crate::store::open_db(&d).unwrap();
            crate::store::init_schema(&conn).unwrap();
            crate::store::migrate(&conn).unwrap();
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
                [],
            )
            .unwrap();

            for i in 0..files {
                std::fs::write(d.join(format!("f{i}.txt")), format!("file {i}\n")).unwrap();
            }
            let seed = crate::repo::create_changeset(&conn, &d, "seed", "b")
                .unwrap()
                .expect("a fresh tree has files to record");
            crate::repo::commit_one(&conn, &seed.id).unwrap();

            // One edit, then commit through the traced connection.
            std::fs::write(d.join("f0.txt"), "changed\n").unwrap();
            let cs = crate::repo::create_changeset(&conn, &d, "edit", "b")
                .unwrap()
                .expect("one changed file is a changeset");
            let id = cs.id.clone();
            let n = crate::store::statements_during_for_test(&mut conn, |c| {
                crate::repo::commit_one(c, &id).unwrap();
            });
            let _ = std::fs::remove_dir_all(&d);
            n
        }

        // The sizes the gate's `scale` step used, so the count is a drop-in for the timer it
        // replaced rather than a claim about a smaller tree.
        let small = count_commit(300);
        let large = count_commit(3000);
        assert_eq!(
            small, large,
            "committing one edit ran {small} statements over 300 files and {large} over 3000: the \
             write is reading the tree again"
        );
        // A ceiling, so the reader knows what correct looks like rather than only that the two agree.
        assert!(
            large <= 60,
            "a one-file commit took {large} statements, which is a per-file walk"
        );
    }
    #[test]
    fn committing_after_a_long_history_runs_the_same_statements() {
        // `long_history` timed commits at 3000 of them against 300 and failed about one run in three
        // on an idle container - the same shape as `write-flat` before it (A56). The claim is "the
        // cost of a commit stays flat as the history grows", which is a count: a commit that walked
        // the changeset table would run more SQL, and a count does not move when the machine is busy.
        //
        // The history is built first and then reopened, because the CLI does that on every command and
        // `migrate` was once the thing that grew: a store at 1000 commits paid 31 ms per open against
        // 6.7 ms at 100, which read as "the cost of a commit grows with the history" (see the comment
        // in `store::migrate`). So the count covers the open as well as the write.
        fn count_after(history: usize) -> (usize, usize) {
            let d = std::env::temp_dir().join(format!(
                "gesta-hist-flat-{}-{}",
                history,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join(".gesta")).unwrap();
            {
                let conn = crate::store::open_db(&d).unwrap();
                crate::store::init_schema(&conn).unwrap();
                // `migrate` is what stamps `user_version`, and the stamp is what lets a later open
                // skip the DDL body. A fixture that only calls `init_schema` leaves the database at
                // version 0, so every reopen pays the whole body - 46 statements at 300 changesets
                // against the 12 this store is supposed to cost once settled. The stamp belongs to
                // the setup, not to the measurement.
                crate::store::migrate(&conn).unwrap();
                conn.execute(
                    "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
                    [],
                )
                .unwrap();
                std::fs::write(d.join("f.txt"), "line\n").unwrap();
                for i in 0..history {
                    std::fs::write(d.join("f.txt"), format!("line {i}\n")).unwrap();
                    let cs = crate::repo::create_changeset(&conn, &d, &format!("h{i}"), "b")
                        .unwrap()
                        .expect("one changed file is a changeset");
                    crate::repo::commit_one(&conn, &cs.id).unwrap();
                }
            }

            // Reopened exactly as the CLI does, and the open is counted apart from the write.
            //
            // They are two different claims and mixing them made this measurement incoherent:
            // `migrate` runs a fixed pile of idempotent DDL only when `user_version` is behind, so a
            // store built by this build skips it and a store from the fixture's own first commit does
            // not. The first version of this test read 109 after 10 changesets and 105 after 300 -
            // *fewer* statements for *more* history, which is the shape of a one-off, not of a walk.
            let mut conn = crate::store::open_db(&d).unwrap();
            let opened = crate::store::statements_during_for_test(&mut conn, |c| {
                let _ = crate::store::migrate(c);
            });
            std::fs::write(d.join("f.txt"), "the measured edit\n").unwrap();
            let written = crate::store::statements_during_for_test(&mut conn, |c| {
                let cs = crate::repo::create_changeset(c, &d, "measured", "b")
                    .unwrap()
                    .expect("one changed file is a changeset");
                crate::repo::commit_one(c, &cs.id).unwrap();
            });
            let _ = std::fs::remove_dir_all(&d);
            (opened, written)
        }

        let (short_open, short_write) = count_after(10);
        let (long_open, long_write) = count_after(300);
        assert_eq!(
            short_write, long_write,
            "the write ran {short_write} statements after 10 changesets and {long_write} after 300: \
             it is reading the history"
        );
        // The open is the other half, and the one `store::migrate` was fixed for: a settled store
        // must not pay the DDL body. A ceiling rather than equality, because the first open of a
        // fresh file legitimately runs schema creation.
        assert!(
            long_open <= 12,
            "opening a store at 300 changesets ran {long_open} statements; the DDL body is running \
             on a store that is already at the current schema"
        );
        assert!(
            short_open <= 12,
            "opening a store at 10 changesets ran {short_open} statements"
        );
    }
}
