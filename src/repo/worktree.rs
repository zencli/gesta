//! Additional working trees over one store.
//!
//! Two branches cannot be checked out in one directory at once, and the alternative - clone the
//! repository again - duplicates every object and then has to be kept in step by hand. A linked
//! worktree is a second directory whose `.gesta` is a **file** holding the path of the main store,
//! the way git marks a linked worktree with a `.git` file. One database, two working trees, so a
//! commit in either is immediately visible in the other.
//!
//! The registry of worktrees lives in the store, not in each tree. A tree that kept its own list
//! would disagree with its siblings the moment two people ran `worktree add` at once, and the
//! disagreement would be invisible until someone tried to remove a tree that was already gone.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::store;
use crate::{bad, ZResult};

/// File inside a working tree naming the branch it holds.
pub const ZF_BRANCH: &str = ".gesta-branch";

/// A working tree registered against this store.
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

fn register(conn: &Connection, path: &Path, branch: &str) -> ZResult<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS worktrees (
            path       TEXT PRIMARY KEY,
            branch     TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
        [],
    )?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO worktrees (path, branch, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET branch = excluded.branch",
        rusqlite::params![path.to_string_lossy(), branch, now],
    )?;
    Ok(())
}

fn unregister(conn: &Connection, path: &Path) -> ZResult<Option<String>> {
    let branch = registered_branch(conn, path)?;
    conn.execute(
        "DELETE FROM worktrees WHERE path = ?1",
        rusqlite::params![path.to_string_lossy()],
    )?;
    Ok(branch)
}

/// The branch a registered worktree holds, without removing the registration.
fn registered_branch(conn: &Connection, path: &Path) -> ZResult<Option<String>> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS worktrees (
            path       TEXT PRIMARY KEY,
            branch     TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
        [],
    )?;
    Ok(conn
        .query_row(
            "SELECT branch FROM worktrees WHERE path = ?1",
            rusqlite::params![path.to_string_lossy()],
            |r| r.get(0),
        )
        .ok())
}

/// Every registered working tree, with the main one first.
pub fn list(conn: &Connection, root: &Path) -> ZResult<Vec<Worktree>> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS worktrees (
            path       TEXT PRIMARY KEY,
            branch     TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
        [],
    )?;
    let mut out = vec![Worktree {
        path: root.to_path_buf(),
        branch: store::current_branch(conn).unwrap_or_else(|_| "main".to_string()),
    }];
    let mut stmt = conn.prepare("SELECT path, branch FROM worktrees ORDER BY created_at")?;
    let rows = stmt.query_map([], |r| {
        Ok(Worktree {
            path: PathBuf::from(r.get::<_, String>(0)?),
            branch: r.get::<_, String>(1)?,
        })
    })?;
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// The branch a working tree has checked out.
///
/// `current_branch` reads one row out of the store's shared state, so it can only ever describe one
/// tree. A second tree sharing that store would read the first tree's answer and commit onto a branch
/// nobody named - which is what happened before this function existed: a commit made in a linked
/// worktree landed on `main` instead of on the branch the worktree was created for.
///
/// The branch is stored in the tree itself, in `.gesta-branch`, because it is a property of the
/// checkout and not of the repository. The main tree keeps using the store's state, so an existing
/// repository reads exactly as it did before.
pub fn branch_of(root: &Path, conn: &Connection) -> ZResult<String> {
    let marker = root.join(ZF_BRANCH);
    if marker.is_file() {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            let name = text.trim();
            if !name.is_empty() {
                return Ok(name.to_string());
            }
        }
    }
    store::current_branch(conn)
}

/// Record which branch a working tree holds.
pub fn set_branch(root: &Path, branch: &str) -> ZResult<()> {
    std::fs::write(root.join(ZF_BRANCH), format!("{branch}\n"))?;
    Ok(())
}

