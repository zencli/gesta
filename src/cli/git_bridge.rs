//! Moving a repository in and out of git.
//!
//! `import` reads a git repository into this one; `export` writes this one out in a form git
//! reads. Both walk the real history rather than the working tree, so a branch that was deleted
//! still arrives with its commits, and an export keeps the shape of the graph.
//!
//! Nothing calls the git binary. A bridge that shells out cannot run on a machine without git,
//! which is the machine an agent is usually on.

use crate::store::gitobj::{self, Entry, Kind, Repo};
use crate::ZResult;
use crate::{bad, store};
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// What an import did.
#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    pub commits: usize,
    pub branches: Vec<String>,
    /// Files written into the store.
    pub files: usize,
    /// Commits already present, which a second import of the same repository reports.
    pub skipped: usize,
}

/// Read a git repository into the store, oldest commit first, one changeset per commit.
///
/// The order matters: a changeset's parent has to exist before the child names it, and a git
/// commit graph is not topologically sorted in the way a branch walk returns it.
pub fn import(conn: &Connection, git_root: &Path, prefix: Option<&str>) -> ZResult<Imported> {
    let repo = Repo::open(git_root)?;
    // Remember the format the objects are being read in. Every name written from here on is that
    // length, and the store has to be able to say which length it used when it serves the repository
    // over the network later.
    crate::store::set_hash_kind(conn, repo.hash)?;
    let mut out = Imported::default();

    let branches = repo.branches()?;
    if branches.is_empty() {
        return Err(bad(format!(
            "no branches under {}. A repository with commits but no branch (a bare clone that has only tags, for example) is not imported",
            repo.git_dir.join("refs/heads").display()
        )));
    }

    // Every commit reachable from every branch, in an order where a parent comes before its
    // children.
    let mut order: Vec<String> = Vec::new();
    let mut placed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for head in branches.values() {
        for hash in walk_commits(&repo, head)? {
            if placed.insert(hash.clone()) {
                order.push(hash);
            }
        }
    }

    // A commit id from git is not a changeset id here, so the mapping is kept as it goes.
    let mut mapped: HashMap<String, String> = HashMap::new();

    for hash in &order {
        let obj = repo.read_object(hash)?;
        if obj.kind != Kind::Commit {
            return Err(bad(format!(
                "{} is a {}, not a commit",
                hash,
                obj.kind.as_str()
            )));
        }
        let commit = gitobj::parse_commit(&obj.body)?;
        let (name, mail) = gitobj::author_parts(&commit.author);

        let already: Option<String> = conn
            .query_row(
                "SELECT id FROM changesets WHERE git_commit = ?1",
                rusqlite::params![hash],
                |r| r.get(0),
            )
            .ok();
        if let Some(id) = already {
            mapped.insert(hash.clone(), id);
            out.skipped += 1;
            continue;
        }

        // The tree of this commit against the tree of its first parent gives the change. A root
        // commit is compared against nothing, so everything in it is new.
        // The parent's tree, in this store's names, which is what the old side of a row holds.
        let parent_tree: HashMap<String, String> = match commit.parents.first() {
            Some(p) => {
                let pc = gitobj::parse_commit(&repo.read_object(p)?.body)?;
                let mut map = HashMap::new();
                for (path, git_hash) in flatten(&repo, &pc.tree, "", prefix)? {
                    let body = repo.read_object(&git_hash)?;
                    let ours = store::hash_content(&String::from_utf8_lossy(&body.body));
                    map.insert(path, ours);
                }
                map
            }
            None => HashMap::new(),
        };
        let here = flatten(&repo, &commit.tree, "", prefix)?;

        let id = store::new_changeset_id(conn, &commit.message, &name)?;
        let parent_id = match commit.parents.first() {
            Some(p) => mapped.get(p).cloned(),
            None => None,
        };
        let second_parent = match commit.parents.get(1) {
            Some(p) => mapped.get(p).cloned(),
            None => None,
        };

        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch,git_commit,tz)
             VALUES(?1,?2,?3,?4,'committed',?5,?6,?7,?8,?9)",
            rusqlite::params![
                id,
                commit.message.trim_end(),
                if mail.is_empty() { name.clone() } else { format!("{} <{}>", name, mail) },
                commit.author_time,
                parent_id,
                second_parent,
                "main",
                hash,
                // git hashes the author line including its offset, so a round trip that writes
                // +0000 where the original said +0700 produces a different commit name for
                // identical content. Measured on this project: 0 of 97 ids survived the trip.
                &commit.timezone
            ],
        )?;

        // The two systems name content differently: git by the SHA-1 of a header and body, this
        // store by the SHA-256 of the body alone. A git name in `new_hash` is a row that points at
        // nothing, which is why a first import looked successful and then could not read a file.
        // The store's own name is written, and the git name is recovered on export by hashing
        // the body again.
        let mut old_now: HashMap<String, String> = HashMap::new();
        for (path, git_hash) in &here {
            let body = repo.read_object(git_hash)?;
            // A pointer file names its oid, and the bytes it stands for are not in this pack. The
            // store already holds them when the import runs on the machine that made the export, so
            // the pointer is resolved against the sidecar and the real object is written under its
            // own hash. When the object is absent the pointer is stored as the text it is, which
            // keeps the round trip lossless and leaves the missing object visible rather than
            // inventing bytes for it.
            let our_hash = match crate::transport::lfs::Pointer::parse(&body.body) {
                Some(pointer) => match crate::store::sidecar::read(
                    &crate::store::root_of(conn).unwrap_or_default(),
                    &pointer.oid,
                ) {
                    Some(bytes) => store::put_object(conn, &String::from_utf8_lossy(&bytes))?,
                    None => store::put_object(conn, &String::from_utf8_lossy(&body.body))?,
                },
                None => store::put_object(conn, &String::from_utf8_lossy(&body.body))?,
            };
            old_now.insert(path.clone(), our_hash.clone());
            if parent_tree.get(path) == Some(&our_hash) {
                continue;
            }
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash) VALUES(?1,?2,?3,?4)",
                rusqlite::params![id, path, parent_tree.get(path), our_hash],
            )?;
            // `file_versions` is what answers "what did this path hold, and when". An import that
            // wrote only `changeset_files` left the content reachable from the tree but invisible to
            // every question about a path over time: blame, `grep --history`, and the secret scan of
            // the history all read this table and all found nothing on an imported repository.
            let next_ver: i64 = conn.query_row(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM file_versions WHERE path = ?1",
                rusqlite::params![path],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT INTO file_versions(path, version, hash, changeset_id, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![path, next_ver, our_hash, id, crate::now()],
            )?;
            out.files += 1;
        }
        for path in parent_tree.keys() {
            if !old_now.contains_key(path) {
                conn.execute(
                    "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash) VALUES(?1,?2,?3,NULL)",
                    rusqlite::params![id, path, parent_tree.get(path)],
                )?;
                out.files += 1;
            }
        }

        mapped.insert(hash.clone(), id.clone());
        out.commits += 1;
    }

    // Branch heads, as they are in git.
    for (branch, head) in &branches {
        let Some(id) = mapped.get(head) else { continue };
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1,?2,?3,NULL)
             ON CONFLICT(name) DO UPDATE SET head = excluded.head",
            rusqlite::params![branch, id, crate::now()],
        )?;
        store::refresh_branch_tree(conn, branch)?;
        out.branches.push(branch.clone());
    }
    if !out.branches.iter().any(|b| b == "main") {
        if let Some(first) = out.branches.first().cloned() {
            store::set_state(conn, "current_branch", &first)?;
        }
    } else {
        store::set_state(conn, "current_branch", "main")?;
    }

    // `history` reads `file_versions` and worked on imported repositories; `blame` reads
    // `blame_ranges`, and this path never wrote them. Computing them once at the end costs one pass
    // over the newest version of each path, against a `blame` that answered "never committed" for
    // everything an import brought in.
    let blamed = crate::repo::rebuild_all_blame(conn)?;
    let _ = blamed;
    Ok(out)
}

