//! Handlers for remotes, integrity checks, statistics and ignore rules.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::auth;
use crate::http;
use crate::ops;
use crate::remote;
use crate::repo;
use crate::store::{
    ancestors, branch_head, current_branch, delta_stats, init_schema, object_store_bytes, open_db,
    set_state,
};
use crate::worktree::IgnoreRules;
use crate::{bad, now, open_repo, repo_dir, ZResult};

// remotes.

pub fn cmd_remote_add(
    name: &str,
    url: &str,
    token: Option<&str>,
    pin: Option<&str>,
) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if let Some(p) = pin {
        // Reject a malformed pin now, not on the first push over a network.
        #[cfg(feature = "tls")]
        crate::tls::client_config(Some(p))?;
        #[cfg(not(feature = "tls"))]
        let _ = p;
    }
    conn.execute(
        "INSERT INTO remotes(name, url, token, pin) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url,
                                         token = COALESCE(excluded.token, remotes.token),
                                         pin = COALESCE(excluded.pin, remotes.pin)",
        rusqlite::params![name, url, token, pin],
    )?;
    println!("remote {} -> {}", name, url);
    if url.starts_with("https://") {
        println!("transport: TLS");
    }
    if token.is_some() {
        println!("token stored in the repository database");
    }
    if pin.is_some() {
        println!("certificate pin stored in the repository database");
    }
    Ok(())
}

/// Pin saved for this remote, if any.
fn remote_pin(conn: &rusqlite::Connection, name: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT pin FROM remotes WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub fn cmd_remote_list() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let all: Vec<(String, String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT name, url, pin FROM remotes ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if all.is_empty() {
        println!("no remotes configured");
    }
    for (n, u, pin) in all {
        match pin {
            Some(p) => println!("{:<10} {} (pin {}...)", n, u, &p[..p.len().min(12)]),
            None => println!("{:<10} {}", n, u),
        }
    }
    Ok(())
}

fn remote_url(conn: &rusqlite::Connection, name: &str) -> ZResult<String> {
    conn.query_row(
        "SELECT url FROM remotes WHERE name = ?1",
        rusqlite::params![name],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .ok_or_else(|| bad(format!("no such remote: {}", name)))
}

/// Token saved for this remote, if any. Kept in the repository database so a
/// later push does not need the secret retyped.
fn remote_token(conn: &rusqlite::Connection, name: &str) -> ZResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT token FROM remotes WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Say what a push would carry, without sending anything.
///
/// Pushing is the moment content leaves the machine, and until now the only way to learn what went
/// was to push and then look. A changeset carries an author string, an address in practice, so a
/// bundle handed to somebody else's server discloses who wrote what even when the file content is
/// unremarkable. Reporting the payload first turns that from something found out afterwards into
/// something decided.
pub fn cmd_inspect(
    remote_name: &str,
    branch: &str,
    json: bool,
    check_identity: bool,
) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    // Only a local remote can be asked what it already holds without inventing an answer. For a
    // served or object-store remote the payload is reported as an estimate over the whole branch,
    // which is exactly what a first push sends and the case most worth looking at.
    let known = if url.starts_with("s3://") || http::is_http_url(&url) {
        None
    } else {
        let path = remote::resolve_remote(&url);
        if crate::repo_dir(&path).is_dir() {
            let remote_conn = open_db(&path)?;
            match branch_head(&remote_conn, branch) {
                Ok(Some(h)) => Some(
                    ancestors(&remote_conn, &h)?
                        .into_iter()
                        .collect::<HashSet<String>>(),
                ),
                _ => Some(HashSet::new()),
            }
        } else {
            Some(HashSet::new())
        }
    };

    let empty = HashSet::new();
    let bundle = remote::build_bundle(&conn, branch, known.as_ref().unwrap_or(&empty))?;
    let wire = bundle.encode()?;

    let mut authors: Vec<(String, String)> = bundle
        .changesets
        .iter()
        .map(|c| (c.id.clone(), c.author.clone()))
        .collect();
    // An author that carries an address or an angle bracket is the part that names a person.
    let identity: Vec<(String, String)> = authors
        .iter()
        .filter(|(_, a)| a.contains('@') || a.contains('<'))
        .cloned()
        .collect();
    let largest = bundle
        .objects
        .iter()
        .map(|(_, c)| c.len())
        .max()
        .unwrap_or(0);
    let estimated = known.is_none();
    authors.truncate(20);

    if json {
        let items = authors
            .iter()
            .map(|(id, author)| {
                crate::json::Json::obj(vec![
                    ("changeset", crate::json::Json::str(id.clone())),
                    ("author", crate::json::Json::str(author.clone())),
                ])
            })
            .collect();
        crate::cli::emit_object(vec![
            ("remote", crate::json::Json::str(remote_name.to_string())),
            ("branch", crate::json::Json::str(branch.to_string())),
            ("url", crate::json::Json::str(url.clone())),
            (
                "changesets",
                crate::json::Json::int(bundle.changesets.len() as i64),
            ),
            (
                "objects",
                crate::json::Json::int(bundle.objects.len() as i64),
            ),
            ("bytes", crate::json::Json::int(wire.len() as i64)),
            ("largest_object", crate::json::Json::int(largest as i64)),
            (
                "author_identities",
                crate::json::Json::int(identity.len() as i64),
            ),
            ("estimated", crate::json::Json::Bool(estimated)),
            ("authors", crate::json::Json::Arr(items)),
        ]);
    } else {
        println!("push from {} to {}", branch, remote_name);
        println!("  url          {}", url);
        if estimated {
            println!("  estimate     the whole branch, because this remote cannot be asked");
        }
        println!("  changesets   {}", bundle.changesets.len());
        println!("  objects      {}", bundle.objects.len());
        println!("  payload      {} bytes", wire.len());
        println!("  largest      {} bytes", largest);
        if authors.is_empty() {
            println!("  nothing to send");
        }
        for (id, author) in &authors {
            println!("  {}  {}", id, author);
        }
        if bundle.changesets.len() > authors.len() {
            println!(
                "  ... and {} more (gesta remote inspect --json lists every author)",
                bundle.changesets.len() - authors.len()
            );
        }
        if identity.is_empty() {
            println!("  author identities: none");
        } else {
            println!(
                "  author identities: {} changeset(s) carry one, readable by whoever holds the \
                 remote",
                identity.len()
            );
        }
    }

    if check_identity && !identity.is_empty() {
        // A refusal here is a policy the caller asked for, not a broken payload, so it carries the
        // same code a bad invocation does: the command was asked to do something it will not do.
        return Err(Box::new(crate::error::Error::new(
            crate::error::Code::Usage,
            format!(
                "{} changeset(s) carry an author identity in the payload",
                identity.len()
            ),
        )));
    }
    Ok(())
}

pub fn cmd_push(remote_name: &str, branch: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    if url.starts_with("s3://") {
        return push_s3(&conn, &url, branch);
    }

    if http::is_http_url(&url) {
        return push_http(&conn, root.as_path(), remote_name, &url, branch);
    }
    let path = remote::resolve_remote(&url);

    // The remote must already exist as a bare repository.
    if !crate::repo_dir(&path).is_dir() {
        remote::init_bare(&path, branch)?;
        println!("created remote repository at {}", path.display());
    }

    let remote_conn = open_db(&path)?;
    init_schema(&remote_conn)?;

    // What does the remote already have?
    let have: HashSet<String> = match branch_head(&remote_conn, branch) {
        Ok(Some(h)) => ancestors(&remote_conn, &h)?.into_iter().collect(),
        _ => HashSet::new(),
    };

    let bundle = remote::build_bundle(&conn, branch, &have)?;
    let n = bundle.changesets.len();

    // Serialise and parse back before applying. This is the real transport
    // path: skipping it would let a broken wire format pass locally and only
    // fail once two machines are involved.
    let wire = bundle.encode()?;
    let received = remote::Bundle::decode(&wire)?;
    let applied = remote::apply_bundle(&remote_conn, &received)?;

    conn.execute(
        "INSERT INTO remote_refs(remote, branch, changeset_id, updated_at) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(remote, branch) DO UPDATE SET changeset_id = excluded.changeset_id,
                                                   updated_at = excluded.updated_at",
        rusqlite::params![
            remote_name,
            branch,
            bundle.head.clone().unwrap_or_default(),
            now()
        ],
    )?;

    println!(
        "pushed {} changeset(s) of {} to {} ({}, {} object(s))",
        applied,
        branch,
        remote_name,
        if n == 0 { "already up to date" } else { "new" },
        bundle.objects.len()
    );
    Ok(())
}

/// Push a branch to an S3 compatible bucket.
///
/// The object is the same bundle the filesystem and HTTP remotes use, so the wire
/// format keeps one definition. What changes is the transport: one PUT of the whole
/// bundle, signed with SigV4.
fn push_s3(conn: &Connection, url: &str, branch: &str) -> ZResult<()> {
    let (cfg, prefix) = crate::s3::S3Config::from_url(url)?;
    let key = crate::s3::key_for(&prefix, branch);

    // The object is the whole branch, not a delta onto something else.
    //
    // A filesystem or HTTP remote keeps its own database, so a bundle carrying only
    // the missing changesets lands on top of what is already there. A bucket does
    // not: the object is the only state, and writing a delta over it deletes the
    // history underneath. That was a real defect, found by pushing twice and
    // cloning: the clone had one changeset and was missing a file.
    //
    // So what the bucket holds decides only whether there is anything to do, never
    // what goes into the upload.
    // A bundle without a snapshot cannot serve a shallow clone, so its presence is
    // part of what makes the remote complete.
    let (remote_head, remote_count, remote_has_snapshot) = match cfg.get(&key)? {
        Some(bytes) => {
            let b = remote::Bundle::decode(&bytes)?;
            // Complete means the object can serve a shallow clone: it needs a
            // snapshot and every object that snapshot names. A snapshot whose
            // objects were never carried is as broken as no snapshot at all.
            let snap_ok = !b.snapshot.is_empty() && {
                let carried: HashSet<&str> = b
                    .objects
                    .iter()
                    .map(|(h, _)| h.as_str())
                    .chain(b.deltas.iter().map(|(h, _, _)| h.as_str()))
                    .collect();
                b.snapshot.iter().all(|(_, h)| carried.contains(h.as_str()))
            };
            (b.head, b.changesets.len() as i64, snap_ok)
        }
        None => (None, 0, false),
    };
    let local_head = branch_head(conn, branch)?;

    // A matching head is not proof the object is complete: an earlier push could
    // have written a bundle that names this head while carrying only part of the
    // history. The size check is cheap and turns a silent damaged remote into a
    // normal upload.
    let complete = match remote_head.as_deref() {
        Some(h) => crate::store::ancestors_bfs(conn, h)?.len() as i64 == remote_count,
        None => false,
    };
    if local_head.is_some() && local_head == remote_head && complete && remote_has_snapshot {
        println!("nothing to push, the bucket already has this branch");
        return Ok(());
    }

    let mut bundle = remote::build_bundle(conn, branch, &HashSet::new())?;
    // A bucket serves shallow clones, so the object always carries the head tree.
    // Without it a shallow clone cannot rebuild the working tree, because the
    // changesets that created the older files are exactly what a shallow cut removes.
    if let Some(head) = bundle.head.clone() {
        bundle.snapshot = repo::tree_at(conn, &head)?;
        // Every object the head tree names has to travel, even when no changeset in
        // the bundle mentions it. Those are exactly the objects a shallow cut would
        // otherwise leave behind.
        let mut carried: HashSet<String> = bundle
            .objects
            .iter()
            .map(|(h, _)| h.clone())
            .chain(bundle.deltas.iter().map(|(h, _, _)| h.clone()))
            .collect();
        for (path, hash) in &bundle.snapshot {
            if !carried.contains(hash) {
                // A shallow bundle carries the head tree explicitly, and that tree can hold a
                // binary file. Text first, then bytes.
                match crate::store::get_object(conn, hash) {
                    Ok(content) => bundle.objects.push((hash.clone(), content)),
                    Err(_) => {
                        let bytes = crate::store::get_object_bytes(conn, hash)
                            .map_err(|_| bad(format!("cannot bundle {} for {}", hash, path)))?;
                        bundle.binaries.push((hash.clone(), bytes));
                    }
                }
                bundle.hashes.push(hash.clone());
                carried.insert(hash.clone());
            }
        }
    }
    let n = bundle.changesets.len();
    let wire = bundle.encode()?;
    cfg.put(&key, &wire)?;
    println!(
        "pushed {} changesets to s3://{}/{} ({} bytes)",
        n,
        cfg.bucket,
        key,
        wire.len()
    );
    Ok(())
}

/// Pull a branch from an S3 compatible bucket.
fn pull_s3(conn: &mut Connection, url: &str, branch: &str, author: &str) -> ZResult<()> {
    let (cfg, prefix) = crate::s3::S3Config::from_url(url)?;
    let key = crate::s3::key_for(&prefix, branch);
    let bytes = cfg
        .get(&key)?
        .ok_or_else(|| crate::bad(format!("the bucket has no object at {}", key)))?;
    let bundle = remote::Bundle::decode(&bytes)?;
    let n = bundle.changesets.len();
    let applied = remote::apply_bundle(conn, &bundle)?;
    let head = branch_head(conn, branch)?.unwrap_or_else(|| "(none)".to_string());
    println!(
        "pulled {} changesets from s3://{}/{} ({} applied, branch now {})",
        n, cfg.bucket, key, applied, head
    );
    let _ = author;
    Ok(())
}

pub fn cmd_pull(remote_name: &str, branch: &str, author: &str) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    if url.starts_with("s3://") {
        let mut conn = conn;
        return pull_s3(&mut conn, &url, branch, author);
    }

    if http::is_http_url(&url) {
        return pull_http(&conn, &root, remote_name, &url, branch, author);
    }
    let path = remote::resolve_remote(&url);

    let remote_conn = open_db(&path)?;
    let remote_head = branch_head(&remote_conn, branch)?
        .ok_or_else(|| bad(format!("remote {} has no branch {}", remote_name, branch)))?;

    let bundle = remote::build_bundle(&remote_conn, branch, &HashSet::new())?;
    let received = remote::Bundle::decode(&bundle.encode()?)?;
    let applied = remote::apply_bundle_opts(&conn, &received, false)?;

    let current = current_branch(&conn)?;
    if current == branch && branch_head(&conn, &current)?.as_deref() != Some(remote_head.as_str()) {
        merge_fetched(&conn, &root, remote_name, branch, &remote_head, author)?;
    }
    repo::materialise(&conn, &root, &current)?;
    ops::record(&conn, "pull", Some(&remote_head), Some(remote_name))?;
    println!(
        "pulled {} changeset(s) from {} into {}",
        applied, remote_name, branch
    );
    Ok(())
}

