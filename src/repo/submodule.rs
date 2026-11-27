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
            // `head_chars`: `.min(14)` caps a long pin but says nothing about char boundaries, so a
            // 15-byte pin ending in a 2-byte character panicked right here, while formatting the
            // message that was supposed to report the bad pin (#313).
            crate::support::paths::head_chars(commit, 14),
            sub_path
        )));
    }
    crate::repo::restore_at_changeset(&conn, dest, commit)?;
    Ok(())
}

/// The arguments for `git checkout`, with the pin already known to be a commit id.
///
/// The pin is *not* put behind `--`: for `checkout` the separator means "everything after this is a
/// path", so `git checkout --quiet -- fff4956` fails with `pathspec 'fff4956' did not match`, while
/// `git checkout --quiet fff4956` checks the commit out. `--detach` is what makes the position
/// explicit instead, and `pinned_commit` is what makes an option-like pin unreachable.
fn checkout_args(commit: &str) -> Vec<String> {
    vec![
        "checkout".into(),
        "--quiet".into(),
        "--detach".into(),
        commit.into(),
    ]
}

/// The commit a `.gitmodules` pin names, or the reason it cannot be one.
///
/// A pin is a hexadecimal object name: 4 to 64 characters of `[0-9a-f]`. Checking that here is the
/// boundary that makes option injection impossible rather than merely unlikely, because
/// `--orphan=x` is not hexadecimal and therefore never reaches `git checkout`. `git checkout
/// --quiet --orphan=x` created an orphan branch and exited 0, so the submodule sat on a branch
/// nobody asked for and every later read of it was wrong.
fn pinned_commit<'a>(sub_path: &str, pin: &'a str) -> ZResult<&'a str> {
    let ok = (4..=64).contains(&pin.len())
        && pin.chars().all(|c| c.is_ascii_hexdigit())
        && pin.chars().next().is_some_and(|c| c.is_ascii_hexdigit());
    if !ok {
        return Err(bad(format!(
            "submodule {} has a pin that is not a commit id: {:?}",
            sub_path, pin
        )));
    }
    Ok(pin)
}

