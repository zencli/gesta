//! Running a command at a well defined moment, such as before a commit.
//!
//! A hook is a program in the repository directory. It runs with the repository
//! root as its working directory and receives the operation name and the
//! changeset id as arguments, so a hook never has to guess what it is reacting to.
//!
//! Three rules keep a hook from turning into a way to lose data:
//!
//! 1. A failing pre-commit hook stops the commit, before anything is written.
//! 2. A failing post-commit hook is reported but does not undo the commit,
//!    because the commit already happened and pretending otherwise would leave
//!    the store and the report disagreeing.
//! 3. A hook cannot change what is being committed. It runs, it passes or it
//!    fails; the content was decided before it started.
//!
//! Hooks are opt in. Nothing runs unless a file with the hook name exists and is
//! executable, so a fresh repository has no chance of executing something it did
//! not intend to.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{bad, repo_dir, ZResult};

/// Hooks this program knows how to run.
/// The hook points a repository can install.
///
/// `pre-land` exists because landing a branch onto a shared target is the moment a change stops
/// being private, and it is the last point where a failing suite costs one person instead of
/// everybody. A commit hook runs per commit, which is too often to run a full suite and too early
/// to know whether the branch is going anywhere.
pub const HOOKS: &[&str] = &["pre-commit", "post-commit", "pre-land"];

/// Directory that holds hook programs, inside the repository directory.
pub fn hooks_dir(root: &Path) -> PathBuf {
    repo_dir(root).join("hooks")
}

pub fn path_of(root: &Path, name: &str) -> PathBuf {
    hooks_dir(root).join(name)
}

/// What a repository recorded about an installed hook when it was installed.
///
/// A hook is a program that runs with the user's rights, so a hook that changed after it was
/// installed is the interesting event: either its author edited it deliberately, or something
/// else did. Recording the hash at install time is what makes that question answerable without
/// asking the filesystem whether it looks suspicious.
pub fn record_installed(root: &Path, name: &str, body: &str) -> ZResult<()> {
    let dir = repo_dir(root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("hooks.installed");
    let mut lines: Vec<String> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            // Replace the entry for this hook, and keep the rest as they were.
            if let Some(rest) = line.strip_prefix(&format!("{}\t", name)) {
                let _ = rest;
                continue;
            }
            if !line.trim().is_empty() {
                lines.push(line.to_string());
            }
        }
    }
    lines.push(format!("{}\t{}", name, crate::store::hash_content(body)));
    lines.sort();
    std::fs::write(&path, lines.join("\n") + "\n")?;
    Ok(())
}

/// One hook's state: installed, what it hashed to when installed, and what it hashes to now.
pub struct HookState {
    pub name: String,
    pub installed: bool,
    pub recorded: Option<String>,
    pub current: Option<String>,
}

impl HookState {
    /// Whether the file changed after it was installed.
    pub fn changed(&self) -> bool {
        match (&self.recorded, &self.current) {
            (Some(r), Some(c)) => r != c,
            // Installed without a record, which an older repository can be: not "changed", but
            // not verifiable either.
            _ => false,
        }
    }
}

/// Compare every hook against what was recorded when it was installed.
pub fn status(root: &Path) -> ZResult<Vec<HookState>> {
    let record_path = repo_dir(root).join("hooks.installed");
    let mut recorded: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Ok(text) = std::fs::read_to_string(&record_path) {
        for line in text.lines() {
            if let Some((name, hash)) = line.split_once('\t') {
                recorded.insert(name.trim().to_string(), hash.trim().to_string());
            }
        }
    }
    let mut out = Vec::new();
    for name in HOOKS {
        let path = path_of(root, name);
        let installed = path.is_file();
        let current = if installed {
            std::fs::read(&path)
                .ok()
                .map(|b| crate::store::hash_bytes(&b))
        } else {
            None
        };
        out.push(HookState {
            name: name.to_string(),
            installed,
            recorded: recorded.get(*name).cloned(),
            current,
        });
    }
    Ok(out)
}

pub fn is_installed(root: &Path, name: &str) -> bool {
    path_of(root, name).is_file()
}