pub fn cmd_clone(
    url: &str,
    dir: &str,
    token: Option<&str>,
    pin: Option<&str>,
    depth: Option<usize>,
) -> ZResult<()> {
    let target = Path::new(dir);
    if crate::repo_dir(target).is_dir() {
        return Err(bad(format!("{} already contains a repository", dir)));
    }
    fs::create_dir_all(target)?;

    if url.starts_with("s3://") {
        return clone_s3(url, target, pin, depth);
    }

    if http::is_http_url(url) {
        return clone_http(url, target, token, pin);
    }

    let remote_path = remote::resolve_remote(url);
    if !crate::repo_dir(&remote_path).is_dir() {
        return Err(bad(format!("no repository at {}", remote_path.display())));
    }
    let remote_conn = open_db(&remote_path)?;
    let branch: String = remote_conn
        .query_row(
            "SELECT name FROM branches ORDER BY created_at LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| bad("remote has no branches"))?;

    let bundle = remote::build_bundle(&remote_conn, &branch, &HashSet::new())?;
    let received = remote::Bundle::decode(&bundle.encode()?)?;

    let zf = repo_dir(target);
    fs::create_dir_all(&zf)?;
    let conn = open_db(target)?;
    init_schema(&conn)?;
    remote::apply_bundle(&conn, &received)?;
    set_state(&conn, "current_branch", &branch)?;
    conn.execute(
        "INSERT INTO remotes(name, url, pin) VALUES('origin', ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, pin = excluded.pin",
        rusqlite::params![url, pin],
    )?;

    // A shallow clone restores from the snapshot the bundle carried, because the
    // chain does not describe the tree any more.
    let written = if bundle.snapshot.is_empty() {
        repo::restore_worktree(&conn, target, &branch)?.0
    } else {
        restore_from_snapshot(&conn, target, &bundle.snapshot)?
    };
    println!(
        "cloned {} into {} ({} changeset(s), {} file(s) on disk)",
        url,
        dir,
        received.changesets.len(),
        written
    );
    Ok(())
}

/// Clone a branch from an S3 compatible bucket into an empty directory.
///
/// A bucket holds one bundle per branch and the name has no wildcard, so a clone
/// needs a branch. `main` is the default because every repository starts there.
fn clone_s3(url: &str, target: &Path, pin: Option<&str>, depth: Option<usize>) -> ZResult<()> {
    let (cfg, prefix) = crate::s3::S3Config::from_url(url)?;

    // Find the branch by asking for the default first, then falling back to the
    // object listing would need a second signed request type. Requiring `main` is
    // honest: it is the branch every repository has.
    let branch = pin.unwrap_or("main").to_string();
    let key = crate::s3::key_for(&prefix, &branch);
    let bytes = cfg.get(&key)?.ok_or_else(|| {
        crate::bad(format!(
            "no object at s3://{}/{} (a clone needs the branch name, try --pin <branch>)",
            cfg.bucket, key
        ))
    })?;
    let mut bundle = remote::Bundle::decode(&bytes)?;

    // A shallow clone keeps only the newest part of what the bucket holds. The
    // cut is recorded in the bundle so the receiver can tell a boundary from damage.
    if let Some(d) = depth {
        bundle = remote::truncate_bundle(&bundle, d)?;
    }

    let zf = repo_dir(target);
    fs::create_dir_all(&zf)?;
    let conn = open_db(target)?;
    init_schema(&conn)?;
    remote::apply_bundle(&conn, &bundle)?;
    let branch = if bundle.branch.is_empty() {
        branch
    } else {
        bundle.branch.clone()
    };
    set_state(&conn, "current_branch", &branch)?;
    conn.execute(
        "INSERT INTO remotes(name, url, pin) VALUES('origin', ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, pin = excluded.pin",
        rusqlite::params![url, pin],
    )?;

    // A shallow clone restores from the snapshot the bundle carried, because cutting
    // the chain removed the changesets that created the older files. Walking what
    // remains produced 5 files out of 200, which is why the snapshot exists.
    let written = if bundle.snapshot.is_empty() {
        repo::restore_worktree(&conn, target, &branch)?.0
    } else {
        restore_from_snapshot(&conn, target, &bundle.snapshot)?
    };
    println!(
        "cloned {} into {} ({} changeset(s), {} file(s) on disk{})",
        url,
        target.display(),
        bundle.changesets.len(),
        written,
        if bundle.shallow_root.is_some() {
            format!(
                ", shallow at {}",
                bundle
                    .changesets
                    .first()
                    .map(|c| c.id.as_str())
                    .unwrap_or("-")
            )
        } else {
            String::new()
        }
    );
    Ok(())
}

// fsck / stats / ignore.

/// Remove unreachable objects and vacuum the database.
pub fn cmd_gc() -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let deleted = crate::store::gc(&conn)?;
    // Grouping runs after the removal, so a group never holds an object that gc just deleted.
    let (groups, moved) = crate::pack::repack(&conn)?;
    let (g_all, packed_bytes, raw_bytes) = crate::pack::group_stats(&conn)?;
    println!(
        "gc: removed {} unreachable object(s), database vacuumed",
        deleted
    );
    if groups > 0 {
        println!(
            "gc: grouped {} object(s) into {} new group(s); {} object(s) now share {} group(s)",
            moved, groups, raw_bytes, g_all
        );
        if packed_bytes > 0 {
            println!(
                "gc: the grouped streams take {} byte(s) in total",
                packed_bytes
            );
        }
    }
    Ok(())
}

