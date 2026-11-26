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
/// The hook points this program runs.
///
/// The set is chosen by where a decision still matters, not by what git happens to name:
///
///   pre-changeset  before a snapshot is taken, so a hook can refuse a tree it does not want recorded
///   pre-commit     before a snapshot becomes a changeset
///   post-commit    after it landed, for the notification that should not be able to block it
///   pre-land       before a branch merges onto a shared target, where a failing suite costs one
///                  person instead of everybody
///   post-checkout  after a working tree moved, to rebuild whatever the new branch needs
///   pre-push       before anything leaves the machine
///
/// A hook that is not installed is not an error: `run` returns immediately. That is what lets a
/// repository grow hooks one at a time instead of installing six no-ops on the first day.
pub const HOOKS: &[&str] = &[
    "pre-changeset",
    "pre-commit",
    "post-commit",
    "pre-land",
    "post-checkout",
    "pre-push",
];

/// Directory that holds hook programs, inside the repository directory.
pub fn hooks_dir(root: &Path) -> PathBuf {
    repo_dir(root).join("hooks")
}

/// The git name for a hook point, where the two differ.
///
/// Three points share git's name (`pre-commit`, `post-commit`, `pre-push`), one does not exist in git
/// (`pre-land`, which is the review step), and `pre-changeset` is closest to `pre-commit` but fires
/// before a snapshot rather than before it lands. Returning None means the point has no git
/// equivalent and only this tool's own hook directory is consulted.
pub fn git_name(name: &str) -> Option<&'static str> {
    match name {
        "pre-commit" => Some("pre-commit"),
        "post-commit" => Some("post-commit"),
        "pre-push" => Some("pre-push"),
        "pre-changeset" => Some("pre-commit"),
        "post-checkout" => Some("post-checkout"),
        _ => None,
    }
}

/// Whether a hook file at this path would actually run.
///
/// The module promises that nothing runs "unless a file with the hook name exists and is
/// executable", so executable is part of the test, not a detail of what happens afterwards. On
/// Unix a file that exists without an execute bit is a draft or a hook somebody disabled by
/// clearing the bit; handing it to `Command::new` fails with permission denied and turns a
/// deliberate "off" into a refusal that blocks every commit, land and push.
///
/// Windows has no execute bit: a file runs by its extension, and the hook name already carries
/// `.cmd`, so existence is the whole test there.
fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(m) => m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A hook program in `.git/hooks`, when the repository sits beside a git checkout.
///
/// Not a compatibility layer with arguments rewritten: a git hook expects the same three arguments
/// git passes (a ref update, a list of paths), and this tool has nothing like them. What it does honour
/// is that a hook written for git and marked executable runs when the matching point fires, because a
/// guard an author already wrote should not silently never run.
pub fn git_hook(root: &Path, name: &str) -> Option<PathBuf> {
    let git = git_name(name)?;
    // The extension matters here for the same reason it does for an installed hook: on Windows a
    // `.cmd` next to the plain name is the one that can run, and the plain name is the one that
    // cannot. Both are offered so a repository written by git for Windows is still found.
    let mut names = vec![git.to_string()];
    if cfg!(windows) {
        names.insert(0, format!("{}.cmd", git));
    }
    names
        .into_iter()
        .flat_map(|n| {
            [
                root.join(".git").join("hooks").join(&n),
                root.join(".git").join(&n),
            ]
        })
        .find(|c| c.is_file())
}

/// The file name a hook is stored under.
///
/// Windows decides how a file runs by its extension, not by anything inside it, and `cmd /C` will
/// only run a `.cmd` or `.bat`. So the same hook is `pre-commit` on Unix and `pre-commit.cmd` on
/// Windows, and one function answers which - otherwise `install`, `run`, `sources` and `remove`
/// each get to disagree about the name, which is how a hook gets installed and never found.
fn hook_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{}.cmd", name)
    } else {
        name.to_string()
    }
}

pub fn path_of(root: &Path, name: &str) -> PathBuf {
    hooks_dir(root).join(hook_file_name(name))
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
    /// The program comes from `.git/hooks` rather than this tool's own directory.
    ///
    /// Reported because otherwise `hook list` shows a point as not installed while a guard for it
    /// runs on every commit. "Not installed" and "installed somewhere else" look identical from the
    /// outside and mean opposite things.
    pub from_git: bool,
}