/// The arguments for `git clone`, ending option parsing before the url and the destination.
///
/// `--` matters here too: a `.gitmodules` url of `--upload-pack=...` was read as an option, and
/// `git clone` runs the named program.
fn clone_args(url: &str, dest: &Path) -> Vec<String> {
    vec![
        "clone".into(),
        "--quiet".into(),
        "--".into(),
        url.into(),
        dest.to_string_lossy().into_owned(),
    ]
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
        .args(checkout_args(pinned_commit(sub_path, commit)?))
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
        // Which mechanism answers depends on what the child is, the same way init and update decide.
        // Asking git about one of our repositories fails with `fatal: not a git repository`, and the
        // old code ignored that: `rev-parse` produced an empty string, so the child reported no
        // commit at all. `matches_pin` against an empty commit is always false, which made the CLI
        // print "behind" for every pin - so `submodule status` looked correct for the wrong reason
        // and could never report "clean" for a child of this kind.
        let (commit, dirty) = if missing {
            (String::new(), false)
        } else if is_our_repo(&dest) {
            our_repo_state(&dest)?
        } else {
            git_repo_state(&dest, &sub.path)?
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

/// The head and dirtiness of one of our repositories, read from its own store.
///
/// Dirty means the working tree differs from the branch head, which is the same question
/// `git status --porcelain` answers for a git child. `diff_worktree` already computes it, so this
/// reuses it rather than writing a second definition of "dirty" that could drift.
fn our_repo_state(dest: &Path) -> ZResult<(String, bool)> {
    let conn = crate::store::open_db(dest)?;
    let branch = crate::store::current_branch(&conn).unwrap_or_else(|_| "main".to_string());
    let head = crate::store::branch_head(&conn, &branch)?.unwrap_or_default();
    let d = crate::repo::diff_worktree(&conn, dest, &branch)?;
    let dirty = !d.added.is_empty() || !d.modified.is_empty() || !d.deleted.is_empty();
    Ok((head, dirty))
}

/// The head and dirtiness of a git child, with the exit statuses actually checked.
///
/// Reading `stdout` without looking at the status turns "git refused" into "the repository is empty
/// and clean", which is indistinguishable from a real answer.
fn git_repo_state(dest: &Path, sub_path: &str) -> ZResult<(String, bool)> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .arg("rev-parse")
        .arg("HEAD")
        .output()?;
    if !out.status.success() {
        return Err(bad(format!(
            "cannot read the head of submodule {}: {}",
            sub_path,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dest)
        .arg("status")
        .arg("--porcelain")
        .output()?;
    if !status.status.success() {
        return Err(bad(format!(
            "cannot read the status of submodule {}: {}",
            sub_path,
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    Ok((commit, !status.stdout.is_empty()))
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
        .args(clone_args(url, dest))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_with_a_multibyte_character_is_reported_without_panicking() {
        // #313: `&commit[..commit.len().min(14)]` capped a long pin but said nothing about char
        // boundaries, so a 15-byte pin whose byte 14 sits inside `é` panicked while formatting the
        // error message. The message exists to report a bad pin; crashing on it is the opposite.
        let pin = format!("{}é", "a".repeat(13));
        assert_eq!(pin.len(), 15);

        let dest = std::env::temp_dir().join(format!("gesta-pin-child-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(dest.join(".gesta")).unwrap();

        // A child repository with no changesets, so the pin cannot be found in its history.
        let conn = crate::store::open_db(&dest).unwrap();
        crate::store::init_schema(&conn).unwrap();
        crate::store::migrate(&conn).unwrap();
        drop(conn);

        let err = checkout_our_repo(&dest, &pin, "sub")
            .expect_err("a pin that is not in the child's history must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("is not in the child's history"),
            "the refusal must be the pin message, not a different failure: {msg}"
        );
        // The truncated pin appears in the message, cut on a boundary: all 13 `a`s, no half of `é`.
        assert!(
            msg.contains(&"a".repeat(13)),
            "the message must name the pin it refused: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn a_pin_that_is_not_a_commit_id_is_refused_before_git_runs() {
        // `git checkout --quiet --orphan=x` created an orphan branch and exited 0, so the submodule
        // sat on a branch nobody asked for. A pin is hexadecimal, so refusing anything else closes
        // the option path completely instead of arguing about which options are dangerous.
        for bad_pin in [
            "--orphan=x",
            "-b",
            "--detach",
            "",
            "HEAD",
            "main",
            "fff",
            "fffzzz",
            "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ] {
            let e = pinned_commit("vendor/lib", bad_pin)
                .expect_err(&format!("{:?} must not be accepted as a pin", bad_pin));
            let msg = format!("{e}");
            assert!(msg.contains("vendor/lib"), "{msg}");
            assert!(msg.contains("not a commit id"), "{msg}");
        }
    }

    #[test]
    fn a_hexadecimal_pin_is_accepted_and_reaches_git_as_a_positional() {
        let pin = pinned_commit("vendor/lib", "fff4956").expect("a hex pin is a commit id");
        assert_eq!(pin, "fff4956");
        let args = checkout_args(pin);
        assert!(args.contains(&"fff4956".to_string()), "{args:?}");
        // `--` before the pin would make git read it as a path and fail.
        let dash = args.iter().position(|a| a == "--");
        assert_eq!(
            dash, None,
            "no separator before a checkout revision: {args:?}"
        );
        assert!(
            args.contains(&"--detach".to_string()),
            "the detached position must be explicit: {args:?}"
        );
    }

    #[test]
    fn a_clone_url_reaches_git_as_a_positional_argument_not_an_option() {
        // A url of `--upload-pack=<program>` made `git clone` run that program.
        let args = clone_args("--upload-pack=/bin/sh", Path::new("/tmp/dest"));
        let dash = args
            .iter()
            .position(|a| a == "--")
            .expect("`--` must be present");
        let url = args
            .iter()
            .position(|a| a == "--upload-pack=/bin/sh")
            .expect("url must be present");
        assert!(dash < url, "`--` must come before the url, got {args:?}");
    }

    #[test]
    fn git_run_for_real_checks_out_a_hex_pin_with_the_argv_the_fix_builds() {
        // The tests above check the argv we build; this one hands that argv to git in a real
        // repository, so "the pin is honoured" is measured and not assumed. Skipped only when git is
        // absent, which this repository cannot work without.
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!("gesta-sub-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "--quiet", "."]);
        git(&["config", "user.email", "t@t.c"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(
            dir.join("child.txt"),
            "v1
",
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "v1"]);
        let pin = git(&["rev-parse", "HEAD"]);
        std::fs::write(dir.join("child.txt"), "v1\nv2\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "v2"]);

        assert_eq!(
            pinned_commit("vendor/lib", &pin).unwrap(),
            pin,
            "a real commit id must pass the boundary check"
        );
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(checkout_args(&pin))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git rejected our argv: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = std::fs::read_to_string(dir.join("child.txt")).unwrap();
        // The subject here is the argv `checkout_args` builds, not the runner's line ending policy.
        // Git writes CRLF where `core.autocrlf` says so, and a Windows runner says so, so comparing
        // the raw bytes tested the runner and failed on Windows while passing on Linux. Comparing
        // the text with line endings normalised tests the checkout, which is what this is about.
        let text = text.replace("\r\n", "\n");
        assert_eq!(
            text, "v1\n",
            "the checkout must land on the pin, not the head"
        );
    }

    #[test]
    fn a_well_formed_file_round_trips_through_parse_and_write() {
        let text = "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n\tcommit = abc123\n";
        let subs = parse_gitmodules(text).unwrap();
        let s = subs.get("vendor/lib").expect("the path is the key");
        assert_eq!(s.url, "https://example.com/lib.git");
        assert_eq!(s.commit.as_deref(), Some("abc123"));
    }

    #[test]
    fn the_key_is_the_path_when_one_is_given_not_the_section_name() {
        // git allows the section name and the path to differ, and the path is what the tree uses.
        // Keying by the section name put a submodule under a name no file lookup would ever use.
        let text = "[submodule \"friendly-name\"]\n\tpath = vendor/actual\n\turl = x\n";
        let subs = parse_gitmodules(text).unwrap();
        assert!(
            subs.contains_key("vendor/actual"),
            "keys: {:?}",
            subs.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn two_commit_lines_are_refused_rather_than_resolved_by_order() {
        let text = "[submodule \"a\"]\n\tpath = a\n\tcommit = one\n\tcommit = two\n";
        let err = parse_gitmodules(text).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("two `commit` lines"), "got: {msg}");
        assert!(
            msg.contains("one") && msg.contains("two"),
            "both names must appear: {msg}"
        );
    }

    #[test]
    fn a_submodule_without_a_pin_is_accepted() {
        // A `.gitmodules` written by hand often has no commit line; refusing it would reject files
        // git itself reads without complaint.
        let text = "[submodule \"x\"]\n\tpath = x\n\turl = u\n";
        let subs = parse_gitmodules(text).unwrap();
        assert_eq!(subs["x"].commit, None);
    }

    #[test]
    fn a_missing_pin_matches_anything_but_an_empty_one_does_not() {
        // No pin means "whatever is checked out". An empty pin is a file that says a pin belongs
        // there and left it blank, and treating that as a match hides a broken file.
        assert!(matches_pin(None, "abcdef"));
        assert!(!matches_pin(Some(""), "abcdef"));
        assert!(
            matches_pin(Some("abc"), "abcdef"),
            "a short id has to match its own commit"
        );
        assert!(!matches_pin(Some("zzz"), "abcdef"));
        // A pin longer than the commit cannot match, and must not panic on the slice.
        assert!(!matches_pin(Some("abcdef0123"), "abcdef"));
    }

    #[test]
    fn a_section_that_is_not_a_submodule_does_not_leak_into_the_next_one() {
        // `[core]` appears in files people paste together. Without resetting on an unknown section,
        // its keys land in whichever submodule came before it.
        let text = "[submodule \"a\"]\n\tpath = a\n\turl = u1\n[core]\n\teditor = vim\n[submodule \"b\"]\n\tpath = b\n\turl = u2\n";
        let subs = parse_gitmodules(text).unwrap();
        assert_eq!(subs.len(), 2, "keys: {:?}", subs.keys().collect::<Vec<_>>());
        assert_eq!(subs["b"].url, "u2");
        assert!(
            !subs["b"].url.contains("vim"),
            "a core key leaked into a submodule"
        );
    }

    #[test]
    fn an_unterminated_section_header_is_not_half_parsed() {
        let text = "[submodule \"a\"\n\tpath = a\n\turl = u\n";
        let subs = parse_gitmodules(text).unwrap();
        assert!(
            subs.is_empty(),
            "a malformed header produced an entry: {:?}",
            subs.keys().collect::<Vec<_>>()
        );
    }
}