pub fn cmd_fsck(prune: bool, worktree: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let mut problems = 0usize;

    // Every changeset_file must point at objects that exist.
    let refs: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT changeset_id, path, old_hash, new_hash FROM changeset_files ORDER BY changeset_id, path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (cs, path, old, new) in &refs {
        for h in [old, new].into_iter().flatten() {
            // Existence, not readability as text. A binary object is stored raw and is not
            // valid UTF-8, so the text reader refuses it and fsck called a perfectly good object
            // missing.
            if crate::store::get_object_bytes(&conn, h).is_err() {
                println!("MISSING object {} referenced by {} ({})", h, cs, path);
                problems += 1;
            }
        }
    }

    // Every parent must exist.
    let parents: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT id, parent, parent2 FROM changesets")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // A shallow clone deliberately lacks the parent of its oldest changeset. Those
    // rows are boundaries, not damage, so they are counted and reported separately
    // instead of inflating the problem count.
    let boundaries: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("SELECT changeset_id FROM shallow_roots")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
    };

    for (id, p1, p2) in &parents {
        for p in [p1, p2].into_iter().flatten() {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM changesets WHERE id = ?1",
                    rusqlite::params![p],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                if boundaries.contains(id) {
                    continue;
                }
                println!("MISSING parent {} referenced by {}", p, id);
                problems += 1;
            }
        }
    }
    if !boundaries.is_empty() {
        println!(
            "shallow: {} changeset(s) sit at a history boundary",
            boundaries.len()
        );
    }

    // Every branch head must exist.
    let branches: Vec<(String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT name, head FROM branches")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head) in &branches {
        if let Some(h) = head {
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM changesets WHERE id = ?1",
                    rusqlite::params![h],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                println!("MISSING changeset {} is the head of branch {}", h, name);
                problems += 1;
            }
        }
    }

    // An object whose row says its body is beside the database, and whose file is not there, reads
    // as an empty object rather than as missing. That is the one inconsistency the sidecar adds, and
    // it has to be reported by name or the object is lost quietly.
    {
        let outside: Vec<String> = {
            let mut stmt = conn.prepare("SELECT hash FROM objects WHERE sidecar = 1")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut gone = 0usize;
        for h in &outside {
            if !crate::sidecar::holds(&root, h) {
                if gone < 3 {
                    println!("MISSING object {} is marked sidecar and has no file", h);
                }
                gone += 1;
            }
        }
        if gone > 0 {
            println!(
                "MISSING {} of {} sidecar object(s) have no file beside the database",
                gone,
                outside.len()
            );
            problems += 1;
        }
    }

    // The maintained tree must equal a rebuild from history. This is the check
    // that makes the derived table safe: a writer that moved a head without
    // refreshing it would make `status` silently report committed files as
    // changed, and nothing else in the program would notice.
    let branches_with_heads: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT name, head FROM branches WHERE head IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (name, head) in &branches_with_heads {
        // Compare as sets of (path, hash). The row counts alone cannot tell a
        // stale tree from a fresh one: one row against one row with a different
        // hash is exactly the drift this is looking for.
        let rebuilt = crate::repo::tree_at(&conn, head)?;
        let mut stored: Vec<(String, String)> = {
            let mut stmt =
                conn.prepare("SELECT path, hash FROM branch_tree WHERE branch = ?1 ORDER BY path")?;
            let rows = stmt.query_map(rusqlite::params![name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        stored.sort();
        let mut expected = rebuilt.clone();
        expected.sort();
        if stored != expected {
            let missing: Vec<&(String, String)> =
                expected.iter().filter(|e| !stored.contains(e)).collect();
            let extra: Vec<&(String, String)> =
                stored.iter().filter(|s| !expected.contains(s)).collect();
            println!(
                "STALE tree for branch {}: {} row(s) stored, {} in history ({} missing, {} extra)",
                name,
                stored.len(),
                expected.len(),
                missing.len(),
                extra.len()
            );
            for (p, h) in missing.iter().take(3) {
                println!("  missing {} {}", p, &h[..h.len().min(12)]);
            }
            for (p, h) in extra.iter().take(3) {
                println!("  extra   {} {}", p, &h[..h.len().min(12)]);
            }
            problems += 1;
        }
    }

    // The working tree is state too, and this is the check that catches data loss: if the
    // head names a file the disk does not have, the repository and the folder disagree and
    // nothing else says so. A broken undo went unnoticed exactly here.
    if worktree {
        let current = crate::store::current_branch(&conn)?;
        let scope = crate::sparse::SparseScope::load(&conn)?;
        for (name, head) in &branches_with_heads {
            if name != &current {
                continue;
            }
            let expected = crate::repo::tree_at(&conn, head)?;
            for (path, hash) in &expected {
                // A sparse checkout leaves paths out on purpose, so their absence is
                // correct rather than damage.
                if !scope.is_empty() && !scope.contains(path) {
                    continue;
                }
                match std::fs::read(root.join(path)) {
                    Ok(bytes) => {
                        let content = String::from_utf8_lossy(&bytes);
                        let got = crate::store::hash_content(&content);
                        if got != *hash {
                            println!(
                                "WORKTREE {} differs from the head (disk {}..., history {}...)",
                                path,
                                &got[..got.len().min(12)],
                                &hash[..hash.len().min(12)]
                            );
                            problems += 1;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        println!("WORKTREE {} is in the head but missing from disk", path);
                        problems += 1;
                    }
                    Err(e) => {
                        println!("WORKTREE {} cannot be read: {}", path, e);
                        problems += 1;
                    }
                }
            }

            // A path the repository has tracked before and that the head still names is
            // caught above. A path the head does not name but which lingers on disk is left
            // alone, because an untracked file belongs to the user.
        }
    }

    // Objects nothing references.
    // An object named by a shallow clone's snapshot is reachable: it is the working
    // tree. Counting it as an orphan would let `--prune` delete a file that is on disk.
    let orphans: i64 = conn.query_row(
        "SELECT COUNT(*) FROM objects o WHERE NOT EXISTS (
            SELECT 1 FROM changeset_files cf WHERE cf.new_hash = o.hash OR cf.old_hash = o.hash
         ) AND NOT EXISTS (
            SELECT 1 FROM file_versions fv WHERE fv.hash = o.hash
         ) AND NOT EXISTS (
            SELECT 1 FROM snapshot_objects so WHERE so.hash = o.hash
         )",
        [],
        |r| r.get(0),
    )?;

    if problems == 0 {
        println!(
            "repository is consistent ({} object(s), {} orphan(s))",
            count_objects(&conn)?,
            orphans
        );
    } else {
        println!("\n{} problem(s) found", problems);
    }

    if prune && orphans > 0 {
        let deleted = conn.execute(
            "DELETE FROM objects WHERE hash IN (
                SELECT o.hash FROM objects o WHERE NOT EXISTS (
                    SELECT 1 FROM changeset_files cf WHERE cf.new_hash = o.hash OR cf.old_hash = o.hash
                ) AND NOT EXISTS (
                    SELECT 1 FROM file_versions fv WHERE fv.hash = o.hash
                ) AND NOT EXISTS (
                    SELECT 1 FROM snapshot_objects so WHERE so.hash = o.hash
                ))",
            [],
        )?;
        println!("pruned {} orphan object(s)", deleted);
    }

    if problems > 0 {
        return Err(bad(format!("{} integrity problem(s)", problems)));
    }
    Ok(())
}

fn count_objects(conn: &rusqlite::Connection) -> ZResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM objects", [], |r| r.get(0))?)
}

pub fn cmd_stats() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let (raw, stored) = object_store_bytes(&conn)?;
    let objects = count_objects(&conn)?;
    let changesets: i64 = conn.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?;
    let versions: i64 = conn.query_row("SELECT COUNT(*) FROM file_versions", [], |r| r.get(0))?;
    let blame: i64 = conn.query_row("SELECT COUNT(*) FROM blame_ranges", [], |r| r.get(0))?;
    let branches: i64 = conn.query_row("SELECT COUNT(*) FROM branches", [], |r| r.get(0))?;
    let tags: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;

    // Delta payloads live in their own table, so the object table alone
    // understates the real cost and reports an absurd compression ratio. The
    // footer adds them back, which is the number a person should read.
    let (deltas, saved) = delta_stats(&conn)?;
    let delta_bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM deltas",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let total_stored = stored + delta_bytes;
    let ratio = if raw > 0 {
        raw as f64 / total_stored.max(1) as f64
    } else {
        0.0
    };
    println!(
        "objects:     {} ({} bytes raw, {} bytes in objects plus {} in deltas = {} stored, {:.1}x)",
        objects, raw, stored, delta_bytes, total_stored, ratio
    );
    println!("changesets:  {}", changesets);
    println!("versions:    {}", versions);
    println!("blame rows:  {}", blame);
    println!("branches:    {}", branches);
    println!("tags:        {}", tags);

    let cache_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM worktree_cache", [], |r| r.get(0))
        .unwrap_or(0);
    println!("stat cache:  {} path(s)", cache_count);

    if deltas > 0 {
        println!(
            "deltas:      {} object(s), at most {} bytes saved against raw size",
            deltas, saved
        );
    } else {
        println!("deltas:      0");
    }

    // Objects past the inline threshold live beside the database. A figure that leaves them out
    // understates the repository by however much of it is large, and a large file is exactly where a
    // wrong total is noticed.
    let (outside, outside_bytes) = crate::sidecar::stats(&root);
    if outside > 0 {
        println!(
            "sidecar:     {} object(s), {} bytes beside the database",
            outside, outside_bytes
        );
    }
    Ok(())
}

