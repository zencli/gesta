//! The git commands: import, export, push, pull.

use super::*;

/// Push this repository's branches to a git remote.
pub fn cmd_git_push(remote: &str, branch: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = crate::cli::git_bridge::push_to_git_remote(&conn, branch, remote)?;
    println!("pushed {} commit(s) to {}", out.commits, remote);
    if !out.branches.is_empty() {
        println!("branches: {}", out.branches.join(", "));
    }
    Ok(())
}

/// Fetch from a git remote and import what arrived.
pub fn cmd_git_pull(remote: &str, prefix: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let got = crate::cli::git_bridge::pull_from_git_remote(&conn, remote, prefix)?;
    println!(
        "imported {} commit(s), {} already present",
        got.commits, got.skipped
    );
    if !got.branches.is_empty() {
        println!("branches: {}", got.branches.join(", "));
    }
    Ok(())
}

/// Read a git repository into this one.
pub fn cmd_git_import(path: &str, prefix: Option<&str>) -> ZResult<()> {
    // The source is checked first. Opening the destination first meant a mistyped source path
    // reported "not a repository: / and no directory above it holds one", which names the directory
    // the tool was run from rather than the argument the person got wrong.
    let git_root = Path::new(path);
    crate::store::gitobj::Repo::open(git_root)?;
    let (_root, conn) = open_repo()?;
    let got = crate::cli::git_bridge::import(&conn, git_root, prefix)?;
    println!(
        "imported {} commit(s) as changesets, {} path change(s), {} already present",
        got.commits, got.files, got.skipped
    );
    if !got.branches.is_empty() {
        println!("branches: {}", got.branches.join(", "));
    }
    Ok(())
}

/// Write this repository out as a git repository.
pub fn cmd_git_export(path: &str, branch: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let out = crate::cli::git_bridge::export(&conn, Path::new(path), branch)?;
    println!("exported {} commit(s)", out.commits);
    if !out.branches.is_empty() {
        println!("branches: {}", out.branches.join(", "));
    }
    if !out.root.is_empty() {
        println!("head: {}", out.root);
    }
    Ok(())
}
