//! Git submodule support.
//!
//! A submodule is a gitlink (mode 160000) in the parent tree that points to a commit in
//! another repository. The configuration lives in `.gitmodules`, which is part of the working
//! tree and therefore versioned like any other file. This module reads that file and makes
//! the submodules visible to the tree walk, the server, and the CLI.

use std::collections::HashMap;
use std::path::Path;

use crate::{bad, ZResult};

#[derive(Debug, Clone)]
pub struct Submodule {
    /// The path where the submodule is checked out, relative to the repository root.
    pub path: String,
    /// The remote URL to clone from.
    pub url: String,
    /// The branch to track (optional).
    pub branch: Option<String>,
    /// The commit the submodule is pinned to (from the parent's tree).
    pub commit: Option<String>,
}

/// Parse a `.gitmodules` file and return the submodules it defines.
///
/// The file is an ini-style format:
/// ```ini
/// [submodule "vendor/lib"]
///     path = vendor/lib
///     url = https://github.com/example/lib.git
///     branch = main
/// ```
pub fn parse_gitmodules(content: &str) -> ZResult<HashMap<String, Submodule>> {
    let mut out = HashMap::new();
    let mut current: Option<Submodule> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if let Some(sub) = current.take() {
                out.insert(sub.path.clone(), sub);
            }
            let inner = &trimmed[1..trimmed.len() - 1];
            if let Some(rest) = inner.strip_prefix("submodule \"") {
                if rest.ends_with('"') {
                    let name = rest.strip_suffix('"').unwrap_or(rest).to_string();
                    current = Some(Submodule {
                        path: name,
                        url: String::new(),
                        branch: None,
                        commit: None,
                    });
                }
            } else {
                current = None;
            }
        } else if let Some(ref mut sub) = current {
            if let Some((key, value)) = trimmed.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                match key {
                    "path" => sub.path = value.to_string(),
                    "url" => sub.url = value.to_string(),
                    "branch" => sub.branch = Some(value.to_string()),
                    "commit" => {
                        // Two pins for one submodule is a file that contradicts itself, and silently
                        // taking the first is how a repository pins two different commits and only
                        // ever checks out one of them. Say so instead.
                        if let Some(first) = &sub.commit {
                            return Err(bad(format!(
                                "submodule {} has two `commit` lines ({} and {}); keep one",
                                sub.path, first, value
                            )));
                        }
                        sub.commit = Some(value.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(sub) = current {
        out.insert(sub.path.clone(), sub);
    }
    Ok(out)
}

/// Read the `.gitmodules` file from the working tree.
pub fn read_gitmodules(root: &Path) -> ZResult<HashMap<String, Submodule>> {
    let path = root.join(".gitmodules");
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path)?;
    parse_gitmodules(&content)
}

/// Write a `.gitmodules` file from the given submodules.
pub fn write_gitmodules(root: &Path, subs: &HashMap<String, Submodule>) -> ZResult<()> {
    let path = root.join(".gitmodules");
    let mut content = String::new();
    // Sort for deterministic output
    let mut subs: Vec<_> = subs.values().collect();
    subs.sort_by(|a, b| a.path.cmp(&b.path));
    for sub in subs {
        content.push_str(&format!("[submodule \"{}\"]\n", sub.path));
        content.push_str(&format!("    path = {}\n", sub.path));
        content.push_str(&format!("    url = {}\n", sub.url));
        if let Some(branch) = &sub.branch {
            content.push_str(&format!(
                "    branch = {}
",
                branch
            ));
        }
        if let Some(commit) = &sub.commit {
            content.push_str(&format!(
                "    commit = {}
",
                commit
            ));
        }
        content.push('\n');
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Initialize submodules: clone missing ones at their pinned commit.
pub fn submodule_init(root: &Path) -> ZResult<Vec<String>> {
    let subs = read_gitmodules(root)?;
    let mut cloned = Vec::new();
    for sub in subs.values() {
        let dest = root.join(&sub.path);
        if !dest.exists() {
            // A child can be either kind of repository, and the two are cloned differently: git needs
            // `git clone`, ours needs a store and an import. Shelling out unconditionally was the bug
            // - `fatal: repository ... does not exist` on a perfectly good repository of our own,
            // which names the wrong problem entirely.
            clone_child(&sub.url, &dest)?;
            cloned.push(sub.path.clone());
        }
        // Checkout the pinned commit if we have one, by whichever mechanism the child needs. This
        // used to call `git checkout` unconditionally, so init on a child of our own kind cloned it
        // correctly and then failed on the checkout with `fatal: not a git repository`.
        if let Some(commit) = &sub.commit {
            if is_our_repo(&dest) {
                checkout_our_repo(&dest, commit, &sub.path)?;
            } else {
                checkout_git_repo(&dest, commit, &sub.path)?;
            }
        }
    }
    Ok(cloned)
}

/// Update submodules to their pinned commit.
///
/// Which mechanism depends on what the child is, the same way init does. `git -C <dest> fetch` on one
/// of our repositories fails with `fatal: not a git repository` - a message that is true and describes
/// the wrong problem, because the directory is a perfectly good repository of another kind.
pub fn submodule_update(root: &Path) -> ZResult<Vec<String>> {
    let subs = read_gitmodules(root)?;
    let mut updated = Vec::new();
    for sub in subs.values() {
        let dest = root.join(&sub.path);
        if !dest.exists() {
            continue; // init will clone it
        }
        let Some(commit) = &sub.commit else {
            continue;
        };
        if is_our_repo(&dest) {
            checkout_our_repo(&dest, commit, &sub.path)?;
        } else {
            checkout_git_repo(&dest, commit, &sub.path)?;
        }
        updated.push(sub.path.clone());
    }
    Ok(updated)
}

/// Whether a directory holds a repository of this kind rather than a git one.
fn is_our_repo(path: &Path) -> bool {
    path.join(crate::ZF_DIR).is_dir() || path.join(crate::LEGACY_DIR).is_dir()
}

/// Move one of our repositories to a named changeset, writing the tree out to match.
fn checkout_our_repo(dest: &Path, commit: &str, sub_path: &str) -> ZResult<()> {
    let conn = crate::store::open_db(dest)?;
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM changesets WHERE id = ?1",
        rusqlite::params![commit],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(bad(format!(
            "the pin {} of submodule {} is not in the child's history",
            &commit[..commit.len().min(14)],
            sub_path
        )));
    }
    crate::repo::restore_at_changeset(&conn, dest, commit)?;
    Ok(())
}

/// Move a git repository to a named commit, fetching it first so a pin from another machine resolves.
fn checkout_git_repo(dest: &Path, commit: &str, sub_path: &str) -> ZResult<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .arg("fetch")
        .arg("--quiet")
        .arg("origin")
        .status()?;
    if !status.success() {
        return Err(bad(format!("failed to fetch submodule {}", sub_path)));
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .arg("checkout")
        .arg("--quiet")
        .arg(commit)
        .status()?;
    if !status.success() {
        return Err(bad(format!(
            "failed to checkout {} in submodule {}",
            commit, sub_path
        )));
    }
    Ok(())
}

/// Checkout paths of every submodule, for the walk to skip.
///
/// A submodule's files belong to the child repository. Recording them here would put another
/// project's content in this one's history, and a later change in the child would look like a
/// change in the parent.
pub fn excluded_paths(root: &Path) -> Vec<String> {
    read_gitmodules(root)
        .map(|subs| subs.into_values().map(|s| s.path).collect())
        .unwrap_or_default()
}

/// Does the checked-out commit match the pin, allowing the pin to be an abbreviation?
///
/// A `.gitmodules` written by hand names the short id from `git log`, and comparing a short id
/// against a full one reports every pinned submodule as behind.
pub fn matches_pin(pinned: Option<&str>, commit: &str) -> bool {
    match pinned {
        None => true,
        Some(p) => !p.is_empty() && commit.starts_with(p),
    }
}

/// Status of submodules: which are clean, which have new commits, which are missing.
pub struct SubmoduleStatus {
    pub path: String,
    pub commit: String,
    pub pinned: Option<String>,
    pub dirty: bool,
    pub missing: bool,
}

pub fn submodule_status(root: &Path) -> ZResult<Vec<SubmoduleStatus>> {
    let subs = read_gitmodules(root)?;
    let mut out = Vec::new();
    for sub in subs.values() {
        let dest = root.join(&sub.path);
        let missing = !dest.exists();
        let (commit, dirty) = if missing {
            (String::new(), false)
        } else {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dest)
                .arg("rev-parse")
                .arg("HEAD")
                .output()?;
            let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dest)
                .arg("status")
                .arg("--porcelain")
                .output()?;
            let dirty = !status.stdout.is_empty();
            (commit, dirty)
        };
        out.push(SubmoduleStatus {
            path: sub.path.clone(),
            commit,
            pinned: sub.commit.clone(),
            dirty,
            missing,
        });
    }
    Ok(out)
}

/// Clone a child repository, whichever kind it is.
///
/// A path that holds a gesta store is imported; anything else goes to `git clone`. The check is on the
/// source's contents rather than on the URL scheme, because a local path is the common case and a
/// scheme says nothing about what is on the other end.
fn clone_child(url: &str, dest: &Path) -> ZResult<()> {
    let src = Path::new(url);
    if src.is_dir() && (src.join(crate::ZF_DIR).is_dir() || src.join(crate::LEGACY_DIR).is_dir()) {
        return import_child(src, dest);
    }
    let status = std::process::Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg(url)
        .arg(dest)
        .status()?;
    if !status.success() {
        return Err(bad(format!(
            "could not clone {url}: git clone failed, and the source is not a repository store of this kind either"
        )));
    }
    Ok(())
}

/// Make `dest` a working copy of the repository at `src`.
///
/// The store is copied whole rather than replayed changeset by changeset: a submodule clone is the
/// child's repository, not a summary of it, and copying preserves every object, branch and blame
/// range without a translation step that could lose one. `VACUUM INTO` writes a consistent database
/// from a live connection, which a plain file copy cannot promise with a WAL beside it.
fn import_child(src: &Path, dest: &Path) -> ZResult<()> {
    std::fs::create_dir_all(dest.join(crate::ZF_DIR))?;
    let from = crate::store::open_db(src)?;
    let target = dest.join(crate::ZF_DIR).join(crate::ZF_DB);
    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    from.execute(
        "VACUUM INTO ?1",
        rusqlite::params![target.to_string_lossy()],
    )?;
    Ok(())
}

/// The head of the repository at `path`, whichever kind it is.
///
/// Used when `submodule add` records the pin. Without it the entry kept `commit: None`, `status`
/// reported `unpinned` forever, and `update` had nothing to return to.
pub fn child_head(path: &Path) -> ZResult<Option<String>> {
    if path.join(crate::ZF_DIR).is_dir() || path.join(crate::LEGACY_DIR).is_dir() {
        let conn = crate::store::open_db(path)?;
        let branch = crate::store::current_branch(&conn)?;
        return crate::store::branch_head(&conn, &branch);
    }
    if path.join(".git").exists() || path.join("objects").is_dir() {
        let repo = crate::store::gitobj::Repo::open(path)?;
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("rev-parse")
            .arg("HEAD")
            .output()?;
        let _ = repo;
        if out.status.success() {
            return Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ));
        }
    }
    Ok(None)
}
