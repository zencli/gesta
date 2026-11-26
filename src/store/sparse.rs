//! Sparse checkout: work on part of a repository.
//!
//! A monorepo can be tens of gigabytes, and an agent that needs one module
//! should not pay for the rest. Sparse checkout records a set of path patterns
//! and confines the working tree to them.
//!
//! The dangerous part is not materialising fewer files, it is what a scan does
//! afterwards. A file that is in the branch tree but was never written to disk
//! looks exactly like a file somebody deleted. Getting that wrong turns every
//! `status` into a repository-wide deletion, and the next commit removes
//! everything outside the sparse scope. So the scope is applied to both sides of
//! the comparison: the scan ignores disk paths outside it, and the deleted
//! computation ignores committed paths outside it.

use rusqlite::Connection;
use std::path::Path;

use crate::store::{get_state, set_state};
use crate::{bad, ZResult};

/// Patterns that define the working set. Empty means "everything", which is the
/// default and behaves exactly as before sparse support existed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SparseScope {
    pub patterns: Vec<String>,
}

impl SparseScope {
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Read the scope from repository state.
    pub fn load(conn: &Connection) -> ZResult<SparseScope> {
        let raw = get_state(conn, "sparse_patterns")?.unwrap_or_default();
        Ok(SparseScope {
            patterns: raw
                .split(',')
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .map(|p| p.to_string())
                .collect(),
        })
    }

    pub fn save(&self, conn: &Connection) -> ZResult<()> {
        set_state(conn, "sparse_patterns", &self.patterns.join(","))
    }

    pub fn clear(conn: &Connection) -> ZResult<()> {
        set_state(conn, "sparse_patterns", "")
    }

    /// Is this repository-relative path part of the working set?
    ///
    /// A directory prefix matches everything under it, so `crates/api` covers
    /// `crates/api/src/main.rs`. A pattern with no slash matches a file name at
    /// any depth, which is what a person writing `*.md` means.
    pub fn contains(&self, path: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        self.patterns.iter().any(|pat| matches_pattern(pat, path))
    }

    /// Materialise this scope for `branch`. Wrapper so callers do not have to
    /// reach into the module function.
    pub fn checkout_sparse(
        &self,
        conn: &Connection,
        root: &Path,
        branch: &str,
    ) -> ZResult<(usize, usize)> {
        checkout_sparse(conn, root, branch, self)
    }

    /// Files in `tree` that fall inside the scope.
    #[cfg(test)]
    pub fn filter<'a, I>(&self, tree: I) -> Vec<String>
    where
        I: IntoIterator<Item = &'a String>,
    {
        if self.patterns.is_empty() {
            return tree.into_iter().cloned().collect();
        }
        tree.into_iter()
            .filter(|p| self.contains(p))
            .cloned()
            .collect()
    }
}

fn matches_pattern(pattern: &str, path: &str) -> bool {
    let pat = pattern.trim_start_matches("./").trim_end_matches('/');
    if pat.is_empty() || pat == "*" || pat == "**" {
        return true;
    }
    if crate::transport::auth::glob_is_match(pat, path) {
        return true;
    }
    // A bare name pattern applies at any depth: `*.md` covers `docs/guide.md`,
    // which is what a person writing it means. Same rule as token scope.
    if !pat.contains('/') {
        let name = path.rsplit('/').next().unwrap_or(path);
        if crate::transport::auth::glob_is_match(pat, name) {
            return true;
        }
    }
    // A directory prefix selects its whole subtree, which is how sparse
    // checkout is described in every tool that has it.
    path.starts_with(&format!("{}/", pat))
}