/// Collect every commit reachable from `head`, parents before children.
///
/// A git branch walk returns a commit before the commits it points back to. Inserting in that
/// order would name a parent that does not exist yet, so the walk collects first and the order is
/// reversed afterwards.
fn walk_commits(repo: &Repo, head: &str) -> ZResult<Vec<String>> {
    let mut seen: Vec<String> = Vec::new();
    let mut known: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack = vec![head.to_string()];
    while let Some(hash) = stack.pop() {
        if !known.insert(hash.clone()) {
            continue;
        }
        let obj = repo.read_object(&hash)?;
        if obj.kind != Kind::Commit {
            return Err(bad(format!(
                "{} is a {}, and a branch head has to be a commit",
                hash,
                obj.kind.as_str()
            )));
        }
        let commit = gitobj::parse_commit(&obj.body)?;
        for p in &commit.parents {
            stack.push(p.clone());
        }
        seen.push(hash);
    }
    // Reversed: the stack produces children before parents, and a parent has to be inserted
    // first.
    seen.reverse();
    Ok(seen)
}

/// Every path in a tree, with the blob it points at, depth first.
fn flatten(
    repo: &Repo,
    tree: &str,
    prefix: &str,
    filter: Option<&str>,
) -> ZResult<HashMap<String, String>> {
    let mut out = HashMap::new();
    let obj = repo.read_object(tree)?;
    for e in gitobj::parse_tree_with(&obj.body, repo.hash)? {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{}/{}", prefix, e.name)
        };
        if e.mode == "40000" {
            out.extend(flatten(repo, &e.hash, &path, filter)?);
        } else if e.mode == "160000" {
            // A submodule pointer is a commit in another repository and has no content here.
            continue;
        } else if let Some(f) = filter {
            if !path.starts_with(f) {
                continue;
            }
        } else {
            out.insert(path, e.hash);
        }
    }
    Ok(out)
}

