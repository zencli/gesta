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
use rusqlite::{Connection, OptionalExtension};
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
    // Every commit to import, parents before children, across all branches at once.
    //
    // This used to walk one branch at a time and reverse each walk. Reversing a depth-first walk does
    // not give parents first: for a merge commit M with parents B and C, both reaching A, the walk
    // from M is `M, C, A, B` and the reversal is `B, A, C, M` - B before A, a child before its parent.
    // The parent lookup then found nothing, and the parent was written as NULL with nothing reported.
    // A merge history imported with its second parent dropped and its first parent sometimes
    // rewritten as null.
    let mut order: Vec<String> = Vec::new();
    {
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Marks nodes on the current path, so a cycle in the object graph is refused instead of
        // recursing forever. Git cannot produce one; a hand-written pack can.
        let mut on_path: std::collections::HashSet<String> = std::collections::HashSet::new();
        for head in branches.values() {
            place_parents_first(&repo, head, &mut on_path, &mut done, &mut order)?;
        }
    }

    // A commit id from git is not a changeset id here, so the mapping is kept as it goes.
    let mut mapped: HashMap<String, String> = HashMap::new();

    // Which branch a commit belongs to. Git commits don't carry branch identity;
    // a commit is reachable from many branches. We assign each changeset the first
    // branch (in BTreeMap order, so 'main' first) that reaches it by walking from
    // each branch head back through parents.
    let mut branch_of: HashMap<String, String> = HashMap::new();
    for (branch, head) in &branches {
        let mut stack = vec![head.clone()];
        while let Some(hash) = stack.pop() {
            if branch_of.contains_key(&hash) {
                continue;
            }
            branch_of.insert(hash.clone(), branch.clone());
            if let Ok(commit) = gitobj::parse_commit(&repo.read_object(&hash)?.body) {
                for p in &commit.parents {
                    stack.push(p.clone());
                }
            }
        }
    }

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
            .optional()?;
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
                for (path, (git_hash, _mode)) in flatten(&repo, &pc.tree, "", prefix)? {
                    let body = repo.read_object(&git_hash)?;
                    // Raw bytes, the same bytes `put_object_bytes` stores below. Hashing the lossy
                    // string gave every binary file a hash of the replacement characters, so two
                    // different images with the same invalid sequences hashed alike, and the
                    // comparison against `old_now` agreed with a lie.
                    let ours = store::hash_bytes(&body.body);
                    map.insert(path, ours);
                }
                map
            }
            None => HashMap::new(),
        };
        let here = flatten(&repo, &commit.tree, "", prefix)?;
        // A gitlink has no blob, so it is collected apart from `here` and recorded as a
        // subrepository pointing at the commit the child was at. Without this the path is not in the
        // tree at all, and an export cannot put it back.
        let links = gitlinks(&repo, &commit.tree, "")?;
        for (path, child_commit) in &links {
            crate::repo::subrepo::record_imported(conn, path, child_commit)?;
        }

        let id = store::new_changeset_id(conn, &commit.message, &name)?;
        // A parent that is not in the map is a bug in the walk, not a root commit. Writing NULL for
        // it is what made a broken import look like a successful one: the changeset landed, the
        // branch moved, and the history quietly lost the link. Every parent is in `order` before its
        // child now, so a miss here means the ordering failed, and that is reported.
        let parent_id = match commit.parents.first() {
            Some(p) => Some(mapped.get(p).cloned().ok_or_else(|| {
                bad(format!(
                    "{} names parent {} which was not imported before it; the commit order is wrong",
                    hash, p
                ))
            })?),
            None => None,
        };
        let second_parent = match commit.parents.get(1) {
            Some(p) => Some(mapped.get(p).cloned().ok_or_else(|| {
                bad(format!(
                    "{} names second parent {} which was not imported before it; the commit order is wrong",
                    hash, p
                ))
            })?),
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
                branch_of.get(hash).cloned().unwrap_or_default(),
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
        for (path, (git_hash, mode)) in &here {
            let body = repo.read_object(git_hash)?;
            // A pointer file names its oid, and the bytes it stands for are not in this pack. The
            // store already holds them when the import runs on the machine that made the export, so
            // the pointer is resolved against the sidecar and the real object is written under its
            // own hash. When the object is absent the pointer is stored as the text it is, which
            // keeps the round trip lossless and leaves the missing object visible rather than
            // inventing bytes for it.
            // Raw bytes, not a lossy string. `put_object` took `&str`, so every imported blob went
            // through `from_utf8_lossy`: each invalid byte became U+FFFD and the object stored was
            // not the file. Export wrote the corrupted bytes back out, and because the parent-tree
            // hash was lossy too the round trip compared equal - the corruption was consistent and
            // therefore invisible. A PNG, an archive or an executable imported as a file of question
            // marks.
            let our_hash = match crate::transport::lfs::Pointer::parse(&body.body) {
                Some(pointer) => match crate::store::sidecar::read(
                    &crate::store::root_of(conn).unwrap_or_default(),
                    &pointer.oid,
                ) {
                    // The sidecar holds the real bytes a pointer stands for; those are what the
                    // object has to be.
                    Some(bytes) => store::put_object_bytes(conn, &bytes)?,
                    None => store::put_object_bytes(conn, &body.body)?,
                },
                None => store::put_object_bytes(conn, &body.body)?,
            };
            old_now.insert(path.clone(), our_hash.clone());
            if parent_tree.get(path) == Some(&our_hash) {
                continue;
            }
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash,mode) VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![id, path, parent_tree.get(path), our_hash, mode],
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
/// Append `hash` and its ancestors to `order`, every parent before its children.
///
/// Post-order rather than a reversed pre-order. The distinction only shows at a merge: a depth-first
/// walk that pushes a node when it is first seen emits a child before its second parent, and reversing
/// the whole list cannot repair that because the interleaving is already wrong.
fn place_parents_first(
    repo: &Repo,
    hash: &str,
    on_path: &mut std::collections::HashSet<String>,
    done: &mut std::collections::HashSet<String>,
    order: &mut Vec<String>,
) -> ZResult<()> {
    if done.contains(hash) {
        return Ok(());
    }
    if !on_path.insert(hash.to_string()) {
        return Err(bad(format!(
            "the commit graph reaches {} twice on one path, so it holds a cycle",
            hash
        )));
    }
    let obj = repo.read_object(hash)?;
    if obj.kind != Kind::Commit {
        return Err(bad(format!(
            "{} is a {}, and a branch head has to be a commit",
            hash,
            obj.kind.as_str()
        )));
    }
    let commit = gitobj::parse_commit(&obj.body)?;
    for p in &commit.parents {
        place_parents_first(repo, p, on_path, done, order)?;
    }
    on_path.remove(hash);
    if done.insert(hash.to_string()) {
        order.push(hash.to_string());
    }
    Ok(())
}

/// Every path in a tree, with the blob it points at and the git mode it carried, depth first.
///
/// The mode is kept because a submodule is a tree entry with mode 160000 whose hash is a commit in
/// another repository - there is no blob to read. Dropping it made the path vanish from the import,
/// so a repository with a submodule lost the submodule and nothing reported it.
fn flatten(
    repo: &Repo,
    tree: &str,
    prefix: &str,
    filter: Option<&str>,
) -> ZResult<HashMap<String, (String, String)>> {
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
            // A submodule pointer names a commit in ANOTHER repository. Collecting it here made the
            // import try to read that commit from this one, and fail with "object ... is in neither a
            // loose object nor a packfile". `gitlinks` collects these instead.
            continue;
        } else if let Some(f) = filter {
            if !path.starts_with(f) {
                continue;
            }
        } else {
            out.insert(path, (e.hash, e.mode));
        }
    }
    Ok(out)
}