/// Materialise only the in-scope files of `branch`, and remove in-scope files
/// that the branch no longer has.
///
/// Files outside the scope are left alone on disk: they may be the product of a
/// wider checkout the user made earlier, and deleting them because a narrow
/// checkout happened would destroy work.
pub fn checkout_sparse(
    conn: &Connection,
    root: &Path,
    branch: &str,
    scope: &SparseScope,
) -> ZResult<(usize, usize)> {
    let head = match crate::store::branch_head(conn, branch)? {
        Some(h) => h,
        None => return Err(bad(format!("branch {} has no changesets", branch))),
    };
    let tree = crate::store::tree_at(conn, &head)?;

    // Build the set of in-scope paths from the target tree.
    let in_scope_from_tree: std::collections::HashSet<String> = tree
        .iter()
        .filter(|(path, _)| scope.contains(path))
        .map(|(path, _)| path.clone())
        .collect();

    let mut written = 0usize;
    let mut kept = 0usize;
    for (path, hash) in &tree {
        if !scope.contains(path) {
            kept += 1;
            continue;
        }
        if !crate::support::paths::is_repo_relative(path) {
            return Err(crate::bad(format!(
                "the tree holds an entry that is not a path inside the repository: {:?}",
                path
            )));
        }
        let content = crate::store::get_object_bytes(conn, hash)?;
        let target = root.join(path);
        if !target.starts_with(root) {
            return Err(crate::bad(format!(
                "the tree entry {} resolves outside the repository",
                path
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &content)?;
        written += 1;
    }

    // Remove in-scope files that are on disk but no longer in the target tree.
    // Also remove empty in-scope directories.
    remove_stale_in_scope(root, scope, &in_scope_from_tree)?;

    Ok((written, kept))
}

/// Remove in-scope files that are on disk but no longer in the target tree.
/// Also prune empty in-scope directories.
fn remove_stale_in_scope(
    root: &Path,
    scope: &SparseScope,
    in_scope_from_tree: &std::collections::HashSet<String>,
) -> ZResult<()> {
    let mut stale_files = Vec::new();

    // Walk the repository root and collect in-scope files on disk.
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = match path.strip_prefix(root) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            if !scope.contains(&rel) {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !in_scope_from_tree.contains(&rel) {
                stale_files.push(path);
            }
        }
    }

    for file in &stale_files {
        let _ = std::fs::remove_file(file);
    }

    // Prune empty in-scope directories. Walk bottom-up so we can remove
    // directories after their children are gone.
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut is_empty = true;
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = match path.strip_prefix(root) {
                Ok(p) => p.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            if !scope.contains(&rel) {
                continue;
            }
            is_empty = false;
            if meta.is_dir() {
                stack.push(path);
            }
        }
        if is_empty && dir != root {
            dirs.push(dir);
        }
    }

    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in &dirs {
        let _ = std::fs::remove_dir(dir);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_entry_that_is_not_a_repo_path_is_refused_before_the_join() {
        // #98: `root.join(path)` with an entry read out of the tree. A tree can hold what a push or a
        // bundle restore put there, so the path is not necessarily one this binary wrote. This runs
        // the real `checkout_sparse` against a tree whose entry climbs, so the guard is what fails if
        // it is removed, not a restatement of the rule.
        use crate::store::tests::mem_db;

        let conn = mem_db();
        let hash = crate::store::put_object_bytes(&conn, b"owned\n").unwrap();
        // The row a corrupted or hostile history would carry.
        conn.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch)
             VALUES('cs1', 'x', 'a', 0, 'committed', NULL, NULL, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES('cs1', '../../escaped.txt', NULL, ?1)",
            rusqlite::params![hash],
        )
        .unwrap();
        // `mem_db` already creates `main`, so the head is moved rather than inserted.
        conn.execute("UPDATE branches SET head = 'cs1' WHERE name = 'main'", [])
            .unwrap();

        let dir = std::env::temp_dir().join(format!("gesta-sparse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The file the escape would create, one level above the repository root.
        let escaped = dir
            .parent()
            .expect("temp dir has a parent")
            .join("escaped.txt");
        let _ = std::fs::remove_file(&escaped);

        let scope = SparseScope::default();
        let err = match scope.checkout_sparse(&conn, &dir, "main") {
            Ok(_) => panic!("a climbing tree entry was checked out"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not a path inside the repository") && err.contains("escaped.txt"),
            "the refusal must name the entry: {err}"
        );
        assert!(!escaped.exists(), "nothing may be written outside the root");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_scope_contains_everything() {
        let s = SparseScope::default();
        assert!(s.contains("src/main.rs"));
        assert!(s.contains("anything/at/all.txt"));
    }

    #[test]
    fn a_directory_pattern_covers_its_subtree() {
        let s = SparseScope {
            patterns: vec!["crates/api".into()],
        };
        assert!(s.contains("crates/api"));
        assert!(s.contains("crates/api/src/main.rs"));
        assert!(!s.contains("crates/web/src/main.rs"));
        // A sibling whose name merely starts the same must not match.
        assert!(!s.contains("crates/api2/src/main.rs"));
    }

    #[test]
    fn a_glob_pattern_matches_across_depths() {
        let s = SparseScope {
            patterns: vec!["docs/**".into()],
        };
        assert!(s.contains("docs/a.md"));
        assert!(s.contains("docs/deep/nested/b.md"));
        assert!(!s.contains("src/a.rs"));
    }

    #[test]
    fn a_bare_name_pattern_matches_at_any_depth() {
        let s = SparseScope {
            patterns: vec!["*.md".into()],
        };
        assert!(s.contains("README.md"));
        assert!(s.contains("docs/guide.md"));
        assert!(!s.contains("src/main.rs"));
    }

    #[test]
    fn filtering_a_tree_keeps_only_in_scope_paths() {
        let s = SparseScope {
            patterns: vec!["src/**".into()],
        };
        let tree = [
            "src/a.rs".to_string(),
            "docs/b.md".to_string(),
            "src/c/d.rs".to_string(),
        ];
        let out = s.filter(tree.iter());
        assert_eq!(out, vec!["src/a.rs".to_string(), "src/c/d.rs".to_string()]);
    }

    #[test]
    fn a_scope_survives_a_save_and_load() {
        let conn = crate::store::tests::mem_db();
        assert!(SparseScope::load(&conn).unwrap().is_empty());
        let s = SparseScope {
            patterns: vec!["src/**".into(), "docs".into()],
        };
        s.save(&conn).unwrap();
        assert_eq!(SparseScope::load(&conn).unwrap(), s);
        SparseScope::clear(&conn).unwrap();
        assert!(SparseScope::load(&conn).unwrap().is_empty());
    }

    #[test]
    fn two_patterns_combine_rather_than_replacing() {
        let s = SparseScope {
            patterns: vec!["src/**".into(), "tests/**".into()],
        };
        assert!(s.contains("src/a.rs"));
        assert!(s.contains("tests/b.rs"));
        assert!(!s.contains("docs/c.md"));
    }
}