/// What an export wrote.
#[derive(Debug, Default, PartialEq)]
pub struct Exported {
    pub commits: usize,
    pub branches: Vec<String>,
    pub root: String,
}

/// Write this repository into a git repository at `git_root`, which is created if missing.
pub fn export(conn: &Connection, git_root: &Path, branch: Option<&str>) -> ZResult<Exported> {
    let root = git_root.to_path_buf();
    std::fs::create_dir_all(root.join(".git").join("objects"))?;
    std::fs::create_dir_all(root.join(".git").join("refs").join("heads"))?;
    std::fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n")?;
    let repo = Repo::open(&root)?;
    export_into(conn, &repo, branch)
}

/// The body of `export`, for a repository whose git directory is already decided.
fn export_into(conn: &Connection, repo: &Repo, branch: Option<&str>) -> ZResult<Exported> {
    let branches: Vec<String> = match branch {
        Some(b) => vec![b.to_string()],
        None => {
            let mut stmt = conn.prepare("SELECT name FROM branches ORDER BY name")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    let mut out = Exported::default();

    for b in &branches {
        let Some(head) = store::branch_head(conn, b)? else {
            continue;
        };
        // Walk back to the root, then write forward, so a parent is always in the store before
        // its child names it.
        let chain = crate::store::ancestors_bfs(conn, &head)?;
        let mut ids: Vec<String> = chain;
        ids.reverse();

        let mut mapped: HashMap<String, String> = HashMap::new();
        for id in ids {
            let (parent, message, author, ts): (Option<String>, String, String, i64) = conn
                .query_row(
                    "SELECT parent, message, author, created_at FROM changesets WHERE id = ?1",
                    rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;
            let tree_hash = write_tree_for(conn, repo, &id)?;
            let (name, mail) = split_author(&author);
            let parents: Vec<String> = parent
                .and_then(|p| mapped.get(&p).cloned())
                .into_iter()
                .collect();
            // The offset the commit was imported with, or +0000 for a changeset made here. A
            // hard-coded +0000 gave every exported commit a different name than the one it came
            // from, which made a round trip useless for anything that compares ids.
            let tz: String = conn
                .query_row(
                    "SELECT COALESCE(tz, '+0000') FROM changesets WHERE id = ?1",
                    rusqlite::params![id],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| "+0000".to_string());
            let body = gitobj::build_commit(&tree_hash, &parents, &name, &mail, ts, &tz, &message);
            let commit = repo.write_object(Kind::Commit, &body)?;
            // Remember the name git gave this changeset. Without it a pull back from the same
            // remote sees an unknown commit and imports a duplicate of work that is already
            // here, which is what a push-then-pull round trip did before this line existed.
            conn.execute(
                "UPDATE changesets SET git_commit = ?2 WHERE id = ?1 AND git_commit IS NULL",
                rusqlite::params![id, commit],
            )?;
            mapped.insert(id.clone(), commit.clone());
            out.commits += 1;
            if let Some(last) = mapped.get(&head) {
                out.root = last.clone();
            }
        }
        if let Some(node) = mapped.get(&head) {
            repo.write_ref(&format!("refs/heads/{}", b), node)?;
            out.branches.push(b.clone());
        }
    }
    Ok(out)
}

/// Write the tree of one changeset, and every tree under it, returning the root tree's name.
///
/// Built bottom up: the deepest directory first, then its parent, which references it. Getting
/// this order wrong writes a root tree with only the files at the top level, which is what git
/// then shows as "path does not exist" while every object is present.
fn write_tree_for(conn: &Connection, repo: &Repo, changeset: &str) -> ZResult<String> {
    let files = crate::store::tree_at(conn, changeset)?;
    if files.is_empty() {
        return Err(bad(format!(
            "changeset {} has no files, so there is no tree to write",
            changeset
        )));
    }

    // Group the paths by their directory, and write each file's blob once.
    let mut by_dir: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for (path, hash) in &files {
        // A large object leaves the tree as a pointer rather than as its bytes. git has no way to
        // learn a blob is large on its own, so the exporter writes what git-lfs expects: a three line
        // text file naming the oid and the size. The bytes stay in the sidecar and are served by
        // `info/lfs/objects`, which is the half the protocol module implements.
        //
        // This is invisible until a client asks: a repository with no large files exports exactly as
        // it did, because nothing here runs for an object the store kept inline.
        // Bytes, not a `String`: a binary object is not valid UTF-8 and the text reader refuses it
        // with a message about encoding, which is how an export of a large binary failed with the
        // store's own hash as the error message.
        let body = store::get_object_bytes(conn, hash)?;
        let blob = match store::object_is_sidecar(conn, hash) {
            true => {
                let pointer = crate::transport::lfs::Pointer {
                    oid: hash.clone(),
                    size: body.len() as u64,
                };
                repo.write_object(Kind::Blob, pointer.render().as_bytes())?
            }
            false => repo.write_object(Kind::Blob, &body)?,
        };
        let (dir, name) = match path.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), path.clone()),
        };
        by_dir.entry(dir).or_default().push(Entry {
            mode: "100644".into(),
            name,
            hash: blob,
        });
    }

    // Deepest first. A directory's tree can only be written once every directory below it has
    // been, and sorting by depth gives that order without a graph walk.
    //
    // Every ancestor of every directory has to be in the list, and the root always is. Without
    // that, a tree whose files all sit under `a/b` has no `""` entry and the root never gets
    // written.
    {
        // The root is a directory even when every file sits under a subdirectory, so it is added
        // outright rather than inferred from a parent link.
        by_dir.entry(String::new()).or_default();
        let all: Vec<String> = by_dir.keys().cloned().collect();
        for d in all {
            let mut cur = d.as_str();
            while let Some((parent, _)) = cur.rsplit_once('/') {
                by_dir.entry(parent.to_string()).or_default();
                cur = parent;
            }
        }
    }
    let mut dirs: Vec<String> = by_dir.keys().cloned().collect();
    dirs.sort_by_key(|d| {
        std::cmp::Reverse(d.matches('/').count() + if d.is_empty() { 0 } else { 1 })
    });

    let mut written: HashMap<String, String> = HashMap::new();
    for dir in dirs {
        let mut entries = by_dir.remove(&dir).unwrap_or_default();
        // Subdirectories that belong directly under this one.
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir)
        };
        let mut children: Vec<(String, String)> = written
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix) && !k.is_empty())
            .filter(|(k, _)| !k[prefix.len()..].contains('/'))
            .map(|(k, v)| (k[prefix.len()..].to_string(), v.clone()))
            .collect();
        children.sort();
        for (name, hash) in children {
            entries.push(Entry {
                mode: "40000".into(),
                name,
                hash,
            });
        }
        let body = gitobj::build_tree_with(entries, repo.hash);
        let hash = repo.write_object(Kind::Tree, &body)?;
        written.insert(dir, hash);
    }

    written
        .get("")
        .cloned()
        .ok_or_else(|| bad(format!("changeset {} produced no root tree", changeset)))
}