/// Run a hook if it exists.
///
/// Returns `Ok(())` when the hook is absent or exits zero, and an error carrying
/// the exit code and whatever the hook printed when it exits non-zero. The output
/// is passed through so a hook can explain itself in the terminal the user is
/// already looking at.
pub fn run(root: &Path, name: &str, args: &[&str]) -> ZResult<()> {
    let path = path_of(root, name);
    if !path.is_file() {
        return Ok(());
    }

    // Refuse to run a hook that no longer matches what was installed.
    //
    // A hook executes with the user's rights, so checking it only when somebody remembers to ask
    // protects nothing: the interesting case is the commit that is about to run it. A hook with
    // no recorded hash is allowed, because a repository can predate the record and refusing would
    // break it; the difference is reported by `hook status`, which is where somebody looks.
    if let Some(state) = status(root)?.into_iter().find(|s| s.name == name) {
        if state.changed() {
            return Err(bad(format!(
                "hook {} changed after it was installed; run `gesta hook status` to see both \
                 hashes, then reinstall it to accept the new body",
                name
            )));
        }
    }

    let out = Command::new(&path)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| bad(format!("hook {} could not run: {}", name, e)))?;

    // A hook's own output is the useful part of a failure, so it goes through rather than being
    // swallowed into an error string. It goes to stderr, which is where a hook's chatter belongs:
    // writing it to stdout put a line of prose in front of a JSON document, and a caller that
    // asked for `--json` got something no parser could read. A hook that wants to change the
    // committed content cannot, so it has nothing to say on stdout that a caller should read.
    if !out.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stdout));
    }
    if !out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }

    if out.status.success() {
        return Ok(());
    }
    let code = out
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "a signal".to_string());
    Err(bad(format!(
        "hook {} refused with exit code {}",
        name, code
    )))
}

/// Write a hook file and make it executable.
pub fn install(root: &Path, name: &str, body: &str) -> ZResult<PathBuf> {
    if !HOOKS.contains(&name) {
        return Err(bad(format!(
            "unknown hook {}: expected one of {}",
            name,
            HOOKS.join(", ")
        )));
    }
    let dir = hooks_dir(root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    std::fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
    }
    // The hash is taken from the bytes just written, not from re-reading the file, so a failure
    // here cannot record something different from what the hook contains.
    record_installed(root, name, body)?;
    Ok(path)
}

pub fn list(root: &Path) -> Vec<(String, bool)> {
    HOOKS
        .iter()
        .map(|h| (h.to_string(), is_installed(root, h)))
        .collect()
}

pub fn remove(root: &Path, name: &str) -> ZResult<bool> {
    let path = path_of(root, name);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gesta-hooks-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(crate::ZF_DIR)).unwrap();
        d
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_hook_is_not_an_error() {
        let d = tmp("absent");
        assert!(run(&d, "pre-commit", &[]).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_passing_hook_returns_ok() {
        let d = tmp("pass");
        install(&d, "pre-commit", "#!/bin/sh\nexit 0\n").unwrap();
        assert!(run(&d, "pre-commit", &[]).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_hook_reports_its_exit_code() {
        let d = tmp("fail");
        install(&d, "pre-commit", "#!/bin/sh\nexit 3\n").unwrap();
        let e = run(&d, "pre-commit", &[]).unwrap_err().to_string();
        assert!(e.contains("3"), "error should name the code, got {}", e);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_receives_the_operation_and_the_id() {
        let d = tmp("args");
        install(&d, "pre-commit", "#!/bin/sh\necho \"$1 $2\" > args.txt\n").unwrap();
        run(&d, "pre-commit", &["commit", "cs123"]).unwrap();
        let seen = std::fs::read_to_string(d.join("args.txt")).unwrap();
        assert_eq!(seen.trim(), "commit cs123");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unknown_hook_name_is_refused() {
        let d = tmp("unknown");
        assert!(install(&d, "post-checkout", "#!/bin/sh\n").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn installed_and_removed_are_visible_in_the_list() {
        let d = tmp("list");
        install(&d, "post-commit", "#!/bin/sh\n").unwrap();
        let found = list(&d);
        assert_eq!(
            found
                .iter()
                .find(|(n, _)| n == "post-commit")
                .map(|(_, i)| *i),
            Some(true)
        );
        assert!(remove(&d, "post-commit").unwrap());
        assert!(!remove(&d, "post-commit").unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