/// The submodule pointers in a tree, as path to the commit the gitlink names.
///
/// Read separately because a gitlink has no blob: the hash is a commit in the child repository, and
/// the only thing that can be stored on this side is the pointer itself.
fn gitlinks(repo: &Repo, tree: &str, prefix: &str) -> ZResult<HashMap<String, String>> {
    let mut out = HashMap::new();
    let obj = repo.read_object(tree)?;
    for e in gitobj::parse_tree_with(&obj.body, repo.hash)? {
        let path = if prefix.is_empty() {
            e.name.clone()
        } else {
            format!("{}/{}", prefix, e.name)
        };
        if e.mode == "40000" {
            out.extend(gitlinks(repo, &e.hash, &path)?);
        } else if e.mode == "160000" {
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
    /// Tags written to `refs/tags/`, in name order (#206).
    ///
    /// Reporting them matters for the same reason `branches` does: an export that silently drops
    /// tags looks identical to one that had none.
    pub tags: Vec<String>,
}

/// Write this repository into a git repository at `git_root`, which is created if missing.
pub fn export(conn: &Connection, git_root: &Path, branch: Option<&str>) -> ZResult<Exported> {
    let root = git_root.to_path_buf();
    std::fs::create_dir_all(root.join(".git").join("objects"))?;
    std::fs::create_dir_all(root.join(".git").join("refs").join("heads"))?;
    let repo = Repo::open(&root)?;
    let out = export_into(conn, &repo, branch)?;
    // HEAD is written after the refs, from the branch that was actually written, and in the git
    // directory the repository really has. Writing it before the export meant a repository whose
    // default branch was `trunk` got `refs/heads/main` and cloned empty, and writing it to
    // `<root>/.git` meant a bare export never got one at all.
    if let Some(first) = out.branches.first() {
        let head_path = repo.git_dir.join("HEAD");
        std::fs::write(&head_path, format!("ref: refs/heads/{}\n", first))?;
    }
    Ok(out)
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
    // Every changeset exported on this run, across all branches, so a tag can name a commit that was
    // written for a branch other than the one the tag's changeset happens to sit on.
    let mut mapped_all: HashMap<String, String> = HashMap::new();

    for b in &branches {
        let Some(head) = store::branch_head(conn, b)? else {
            continue;
        };
        // Write parents before children, in a real topological order (#187).
        //
        // This used `ancestors_bfs` reversed, and a reversed BFS is not a topological order. For a
        // head H with parents A and B where B's parent is A, BFS gives `[H, A, B, C]` because A is
        // one hop away and B is two; reversed that is `[C, B, A, H]`, so B is written while A is
        // still missing from `mapped` and B's link to A is silently dropped. git produces such
        // shapes routinely - it is an ordinary criss-cross merge - so an export could lose parent
        // links from a history git itself considers unremarkable.
        let ids = crate::store::topological_order(conn, &head)?;

        let mut mapped: HashMap<String, String> = HashMap::new();
        for id in ids {
            let (parent, parent2, message, author, ts): (
                Option<String>,
                Option<String>,
                String,
                String,
                i64,
            ) = conn.query_row(
                "SELECT parent, parent2, message, author, created_at FROM changesets WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            let tree_hash = write_tree_for(conn, repo, &id)?;
            let (name, mail) = split_author(&author);
            // Both parents, in order (#178). Reading only `parent` wrote every merge as a
            // single-parent commit: the tree was right so the content survived, but `git log
            // --graph`, `git merge-base` and `git branch --merged` all answered differently from the
            // history this repository holds. `docs/git-parity.md` names merge parents as a shape the
            // round-trip claim does not cover, which is a note about a gap, not permission to keep
            // it.
            let mut parents: Vec<String> = Vec::new();
            for p in [parent, parent2].into_iter().flatten() {
                match mapped.get(&p) {
                    Some(git) => parents.push(git.clone()),
                    // A parent that is not in `mapped` after a topological walk is a hole in the
                    // history, not a shape to paper over. Writing the commit with fewer parents
                    // looks like success and loses the link; the old code did exactly that for any
                    // parent it could not find.
                    None => {
                        return Err(bad(format!(
                            "changeset {} names parent {}, which was not exported before it; \
                             refusing to write a commit with a dropped parent link",
                            id, p
                        )))
                    }
                }
            }
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
            mapped_all.insert(id.clone(), commit.clone());
            out.commits += 1;
            if let Some(last) = mapped.get(&head) {
                out.root = last.clone();
            }
        }
        if let Some(node) = mapped.get(&head) {
            repo.write_ref(&format!("refs/heads/{}", b), node)?;
            // Also point HEAD at the first branch written, when it points at nothing that exists.
            // `git init --bare` leaves HEAD on `refs/heads/master`; exporting `main` into that
            // repository leaves a valid ref that no clone follows, so a clone reports an empty
            // repository while every object is present. The ref is right and the checkout is empty,
            // which is the hardest kind of wrong to notice.
            if out.branches.is_empty() {
                let head_path = repo.git_dir.join("HEAD");
                let current = std::fs::read_to_string(&head_path).unwrap_or_default();
                let wanted = format!("refs/heads/{}", b);
                if !current.trim_end().ends_with(&wanted) {
                    std::fs::write(&head_path, format!("ref: {}\n", wanted))?;
                }
            }
            out.branches.push(b.clone());
        }
    }

    // Tags, after every branch (#206). Never written at all before this, so an export silently lost
    // them, which is indistinguishable from a repository that had none.
    //
    // A tag points at the git commit its changeset became, and a changeset that was not exported on
    // this run has no mapping - a tag on an unreachable changeset is not exportable, and saying so is
    // better than writing a ref that names nothing. Annotated tags are not reconstructed: the store
    // records a message, but git's tag object also carries a tagger and a signature this repository
    // has no field for, so a lightweight ref is the honest representation of what is known.
    let tags: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT name, changeset_id FROM tags ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, changeset) in tags {
        match mapped_all.get(&changeset) {
            Some(git) => {
                repo.write_ref(&format!("refs/tags/{}", name), git)?;
                out.tags.push(name);
            }
            None => {
                return Err(bad(format!(
                    "tag {} names changeset {}, which is not reachable from any exported branch; \
                     refusing to write a tag that points at nothing",
                    name, changeset
                )))
            }
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
    // Modes come back with the paths. Writing `100644` for everything was how a script marked
    // executable arrived in git as a file git would not run: the bytes matched and the mode did not.
    let files = crate::store::tree_at_with_mode(conn, changeset)?;
    // An empty tree is a legitimate state, not an error (#206). Refusing here meant one branch whose
    // head tree happened to be empty aborted the whole export, including every other branch that was
    // perfectly fine. git has an empty tree object and a commit can point at it, so the honest
    // answer is to write it. The empty-tree hash is fixed by the object format, so it is computed
    // once and not cached: `write_object` is idempotent on content anyway.
    if files.is_empty() {
        // `build_tree_with` needs the hash kind; the repository being written decides it.
        let body = crate::store::gitobj::build_tree_with(Vec::new(), repo.hash);
        return repo.write_object(Kind::Tree, &body);
    }

    // Group the paths by their directory, and write each file's blob once.
    let mut by_dir: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for (path, hash, mode) in &files {
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
        let entry_mode = match mode.as_str() {
            // A symlink is stored by this tree as an ordinary file whose content is the target, which
            // is the same thing git stores; writing it back as `120000` is what makes it a link again.
            "120000" => "120000",
            // Anything else is a regular file, and the only distinction git records is the bit.
            "100755" => "100755",
            _ => "100644",
        };
        by_dir.entry(dir).or_default().push(Entry {
            mode: entry_mode.into(),
            name,
            hash: blob,
        });
    }

    // A subrepository is a tree entry with mode 160000 whose hash is a commit in the CHILD repository.
    // Writing it back is what keeps a submodule a submodule across a round trip; leaving it out gave
    // an export whose tree simply did not mention the path.
    //
    // Only the subrepositories that already existed at this changeset (#206). The old code added all
    // of them to every tree, so exporting history wrote submodule entries into commits from before
    // the submodule was added - a history that never existed. `subrepos` records `created_at` and no
    // per-changeset history, so that column is the only evidence available and this filters on it.
    // Timestamps made in one burst can share a second, which makes the cut-off coarse; that is a
    // limitation of what the store records, and it is still strictly better than ignoring time
    // entirely. A real fix needs a history table, which is a schema change and not this bug.
    let at: i64 = conn.query_row(
        "SELECT created_at FROM changesets WHERE id = ?1",
        rusqlite::params![changeset],
        |r| r.get(0),
    )?;
    // `subrepo::list` used to call `init_schema` for us; the query is written out here so it can
    // filter on `created_at`, so the schema has to be ensured explicitly. `subrepos` is created by
    // `migrate`, not `init_schema` - a store written before submodules existed has no table, and a
    // test built with `mem_db` never ran the migration.
    crate::store::migrate(conn)?;
    let subs: Vec<crate::repo::subrepo::Sub> = {
        let mut stmt = conn.prepare(
            "SELECT path, remote, pinned FROM subrepos WHERE created_at <= ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(rusqlite::params![at], |r| {
            Ok(crate::repo::subrepo::Sub {
                path: r.get(0)?,
                remote: r.get(1)?,
                pinned: r.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for sub in subs {
        if sub.pinned.is_empty() {
            continue;
        }
        let (dir, name) = match sub.path.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), sub.path.clone()),
        };
        by_dir.entry(dir).or_default().push(Entry {
            mode: "160000".into(),
            name,
            hash: sub.pinned.clone(),
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
    //
    // The exit status is checked, which it was not: a `git init` that failed (a read-only temp dir,
    // an unwritable path) left `export_bare` writing into a directory that was not a repository, and
    // the failure surfaced later as a confusing error about objects rather than as the init that
    // actually went wrong.
    let init = std::process::Command::new("git")
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
    if !init.status.success() {
        let err = String::from_utf8_lossy(&init.stderr);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Remote,
            format!("git init --bare failed: {}", err.trim()),
        )));
    }

    // Cleaned up on the way out however this ends. The staging directory is this process's own
    // scratch space, and leaving it behind on a failed export was a leak nobody would notice until
    // the temp directory filled.
    let written = match export_bare(conn, &staging, branch) {
        Ok(w) => w,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    let mut push = std::process::Command::new("git");
    push.arg("--git-dir").arg(&staging).arg("push").arg(remote);
    // One argument per refspec. Joining them with a space made git read the whole string as a single
    // malformed refspec: a single branch worked and two or more failed with a message about a
    // refspec the caller never typed. Pushing several branches is the normal multi-branch workflow,
    // so this was the common case broken and the rare one working.
    for b in &written.branches {
        push.arg(format!("refs/heads/{}:refs/heads/{}", b, b));
    }
    let out = push.output().map_err(|e| {
        let _ = std::fs::remove_dir_all(&staging);
        Box::new(crate::support::error::Error::new(
            crate::support::error::Code::Remote,
            format!("cannot run git: {}", e),
        )) as Box<dyn std::error::Error>
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

/// Hand each pack file in `<staging>/objects/pack` to `git unpack-objects`, one process per pack.
///
/// The pack is streamed from the file into the child's stdin, never held. It used to be
/// `std::fs::read(&path)` and then written to stdin, so a pull held the whole pack in memory on
/// top of whatever the import needed. A remote chooses that size: a hostile or careless one can
/// serve a pack of any size, and the pull has no reason to keep it.
fn unpack_each_pack(staging: &Path) -> ZResult<()> {
    use std::io::Write;
    let pack_dir = staging.join("objects").join("pack");
    let Ok(entries) = std::fs::read_dir(&pack_dir) else {
        return Ok(());
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "pack") {
            continue;
        }
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let child = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(staging)
            .args(["unpack-objects", "-q"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            // A copy, not a read: the file's bytes pass through an 8 KB buffer on their way to the
            // child. `io::copy` returning early is not silent: the child then sees a short pack and
            // says so on stderr, which is the error reported below.
            let _ = std::io::copy(&mut std::io::BufReader::new(file), &mut stdin);
            let _ = stdin.flush();
            // Dropping stdin closes the pipe, which is what tells the child the pack is complete.
        }
        let out = child.wait_with_output();
        if let Ok(o) = out {
            if !o.status.success() {
                let err = String::from_utf8_lossy(&o.stderr);
                return Err(bad(format!(
                    "unpacking {} failed: {}",
                    path.display(),
                    err.trim()
                )));
            }
        }
    }
    Ok(())
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
    if let Err(e) = unpack_each_pack(&staging) {
        // The staging directory holds the packs the remote sent. A failed unpack still leaves it
        // behind, and the earlier shape removed it on the error path too.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
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
    /// Peak resident size of this process so far, in KiB, or `None` where the kernel is not asked.
    fn peak_rss_kib() -> Option<u64> {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }

    /// Put a pack file of `size` bytes under `<staging>/objects/pack`, made of a header followed by a
    /// hole, and return the staging directory.
    ///
    /// A hole rather than a `Vec`: the fixture must not itself allocate the size being measured, or
    /// the peak is spent before the measurement starts and the test passes on any code.
    ///
    /// `git unpack-objects` then refuses the pack, and that is fine: the refusal arrives after the
    /// pass has measured the writing, which is where the copy happens. `unpack_each_pack` returns its
    /// error, and the assertion below is about memory, not about the exit status.
    fn staging_with_padded_pack(size: u64) -> std::path::PathBuf {
        use std::io::Write;
        let staging =
            crate::support::scratch::scratch("gesta-unpack").expect("a test scratch directory");
        let pack_dir = staging.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let path = pack_dir.join("pack-t.pack");
        let mut f = std::fs::File::create(&path).unwrap();
        // A real pack header so the child gets as far as reading a body before refusing.
        f.write_all(b"PACK").unwrap();
        f.write_all(&2u32.to_be_bytes()).unwrap();
        f.write_all(&1u32.to_be_bytes()).unwrap();
        f.set_len(size).unwrap();
        staging
    }

    #[test]
    fn unpacking_a_pack_does_not_hold_the_pack() {
        // `pull_from_git_remote` read each pack file whole and then wrote it to the child's stdin,
        // so a pull was resident at the pack's size, and a remote chooses that size.
        //
        // The fixture is a 256 MB pack that the child will reject. What is measured is this
        // process's peak RSS across the passthrough, which a whole-file read would push up by
        // 256 MB.
        let size = 256u64 << 20;
        let staging = staging_with_padded_pack(size);

        let before = peak_rss_kib();
        let _ = super::unpack_each_pack(&staging); // the child refuses the pack, which is expected
        let after = peak_rss_kib();
        // Removed before the assertion, so a failing run does not leave 256 MB behind in /tmp.
        let _ = std::fs::remove_dir_all(&staging);

        match (before, after) {
            (Some(b), Some(a)) => {
                let grew_kib = a.saturating_sub(b);
                let allowance_kib = size / 4 / 1024; // 64 MiB
                assert!(
                    grew_kib < allowance_kib,
                    "streaming a {} MB pack to the child grew peak RSS by {} KiB, which is the \
                     pack being held rather than copied (allowance {} KiB)",
                    size >> 20,
                    grew_kib,
                    allowance_kib
                );
            }
            _ => eprintln!("no /proc/self/status here, so the memory ceiling was not checked"),
        }
    }

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
    /// The git commit name the export recorded for a changeset.
    fn git_of(conn: &Connection, changeset: &str) -> String {
        conn.query_row(
            "SELECT git_commit FROM changesets WHERE id = ?1",
            rusqlite::params![changeset],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
        .unwrap_or_else(|| panic!("changeset {} has no git_commit recorded", changeset))
    }

    /// A branch row with no head, which is what a fresh branch looks like.
    fn ensure_branch_row(conn: &Connection, branch: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch)
             VALUES(?1, NULL, 0, NULL)",
            rusqlite::params![branch],
        )
        .unwrap();
    }

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

    /// A repository whose history has a real merge, built by hand.
    ///
    /// The shape that exposed #177: `main` and `side` both descend from `base`, and a merge commit on
    /// `main` names both. A depth-first walk reversed per branch visits `main` before `side`'s parent,
    /// so `side`'s parent was not in the map when the child asked for it and the link was written as
    /// NULL with nothing reported.
    fn make_git_repo_with_merge(root: &std::path::Path) -> (String, String, String, String) {
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repo::open(root).unwrap();

        let blob = repo.write_object(Kind::Blob, b"base\n").unwrap();
        let blob_side = repo.write_object(Kind::Blob, b"side\n").unwrap();
        let tree = |entries: Vec<Entry>| {
            repo.write_object(
                Kind::Tree,
                &gitobj::build_tree_with(entries, HashKind::Sha1),
            )
            .unwrap()
        };
        let e = |name: &str, hash: &str| Entry {
            mode: "100644".into(),
            name: name.into(),
            hash: hash.to_string(),
        };

        let t1 = tree(vec![e("a.txt", &blob)]);
        let base = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(&t1, &[], "T", "t@e.st", 100, "+0000", "base\n"),
            )
            .unwrap();

        // main adds b.txt on top of base
        let t2 = tree(vec![e("a.txt", &blob), e("b.txt", &blob)]);
        let ours = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(
                    &t2,
                    std::slice::from_ref(&base),
                    "T",
                    "t@e.st",
                    200,
                    "+0000",
                    "ours\n",
                ),
            )
            .unwrap();

        // side adds c.txt on top of base
        let t3 = tree(vec![e("a.txt", &blob_side)]);
        let theirs = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(
                    &t3,
                    std::slice::from_ref(&base),
                    "T",
                    "t@e.st",
                    300,
                    "+0000",
                    "theirs\n",
                ),
            )
            .unwrap();
        repo.write_ref("refs/heads/side", &theirs).unwrap();

        // the merge, naming both
        let t4 = tree(vec![e("a.txt", &blob_side), e("b.txt", &blob)]);
        let merge = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(
                    &t4,
                    &[ours.clone(), theirs.clone()],
                    "T",
                    "t@e.st",
                    400,
                    "+0000",
                    "merge\n",
                ),
            )
            .unwrap();
        repo.write_ref("refs/heads/main", &merge).unwrap();

        (base, ours, theirs, merge)
    }

    #[test]
    fn a_merge_history_keeps_both_its_parents() {
        // #177: parent and parent2 were looked up in a map that the walk filled children-first, so a
        // merge commit's links came out NULL and nothing said so.
        let d = temp("merge-parents");
        let (base, ours, theirs, merge) = make_git_repo_with_merge(&d);

        let conn = crate::store::tests::mem_db();
        import(&conn, &d, None).unwrap();

        let id_of = |git_hash: &str| -> String {
            conn.query_row(
                "SELECT id FROM changesets WHERE git_commit = ?1",
                rusqlite::params![git_hash],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("no changeset for {git_hash}: {e}"))
        };
        let merge_id = id_of(&merge);
        let (p1, p2): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT parent, parent2 FROM changesets WHERE id = ?1",
                rusqlite::params![merge_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert_eq!(
            p1.as_deref(),
            Some(id_of(&ours).as_str()),
            "the first parent must be the first side of the merge"
        );
        assert_eq!(
            p2.as_deref(),
            Some(id_of(&theirs).as_str()),
            "the second parent must be kept, not written as NULL"
        );
        // And the base is still reachable through both.
        let base_id = id_of(&base);
        let _ = base_id;
        let _ = std::fs::remove_dir_all(&d);
    }

    fn make_git_repo_with_binary(root: &std::path::Path) -> Vec<u8> {
        // Bytes that are not valid UTF-8 and not valid Latin-1 either: a real PNG header plus bytes
        // that `from_utf8_lossy` turns into replacement characters.
        let payload: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0xFF, 0xFE,
            0x80, 0x81, 0xC0, 0xC1, 0x00, 0x01,
        ];
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repo::open(root).unwrap();

        let blob = repo.write_object(Kind::Blob, &payload).unwrap();
        let tree = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![Entry {
                        mode: "100644".into(),
                        name: "logo.png".into(),
                        hash: blob,
                    }],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let c1 = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(&tree, &[], "T", "t@e.st", 100, "+0000", "img\n"),
            )
            .unwrap();
        repo.write_ref("refs/heads/main", &c1).unwrap();
        payload
    }

    #[test]
    fn a_binary_file_imports_byte_for_byte() {
        // #179: every imported blob went through `String::from_utf8_lossy` before `put_object`, so
        // each invalid byte became U+FFFD. Export wrote the replacement characters back out. The
        // round trip compared equal because the parent-tree hash was lossy in the same way, which is
        // why this survived: the corruption was consistent and therefore invisible.
        let d = temp("binary");
        let original = make_git_repo_with_binary(&d);

        let conn = crate::store::tests::mem_db();
        import(&conn, &d, None).unwrap();

        let head: String = conn
            .query_row(
                "SELECT cs.id FROM branches b JOIN changesets cs ON cs.id = b.head WHERE b.name = 'main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let tree = crate::store::tree_at(&conn, &head).unwrap();
        let (_, hash) = tree
            .iter()
            .find(|(p, _)| p == "logo.png")
            .unwrap_or_else(|| panic!("logo.png is not in the imported tree: {tree:?}"));

        let stored = crate::store::get_object_bytes(&conn, hash).unwrap();
        assert_eq!(
            stored,
            original,
            "the imported object is not the file's bytes: {} vs {} bytes",
            stored.len(),
            original.len()
        );
        // And the hash is of those bytes, not of a lossy string.
        assert_eq!(*hash, crate::store::hash_bytes(&original));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_binaries_with_the_same_replacement_characters_stay_distinct() {
        // The lossy hash made different files hash alike: 0x80 and 0x81 are both U+FFFD. Two images
        // differing only in such bytes collapsed into one object.
        assert_ne!(
            crate::store::hash_bytes(&[0x80]),
            crate::store::hash_bytes(&[0x81]),
            "raw bytes must hash apart"
        );
        let lossy = String::from_utf8_lossy(&[0x80]);
        let lossy2 = String::from_utf8_lossy(&[0x81]);
        assert_eq!(
            crate::store::hash_content(&lossy),
            crate::store::hash_content(&lossy2),
            "and this is the collision the lossy form had"
        );
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

    /// A one commit repository whose tree holds a script with the executable bit and the same bytes
    /// as a plain file. The two blobs are identical on purpose: only the mode tells them apart.
    fn make_git_repo_with_a_mode(root: &std::path::Path) -> String {
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo = Repo::open(root).unwrap();

        let body = b"#!/bin/sh\necho hi\n".to_vec();
        let blob = repo.write_object(Kind::Blob, &body).unwrap();
        let tree = repo
            .write_object(
                Kind::Tree,
                &gitobj::build_tree_with(
                    vec![
                        Entry {
                            mode: "100755".into(),
                            name: "run.sh".into(),
                            hash: blob.clone(),
                        },
                        Entry {
                            mode: "100644".into(),
                            name: "run-copy.sh".into(),
                            hash: blob,
                        },
                    ],
                    HashKind::Sha1,
                ),
            )
            .unwrap();
        let commit = repo
            .write_object(
                Kind::Commit,
                &gitobj::build_commit(&tree, &[], "a", "a@b", 1700000000, "+0000", "with modes\n"),
            )
            .unwrap();
        std::fs::write(git.join("refs/heads/main"), format!("{}\n", commit)).unwrap();
        commit
    }

    #[test]
    fn the_executable_bit_survives_a_round_trip() {
        let src = temp("mode-src");
        make_git_repo_with_a_mode(&src);
        let conn = crate::store::tests::mem_db();
        import(&conn, &src, None).unwrap();

        // The store has to have kept the distinction, not just the bytes.
        let head: String = conn
            .query_row("SELECT head FROM branches WHERE name = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let tree = crate::store::tree_at_with_mode(&conn, &head).unwrap();
        let mode_of = |want: &str| {
            tree.iter()
                .find(|(p, _, _)| p == want)
                .map(|(_, _, m)| m.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            mode_of("run.sh"),
            "100755",
            "import lost the executable bit"
        );
        assert_eq!(
            mode_of("run-copy.sh"),
            "100644",
            "a plain file became executable"
        );

        // And the export has to write it back as git mode, which is the only form that matters.
        let out = temp("mode-out");
        export(&conn, &out, None).unwrap();
        let repo = Repo::open(&out).unwrap();
        let head_git = head_of(&repo, "main");
        let commit = gitobj::parse_commit(&repo.read_object(&head_git).unwrap().body).unwrap();
        let entries = gitobj::parse_tree(&repo.read_object(&commit.tree).unwrap().body).unwrap();
        let git_mode = |want: &str| {
            entries
                .iter()
                .find(|e| e.name == want)
                .map(|e| e.mode.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            git_mode("run.sh"),
            "100755",
            "export dropped the executable bit"
        );
        assert_eq!(git_mode("run-copy.sh"), "100644");

        let _ = std::fs::remove_dir_all(&src);
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
    fn an_export_writes_both_parents_of_a_merge() {
        // #178: `export_into` selected only `parent`, so every merge changeset became a
        // single-parent git commit. The tree survived so the content was right, and `git log
        // --graph`, `git merge-base` and `git branch --merged` all answered differently from the
        // history this repository holds.
        let conn = crate::store::tests::mem_db();
        crate::store::init_schema(&conn).unwrap();

        // A real merge shape, with every parent written explicitly: `commit_files` chains each commit
        // onto the current head, which quietly turns siblings into a line. Distinct messages and
        // distinct trees keep the four git commits distinct.
        let commit =
            |id: &str, parent: Option<&str>, parent2: Option<&str>, path: &str, body: &str| {
                let hash = crate::store::put_object(&conn, body).unwrap();
                conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
                 VALUES(?1,?1,'a',1,'pending',?2,?3,'main')",
                rusqlite::params![id, parent, parent2],
            )
            .unwrap();
                conn.execute(
                    "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,?2,NULL,?3)",
                    rusqlite::params![id, path, &hash],
                )
                .unwrap();
                crate::repo::commit_one(&conn, id).unwrap();
                // `commit_one` sets `parent` from the branch head, which is what a real commit does but
                // is not what a fixture for an arbitrary graph wants: leaving it alone turned this
                // merge into a commit whose two parents were the same. The intended parents are written
                // back afterwards.
                conn.execute(
                    "UPDATE changesets SET parent = ?2, parent2 = ?3 WHERE id = ?1",
                    rusqlite::params![id, parent, parent2],
                )
                .unwrap();
            };
        commit("cs1", None, None, "a.txt", "one\n");
        commit("cs2", Some("cs1"), None, "a.txt", "left\n");
        commit("cs3", Some("cs1"), None, "b.txt", "right\n");
        commit("cs4", Some("cs2"), Some("cs3"), "a.txt", "left\n");
        crate::store::set_branch_head(&conn, "main", "cs4").unwrap();

        let out = temp("merge-parents-out");
        let _ = std::fs::remove_dir_all(&out);
        let got = export(&conn, &out, None).unwrap();
        assert_eq!(got.commits, 4, "every changeset must be exported");

        let repo = Repo::open(&out).unwrap();
        let head = head_of(&repo, "main");
        let commit = gitobj::parse_commit(&repo.read_object(&head).unwrap().body).unwrap();
        assert_eq!(
            commit.parents.len(),
            2,
            "a merge must keep both parents, got {:?}",
            commit.parents
        );

        // Both parents must be the commits of cs2 and cs3, in that order - first parent is the
        // branch that was merged into.
        assert_eq!(
            commit.parents,
            vec![git_of(&conn, "cs2"), git_of(&conn, "cs3")],
            "the parents must be the two changesets the merge named, in order"
        );
        assert_ne!(
            git_of(&conn, "cs2"),
            git_of(&conn, "cs3"),
            "the fixture must produce two distinct parent commits, or it proves nothing"
        );

        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn an_export_survives_a_history_where_the_shortcut_is_not_a_parent() {
        // #187: `ancestors_bfs` reversed is not a topological order.
        //
        // The shape that breaks it: head cs4 has parents A=cs3 and B=cs2, and B is a DESCENDANT of A
        // (cs3 -> cs2). A BFS from the head reaches both A and B at distance 1, and it queues them in
        // parent order, so B can come out of the queue before A has been reached again as B's
        // ancestor. Reversed, that writes B before A and B's link to A is dropped.
        //
        // The first two versions of this test were wrong in different ways and both passed against
        // the old code, which is why the ordering check below comes before the export: the fixture
        // has to be a shape that actually breaks, or the test proves nothing.
        let conn = crate::store::tests::mem_db();
        crate::store::init_schema(&conn).unwrap();
        let commit =
            |id: &str, parent: Option<&str>, parent2: Option<&str>, path: &str, body: &str| {
                let hash = crate::store::put_object(&conn, body).unwrap();
                conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
                 VALUES(?1,?1,'a',1,'pending',?2,?3,'main')",
                rusqlite::params![id, parent, parent2],
            )
            .unwrap();
                conn.execute(
                    "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,?2,NULL,?3)",
                    rusqlite::params![id, path, &hash],
                )
                .unwrap();
                crate::repo::commit_one(&conn, id).unwrap();
                // `commit_one` takes the parent from the branch head; the graph this test needs is not
                // the one a linear commit produces.
                conn.execute(
                    "UPDATE changesets SET parent = ?2, parent2 = ?3 WHERE id = ?1",
                    rusqlite::params![id, parent, parent2],
                )
                .unwrap();
            };
        // cs1 -> cs3 -> cs2, then a head that merges cs3 and cs2. No cycle: cs2 descends from cs3.
        commit("cs1", None, None, "a.txt", "one\n");
        commit("cs3", Some("cs1"), None, "b.txt", "three\n");
        commit("cs2", Some("cs3"), None, "c.txt", "two\n");
        commit("cs4", Some("cs3"), Some("cs2"), "a.txt", "merged\n");
        crate::store::set_branch_head(&conn, "main", "cs4").unwrap();

        // The ordering rule itself, before any export: every parent must come before its children.
        let order = crate::store::topological_order(&conn, "cs4").unwrap();
        assert_eq!(
            order.len(),
            4,
            "every reachable changeset exactly once: {order:?}"
        );
        for (i, id) in order.iter().enumerate() {
            let (p1, p2): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT parent, parent2 FROM changesets WHERE id = ?1",
                    rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            for p in [p1, p2].into_iter().flatten() {
                let at = order.iter().position(|x| *x == p).unwrap_or_else(|| {
                    panic!("parent {} of {} is missing from {:?}", p, id, order)
                });
                assert!(
                    at < i,
                    "{} is listed before its parent {} in {:?}",
                    id,
                    p,
                    order
                );
            }
        }

        // And the shape must be one where a reversed BFS really does come out wrong, or this test
        // cannot fail on the bug it is about.
        let mut bfs = crate::store::ancestors_bfs(&conn, "cs4").unwrap();
        bfs.reverse();
        let bfs_pos = |x: &str| bfs.iter().position(|y| y == x).unwrap();
        assert!(
            bfs_pos("cs2") < bfs_pos("cs3"),
            "the fixture must be a shape where reversed BFS puts cs2 before its ancestor cs3, \
             otherwise this test cannot observe the bug: {bfs:?}"
        );

        let out = temp("criss-cross-out");
        let _ = std::fs::remove_dir_all(&out);
        let got = export(&conn, &out, None).expect("export must not drop a parent link");
        assert_eq!(got.commits, 4);

        let repo = Repo::open(&out).unwrap();
        let head = head_of(&repo, "main");
        let c = gitobj::parse_commit(&repo.read_object(&head).unwrap().body).unwrap();
        assert_eq!(c.parents.len(), 2, "the merge lost a parent");
        for p in &c.parents {
            assert!(
                repo.read_object(p).is_ok(),
                "the merge points at {} which was never written",
                p
            );
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn an_empty_tree_exports_instead_of_failing_the_whole_export() {
        // #206: `write_tree_for` errored on an empty tree, so one branch whose head had no files
        // aborted the entire export - every other branch included.
        let conn = crate::store::tests::mem_db();
        crate::store::init_schema(&conn).unwrap();
        ensure_branch_row(&conn, "empty");
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
             VALUES('cs1','nothing yet','a',10,'pending',NULL,NULL,'empty')",
            [],
        )
        .unwrap();
        crate::repo::commit_one(&conn, "cs1").unwrap();
        crate::store::set_branch_head(&conn, "empty", "cs1").unwrap();

        let out = temp("empty-tree-out");
        let _ = std::fs::remove_dir_all(&out);
        let got = export(&conn, &out, None).expect("an empty tree must not fail the export");
        assert_eq!(got.commits, 1);

        let repo = Repo::open(&out).unwrap();
        let head = head_of(&repo, "empty");
        let commit = gitobj::parse_commit(&repo.read_object(&head).unwrap().body).unwrap();
        let tree = repo
            .read_object(&commit.tree)
            .expect("the tree object must exist");
        assert!(
            gitobj::parse_tree(&tree.body).unwrap().is_empty(),
            "an empty changeset must export an empty tree"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn tags_are_exported() {
        // #206: `export_into` wrote branches and never tags, so they vanished silently.
        let conn = crate::store::tests::mem_db();
        crate::store::init_schema(&conn).unwrap();
        crate::repo::tests::commit_files(&conn, "cs1", "root", &[("a.txt", "one\n")]);
        conn.execute(
            "INSERT INTO tags(name, changeset_id, message, created_at) VALUES('v1.0','cs1',NULL,20)",
            [],
        )
        .unwrap();

        let out = temp("tags-out");
        let _ = std::fs::remove_dir_all(&out);
        let got = export(&conn, &out, None).unwrap();
        assert_eq!(got.tags, vec!["v1.0".to_string()]);

        let repo = Repo::open(&out).unwrap();
        let ref_path = repo.git_dir.join("refs").join("tags").join("v1.0");
        let written = std::fs::read_to_string(&ref_path)
            .unwrap_or_else(|e| panic!("refs/tags/v1.0 was not written: {e}"));
        assert_eq!(
            written.trim(),
            git_of(&conn, "cs1"),
            "the tag must point at the commit its changeset became"
        );
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