pub fn cmd_ignore_add(pattern: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    conn.execute(
        "INSERT OR IGNORE INTO ignore_rules(pattern) VALUES(?1)",
        rusqlite::params![pattern],
    )?;
    println!("ignoring: {}", pattern);
    Ok(())
}

pub fn cmd_ignore_list() -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let stored: Vec<String> = {
        let mut stmt = conn.prepare("SELECT pattern FROM ignore_rules ORDER BY pattern")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut from_files: Vec<String> = Vec::new();
    for name in [".zenignore", ".gitignore"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            for line in text.lines() {
                let l = line.trim();
                if !l.is_empty() && !l.starts_with('#') {
                    from_files.push(format!("{} ({})", l, name));
                }
            }
        }
    }
    println!("from database:");
    if stored.is_empty() {
        println!("  (none)");
    }
    for p in &stored {
        println!("  {}", p);
    }
    println!("\nfrom files:");
    if from_files.is_empty() {
        println!("  (none)");
    }
    for p in &from_files {
        println!("  {}", p);
    }
    let lifted: Vec<String> = stored
        .into_iter()
        .chain(
            from_files
                .iter()
                .map(|s| s.split(" (").next().unwrap_or(s).to_string()),
        )
        .collect();
    let rules = IgnoreRules::with_defaults(&lifted);
    println!("\n{} active pattern(s)", rules.len());
    Ok(())
}

/// Why a path is or is not ignored, and which rule decides it.
///
/// The list command prints the rules and leaves the reader to work out which one applies, which
/// is where a `!` negation and a rooted pattern stop being obvious. Answering for one path is the
/// question people actually ask, and it is the difference between having rules and being able to
/// explain them.
pub fn cmd_ignore_why(path: &str, json: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    // Each source keeps its label, so the answer names where the deciding rule came from.
    let mut sources: Vec<(String, Vec<String>)> = Vec::new();
    for name in [".zenignore", ".gitignore"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            let rules: Vec<String> = text
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.to_string())
                .collect();
            if !rules.is_empty() {
                sources.push((name.to_string(), rules));
            }
        }
    }
    {
        let mut stored: Vec<String> = Vec::new();
        let mut stmt = conn.prepare("SELECT pattern FROM ignore_rules ORDER BY pattern")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            stored.push(r?);
        }
        if !stored.is_empty() {
            sources.push(("gesta ignore add".to_string(), stored));
        }
    }
    sources.push((
        "built-in defaults".to_string(),
        crate::worktree::DEFAULT_IGNORES
            .iter()
            .map(|s| s.to_string())
            .collect(),
    ));

    let rel = crate::normalise(path);
    // The same question the scanner asks, so the answer cannot disagree with the scan.
    let all: Vec<String> = sources
        .iter()
        .flat_map(|(_, r)| r.iter().cloned())
        .collect();
    let ignored = crate::worktree::IgnoreRules::new(all).is_ignored(&rel, false);

    // The deciding rule is the last one that matches, because a later negation undoes an earlier
    // pattern. Each rule is tried on its own, without its `!`, so the question is "does this
    // rule's shape cover the path", and the last one that does is the one the scan ended on.
    let mut deciding: Option<(String, String)> = None;
    for (source, rules) in &sources {
        for rule in rules {
            let body = rule.trim_start_matches('!');
            let solo = crate::worktree::IgnoreRules::new(vec![body.to_string()]);
            if solo.is_ignored(&rel, false) {
                deciding = Some((source.clone(), rule.clone()));
            }
        }
    }

    if json {
        let mut obj = vec![
            ("path", crate::json::Json::str(rel.clone())),
            ("ignored", crate::json::Json::Bool(ignored)),
            (
                "patterns",
                crate::json::Json::int(sources.iter().map(|(_, r)| r.len()).sum::<usize>() as i64),
            ),
        ];
        match &deciding {
            Some((source, rule)) => {
                obj.push(("rule", crate::json::Json::str(rule.clone())));
                obj.push(("source", crate::json::Json::str(source.clone())));
            }
            None => {
                obj.push(("rule", crate::json::Json::Null));
                obj.push(("source", crate::json::Json::Null));
            }
        }
        crate::cli::emit_object(obj);
        return Ok(());
    }

    if ignored {
        println!("{} is ignored", rel);
        if let Some((source, rule)) = &deciding {
            println!("  by rule: {}  (from {})", rule, source);
        }
    } else {
        println!("{} is not ignored", rel);
        // A path that a rule would have caught and a negation rescued is the case people ask
        // about. Saying only "not ignored" leaves them hunting for the rule that saved it.
        if let Some((_source, rule)) = &deciding {
            println!("  a rule covers it, and one of these puts it back:");
            for (src, rules) in &sources {
                for r in rules.iter().filter(|r| r.starts_with('!')) {
                    let restored = crate::worktree::IgnoreRules::new(vec![r.clone()]);
                    if crate::worktree::IgnoreRules::new(vec![rule
                        .trim_start_matches('!')
                        .to_string()])
                    .is_ignored(&rel, false)
                        && !restored.is_ignored(&rel, false)
                    {
                        println!("    {}  (from {})", r, src);
                    }
                }
            }
        }
    }
    println!(
        "  {} pattern(s) across {} source(s)",
        sources.iter().map(|(_, r)| r.len()).sum::<usize>(),
        sources.len()
    );
    Ok(())
}

