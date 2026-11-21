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
                    "commit" => sub.commit = Some(value.to_string()),
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
            let mut cmd = std::process::Command::new("git");
            cmd.arg("clone");
            cmd.arg("--quiet");
            if let Some(branch) = &sub.branch {
                cmd.arg("--branch").arg(branch);
            }
            cmd.arg(&sub.url).arg(&dest);
            let status = cmd.status()?;
            if !status.success() {
                return Err(bad(format!("failed to clone submodule {}", sub.path)));
            }
            cloned.push(sub.path.clone());
        }
        // Checkout the pinned commit if we have one
        if let Some(commit) = &sub.commit {
            let mut cmd = std::process::Command::new("git");
            cmd.arg("-C")
                .arg(&dest)
                .arg("checkout")
                .arg("--quiet")
                .arg(commit);
            let status = cmd.status()?;
            if !status.success() {
                return Err(bad(format!(
                    "failed to checkout {} in submodule {}",
                    commit, sub.path
                )));
            }
        }
    }
    Ok(cloned)
}

/// Update submodules to their pinned commit.
pub fn submodule_update(root: &Path) -> ZResult<Vec<String>> {
    let subs = read_gitmodules(root)?;
    let mut updated = Vec::new();
    for sub in subs.values() {
        let dest = root.join(&sub.path);
        if !dest.exists() {
            continue; // init will clone it
        }
        if let Some(commit) = &sub.commit {
            let mut cmd = std::process::Command::new("git");
            cmd.arg("-C")
                .arg(&dest)
                .arg("fetch")
                .arg("--quiet")
                .arg("origin");
            let status = cmd.status()?;
            if !status.success() {
                return Err(bad(format!("failed to fetch submodule {}", sub.path)));
            }
            let mut cmd = std::process::Command::new("git");
            cmd.arg("-C")
                .arg(&dest)
                .arg("checkout")
                .arg("--quiet")
                .arg(commit);
            let status = cmd.status()?;
            if !status.success() {
                return Err(bad(format!(
                    "failed to checkout {} in submodule {}",
                    commit, sub.path
                )));
            }
            updated.push(sub.path.clone());
        }
    }
    Ok(updated)
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