impl HookState {
    /// Whether the file changed after it was installed.
    pub fn changed(&self) -> bool {
        // A git hook has no record, because this tool did not install it. Reporting it as changed
        // would make every git hook look tampered with.
        if self.from_git {
            return false;
        }
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
        // `installed` here means "would run", the same question `run` asks. A file whose execute bit
        // was cleared is present but off, and reporting it as installed would put `hook status` at
        // odds with a commit that silently skipped it.
        let own = is_executable(&path);
        let git = if own {
            None
        } else {
            git_hook(root, name).filter(|p| is_executable(p))
        };
        let installed = own || git.is_some();
        let source = if own { Some(path) } else { git };
        let current = source
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .map(|b| crate::store::hash_bytes(&b));
        out.push(HookState {
            name: name.to_string(),
            installed,
            recorded: recorded.get(*name).cloned(),
            current,
            from_git: !own && installed,
        });
    }
    Ok(out)
}

/// Run a hook if it exists.
///
/// Returns `Ok(())` when the hook is absent or exits zero, and an error carrying
/// the exit code and whatever the hook printed when it exits non-zero. The output
/// is passed through so a hook can explain itself in the terminal the user is
/// already looking at.
pub fn run(root: &Path, name: &str, args: &[&str]) -> ZResult<()> {
    let path = path_of(root, name);
    // A hook installed here takes precedence. When there is none, a git hook at the matching point
    // runs: an author who already wrote that guard should not find it silently ignored just because
    // this tool keeps its hooks in a different directory.
    //
    // `is_executable`, not `is_file`: a file that exists without the execute bit is a hook somebody
    // turned off, and running it anyway would block every commit with a permission error instead of
    // doing nothing. See `is_executable` for why that distinction is the whole point.
    let (path, from_git) = if is_executable(&path) {
        (path, false)
    } else {
        match git_hook(root, name) {
            // git itself ignores a hook file without the execute bit, so honouring one here would
            // run something git would not - and block the operation doing it.
            Some(p) if is_executable(&p) => (p, true),
            _ => return Ok(()),
        }
    };

    // Refuse to run a hook that no longer matches what was installed.
    //
    // A hook executes with the user's rights, so checking it only when somebody remembers to ask
    // protects nothing: the interesting case is the commit that is about to run it. A hook with
    // no recorded hash is allowed, because a repository can predate the record and refusing would
    // break it; the difference is reported by `hook status`, which is where somebody looks.
    //
    // A git hook has no recorded hash, because this tool did not install it. Its integrity is the
    // git checkout's business, and hashing it here would refuse every git hook on first sight.
    if !from_git {
        if let Some(state) = status(root)?.into_iter().find(|s| s.name == name) {
            if state.changed() {
                return Err(bad(format!(
                    "hook {} changed after it was installed; run `gesta hook status` to see both \
                     hashes, then reinstall it to accept the new body",
                    name
                )));
            }
        }
    }

    // On Windows a hook is a script, and Windows has no shebang: `Command::new` on a file without a
    // `.exe` extension fails with `%1 is not a valid Win32 application (os error 193)`. So every hook
    // was unrunnable there - which nobody noticed because every hook test was `#[cfg(unix)]`, so the
    // path was never executed on the one platform where it could not work. A batch file has to be
    // handed to the interpreter.
    //
    // `cmd /C` is the interpreter Windows always has, and it is what runs a `.bat` or `.cmd` hook.
    // The path is passed as its own argument so a space in a directory name does not split it.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(&path);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new(&path);

    let out = cmd
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
    let path = dir.join(hook_file_name(name));
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

/// The hook points and where each program comes from, for a reader who needs to know what runs.
pub fn sources(root: &Path) -> Vec<(String, Option<&'static str>)> {
    HOOKS
        .iter()
        .map(|h| {
            let source = if is_executable(&path_of(root, h)) {
                Some("installed")
            } else if git_hook(root, h).filter(|p| is_executable(p)).is_some() {
                Some("git")
            } else {
                None
            };
            (h.to_string(), source)
        })
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
        // A counter as well as the tag: two test binaries can run at once, and a directory left
        // behind by a previous run with the same pid is the one the next run deletes mid-test.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("gesta-hooks-{}-{}-{}", tag, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(crate::ZF_DIR)).unwrap();
        d
    }

    #[cfg(unix)]
    #[test]
    fn a_hook_without_the_execute_bit_is_skipped_not_refused() {
        // The module promises nothing runs "unless a file exists and is executable". A file that
        // exists without the bit is a hook somebody turned off, and refusing the operation over it
        // blocks every commit for a guard the author deliberately disabled.
        let d = tmp("noexec");
        let path = path_of(&d, "pre-commit");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
        }

        // A body that fails when run proves the difference: skipping is not "it happened to pass".
        assert!(
            run(&d, "pre-commit", &[]).is_ok(),
            "a non-executable hook must be treated as absent, not as a refusal"
        );
        // And it is not reported as installed, or `hook status` would disagree with the commit.
        let sources = sources(&d);
        assert_eq!(
            sources
                .iter()
                .find(|(n, _)| n == "pre-commit")
                .map(|(_, s)| *s),
            Some(None),
            "a hook that would not run must not be listed as a source"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn making_the_execute_bit_fail_makes_the_hook_run_again() {
        // The complement of the test above: set the bit and the same body must now run and fail.
        // Without this, a fix that skipped every hook would pass the skip test.
        let d = tmp("chan");
        let path = path_of(&d, "pre-commit");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\nexit 7\n").unwrap();
        record_installed(&d, "pre-commit", "#!/bin/sh\nexit 7\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        assert!(
            run(&d, "pre-commit", &[]).is_ok(),
            "off while not executable"
        );

        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        let e = run(&d, "pre-commit", &[]).unwrap_err().to_string();
        assert!(
            e.contains('7'),
            "with the bit set the hook runs and its code is reported, got {}",
            e
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_hook_is_not_an_error() {
        let d = tmp("absent");
        assert!(run(&d, "pre-commit", &[]).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A hook body that succeeds, written in the platform's own language.
    ///
    /// These three tests were all `#[cfg(unix)]`, which is why nobody noticed that a hook could not
    /// run on Windows at all: `Command::new` on a file with no executable extension fails there, and
    /// the only tests that executed a hook were skipped on that platform. A hook is a script, so each
    /// platform needs its own spelling.
    fn ok_hook() -> String {
        if cfg!(windows) {
            "@echo off\r\nexit /b 0\r\n".to_string()
        } else {
            "#!/bin/sh\nexit 0\n".to_string()
        }
    }

    fn fail_hook(code: i32) -> String {
        if cfg!(windows) {
            format!("@echo off\r\nexit /b {}\r\n", code)
        } else {
            format!("#!/bin/sh\nexit {}\n", code)
        }
    }

    /// A hook that writes its two arguments to `args.txt`.
    fn arg_echo_hook() -> String {
        if cfg!(windows) {
            "@echo off\r\necho %1 %2 > args.txt\r\n".to_string()
        } else {
            "#!/bin/sh\necho \"$1 $2\" > args.txt\n".to_string()
        }
    }

    #[test]
    fn a_passing_hook_returns_ok() {
        let d = tmp("pass");
        install(&d, "pre-commit", &ok_hook()).unwrap();
        assert!(run(&d, "pre-commit", &[]).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failing_hook_reports_its_exit_code() {
        let d = tmp("fail");
        install(&d, "pre-commit", &fail_hook(3)).unwrap();
        let e = run(&d, "pre-commit", &[]).unwrap_err().to_string();
        assert!(e.contains('3'), "error should name the code, got {}", e);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_hook_receives_the_operation_and_the_id() {
        let d = tmp("args");
        install(&d, "pre-commit", &arg_echo_hook()).unwrap();
        run(&d, "pre-commit", &["commit", "cs123"]).unwrap();
        let seen = std::fs::read_to_string(d.join("args.txt")).unwrap();
        assert_eq!(seen.trim(), "commit cs123");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unknown_hook_name_is_refused() {
        let d = tmp("unknown");
        // `post-checkout` used to be the example here, and it stopped being unknown when the point
        // was added. A test whose subject is "not a hook name" has to use something that is not one.
        assert!(install(&d, "mid-rebase-and-then-some", "#!/bin/sh\n").is_err());
        // The known points are still accepted, so the refusal is about the name and not the call.
        assert!(install(&d, "post-checkout", "#!/bin/sh\n").is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn installed_and_removed_are_visible_in_the_list() {
        let d = tmp("list");
        install(&d, "post-commit", "#!/bin/sh\n").unwrap();
        let found = sources(&d);
        assert_eq!(
            found
                .iter()
                .find(|(n, _)| n == "post-commit")
                .map(|(_, s)| s.is_some()),
            Some(true)
        );
        assert!(remove(&d, "post-commit").unwrap());
        assert!(!remove(&d, "post-commit").unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
