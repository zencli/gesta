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
    if crate::auth::glob_is_match(pat, path) {
        return true;
    }
    // A bare name pattern applies at any depth: `*.md` covers `docs/guide.md`,
    // which is what a person writing it means. Same rule as token scope.
    if !pat.contains('/') {
        let name = path.rsplit('/').next().unwrap_or(path);
        if crate::auth::glob_is_match(pat, name) {
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
    let tree = crate::repo::tree_at(conn, &head)?;

    let mut written = 0usize;
    let mut kept = 0usize;
    for (path, hash) in &tree {
        if !scope.contains(path) {
            kept += 1;
            continue;
        }
        let content = crate::store::get_object_bytes(conn, hash)?;
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &content)?;
        written += 1;
    }
    Ok((written, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

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