fn split_author(author: &str) -> (String, String) {
    match (author.find('<'), author.find('>')) {
        (Some(a), Some(b)) if b > a => (
            author[..a].trim().to_string(),
            author[a + 1..b].trim().to_string(),
        ),
        _ => (author.trim().to_string(), String::new()),
    }
}

/// Push this repository's branches to a git remote, through the git binary.
///
/// The objects are written first with [`export`], into a bare repository that holds nothing but
/// `.git`. git then pushes from there. Doing it in that order means the transfer is an ordinary
/// git push: an authentication prompt, a token, an SSH key and a forge's hooks all work exactly
/// as they do for any other repository, which no hand written protocol implementation could
/// promise.
///
/// This is the one place that shells out to git, and it is deliberate. Reading and writing
/// objects does not need the binary; talking to a forge does.
pub fn push_to_git_remote(
    conn: &Connection,
    branch: Option<&str>,
    remote: &str,
) -> ZResult<Exported> {
    let staging = std::env::temp_dir().join(format!("gesta-git-push-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    // `git init --bare <dir>` makes <dir> the git directory itself, so `export` has to be told
    // where the objects go rather than being pointed at a working tree that does not exist.
    std::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg("-q")
        .arg(&staging)
        .output()
        .map_err(|e| {
            Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Remote,
                format!("git is needed to push to a remote: {}", e),
            ))
        })?;

    let written = export_bare(conn, &staging, branch)?;

    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&staging)
        .arg("push")
        .arg(remote)
        .arg(
            written
                .branches
                .iter()
                .map(|b| format!("refs/heads/{}:refs/heads/{}", b, b))
                .collect::<Vec<_>>()
                .join(" "),
        )
        .output()
        .map_err(|e| {
            Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Remote,
                format!("cannot run git: {}", e),
            ))
        })?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let _ = std::fs::remove_dir_all(&staging);
        // git's own message is the useful one: it names the ref, the reason and the fix.
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Remote,
            format!("git push failed: {}", err.trim()),
        )));
    }
    let _ = std::fs::remove_dir_all(&staging);
    let _ = adjust_remote_head(remote, &written.branches);
    Ok(written)
}