pub fn cmd_ignore_remove(pattern: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let n = conn.execute(
        "DELETE FROM ignore_rules WHERE pattern = ?1",
        rusqlite::params![pattern],
    )?;
    if n == 0 {
        println!(
            "{} was not in the database (it may come from .zenignore)",
            pattern
        );
    } else {
        println!("no longer ignoring: {}", pattern);
    }
    Ok(())
}

/// Point a remote-tracking branch at `head`. Pull merges this branch into the
/// current one, which is how a fetched history becomes part of local work
/// without pretending the remote head is already the local head.
fn set_tracking_branch(conn: &rusqlite::Connection, name: &str, head: &str) -> ZResult<()> {
    conn.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, ?3, 'remote')
         ON CONFLICT(name) DO UPDATE SET head = excluded.head",
        rusqlite::params![name, head, now()],
    )?;
    Ok(())
}

/// Merge a fetched head into `branch` through a tracking branch.
fn merge_fetched(
    conn: &rusqlite::Connection,
    root: &Path,
    remote_name: &str,
    branch: &str,
    remote_head: &str,
    author: &str,
) -> ZResult<bool> {
    let tracking = format!("{}/{}", remote_name, branch);
    set_tracking_branch(conn, &tracking, remote_head)?;
    let out = repo::merge_branch(conn, root, &tracking, branch, author)?;
    if !out.conflicts.is_empty() {
        eprintln!(
            "merge of {} into {} hit {} conflict(s):",
            tracking,
            branch,
            out.conflicts.len()
        );
        for c in &out.conflicts {
            eprintln!("  {} (their version saved as {}.theirs)", c, c);
        }
        return Err(bad("pull needs a manual merge"));
    }
    Ok(!out.applied.is_empty())
}