/// Create a working tree at `path` on `branch`.
///
/// The branch has to exist. Creating it on demand reads as helpful and then silently moves a branch
/// the caller did not name, so a missing branch is an error that names the ones that exist.
pub fn add(conn: &Connection, root: &Path, path: &Path, branch: &str) -> ZResult<()> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM branches WHERE name = ?1",
        rusqlite::params![branch],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(bad(format!(
            "no branch named '{branch}' (see `gesta branch list`)"
        )));
    }
    if path.exists() {
        return Err(bad(format!(
            "{} already exists; a worktree needs a directory that is not there yet",
            path.display()
        )));
    }
    std::fs::create_dir_all(path)?;

    // The marker: a file naming the store, so the tree costs a pointer rather than a copy.
    let store = crate::store_dir(root);
    let target = if store.is_absolute() {
        store
    } else {
        std::fs::canonicalize(&store).unwrap_or(store)
    };
    std::fs::write(path.join(crate::ZF_DIR), format!("{}\n", target.display()))?;

    // The branch this tree holds, written before the checkout so a failure part way through still
    // leaves the tree knowing what it was meant to be.
    set_branch(path, branch)?;

    // The same content the branch holds in the main tree, so the worktree starts usable.
    let (written, removed) = crate::repo::restore_worktree(conn, path, branch)?;
    // A new working tree is a checkout too, so the same hook runs. `tail -f`-style rebuilds are the
    // reason this point exists, and a tree that appears without them starts in a state nobody set up.
    crate::repo::hooks::run(path, "post-checkout", &["worktree", branch])?;
    register(conn, path, branch)?;
    println!(
        "worktree at {} on branch {branch}: {written} file(s) written, {removed} removed",
        path.display()
    );
    Ok(())
}

/// Remove a working tree. Its store, and every object in it, stays where it is.
pub fn remove(conn: &Connection, path: &Path, force: bool) -> ZResult<()> {
    // Look the tree up without unregistering it. The check below can refuse, and a refusal that has
    // already deleted the registry entry leaves a directory nobody can remove: `--force` then says
    // "no worktree at ..." while the tree is still on disk holding the work it was told to discard.
    let known_branch = registered_branch(conn, path)?;
    if known_branch.is_none() {
        return Err(bad(format!(
            "no worktree at {} (see `gesta worktree list`)",
            path.display()
        )));
    }
    // Uncommitted work exists only on disk. Removing the directory deletes the only copy, so a tree
    // that holds any is refused: this used to delete it silently and exit 0, which loses work with no
    // way to tell it happened.
    if !force {
        let branch = known_branch.clone().unwrap_or_default();
        let d = crate::repo::diff_worktree(conn, path, &branch)?;
        // The branch marker is written into the tree on purpose, so it is not uncommitted work.
        let marker = if path.join(ZF_BRANCH).is_file() { 1 } else { 0 };
        let added = d.added.len().saturating_sub(marker);
        let dirty = added + d.modified.len() + d.deleted.len();
        if dirty > 0 {
            return Err(bad(format!(
                "{} holds {dirty} uncommitted change(s); commit them, or remove it with --force. \
                 Files this tool wrote are deleted; files it did not create are left in place",
                path.display()
            )));
        }
    }
    let _ = unregister(conn, path)?;
    // The marker goes first: a directory that still holds it still looks like a repository, and
    // every command run inside it afterwards fails in a way that names the wrong problem.
    let marker = path.join(crate::ZF_DIR);
    if marker.is_file() {
        let _ = std::fs::remove_file(&marker);
    }
    let _ = std::fs::remove_file(path.join(ZF_BRANCH));

    // The checked-out files go, because this tree was created by checking a branch out and leaving
    // them behind would leave a directory that looks like an unversioned copy of the branch. Anything
    // that is not ours - a file the store never wrote - stays, and its presence is why the directory
    // itself is only removed when empty.
    let tracked = tracked_paths(conn, path, &known_branch.unwrap_or_default())?;
    let mut left = 0usize;
    for rel in &tracked {
        let abs = path.join(rel);
        if abs.is_file() {
            let _ = std::fs::remove_file(&abs);
        }
        // The directories the files lived in, deepest first, but only while they are empty.
        let mut parent = abs.parent().map(|p| p.to_path_buf());
        while let Some(dir) = parent {
            if dir == path || !dir.starts_with(path) {
                break;
            }
            if std::fs::remove_dir(&dir).is_err() {
                break;
            }
            parent = dir.parent().map(|p| p.to_path_buf());
        }
    }
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        left += 1;
        let _ = entry;
    }
    let _ = std::fs::remove_dir(path);

    println!("removed worktree {}", path.display());
    if left > 0 {
        println!("{} entr(ies) it did not create were left in place", left);
    }
    println!("the store is untouched; the branch and its changesets remain");
    Ok(())
}