/// Fetch branches from a git remote into a bare staging repository, then import them.
pub fn pull_from_git_remote(
    conn: &Connection,
    remote: &str,
    prefix: Option<&str>,
) -> ZResult<Imported> {
    let staging = std::env::temp_dir().join(format!("gesta-git-pull-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    // `git init --bare <dir>` makes <dir> itself the git directory, not <dir>/.git. Pointing
    // --git-dir at staging/.git then names a path that does not exist, and every command after
    // it fails with "not a git repository".
    let init = std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&staging)
        .output()
        .map_err(|e| {
            Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Remote,
                format!("git is needed to pull from a remote: {}", e),
            ))
        })?;
    if !init.status.success() {
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Remote,
            "git init failed in the staging directory",
        )));
    }

    let fetch = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&staging)
        .args(["fetch", remote, "+refs/heads/*:refs/heads/*"])
        .output()
        .map_err(|e| {
            Box::new(crate::support::error::Error::new(
                crate::support::error::Code::Remote,
                format!("cannot run git: {}", e),
            ))
        })?;
    if !fetch.status.success() {
        let err = String::from_utf8_lossy(&fetch.stderr);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Remote,
            format!("git fetch failed: {}", err.trim()),
        )));
    }

    // `fetch` writes packfiles and an import reads loose objects, so the packs are taken apart
    // first. `git unpack-objects` reads one pack per invocation from stdin, and a repository
    // fetched with several refs can hold more than one, so each pack gets its own run. Piping
    // them all into one process produced "fatal: early EOF" and unpacked only the first.
    let pack_dir = staging.join("objects").join("pack");
    if let Ok(entries) = std::fs::read_dir(&pack_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "pack") {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            use std::io::Write;
            if let Ok(mut child) = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&staging)
                .args(["unpack-objects", "-q"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(&bytes);
                }
                let out = child.wait_with_output();
                if let Ok(o) = out {
                    if !o.status.success() {
                        let err = String::from_utf8_lossy(&o.stderr);
                        let _ = std::fs::remove_dir_all(&staging);
                        return Err(bad(format!(
                            "unpacking {} failed: {}",
                            path.display(),
                            err.trim()
                        )));
                    }
                }
            }
        }
    }

    let got = import(conn, &staging, prefix);
    let _ = std::fs::remove_dir_all(&staging);
    got
}

