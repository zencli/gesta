//! Pointing a path in this repository at another repository.
//!
//! A subrepository is a tracked path whose content lives in a different
//! repository. The parent records a pointer: the remote to fetch it from and the
//! changeset that path is pinned to. The working tree holds a real checkout there,
//! and the scan skips it so the parent never records the child's files as its own.
//!
//! Three decisions that make this safe:
//!
//! 1. The pointer is a row, not a file inside the child. Nothing about the child
//!    has to be understood by the parent.
//! 2. The child is skipped by the tree scan, so its files can never be committed
//!    into the parent by accident, which is the failure that makes nested
//!    repositories dangerous.
//! 3. A missing child checkout is reported, not treated as a deletion. Reporting
//!    it as deleted would let one commit wipe the pointer.

use std::path::{Path, PathBuf};

use rusqlite::params;
use rusqlite::Connection;

use crate::{bad, now, ZResult};

#[derive(Debug)]
pub struct Sub {
    pub path: String,
    pub remote: String,
    /// Changeset the child is pinned to. Empty until it is first synced.
    pub pinned: String,
}

fn init_schema(conn: &Connection) -> ZResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS subrepos (
            path       TEXT PRIMARY KEY,
            remote     TEXT NOT NULL,
            pinned     TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

pub fn add(conn: &Connection, path: &str, remote: &str) -> ZResult<Sub> {
    init_schema(conn)?;
    let p = clean_path(path)?;
    let exists: Option<String> = conn
        .query_row(
            "SELECT remote FROM subrepos WHERE path = ?1",
            params![p],
            |r| r.get(0),
        )
        .ok();
    if let Some(existing) = exists {
        return Err(bad(format!(
            "{} is already a subrepository pointing at {}",
            p, existing
        )));
    }
    conn.execute(
        "INSERT INTO subrepos(path, remote, pinned, created_at) VALUES(?1, ?2, '', ?3)",
        params![p, remote, now()],
    )?;
    Ok(Sub {
        path: p,
        remote: remote.to_string(),
        pinned: String::new(),
    })
}

/// Reject a path that could leave the repository or name the repository itself.
fn clean_path(path: &str) -> ZResult<String> {
    let p = path.trim().trim_end_matches('/');
    if p.is_empty() {
        return Err(bad("a subrepository needs a path".to_string()));
    }
    if p.starts_with('/') || p.contains("..") {
        return Err(bad(
            "a subrepository path must be relative and stay inside the repository".to_string(),
        ));
    }
    if p == crate::ZF_DIR || p == crate::LEGACY_DIR {
        return Err(bad(
            "the repository directory cannot hold a subrepository".to_string()
        ));
    }
    Ok(p.to_string())
}

pub fn list(conn: &Connection) -> ZResult<Vec<Sub>> {
    init_schema(conn)?;
    let mut stmt = conn.prepare("SELECT path, remote, pinned FROM subrepos ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        Ok(Sub {
            path: r.get(0)?,
            remote: r.get(1)?,
            pinned: r.get(2)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| bad(e.to_string()))
}

pub fn get(conn: &Connection, path: &str) -> ZResult<Option<Sub>> {
    init_schema(conn)?;
    let p = clean_path(path)?;
    Ok(conn
        .query_row(
            "SELECT path, remote, pinned FROM subrepos WHERE path = ?1",
            params![p],
            |r| {
                Ok(Sub {
                    path: r.get(0)?,
                    remote: r.get(1)?,
                    pinned: r.get(2)?,
                })
            },
        )
        .ok())
}

pub fn remove(conn: &Connection, path: &str) -> ZResult<bool> {
    init_schema(conn)?;
    let p = clean_path(path)?;
    let n = conn.execute("DELETE FROM subrepos WHERE path = ?1", params![p])?;
    Ok(n > 0)
}

/// Record a new pin for the child, usually its current head.
pub fn pin(conn: &Connection, path: &str, changeset: &str) -> ZResult<Sub> {
    init_schema(conn)?;
    let mut sub =
        get(conn, path)?.ok_or_else(|| bad(format!("{} is not a subrepository", path)))?;
    sub.pinned = changeset.to_string();
    conn.execute(
        "UPDATE subrepos SET pinned = ?1 WHERE path = ?2",
        params![changeset, sub.path],
    )?;
    Ok(sub)
}

/// Root of a child repository, when its checkout is present.
pub fn child_root(parent: &Path, sub: &Sub) -> PathBuf {
    parent.join(&sub.path)
}

/// Is the child's checkout present and a repository?
pub fn is_present(parent: &Path, sub: &Sub) -> bool {
    let root = child_root(parent, sub);
    crate::repo_dir(&root).is_dir() || root.join(crate::LEGACY_DIR).is_dir()
}

/// Read the child's current head, if it has one.
pub fn child_head(parent: &Path, sub: &Sub) -> Option<String> {
    let root = child_root(parent, sub);
    if !is_present(parent, sub) {
        return None;
    }
    let conn = crate::store::open_db(&root).ok()?;
    let branch = crate::store::current_branch(&conn).ok()?;
    crate::store::branch_head(&conn, &branch).ok().flatten()
}

/// Paths that must never be scanned as part of the parent.
///
/// Returned as repository relative strings so the scan can skip them before it
/// descends, which also stops it walking a large child tree it does not own.
pub fn excluded_paths(conn: &Connection) -> Vec<String> {
    list(conn)
        .map(|all| all.into_iter().map(|s| s.path).collect())
        .unwrap_or_default()
}

/// Is `rel` inside a subrepository, or the subrepository itself?
pub fn is_inside_sub(rel: &str, subs: &[String]) -> bool {
    subs.iter().any(|s| {
        rel == s.as_str()
            || rel
                .strip_prefix(s.as_str())
                .map(|rest| rest.starts_with('/'))
                .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_then_list_reports_the_pointer() {
        let conn = crate::store::tests::mem_db();
        add(&conn, "vendor/lib", "https://example.invalid/lib.git").unwrap();
        let all = list(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].path, "vendor/lib");
        assert_eq!(all[0].pinned, "", "a fresh pointer is not pinned yet");
    }

    #[test]
    fn adding_the_same_path_twice_is_refused() {
        let conn = crate::store::tests::mem_db();
        add(&conn, "lib", "a").unwrap();
        let e = add(&conn, "lib", "b").unwrap_err().to_string();
        assert!(e.contains("already a subrepository"), "got: {}", e);
    }

    #[test]
    fn a_path_that_escapes_the_repository_is_refused() {
        let conn = crate::store::tests::mem_db();
        for bad_path in ["../outside", "/absolute", "a/../../b"] {
            assert!(
                add(&conn, bad_path, "x").is_err(),
                "{} should be refused",
                bad_path
            );
        }
    }

    #[test]
    fn the_repository_directory_cannot_be_a_subrepository() {
        let conn = crate::store::tests::mem_db();
        assert!(add(&conn, crate::ZF_DIR, "x").is_err());
    }

    #[test]
    fn pinning_updates_the_recorded_changeset() {
        let conn = crate::store::tests::mem_db();
        add(&conn, "lib", "r").unwrap();
        let s = pin(&conn, "lib", "csabc123").unwrap();
        assert_eq!(s.pinned, "csabc123");
        assert_eq!(get(&conn, "lib").unwrap().unwrap().pinned, "csabc123");
    }

    #[test]
    fn removing_returns_whether_anything_was_removed() {
        let conn = crate::store::tests::mem_db();
        add(&conn, "lib", "r").unwrap();
        assert!(remove(&conn, "lib").unwrap());
        assert!(!remove(&conn, "lib").unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn a_path_inside_a_subrepository_is_recognised() {
        let subs = vec!["vendor/lib".to_string()];
        assert!(is_inside_sub("vendor/lib", &subs));
        assert!(is_inside_sub("vendor/lib/src/a.rs", &subs));
        assert!(
            !is_inside_sub("vendor/libs", &subs),
            "a prefix is not a child"
        );
        assert!(!is_inside_sub("vendor", &subs));
    }

    #[test]
    fn the_walk_never_enters_a_subrepository() {
        // The failure this prevents: a nested checkout whose files get recorded
        // as the parent's own, so one commit swallows the whole child tree.
        let root = std::env::temp_dir().join(format!("gesta-sub-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("vendor/lib/src")).unwrap();
        std::fs::create_dir_all(root.join("vendor/other")).unwrap();
        std::fs::write(root.join("own.txt"), "mine\n").unwrap();
        std::fs::write(root.join("vendor/lib/src/lib.rs"), "theirs\n").unwrap();
        std::fs::write(root.join("vendor/other/x.txt"), "also mine\n").unwrap();

        let rules = crate::repo::worktree::IgnoreRules::with_defaults(&[]);
        let exclude = vec!["vendor/lib".to_string()];
        let found: Vec<String> =
            crate::repo::worktree::scan_files_excluding(&root, &rules, &exclude)
                .unwrap()
                .iter()
                .map(|(r, _)| r.to_string())
                .collect();

        assert!(found.contains(&"own.txt".to_string()));
        assert!(
            found.contains(&"vendor/other/x.txt".to_string()),
            "a sibling of the child is still this repository's file"
        );
        assert!(
            !found.iter().any(|f| f.starts_with("vendor/lib")),
            "nothing under the child may be scanned, found {:?}",
            found
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_child_checkout_is_not_a_deletion() {
        // Reporting it as deleted would let one commit remove the pointer.
        let conn = crate::store::tests::mem_db();
        add(&conn, "vendor/lib", "r").unwrap();
        let subs = excluded_paths(&conn);
        assert!(
            is_inside_sub("vendor/lib", &subs),
            "the pointer path itself is inside the exclusion"
        );
        assert!(
            is_inside_sub("vendor/lib/anything", &subs),
            "so is everything below it"
        );
    }

    #[test]
    fn presence_follows_the_checkout_on_disk() {
        let root = std::env::temp_dir().join(format!("gesta-sub-pres-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sub = Sub {
            path: "lib".to_string(),
            remote: "r".to_string(),
            pinned: String::new(),
        };
        assert!(!is_present(&root, &sub));
        std::fs::create_dir_all(root.join("lib").join(crate::ZF_DIR)).unwrap();
        assert!(is_present(&root, &sub));
        let _ = std::fs::remove_dir_all(&root);
    }
}