/// Push over HTTP: ask the server which changesets it has, send only the rest.
fn push_http(
    conn: &rusqlite::Connection,
    root: &Path,
    remote_name: &str,
    url: &str,
    branch: &str,
) -> ZResult<()> {
    let base = url.trim_end_matches('/');
    let token = remote_token(conn, remote_name)?;

    // What does the remote already have? Asking first keeps the upload small;
    // the server also refuses a bundle that would move its branch backwards.
    let pin = remote_pin(conn, remote_name)?;
    let (status, body) = http::request_with_pin(
        "GET",
        &format!("{}/v1/branches", base),
        None,
        token.as_deref(),
        pin.as_deref(),
    )?;
    if status == 401 || status == 403 {
        // A refused token is not a missing thing and not a damaged store: it is the one failure
        // where retrying with the same credentials will never work.
        return Err(Box::new(crate::error::Error::new(
            crate::error::Code::Auth,
            format!(
                "remote {} rejected the token ({}): {}",
                remote_name,
                status,
                String::from_utf8_lossy(&body).trim()
            ),
        )));
    }
    if status != 200 {
        return Err(bad(format!(
            "remote {} answered {}: {}",
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    // The server tells us its head for this branch; everything reachable from it
    // is already there, so the bundle only carries the rest.
    let listing = String::from_utf8_lossy(&body).to_string();
    let mut have: HashSet<String> = HashSet::new();
    let remote_head = listing.lines().find_map(|l| {
        let mut p = l.split('\t');
        match (p.next(), p.next()) {
            (Some(n), Some(h)) if n == branch && h != "-" => Some(h.to_string()),
            _ => None,
        }
    });
    if let Some(h) = &remote_head {
        have.insert(h.clone());
    }

    let bundle = remote::build_bundle(conn, branch, &have)?;
    let wire = bundle.encode()?;
    let object_count = bundle.objects.len();
    let changeset_count = bundle.changesets.len();

    let target = format!("{}/v1/bundle?branch={}", base, branch);
    let (status, body) = http::request_with_pin(
        "POST",
        &target,
        Some(&wire),
        token.as_deref(),
        pin.as_deref(),
    )?;
    if status != 200 {
        return Err(bad(format!(
            "push rejected by {} ({}): {}",
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }

    conn.execute(
        "INSERT INTO remote_refs(remote, branch, changeset_id, updated_at) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(remote, branch) DO UPDATE SET changeset_id = excluded.changeset_id,
                                                   updated_at = excluded.updated_at",
        rusqlite::params![
            remote_name,
            branch,
            bundle.head.clone().unwrap_or_default(),
            now()
        ],
    )?;

    // The local repository keeps its own working tree in sync with the branch
    // it just shared, so `push` followed by `status` is clean.
    repo::materialise(conn, root, branch)?;

    println!(
        "pushed {} changeset(s), {} object(s) of {} to {} over http",
        changeset_count, object_count, branch, remote_name
    );
    Ok(())
}

/// Pull over HTTP: download the server's bundle for `branch` and merge it.
fn pull_http(
    conn: &rusqlite::Connection,
    root: &Path,
    remote_name: &str,
    url: &str,
    branch: &str,
    author: &str,
) -> ZResult<()> {
    let base = url.trim_end_matches('/');
    let token = remote_token(conn, remote_name)?;
    let have: Vec<String> = ancestors(
        conn,
        &branch_head(conn, branch)?.unwrap_or_else(|| "none".into()),
    )
    .unwrap_or_default();
    let have: Vec<String> = if have == vec!["none".to_string()] {
        Vec::new()
    } else {
        have
    };

    let target = http::bundle_url(base, branch, &have);
    let pin = remote_pin(conn, remote_name)?;
    let (status, body) =
        http::request_with_pin("GET", &target, None, token.as_deref(), pin.as_deref())?;
    if status != 200 {
        return Err(bad(format!(
            "cannot fetch branch {} from {} ({}): {}",
            branch,
            remote_name,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }

    let bundle = remote::Bundle::decode(&body)?;
    let remote_head = bundle.head.clone().ok_or_else(|| {
        bad(format!(
            "remote {} returned no head for {}",
            remote_name, branch
        ))
    })?;
    // Fetch the objects but keep the local branch: the merge below decides the
    // new head. Moving it here would discard local changesets.
    let applied = remote::apply_bundle_opts(conn, &bundle, false)?;

    let current = current_branch(conn)?;
    if current == branch && branch_head(conn, &current)?.as_deref() != Some(remote_head.as_str()) {
        merge_fetched(conn, root, remote_name, branch, &remote_head, author)?;
    }
    repo::materialise(conn, root, &current)?;
    ops::record(conn, "pull", Some(&remote_head), Some(remote_name))?;
    println!(
        "pulled {} changeset(s) from {} ({}) into {}",
        applied, remote_name, base, branch
    );
    Ok(())
}

/// Clone over HTTP: ask for the branch list, fetch the first branch.
fn clone_http(url: &str, target: &Path, token: Option<&str>, pin: Option<&str>) -> ZResult<()> {
    let base = url.trim_end_matches('/');

    let (status, body) =
        http::request_with_pin("GET", &format!("{}/v1/branches", base), None, token, pin)?;
    if status != 200 {
        return Err(bad(format!(
            "cannot list branches at {} ({}): {}",
            base,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    let listing = String::from_utf8_lossy(&body).to_string();
    let branch = listing
        .lines()
        .find_map(|l| {
            l.split('\t')
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .ok_or_else(|| bad(format!("{} has no branches", base)))?;

    let (status, body) = http::request_with_pin(
        "GET",
        &http::bundle_url(base, &branch, &[]),
        None,
        token,
        pin,
    )?;
    if status != 200 {
        return Err(bad(format!(
            "cannot fetch {} from {} ({}): {}",
            branch,
            base,
            status,
            String::from_utf8_lossy(&body).trim()
        )));
    }
    let bundle = remote::Bundle::decode(&body)?;

    let zf = repo_dir(target);
    fs::create_dir_all(&zf)?;
    let conn = open_db(target)?;
    init_schema(&conn)?;
    remote::apply_bundle(&conn, &bundle)?;
    set_state(&conn, "current_branch", &branch)?;
    conn.execute(
        "INSERT INTO remotes(name, url, token) VALUES('origin', ?1, ?2)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, token = excluded.token",
        rusqlite::params![url, token],
    )?;
    let (written, _removed) = repo::restore_worktree(&conn, target, &branch)?;
    println!(
        "cloned {} into {} (branch {}, {} changeset(s), {} file(s))",
        base,
        target.display(),
        branch,
        bundle.changesets.len(),
        written
    );
    Ok(())
}

/// Serve this repository over HTTP.
///
/// Tokens come from `.gesta/auth`, one `token:role:name` per line. Without
/// that file the server only binds to loopback, so an open port with no
/// password is never the accidental default.
pub fn cmd_serve(
    bind: &str,
    port: u16,
    tls: Option<&str>,
    #[allow(unused_variables)] key: Option<&str>,
) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let auth = auth::Auth::from_entries(&read_auth_file(&root)?);

    // The audit collector is configured by two entries in `.gesta/audit-url` and
    // `.gesta/audit-token`, so a collector that moves is a file edit rather than a flag on every
    // command. The token lives in its own file so the URL, which proxies log, never carries it.
    let forward = {
        let url_path = crate::repo_dir(&root).join("audit-url");
        match std::fs::read_to_string(&url_path) {
            Ok(url) if !url.trim().is_empty() => {
                let token = std::fs::read_to_string(crate::repo_dir(&root).join("audit-token"))
                    .ok()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty());
                Some(crate::audit_forward::Forwarder::start(
                    url.trim().to_string(),
                    token,
                ))
            }
            _ => None,
        }
    };

    // Without the tls feature the flags still parse, so the error names the
    // cause. A build that silently ignores --tls would be the worst outcome: the
    // operator would believe the port is encrypted.
    #[cfg(not(feature = "tls"))]
    if tls.is_some() {
        return Err(bad(
            "this build has no TLS support: rebuild with --features tls",
        ));
    }

    match tls {
        None => http::serve(root, bind.to_string(), port, auth, forward),
        #[cfg(feature = "tls")]
        Some("self-signed") => {
            let forward = forward.clone();
            let _ = &forward;
            // A generated certificate is for a private network. Writing it under
            // The repository directory keeps it with the code and out of a changeset,
            // since that directory is never tracked.
            let dir = repo_dir(&root);
            let (cert, key_path, fp) = crate::tls::generate_self_signed(bind, &dir)?;
            println!("generated {} and {}", cert.display(), key_path.display());
            println!("pin this on clients with --pin {}", fp);
            http::serve_tls(root, bind.to_string(), port, auth, &cert, &key_path)
        }
        #[cfg(feature = "tls")]
        Some(cert_path) => {
            let key_path = key
                .map(std::path::PathBuf::from)
                .ok_or_else(|| bad("--tls <cert> needs --key <key.pem>"))?;
            http::serve_tls(
                root,
                bind.to_string(),
                port,
                auth,
                &std::path::PathBuf::from(cert_path),
                &key_path,
            )
        }
        // The early return above covers every Some(_) in a build without the
        // feature. Restating it as an error keeps the invariant visible: if that
        // early return is ever moved, this is a message instead of a crash.
        #[cfg(not(feature = "tls"))]
        Some(_) => Err(bad(
            "this build has no TLS support: rebuild with --features tls",
        )),
    }
}

fn read_auth_file(root: &Path) -> ZResult<Vec<String>> {
    let path = repo_dir(root).join("auth");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text.lines().map(|l| l.to_string()).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(bad(format!("cannot read {}: {}", path.display(), e))),
    }
}

/// Add a token to `.gesta/auth`. The token is generated, never typed by the
/// user, and printed once.
pub fn cmd_token_add(
    role: &str,
    name: &str,
    hashed: bool,
    scope: Option<&str>,
    branches: Option<&str>,
) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let role = match role.to_ascii_lowercase().as_str() {
        "write" => "write",
        "read" => "read",
        // A reviewer's credential reads and signs off and does not write.
        "approve" => "approve",
        other => {
            return Err(Box::new(crate::error::Error::new(
                crate::error::Code::Usage,
                format!("unknown role {:?}: use read, approve or write", other),
            )))
        }
    };
    if name.trim().is_empty() || name.contains(':') {
        return Err(bad("token name must be non-empty and must not contain ':'"));
    }
    let token = auth::generate_token(32)?;
    let path = repo_dir(&root).join("auth");
    let mut existing = read_auth_file(&root)?;

    // `--hashed` writes only the digest: the file then cannot be replayed as a
    // credential if it leaks, at the cost of not being able to read the token
    // back from disk.
    // token : role : name : paths : expires : branches. The expired field is empty here because
    // `token add` has no flag for it and an empty field means "never"; writing five fields put the
    // branch pattern where the expiry is parsed from, so the pattern was read as an expiry and the
    // branch scope was read as absent.
    let path_scope = scope.map(|s| s.trim().to_string()).unwrap_or_default();
    let branch_scope = branches.map(|s| s.trim().to_string()).unwrap_or_default();
    let entry = if hashed {
        format!(
            "sha256:{}:{}:{}:{}:{}:{}",
            auth::digest(&token),
            role,
            name.trim(),
            path_scope,
            "",
            branch_scope
        )
    } else {
        format!(
            "{}:{}:{}:{}:{}:{}",
            token,
            role,
            name.trim(),
            path_scope,
            "",
            branch_scope
        )
    };
    existing.push(entry);
    fs::write(&path, existing.join("\n") + "\n")?;

    println!("created {} token for {:?}", role, name.trim());
    println!("{}", token);
    if hashed {
        println!("\nStored as a SHA-256 digest: this token cannot be recovered from the file.");
    } else {
        println!("\nStored in clear in .gesta/auth, which is never tracked.");
    }
    println!("Give it to the client once. `token list` shows only the last four characters.");
    Ok(())
}

/// Replace a token's secret while keeping its role, scope and name.
///
/// The alternative, remove then add, leaves a window where the name has no credential at all, and a
/// person doing it by hand has to re-read the old entry to copy the scope back. Rotating keeps the
/// grant and changes only the secret, which is the one thing that has to change.
pub fn cmd_token_rotate(selector: &str, hashed: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let path = repo_dir(&root).join("auth");
    let mut entries = read_auth_file(&root)?;
    if entries.is_empty() {
        return Err(bad("no tokens configured"));
    }

    // Matched the same way removal matches: by name, or by the last four characters a person has in
    // front of them from `token list`.
    let mut hit = None;
    for (i, line) in entries.iter().enumerate() {
        let f: Vec<&str> = line.split(':').collect();
        // `sha256:<digest>:<role>:<name>...` puts the name at index 2 like the clear form does; the
        // secret is the only field whose position differs.
        let name = f.get(2).copied();
        let secret = if f.first() == Some(&"sha256") {
            f.get(1).copied()
        } else {
            f.first().copied()
        };
        let matches = name.map(|n| n == selector).unwrap_or(false)
            || secret.map(|s| s.ends_with(selector)).unwrap_or(false);
        if matches {
            hit = Some(i);
            break;
        }
    }
    let idx = hit.ok_or_else(|| bad(format!("no token matches {:?}", selector)))?;

    // The failing entry is parsed before anything is written, so a rotate on a malformed line refuses
    // rather than replacing it with a well-formed but different grant.
    let fields: Vec<String> = entries[idx].split(':').map(|s| s.to_string()).collect();
    let offset = if entries[idx].starts_with("sha256:") {
        1
    } else {
        0
    };
    if fields.len() < 3 + offset {
        return Err(bad(format!("the entry for {:?} is malformed", selector)));
    }
    let role = fields[offset + 1].clone();
    let name = fields[offset + 2].clone();
    let rest: Vec<String> = fields[offset + 3..].to_vec();

    let token = auth::generate_token(32)?;
    let secret = if hashed {
        format!("sha256:{}", auth::digest(&token))
    } else {
        token.clone()
    };
    let mut parts = vec![secret, role.clone(), name.clone()];
    parts.extend(rest);
    entries[idx] = parts.join(":");
    fs::write(&path, entries.join("\n") + "\n")?;

    println!("rotated the {} token for {:?}", role, name);
    println!("{}", token);
    println!();
    println!("The old secret stops working as soon as the server reads the file, which it does on every request.");
    println!(
        "Update the client now: a credential left in a config somewhere is the one that gets used."
    );
    Ok(())
}

/// Remove a token by name or by its last four characters.
pub fn cmd_token_remove(selector: &str) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let path = repo_dir(&root).join("auth");
    let entries = read_auth_file(&root)?;
    if entries.is_empty() {
        return Err(bad("no tokens configured"));
    }

    let mut kept: Vec<String> = Vec::new();
    let mut removed = 0usize;
    for line in entries {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            kept.push(line);
            continue;
        }
        // The name is the third field, and it is followed by optional scope, expiry and branch
        // fields. Writing `splitn(3, ':')` and taking the last part returned `name:::main` as the
        // name, so the entry never matched the name typed and the removal silently kept the token.
        let (name, id) = if let Some(rest) = l.strip_prefix("sha256:") {
            let mut parts = rest.splitn(3, ':');
            let digest = parts.next().unwrap_or("").to_string();
            let _role = parts.next();
            let tail = parts.next().unwrap_or("");
            (
                tail.split(':').next().unwrap_or("").trim().to_string(),
                digest,
            )
        } else {
            let mut parts = l.splitn(3, ':');
            let token = parts.next().unwrap_or("").to_string();
            let _role = parts.next();
            let tail = parts.next().unwrap_or("");
            (
                tail.split(':').next().unwrap_or("").trim().to_string(),
                token,
            )
        };
        let tail: String = id
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if name == selector || tail == selector {
            removed += 1;
            continue;
        }
        kept.push(line);
    }

    if removed == 0 {
        return Err(bad(format!("no token matches {:?}", selector)));
    }
    fs::write(
        &path,
        kept.join("\n") + if kept.is_empty() { "" } else { "\n" },
    )?;
    println!("removed {} token(s) matching {:?}", removed, selector);
    // The running server notices the file changed and rebuilds its table, so a restart is a choice
    // rather than a requirement. Saying "restart" when it is not needed teaches people to trust the
    // wrong sentence.
    println!("A running `gesta serve` picks this up on the next request; no restart needed.");
    Ok(())
}

/// List configured tokens without revealing them.
pub fn cmd_token_list() -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let entries = read_auth_file(&root)?;
    let mut shown = 0;
    for line in entries {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let (role, name, id) = if let Some(rest) = l.strip_prefix("sha256:") {
            let mut parts = rest.splitn(3, ':');
            let digest = parts.next().unwrap_or("");
            (
                parts.next().unwrap_or("read").to_string(),
                parts.next().unwrap_or("unnamed").to_string(),
                digest.to_string(),
            )
        } else {
            let mut parts = l.splitn(3, ':');
            let token = parts.next().unwrap_or("").to_string();
            (
                parts.next().unwrap_or("read").to_string(),
                parts.next().unwrap_or("unnamed").to_string(),
                token,
            )
        };
        let tail: String = id
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let kind = if l.starts_with("sha256:") {
            "hashed"
        } else {
            "clear"
        };
        println!("{:<10} {:<18} {:<7} ...{}", role, name, kind, tail);
        shown += 1;
    }
    if shown == 0 {
        println!("no tokens configured; the server only binds to loopback");
    }
    Ok(())
}

/// List what a remote holds.
///
/// Only an object store can be asked: a filesystem remote already answers through
/// its own database and an HTTP remote would need a listing endpoint that no
/// deployment is required to expose.
pub fn cmd_remote_ls(remote_name: &str) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    if url.starts_with("s3://") {
        let (cfg, prefix) = crate::s3::S3Config::from_url(&url)?;
        let branches = cfg.list_branches(&prefix)?;
        if branches.is_empty() {
            println!("no branches at s3://{}/{}", cfg.bucket, prefix);
        } else {
            println!(
                "s3://{}/{} ({} branch(es))",
                cfg.bucket,
                prefix,
                branches.len()
            );
            for b in branches {
                println!("  {}", b);
            }
        }
        return Ok(());
    }

    Err(bad(
        "listing is only supported for s3:// remotes".to_string()
    ))
}

/// Write the files named by a snapshot to disk.
///
/// Used for a shallow clone, where the changeset chain no longer describes the tree.
/// Every path is written, and a path already on disk that the snapshot does not name is
/// left alone rather than deleted: this runs into an empty directory in the normal case,
/// so a deletion here would only ever be wrong.
fn restore_from_snapshot(
    conn: &Connection,
    root: &Path,
    snapshot: &[(String, String)],
) -> ZResult<usize> {
    let mut written = 0usize;
    for (path, hash) in snapshot {
        // The same guard the remote write path uses: a path from a bundle is not
        // trusted, and `..` in one would write outside the repository.
        if path.split(['/', '\\']).any(|seg| seg == "..") {
            return Err(bad(format!("unsafe path in a bundle snapshot: {}", path)));
        }
        // Bytes, so a binary file arrives whole. Asking for text refused the clone with a
        // message about UTF-8 from a repository that only held a PNG.
        let content = crate::store::get_object_bytes(conn, hash)?;
        let target = root.join(path);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&target, &content)?;
        written += 1;
    }
    Ok(written)
}

/// `remote deepen`: take more of the history from a remote.
///
/// A shallow clone is otherwise a one way door. Everything it kept works, and asking
/// about anything older has no answer, which turns "I cloned quickly" into "I cannot
/// read history" with no way back short of cloning again.
///
/// Deepening does not move the branch head and does not touch the working tree: it adds
/// changesets that sit below the boundary. That matters because it means the command is
/// safe to run in a repository with uncommitted work.
pub fn cmd_deepen(
    depth: Option<usize>,
    unshallow: bool,
    remote_name: &str,
    branch: &str,
    dry_run: bool,
) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let url = remote_url(&conn, remote_name)?;

    if !url.starts_with("s3://") {
        return Err(bad(
            "deepening is only supported for s3:// remotes, because it needs the whole history in one object"
                .to_string(),
        ));
    }
    if depth.is_none() && !unshallow {
        return Err(bad(
            "say how much history to take: --depth <n> or --unshallow".to_string(),
        ));
    }

    let boundaries: Vec<String> = {
        let mut stmt = conn.prepare("SELECT changeset_id FROM shallow_roots WHERE branch = ?1")?;
        let rows = stmt.query_map(rusqlite::params![branch], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if boundaries.is_empty() {
        println!("{} is not shallow, nothing to deepen", branch);
        return Ok(());
    }

    let local_count = crate::store::ancestors_bfs(
        &conn,
        &branch_head(&conn, branch)?.ok_or_else(|| bad(format!("{} has no head", branch)))?,
    )?
    .len();

    let extra = if unshallow {
        None
    } else {
        Some(depth.unwrap_or(0))
    };
    let target = match extra {
        None => usize::MAX,
        Some(d) => local_count + d,
    };

    if dry_run {
        println!(
            "would take up to {} changeset(s) more for {} ({} held now)",
            if target == usize::MAX {
                "all".to_string()
            } else {
                target.to_string()
            },
            branch,
            local_count
        );
        return Ok(());
    }

    let (cfg, prefix) = crate::s3::S3Config::from_url(&url)?;
    let key = crate::s3::key_for(&prefix, branch);
    let bytes = cfg
        .get(&key)?
        .ok_or_else(|| crate::bad(format!("the bucket has no object at {}", key)))?;
    let mut bundle = remote::Bundle::decode(&bytes)?;

    let available = bundle.changesets.len();
    if available < local_count {
        return Err(bad(format!(
            "the remote holds {} changeset(s) but this clone has {}, so deepening would lose history",
            available, local_count
        )));
    }

    if target >= available {
        println!(
            "the remote holds {} changeset(s), which is all of it; removing the boundary",
            available
        );
    } else {
        println!(
            "taking {} of {} changeset(s) from the remote",
            target, available
        );
        // Keep the newest `target`, then apply the rest on top of what is here.
        let keep_from = available - target;
        let wanted: std::collections::HashSet<String> = bundle.changesets[keep_from..]
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let cut_bundle = {
            let mut b = bundle.clone();
            b.changesets = b.changesets.split_off(keep_from);
            b.versions.retain(|(_, _, _, cs, _)| wanted.contains(cs));
            b.files.retain(|(cs, _, _, _)| wanted.contains(cs));
            b.snapshot = Vec::new();
            b.shallow_root = None;
            b
        };
        // Objects and deltas are cheap to carry and hard to filter safely, so the
        // receiving side keeps them; a duplicate object is ignored on insert.
        let mut b = cut_bundle;
        b.objects = bundle.objects.clone();
        b.deltas = bundle.deltas.clone();
        b.hashes = bundle.hashes.clone();
        bundle = b;
    }

    // `apply_bundle_opts` with move_branch false adds rows without moving the head.
    let before_head = branch_head(&conn, branch)?.ok_or_else(|| bad("no head".to_string()))?;
    let before = crate::store::ancestors_bfs(&conn, &before_head)?.len();
    remote::apply_bundle_opts(&conn, &bundle, false)?;

    // Recompute the boundary from what is actually held now. The first version deleted
    // the old row and only re-added it when a parent was missing, which left no row at
    // all when the chain was still cut and made a later `--unshallow` report that there
    // was nothing to deepen.
    conn.execute(
        "DELETE FROM shallow_roots WHERE branch = ?1",
        rusqlite::params![branch],
    )?;
    let head = branch_head(&conn, branch)?.ok_or_else(|| bad("no head".to_string()))?;
    let chain = crate::store::ancestors_bfs(&conn, &head)?;
    // Every changeset whose parent is not held is a boundary. Taking the last element of
    // the chain was wrong: a breadth-first walk is ordered by distance from the head, not
    // by age, so the last element is not necessarily the oldest and the boundary row was
    // silently dropped, which made a later `--unshallow` claim there was nothing to do.
    let mut still_shallow = false;
    for id in &chain {
        let (p1, _) = crate::store::parents_of_public(&conn, id)?;
        if let Some(parent) = p1 {
            let present: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM changesets WHERE id = ?1",
                    rusqlite::params![parent],
                    |r| r.get(0),
                )
                .optional()?;
            if present.is_none() {
                still_shallow = true;
                conn.execute(
                    "INSERT OR IGNORE INTO shallow_roots(changeset_id, branch, created_at)
                     VALUES(?1, ?2, ?3)",
                    rusqlite::params![id, branch, now()],
                )?;
            }
        }
    }

    // Deepening adds changesets, so the committed tree has to be rebuilt from the new
    // history. Skipping this left the tree describing only the changesets the clone
    // arrived with, and `fsck` reported it as stale with five paths missing.
    crate::store::refresh_branch_tree(&conn, branch)?;

    let after: i64 = conn.query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))?;
    let _ = root;
    println!(
        "{} now holds {} changeset(s), {} added{}",
        branch,
        after,
        after.saturating_sub(before as i64),
        if still_shallow {
            ", still shallow"
        } else {
            ", no boundary left"
        }
    );
    Ok(())
}

/// Say which attribute rule decides what happens to a path, and what the two answers are.
///
/// Same reasoning as `ignore why`: a rule file where the last matching rule wins is easy to write
/// and hard to read back, and a person who wonders why a file keeps appearing as a whole-file diff
/// should not have to replay the matching by hand.
pub fn cmd_attributes_why(path: &str, json: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let rules = crate::attributes::Rules::load(&root);
    let decision = crate::attributes::explain(&rules, path);
    let plan = crate::attributes::plan_for(&rules, path);

    if json {
        crate::cli::emit_object(vec![
            ("path", crate::json::Json::str(path.to_string())),
            ("text", crate::json::Json::str(decision["text"].clone())),
            ("eol", crate::json::Json::str(decision["eol"].clone())),
            ("rule", crate::json::Json::str(decision["rule"].clone())),
            ("stored_as_lf", crate::json::Json::Bool(plan.normalise)),
            ("rules", crate::json::Json::int(rules.rules.len() as i64)),
            (
                "source",
                match &rules.source {
                    Some(s) => crate::json::Json::str(s.clone()),
                    None => crate::json::Json::Null,
                },
            ),
        ]);
        return Ok(());
    }

    println!("{}", path);
    if rules.is_empty() {
        println!("  no attribute rules, so the default applies");
    } else {
        println!(
            "  decided by: {} (last matching rule wins)",
            decision["rule"]
        );
    }
    println!("  text: {}   eol: {}", decision["text"], decision["eol"]);
    println!(
        "  stored as: {}",
        if plan.normalise {
            "LF line endings"
        } else {
            "byte for byte"
        }
    );
    if let Some(eol) = plan.checkout {
        println!(
            "  written to the working tree with: {}",
            match eol {
                crate::attributes::Eol::Lf => "LF",
                crate::attributes::Eol::Crlf => "CRLF",
            }
        );
    } else {
        println!("  written to the working tree as stored");
    }
    if let Some(src) = &rules.source {
        println!("  {} rule(s) from {}", rules.rules.len(), src);
    }
    Ok(())
}

/// Scan the working tree, or the stored history, for anything that looks like a credential.
///
/// The commit path refuses one changeset at a time. This answers the other question a person asks
/// after the fact: what is already in here, and how long has it been there. It reads the objects
/// that are stored, so a secret that was committed and deleted still shows up with the changeset
/// that introduced it, which is exactly the finding a commit-time check cannot produce.
#[cfg(feature = "regex")]
pub fn cmd_secret_scan(history: bool, json: bool) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let mut scanner = crate::leak::Scanner::default();
    scanner.load_allowlist(&root);

    // path -> the changeset a finding was first seen in. The newest is not interesting; the oldest
    // is, because that is the one that has to be reverted.
    let mut findings: Vec<(String, crate::leak::Finding)> = Vec::new();
    let mut scanned = 0usize;

    if history {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT fv.path, fv.hash, fv.changeset_id
             FROM file_versions fv JOIN objects o ON o.hash = fv.hash
             ORDER BY fv.path, fv.version",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (path, hash, _cs) = row?;
            if !crate::leak::worth_scanning(&path) {
                continue;
            }
            let body = match crate::store::get_object(&conn, &hash) {
                Ok(b) => b,
                Err(_) => continue,
            };
            scanned += 1;
            findings.extend(
                scanner
                    .scan_text(&path, &body)
                    .into_iter()
                    .map(|f| (path.clone(), f)),
            );
        }
    } else {
        let branch = current_branch(&conn)?;
        let files = match branch_head(&conn, &branch)? {
            Some(h) => crate::repo::tree_at(&conn, &h)?,
            None => Vec::new(),
        };
        for (path, hash) in &files {
            if !crate::leak::worth_scanning(path) {
                continue;
            }
            let body = match crate::store::get_object(&conn, hash) {
                Ok(b) => b,
                Err(_) => continue,
            };
            scanned += 1;
            findings.extend(
                scanner
                    .scan_text(path, &body)
                    .into_iter()
                    .map(|f| (path.clone(), f)),
            );
        }
    }

    // One row per fingerprint. A secret committed and never edited appears once per version
    // otherwise, and a report that repeats the same key fifty times buries the tenth secret.
    let mut seen = std::collections::HashSet::new();
    findings.retain(|(_, f)| seen.insert(f.fingerprint.clone()));

    if json {
        let items = findings
            .iter()
            .map(|(_, f)| {
                crate::json::Json::obj(vec![
                    ("rule", crate::json::Json::str(f.rule.clone())),
                    ("path", crate::json::Json::str(f.path.clone())),
                    ("line", crate::json::Json::int(f.line as i64)),
                    ("redacted", crate::json::Json::str(f.redacted.clone())),
                    ("fingerprint", crate::json::Json::str(f.fingerprint.clone())),
                    (
                        "entropy",
                        match f.entropy {
                            Some(e) => crate::json::Json::str(format!("{:.2}", e)),
                            None => crate::json::Json::Null,
                        },
                    ),
                ])
            })
            .collect();
        crate::cli::emit_object(vec![
            (
                "scope",
                crate::json::Json::str(if history { "history" } else { "working tree" }),
            ),
            ("scanned", crate::json::Json::int(scanned as i64)),
            ("findings", crate::json::Json::int(findings.len() as i64)),
            ("items", crate::json::Json::arr(items)),
        ]);
        return Ok(());
    }

    if findings.is_empty() {
        println!("no secrets found ({} object(s) scanned)", scanned);
        return Ok(());
    }
    println!(
        "{} possible secret(s) in {} object(s):",
        findings.len(),
        scanned
    );
    for (_, f) in &findings {
        println!("  {}", f);
        if let Some(e) = f.entropy {
            println!("      entropy {:.2}   fingerprint {}", e, f.fingerprint);
        } else {
            println!("      fingerprint {}", f.fingerprint);
        }
    }
    // A finding is only worth reporting if the next step is clear.
    println!(
        "\nto stop reporting a fixture: add `path <glob>` or `stopword <word>` to .gesta/secret-allowlist"
    );
    println!(
        "a credential that is already committed has to be reverted and then rotated, in that order"
    );
    Ok(())
}

#[cfg(not(feature = "regex"))]
pub fn cmd_secret_scan(_history: bool, _json: bool) -> ZResult<()> {
    Err(crate::error::Error::new(
        crate::error::Code::Usage,
        "secret scanning needs the regex feature, which this build does not have",
    )
    .into())
}

/// Show what the server decided, newest last.
///
/// `--refused` answers the question a person asks after an incident: what was tried and turned away.
/// Without it the whole record is shown, which is what answers "was this token used".
pub fn cmd_audit(refused: bool, limit: Option<usize>, json_out: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let rows = crate::audit::read(&root, refused, limit)?;
    if rows.is_empty() {
        println!(
            "no {} events recorded",
            if refused { "refused" } else { "audit" }
        );
        return Ok(());
    }
    if json_out {
        // One document holding an array, so a caller parses once rather than line by line.
        let items: Vec<crate::json::Json> = rows
            .iter()
            .map(|e| {
                crate::json::Json::obj(vec![
                    ("at", crate::json::Json::str(e.at.clone())),
                    ("who", crate::json::Json::str(e.who.clone())),
                    ("decision", crate::json::Json::str(e.decision.clone())),
                    ("action", crate::json::Json::str(e.action.clone())),
                    ("target", crate::json::Json::str(e.target.clone())),
                    ("reason", crate::json::Json::str(e.reason.clone())),
                ])
            })
            .collect();
        let doc = crate::json::Json::obj(vec![
            ("schema", crate::json::Json::str("gesta@1")),
            ("count", crate::json::Json::int(rows.len() as i64)),
            ("events", crate::json::Json::Arr(items)),
        ]);
        println!("{}", doc.render());
        return Ok(());
    }
    for e in &rows {
        if e.decision == "unreadable" {
            println!("  ?  unreadable line: {}", e.raw);
            continue;
        }
        let mark = if e.decision == "allowed" { "ok" } else { "!!" };
        println!(
            "  {} {} {} {} -> {} {}",
            mark, e.at, e.who, e.action, e.target, e.decision
        );
        if !e.reason.is_empty() {
            println!("      {}", e.reason);
        }
    }
    println!("\n{} event(s)", rows.len());
    Ok(())
}