/// Point a remote's HEAD at `main` when it points at nothing.
///
/// A freshly created bare repository has `HEAD` on `master`. Pushing `main` to it leaves a
/// repository that clones with "remote HEAD refers to nonexistent ref" and no files, which
/// looks like the push failed when it did not. Only a local path can be fixed this way; a
/// remote URL belongs to somebody else and is left alone.
fn adjust_remote_head(remote: &str, branches: &[String]) -> ZResult<()> {
    if !branches.iter().any(|b| b == "main") {
        return Ok(());
    }
    let path = std::path::Path::new(remote);
    let bare = path.join("HEAD");
    if !bare.is_file() {
        return Ok(());
    }
    let current = std::fs::read_to_string(&bare).unwrap_or_default();
    if current.contains("refs/heads/main") {
        return Ok(());
    }
    std::fs::write(&bare, "ref: refs/heads/main\n")?;
    Ok(())
}

/// Write this repository into a git directory that is already a bare repository.
fn export_bare(conn: &Connection, git_dir: &Path, branch: Option<&str>) -> ZResult<Exported> {
    // The caller already made the directory, so the format is read from what is on disk rather
    // than assumed: exporting into a SHA-256 bare repository has to write 64 character names.
    let repo = Repo::open(git_dir)?;
    export_into(conn, &repo, branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::gitobj::HashKind;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gesta-gb-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a small git repository by hand, so the test needs no git binary.
    fn make_git_repo(root: &std::path::Path) -> Vec<String> {
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repo::open(root).unwrap();

        let blob_a = repo.write_object(Kind::Blob, b"first\n").unwrap();
        let blob_b = repo.write_object(Kind::Blob, b"second\n").unwrap();

        // First commit: one file.
        let tree1 = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![Entry {
                        mode: "100644".into(),
                        name: "a.txt".into(),
                        hash: blob_a.clone(),
                    }],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let c1 = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(&tree1, &[], "T", "t@e.st", 100, "+0000", "one\n"),
            )
            .unwrap();

        // Second commit: two files, parent is the first.
        let tree2 = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![
                        Entry {
                            mode: "100644".into(),
                            name: "a.txt".into(),
                            hash: blob_a,
                        },
                        Entry {
                            mode: "100644".into(),
                            name: "b.txt".into(),
                            hash: blob_b,
                        },
                    ],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let c2 = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(
                    &tree2,
                    std::slice::from_ref(&c1),
                    "T",
                    "t@e.st",
                    200,
                    "+0000",
                    "two\n",
                ),
            )
            .unwrap();
        repo.write_ref("refs/heads/main", &c2).unwrap();
        vec![c1, c2]
    }

    /// The commit id a git branch points at, read the way git stores it.
    ///
    /// A test helper rather than a method on `Repo`: the production code never asks a git
    /// repository for a ref, and a public method kept alive only by its own test is a method that
    /// will drift from what anybody needs.
    fn head_of(repo: &Repo, branch: &str) -> String {
        let loose = repo.git_dir.join("refs").join("heads").join(branch);
        if let Ok(text) = std::fs::read_to_string(&loose) {
            return text.trim().to_string();
        }
        // A ref that has been packed lives in packed-refs, one line per ref.
        let packed = std::fs::read_to_string(repo.git_dir.join("packed-refs")).unwrap_or_default();
        for line in packed.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            if let Some((hash, name)) = line.split_once(' ') {
                if name.trim() == format!("refs/heads/{}", branch) {
                    return hash.trim().to_string();
                }
            }
        }
        panic!("no such ref: refs/heads/{}", branch);
    }

    #[test]
    fn an_imported_repository_can_read_its_files() {
        let src = temp("import-src");
        make_git_repo(&src);
        let conn = crate::store::tests::mem_db();
        let got = import(&conn, &src, None).unwrap();
        assert_eq!(got.commits, 2, "expected two changesets");
        assert!(got.branches.contains(&"main".to_string()));

        // The store must hold the content, not the git object name: they are different hashes and
        // putting one where the other belongs made an import look fine and read nothing.
        let head = store::branch_head(&conn, "main").unwrap().unwrap();
        let tree = crate::store::tree_at(&conn, &head).unwrap();
        let b = tree
            .iter()
            .find(|(p, _)| p == "b.txt")
            .expect("b.txt is missing from the imported tree");
        assert_eq!(store::get_object(&conn, &b.1).unwrap(), "second\n");
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn a_non_utc_offset_survives_the_round_trip() {
        // git hashes the author line including its offset, so a source written at +0700 and an
        // export written at +0000 are different commits for identical content. Every id matched in
        // the test above until a source with a real offset was used, because +0000 matches whether
        // the code is right or wrong.
        let dir = temp("tz-offset");
        let git_dir = dir.join("repo");
        std::fs::create_dir_all(&git_dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main", "."],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
        ] {
            let st = std::process::Command::new("git")
                .args(&args)
                .current_dir(&git_dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed in the test setup", args);
        }
        std::fs::write(git_dir.join("a.txt"), "one\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&git_dir)
            .status()
            .unwrap();
        assert!(add.success(), "could not stage the source file");
        // The offset is set on the commit, where git reads it. Setting it on `git add` did nothing
        // and the test then failed for a reason that had nothing to do with what it checks.
        let out = std::process::Command::new("git")
            .args(["commit", "-qm", "first"])
            .env("GIT_AUTHOR_DATE", "1700000000 +0700")
            .env("GIT_COMMITTER_DATE", "1700000000 +0700")
            .current_dir(&git_dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "could not make the source commit: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let conn = store::open_db(&dir).unwrap();
        store::init_schema(&conn).unwrap();
        import(&conn, &git_dir, None).unwrap();

        // The recorded offset has to be the one git wrote, not a default.
        let tz: String = conn
            .query_row("SELECT tz FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tz, "+0700", "the import dropped the author's offset");

        let out = temp("tz-out");
        export(&conn, &out, None).unwrap();
        let repo_out = Repo::open(&out).unwrap();
        let exported = head_of(&repo_out, "main");
        let original = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&git_dir)
            .output()
            .unwrap();
        assert_eq!(
            exported,
            String::from_utf8_lossy(&original.stdout).trim(),
            "the export gave the commit a different name"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn importing_twice_adds_nothing() {
        let src = temp("import-twice");
        make_git_repo(&src);
        let conn = crate::store::tests::mem_db();
        let first = import(&conn, &src, None).unwrap();
        assert_eq!(first.commits, 2);
        let second = import(&conn, &src, None).unwrap();
        assert_eq!(second.commits, 0, "a second import duplicated the history");
        assert_eq!(second.skipped, 2);
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn an_export_is_readable_and_keeps_the_commit_names() {
        let src = temp("export-src");
        let original = make_git_repo(&src);
        let conn = crate::store::tests::mem_db();
        import(&conn, &src, None).unwrap();

        let out = temp("export-out");
        let got = export(&conn, &out, None).unwrap();
        assert_eq!(got.commits, 2);

        // The names a round trip produces have to match the originals: a commit id is the hash of
        // its tree, its parents, its author line and its message, so equal ids prove all four.
        let repo = Repo::open(&out).unwrap();
        let head = head_of(&repo, "main");
        assert_eq!(
            head, original[1],
            "the exported commit got a different name than the one it came from"
        );

        // And the content is there, not just the name.
        let commit = gitobj::parse_commit(&repo.read_object(&head).unwrap().body).unwrap();
        let tree = gitobj::parse_tree(&repo.read_object(&commit.tree).unwrap().body).unwrap();
        let b = tree
            .iter()
            .find(|e| e.name == "b.txt")
            .expect("b.txt missing");
        assert_eq!(repo.read_object(&b.hash).unwrap().body, b"second\n");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn a_nested_directory_keeps_its_shape() {
        let src = temp("nested-src");
        let git = src.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        let repo = Repo::open(&src).unwrap();

        let blob = repo.write_object(Kind::Blob, b"deep\n").unwrap();
        let inner = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![Entry {
                        mode: "100644".into(),
                        name: "c.rs".into(),
                        hash: blob,
                    }],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let mid = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![Entry {
                        mode: "40000".into(),
                        name: "b".into(),
                        hash: inner,
                    }],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let root = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![Entry {
                        mode: "40000".into(),
                        name: "a".into(),
                        hash: mid,
                    }],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let c = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(&root, &[], "T", "t@e.st", 1, "+0000", "deep\n"),
            )
            .unwrap();
        repo.write_ref("refs/heads/main", &c).unwrap();

        let conn = crate::store::tests::mem_db();
        import(&conn, &src, None).unwrap();
        let head = store::branch_head(&conn, "main").unwrap().unwrap();
        let tree = crate::store::tree_at(&conn, &head).unwrap();
        assert_eq!(
            tree.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["a/b/c.rs"],
            "a nested path lost its shape"
        );

        // And exporting puts the directories back.
        let out = temp("nested-out");
        export(&conn, &out, None).unwrap();
        let back = Repo::open(&out).unwrap();
        let head2 = head_of(&back, "main");
        assert_eq!(head2, c, "the round trip changed the commit name");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&out);
    }
    #[test]
    fn a_bare_repository_is_read_as_a_repository() {
        let dir = temp("bare");
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs").join("heads")).unwrap();
        // A bare repository has no `.git` inside: it is the git directory.
        let repo = Repo::open(&dir).unwrap();
        assert_eq!(repo.git_dir, dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A push records the name git gave each changeset. Without it, pulling from the same remote
    /// sees an unknown commit and imports a duplicate of the work that was just pushed.
    #[test]
    fn an_export_remembers_the_name_git_gave_each_changeset() {
        let conn = crate::store::tests::mem_db();
        let dir = temp("export-name");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        crate::repo::tests::commit_files(&conn, "e1", "one", &[("a.txt", "v1\n")]);
        crate::store::set_branch_head(&conn, "main", "e1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();

        let before: Option<String> = conn
            .query_row(
                "SELECT git_commit FROM changesets WHERE id = 'e1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(before.is_none(), "the fixture already had a git name");

        export(&conn, &dir, None).unwrap();

        let after: Option<String> = conn
            .query_row(
                "SELECT git_commit FROM changesets WHERE id = 'e1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(after.is_some(), "export did not record the git commit name");
        assert_eq!(
            after.unwrap().len(),
            40,
            "a git name is forty hex characters"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