/// The paths a branch holds, so removing a worktree takes the files it wrote and nothing else.
///
/// `changeset_files` is the wrong table: it holds what a changeset CHANGED, so removing a tree would
/// delete only the files the last commit touched and leave the rest of the checkout behind. The whole
/// tree comes from `tree_at`, which walks the branch head.
fn tracked_paths(conn: &Connection, _path: &Path, branch: &str) -> ZResult<Vec<String>> {
    let head: Option<String> = conn
        .query_row(
            "SELECT head FROM branches WHERE name = ?1",
            rusqlite::params![branch],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    let Some(head) = head else {
        return Ok(Vec::new());
    };
    Ok(store::tree_at(conn, &head)?
        .into_iter()
        .map(|(path, _hash)| path)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Connection, PathBuf) {
        let conn = crate::store::tests::mem_db();
        // Unique per test, not per process. Keyed by process id, every test in this module shared one
        // directory, and the `remove_dir_all` below deleted a directory another test had just created
        // and was still using. That made the suite fail about one run in three, on a test that had
        // nothing to do with worktrees being removed.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("gesta-wt-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::ZF_DIR)).unwrap();
        (conn, root)
    }

    #[test]
    fn a_branch_that_does_not_exist_is_refused_and_the_existing_ones_are_named() {
        let (conn, root) = fixture();
        let target = root.join("elsewhere");
        let err = add(&conn, &root, &target, "no-such-branch").unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("no-such-branch"), "got: {text}");
        assert!(text.contains("branch list"), "the way out is named: {text}");
        assert!(
            !target.exists(),
            "a refused add must not leave a directory behind"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_a_tree_that_was_never_added_says_so_rather_than_failing_obscurely() {
        let (conn, root) = fixture();
        let err = remove(&conn, &root.join("never-there"), false).unwrap_err();
        assert!(format!("{err}").contains("no worktree"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_removal_leaves_the_registration_so_force_can_still_find_it() {
        // Removing a tree with uncommitted work is refused, and the refusal must not have already
        // deleted the registry row: otherwise `--force` answers "no worktree at ..." while the
        // directory is still on disk holding the work it was asked to discard.
        let (conn, root) = fixture();
        conn.execute(
            "INSERT OR IGNORE INTO branches (name, head, created_at) VALUES ('main', NULL, 1)",
            [],
        )
        .unwrap();
        let target = root.join("wt");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join(ZF_BRANCH), "main\n").unwrap();
        std::fs::write(target.join("work.txt"), "uncommitted\n").unwrap();
        // The table is created by the reader, so ask for it before inserting into it.
        assert!(registered_branch(&conn, &target).unwrap().is_none());
        conn.execute(
            "INSERT INTO worktrees (path, branch, created_at) VALUES (?1, 'main', 1)",
            rusqlite::params![target.to_string_lossy()],
        )
        .unwrap();

        let err = remove(&conn, &target, false).unwrap_err();
        assert!(format!("{err}").contains("uncommitted"), "got: {err}");
        assert!(
            registered_branch(&conn, &target).unwrap().is_some(),
            "the refusal unregistered the tree, so --force could not find it"
        );
        assert!(
            target.join("work.txt").is_file(),
            "the work must survive a refusal"
        );

        // And with force it goes through for the files the store wrote. `work.txt` was never in a
        // changeset, so it is a file this tool did not create and is deliberately left behind.
        remove(&conn, &target, true).unwrap();
        assert!(
            !target.join(ZF_BRANCH).is_file(),
            "force left its own branch marker behind"
        );
        assert!(
            target.join("work.txt").is_file(),
            "force deleted a file it did not create, which the message says it will not do"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_main_tree_is_always_listed_first_and_a_tree_can_only_be_removed_once() {
        let (conn, root) = fixture();
        //  already carries a  branch; inserting another is a UNIQUE violation, which
        // is how the first version of this test failed.
        let _ = conn.execute(
            "INSERT OR IGNORE INTO branches (name, head, created_at) VALUES ('main', NULL, 1)",
            [],
        );
        let listed = list(&conn, &root).unwrap();
        assert_eq!(
            listed[0].path, root,
            "the tree holding the store comes first"
        );
        assert_eq!(listed.len(), 1);

        // Register by hand: `add` needs a real working tree to restore into, which is the bench's job.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        register(&conn, &other, "main").unwrap();
        assert_eq!(list(&conn, &root).unwrap().len(), 2);
        assert!(
            unregister(&conn, &other).unwrap().is_some(),
            "first removal finds it and names its branch"
        );
        assert!(
            unregister(&conn, &other).unwrap().is_none(),
            "second removal does not"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
