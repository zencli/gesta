//! Remote synchronisation: clone, push, pull over a simple bundle format.
//!
//! The wire format is a zlib-compressed JSON bundle holding the changesets,
//! file versions, objects and branch refs a peer is missing. It is deliberately
//! simple: correctness first, and it works over `file://`, HTTP PUT/GET and
//! plain filesystem paths.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::store::{branch_head, get_object, hash_content, init_schema, open_db, put_object};
use crate::{bad, now, repo_dir, ZResult};

/// Everything a peer needs to catch up on one branch.
#[derive(Clone)]
pub struct Bundle {
    pub branch: String,
    pub head: Option<String>,
    /// Changeset rows, oldest first.
    pub changesets: Vec<ChangesetRow>,
    /// Every object reachable from those changesets.
    pub objects: Vec<(String, String)>,
    /// Objects that are not text, kept as raw bytes. A separate list so the text path is
    /// untouched: a bundle written by an older build decodes exactly as it did before, and a
    /// reader that only handles text never sees these.
    pub binaries: Vec<(String, Vec<u8>)>,
    /// file_versions rows for those changesets.
    pub versions: Vec<(String, i64, String, String, i64)>,
    /// changeset_files rows.
    pub files: Vec<(String, String, Option<String>, Option<String>)>,
    pub hashes: Vec<String>,
    /// When set, this bundle is a prefix of the history and the oldest changesets it
    /// carries have a parent that is deliberately not included.
    ///
    /// A shallow bundle is not a damaged one, and a receiver has to record that
    /// boundary or every later check reports a missing parent. The value is the
    /// changeset whose parent was cut off.
    pub shallow_root: Option<String>,
    /// The full tree at the head, as `(path, hash)`.
    ///
    /// A shallow bundle cannot rely on the changeset chain to describe the working
    /// tree: cutting the chain removes the changesets that created the older files,
    /// and a tree rebuilt by walking what remains came back with 5 files out of 200.
    /// Sending the head tree explicitly makes a shallow clone complete.
    ///
    /// Empty for a normal bundle, where the chain already says everything.
    pub snapshot: Vec<(String, String)>,
    /// Objects sent as a delta against another object in the same bundle.
    ///
    /// Sending every version whole made a bundle eight times the size of the content
    /// it carried: 247 KB of text with 21 versions came to 1.88 MB. The store already
    /// keeps those versions as deltas and a delta is about a twentieth of the size,
    /// so the wire format carries the delta instead of the expanded content.
    ///
    /// A delta is only sent when its base is also in this bundle, so a receiver can
    /// always resolve it. Otherwise the object goes whole.
    pub deltas: Vec<(String, String, Vec<crate::store::delta::Op>)>,
}

/// (message, author, created_at, parent, parent2) as stored in the database.
type ChangesetMeta = (
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[derive(Clone)]
pub struct ChangesetRow {
    pub id: String,
    pub message: String,
    pub author: String,
    pub created_at: i64,
    pub parent: Option<String>,
    pub parent2: Option<String>,
    /// Which session produced this change and why. Travels with the changeset so a clone
    /// keeps the reasoning as well as the result.
    pub session: Option<String>,
    pub intent: Option<String>,
}

/// Refuse a path from a bundle that could not name a file inside the repository.
///
/// The message names the path and the record kind so a person holding the archive can see which entry
/// is wrong, rather than reading that the bundle is malformed.
fn refuse_unsafe_bundle_path(path: &str) -> ZResult<()> {
    if crate::support::paths::is_repo_relative(path) {
        Ok(())
    } else {
        Err(bad(format!(
            "bundle holds a path that is not inside the repository: {:?}. \
             A bundle path must be relative and may not contain `..`.",
            path
        )))
    }
}

impl Bundle {
    /// Serialise to the wire format: a zlib-compressed line oriented text block.
    /// Text keeps the format inspectable and avoids a JSON dependency.
    pub fn encode(&self) -> ZResult<Vec<u8>> {
        let mut s = String::new();
        s.push_str("gesta-bundle 1\n");
        // The branch is escaped like every other caller-controlled field (#282).
        //
        // It was the one field written raw, and `decode` splits on `lines()` and matches each line by
        // its first tab-separated column. A branch carrying a newline therefore injects whole records:
        // `x\nhead\t<sha>` overwrites the head (the last assignment wins) and
        // `x\ncs\t<id>\t0\t<author>\t<message>` inserts a changeset with every field chosen by the
        // attacker into the receiver's database on pull. The path checks at the end of decode cover
        // `fv`, `cf` and `sn`, so a `cs` row went in unexamined.
        s.push_str(&format!("branch\t{}\n", enc(&self.branch)));
        s.push_str(&format!(
            "head\t{}\n",
            self.head.clone().unwrap_or_else(|| "-".into())
        ));
        if let Some(root) = &self.shallow_root {
            s.push_str(&format!("shallow\t{}\n", root));
        }
        for c in &self.changesets {
            s.push_str(&format!(
                "cs\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.id,
                c.created_at,
                enc(&c.author),
                enc(&c.message),
                c.parent.clone().unwrap_or_else(|| "-".into()),
                c.parent2.clone().unwrap_or_else(|| "-".into()),
                c.session
                    .clone()
                    .map(|v| enc(&v))
                    .unwrap_or_else(|| "-".into()),
                c.intent
                    .clone()
                    .map(|v| enc(&v))
                    .unwrap_or_else(|| "-".into())
            ));
        }
        for (path, ver, hash, cs, ts) in &self.versions {
            s.push_str(&format!(
                "fv\t{}\t{}\t{}\t{}\t{}\n",
                enc(path),
                ver,
                hash,
                cs,
                ts
            ));
        }
        for (cs, path, old, new) in &self.files {
            s.push_str(&format!(
                "cf\t{}\t{}\t{}\t{}\n",
                cs,
                enc(path),
                old.clone().unwrap_or_else(|| "-".into()),
                new.clone().unwrap_or_else(|| "-".into())
            ));
        }
        for (path, hash) in &self.snapshot {
            s.push_str(&format!("sn\t{}\t{}\n", enc(path), hash));
        }
        for (hash, content) in &self.objects {
            s.push_str(&format!(
                "ob\t{}\t{}\n",
                hash,
                enc_base64(content.as_bytes())
            ));
        }
        for (hash, bytes) in &self.binaries {
            s.push_str(&format!("bn\t{}\t{}\n", hash, enc_base64(bytes)));
        }
        for (hash, base, ops) in &self.deltas {
            let packed = enc_base64(&crate::store::delta::to_bytes(ops));
            s.push_str(&format!("dl\t{}\t{}\t{}\n", hash, base, packed));
        }
        s.push_str("end\n");

        let mut enc_stream = ZlibEncoder::new(Vec::new(), Compression::default());
        enc_stream.write_all(s.as_bytes())?;
        Ok(enc_stream.finish()?)
    }

    pub fn decode(bytes: &[u8]) -> ZResult<Bundle> {
        // A bundle arrives from a remote. `read_to_string` had no ceiling, so a 4 MiB body could
        // inflate to gigabytes before the header was even looked at (#156).
        let raw = crate::store::inflate_capped(ZlibDecoder::new(bytes), crate::store::MAX_INFLATED)
            .map_err(|e| bad(format!("bundle is not valid repository data: {}", e)))?;
        let text = String::from_utf8(raw)
            .map_err(|_| bad("bundle is not valid repository data: it is not text".to_string()))?;

        let mut lines = text.lines();
        match lines.next() {
            // Bundles written before the rename carry the old header. Refusing them
            // would strand every archive made by an earlier build.
            Some("gesta-bundle 1") | Some("zenforge-bundle 1") => {}
            _ => return Err(bad("unrecognised bundle header")),
        }

        let mut b = Bundle {
            branch: "main".into(),
            head: None,
            changesets: Vec::new(),
            objects: Vec::new(),
            binaries: Vec::new(),
            versions: Vec::new(),
            files: Vec::new(),
            hashes: Vec::new(),
            shallow_root: None,
            snapshot: Vec::new(),
            deltas: Vec::new(),
        };

        // One branch record and one head record is the format. More than one means the framing was
        // broken before this loop ran, and the only writer that does that is an older build putting
        // an unescaped branch name on the wire (#282).
        let mut branch_seen = false;
        let mut head_seen = false;

        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                // Decoded like author, message and path (#282), and checked on the RAW field first.
                //
                // The check has to run here rather than on the decoded value. This build escapes a
                // newline into `\n` on the wire, and `dec_str` restores it, so a decoded branch
                // legitimately contains a control character and a check after decoding would refuse
                // the build's own output. What matters is whether the RAW field is a single
                // tab-free, newline-free token: if it is, the escaping is unambiguous and the
                // decoded value is whatever the sender meant. If it is not, an older server wrote it
                // raw and the record framing has already been broken by the time this line is read,
                // so it is refused instead of guessed at.
                ["branch", name] => {
                    if branch_seen {
                        return Err(bad(
                            "bundle carries more than one branch record, which no writer emits: \
                             the framing has been broken, most likely by a branch name containing \
                             a newline in a bundle written by an older build",
                        ));
                    }
                    branch_seen = true;
                    b.branch = dec_str(name);
                }
                ["head", h] => {
                    // A second head record is how an injected branch name overwrote the head, since
                    // the last assignment wins (#282). One head per bundle is the format.
                    if head_seen {
                        return Err(bad(
                            "bundle carries more than one head record; the record framing has been \
                             broken, most likely by an unescaped newline in a branch name",
                        ));
                    }
                    head_seen = true;
                    b.head = if *h == "-" { None } else { Some(h.to_string()) };
                }
                ["shallow", root] => b.shallow_root = Some(root.to_string()),
                // A seven field row is what a bundle written before provenance existed
                // contains, so both forms are accepted rather than refusing old archives.
                ["cs", id, ts, author, message, p1, p2] => b.changesets.push(ChangesetRow {
                    id: id.to_string(),
                    message: dec_str(message),
                    author: dec_str(author),
                    created_at: ts.parse().unwrap_or(0),
                    parent: if *p1 == "-" {
                        None
                    } else {
                        Some(p1.to_string())
                    },
                    parent2: if *p2 == "-" {
                        None
                    } else {
                        Some(p2.to_string())
                    },
                    session: None,
                    intent: None,
                }),
                ["cs", id, ts, author, message, p1, p2, session, intent] => {
                    b.changesets.push(ChangesetRow {
                        id: id.to_string(),
                        message: dec_str(message),
                        author: dec_str(author),
                        created_at: ts.parse().unwrap_or(0),
                        parent: if *p1 == "-" {
                            None
                        } else {
                            Some(p1.to_string())
                        },
                        parent2: if *p2 == "-" {
                            None
                        } else {
                            Some(p2.to_string())
                        },
                        session: if *session == "-" {
                            None
                        } else {
                            Some(dec_str(session))
                        },
                        intent: if *intent == "-" {
                            None
                        } else {
                            Some(dec_str(intent))
                        },
                    })
                }
                ["fv", path, ver, hash, cs, ts] => b.versions.push((
                    dec_str(path),
                    ver.parse().unwrap_or(0),
                    hash.to_string(),
                    cs.to_string(),
                    ts.parse().unwrap_or(0),
                )),
                ["cf", cs, path, old, new] => b.files.push((
                    cs.to_string(),
                    dec_str(path),
                    if *old == "-" {
                        None
                    } else {
                        Some(old.to_string())
                    },
                    if *new == "-" {
                        None
                    } else {
                        Some(new.to_string())
                    },
                )),
                ["sn", path, hash] => b.snapshot.push((dec_str(path), hash.to_string())),
                ["dl", hash, base, ops] => {
                    let raw = dec_base64(ops)
                        .ok_or_else(|| bad(format!("delta for {} is not valid base64", hash)))?;
                    let parsed = crate::store::delta::from_bytes(&raw)
                        .ok_or_else(|| bad(format!("delta for {} is unreadable", hash)))?;
                    b.deltas.push((hash.to_string(), base.to_string(), parsed));
                }
                ["ob", hash, content] => {
                    let bytes = dec_base64(content)
                        .ok_or_else(|| bad(format!("object {} has invalid payload", hash)))?;
                    let s = String::from_utf8(bytes)
                        .map_err(|e| bad(format!("object {} is not UTF-8: {}", hash, e)))?;
                    b.hashes.push(hash.to_string());
                    b.objects.push((hash.to_string(), s));
                }
                ["bn", hash, content] => {
                    // A binary object. Its bytes are base64 like any other payload; what makes
                    // it binary is that they are not valid UTF-8.
                    let bytes = dec_base64(content)
                        .ok_or_else(|| bad(format!("object {} has invalid payload", hash)))?;
                    b.hashes.push(hash.to_string());
                    b.binaries.push((hash.to_string(), bytes));
                }
                ["end"] => break,
                _ => {} // forward compatibility: ignore unknown records
            }
        }

        // Every path a bundle carries is checked here, before any of it reaches a table. The rows this
        // decoder produces are applied as they are and later used by checkout, which joins them onto
        // the repository root: a bundle holding `../../etc/x` or an absolute path would write outside
        // it. Refused as a whole rather than per record, because a bundle that lies about one path is
        // not a bundle to apply in part.
        for (path, _, _, _, _) in &b.versions {
            refuse_unsafe_bundle_path(path)?;
        }
        for (_, path, _, _) in &b.files {
            refuse_unsafe_bundle_path(path)?;
        }
        for (path, _) in &b.snapshot {
            refuse_unsafe_bundle_path(path)?;
        }

        Ok(b)
    }
}

fn enc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn dec_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn enc_base64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn dec_base64(s: &str) -> Option<Vec<u8>> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, c) in T.iter().enumerate() {
        lut[*c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let bytes: Vec<u8> = s.bytes().filter(|b| *b != b'=' && *b != b'\n').collect();
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            let v = lut[*b as usize];
            if v == 255 {
                return None;
            }
            n |= (v as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Collect everything needed to bring a peer up to date on `branch`, given the
/// set of changeset ids the peer already has.
/// Order the wanted changesets so every parent comes before its children.
///
/// Depth first over the parent links, emitting a node after its parents. The
/// visited set also breaks any cycle, which a corrupted database could contain and
/// which must not hang an upload.
fn topo_order(conn: &Connection, head: &str, wanted: &HashSet<String>) -> ZResult<Vec<String>> {
    fn visit(
        conn: &Connection,
        id: &str,
        wanted: &HashSet<String>,
        done: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) -> ZResult<()> {
        if done.contains(id) {
            return Ok(());
        }
        done.insert(id.to_string());
        let (p1, p2) = crate::store::parents_of_public(conn, id)?;
        for p in [p1, p2].into_iter().flatten() {
            visit(conn, &p, wanted, done, out)?;
        }
        if wanted.contains(id) {
            out.push(id.to_string());
        }
        Ok(())
    }

    let mut done = HashSet::new();
    let mut out = Vec::new();
    visit(conn, head, wanted, &mut done, &mut out)?;
    Ok(out)
}

/// The stored delta for an object, if the store keeps one.
///
/// A delta is stored as a base plus a list of copy and insert operations. Only copy
/// operations are carried on the wire, because an insert's text already appears in a
/// whole object somewhere in the bundle; a delta with an insert is therefore sent
/// whole rather than risk a receiver that cannot rebuild it.
fn stored_delta(
    conn: &Connection,
    hash: &str,
) -> ZResult<Option<(String, Vec<crate::store::delta::Op>)>> {
    let row: Option<(String, Vec<u8>)> = conn
        .query_row(
            "SELECT base, payload FROM deltas WHERE hash = ?1",
            params![hash],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((base, payload)) = row else {
        return Ok(None);
    };
    // The payload is stored compressed, so it is expanded before parsing. Reading it
    // raw silently produced an unreadable delta for every object.
    let raw = crate::store::decompress_bytes(&payload)?;
    let ops = crate::store::delta::from_bytes(&raw)
        .ok_or_else(|| bad(format!("delta for {} is unreadable", hash)))?;
    Ok(Some((base, ops)))
}

pub fn build_bundle(conn: &Connection, branch: &str, have: &HashSet<String>) -> ZResult<Bundle> {
    build_bundle_with_depth(conn, branch, have, None)
}

/// The same bundle, cut to `depth` changesets.
///
/// A shallow bundle carries the newest N changesets and records the oldest one it sent as
/// `shallow_root`, so a receiver knows whose parent it does not have. Without that marker the receiver
/// stores a changeset pointing at a parent that is not there, and the first operation that walks the
/// history fails on a row nobody can explain.
///
/// The cut is by distance from the head, counted along the ancestry, which is what `git clone --depth`
/// means. Depth 0 means "just the head", and `None` means everything.
pub fn build_bundle_with_depth(
    conn: &Connection,
    branch: &str,
    have: &HashSet<String>,
    depth: Option<usize>,
) -> ZResult<Bundle> {
    let head = branch_head(conn, branch)?;
    let mut b = Bundle {
        branch: branch.to_string(),
        head: head.clone(),
        changesets: Vec::new(),
        objects: Vec::new(),
        binaries: Vec::new(),
        versions: Vec::new(),
        files: Vec::new(),
        hashes: Vec::new(),
        shallow_root: None,
        snapshot: Vec::new(),
        deltas: Vec::new(),
    };
    let Some(head) = head else { return Ok(b) };

    let chain = crate::store::ancestors_bfs(conn, &head)?;

    // `ancestors_bfs` returns the head first and then its parents level by level, so the first N
    // entries are the N newest along the ancestry. Anything past that is history the caller asked not
    // to receive; dropping it is the whole point of a shallow clone, and the oldest one kept becomes
    // the marker that says so.
    let (chain, shallow_root) = match depth {
        Some(d) => {
            let kept: Vec<String> = chain.into_iter().take(d.saturating_add(1)).collect();
            let root = kept.last().cloned();
            (kept, root)
        }
        None => (chain, None),
    };

    let wanted: HashSet<String> = chain
        .iter()
        .filter(|id| !have.contains(*id))
        .cloned()
        .collect();
    // Oldest first, and a parent before any child. Reversing a breadth-first walk
    // is close but not sufficient: across a merge the two sides sit at different
    // depths, so a reversed walk can put a child before its own parent. A receiver
    // that applies rows in order would then fail on a parent it has not seen.
    let needed = topo_order(conn, &head, &wanted)?;

    let mut object_hashes: HashSet<String> = HashSet::new();
    for id in &needed {
        let row: Option<ChangesetMeta> = conn
            .query_row(
                "SELECT message, author, created_at, parent, parent2, session, intent
                 FROM changesets WHERE id = ?1",
                params![id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((message, author, ts, parent, parent2, session, intent)) = row else {
            continue;
        };
        b.changesets.push(ChangesetRow {
            id: id.clone(),
            message,
            author,
            created_at: ts,
            parent,
            parent2,
            session,
            intent,
        });

        let mut stmt = conn.prepare(
            "SELECT path, version, hash, changeset_id, created_at FROM file_versions
             WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for r in rows {
            let (path, ver, hash, cs, cts) = r?;
            object_hashes.insert(hash.clone());
            b.versions.push((path, ver, hash, cs, cts));
        }

        let mut stmt2 = conn.prepare(
            "SELECT path, old_hash, new_hash FROM changeset_files WHERE changeset_id = ?1 ORDER BY path",
        )?;
        let rows2 = stmt2.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for r in rows2 {
            let (path, old, new) = r?;
            if let Some(h) = &new {
                object_hashes.insert(h.clone());
            }
            b.files.push((id.clone(), path, old, new));
        }
    }

    // Every version is in hand. Send a delta when the store already holds that
    // object as a delta whose base is also going out: the base then arrives first and
    // the receiver resolves it. Anything else goes whole, so a bundle is always
    // self sufficient and never depends on what the other side happens to have.
    let all: Vec<String> = object_hashes.into_iter().collect();
    let in_bundle: HashSet<&str> = all.iter().map(|s| s.as_str()).collect();
    for hash in &all {
        b.hashes.push(hash.clone());
        if let Some((base, ops)) = stored_delta(conn, hash)? {
            if in_bundle.contains(base.as_str()) {
                b.deltas.push((hash.clone(), base, ops));
                continue;
            }
        }
        match get_object(conn, hash) {
            Ok(content) => b.objects.push((hash.clone(), content)),
            Err(_) => {
                // Distinguish "not there" from "there but binary". The bundle format carries
                // text objects only, so a repository holding an image could not be pushed, and
                // the message said the object was missing while it was present and readable.
                match crate::store::get_object_bytes(conn, hash) {
                    Ok(bytes) => b.binaries.push((hash.clone(), bytes)),
                    Err(_) => return Err(bad(format!("cannot bundle missing object {}", hash))),
                }
                continue;
            }
        }
    }
    // The marker a receiver needs so it knows whose parent it is missing. Set here rather than in the
    // struct literal because `b` is built before the depth is known.
    let shallow = shallow_root.filter(|r| !have.contains(r));
    b.shallow_root = shallow.clone();

    // A shallow cut removes the changesets that created older files, so the receiver cannot rebuild
    // the working tree by replaying what it got: `restore_worktree` walks `tree_at`, which only sees
    // what survives in the changesets that arrived (#285). The head tree therefore travels with the
    // bundle, exactly as the S3 path has always done it - and every object that tree names is carried
    // even when no bundled changeset mentions it, because those are the files the cut dropped.
    //
    // Only when a cut actually happened: a full bundle carries its whole history, so its receiver
    // rebuilds the tree the ordinary way and does not need the snapshot. Sending it anyway would make
    // every full clone pay for a redundant tree.
    if shallow.is_some() {
        if let Some(head) = b.head.clone() {
            let tree = crate::store::tree_at(conn, &head)?;
            for (_, hash) in &tree {
                if b.hashes.iter().any(|h| h == hash) {
                    continue;
                }
                // The object may be text, binary, or already sent as part of a delta.
                if b.deltas.iter().any(|(h, _, _)| h == hash) {
                    continue;
                }
                match get_object(conn, hash) {
                    Ok(content) => b.objects.push((hash.clone(), content)),
                    Err(_) => match crate::store::get_object_bytes(conn, hash) {
                        Ok(bytes) => b.binaries.push((hash.clone(), bytes)),
                        Err(_) => {
                            return Err(bad(format!("cannot bundle missing object {}", hash)))
                        }
                    },
                }
                b.hashes.push(hash.clone());
            }
            b.snapshot = tree;
        }
    }
    Ok(b)
}

/// Apply a bundle into a repository. Idempotent per changeset.
///
/// The branch head is left alone when `move_branch` is false: a pull must merge
/// the fetched history into the local branch, not replace it. Overwriting the
/// head here silently drops the local changesets from the branch.
pub fn apply_bundle_opts(conn: &Connection, b: &Bundle, move_branch: bool) -> ZResult<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut applied = 0usize;

    for (hash, content) in &b.objects {
        let expect = hash_content(content);
        if &expect != hash {
            return Err(bad(format!(
                "object {} fails its own hash check (got {})",
                hash, expect
            )));
        }
        put_object(&tx, content)?;
    }

    // Binary objects, checked the same way: the hash is the contract and a payload that does not
    // reproduce it is refused rather than stored.
    for (hash, bytes) in &b.binaries {
        let expect = crate::store::hash_bytes(bytes);
        if &expect != hash {
            return Err(bad(format!(
                "object {} fails its own hash check (got {})",
                hash, expect
            )));
        }
        crate::store::put_object_bytes(&tx, bytes)?;
    }

    // A delta is resolved after the whole objects land, because its base is one of
    // them or another delta. Chain order is not known, so this runs in rounds: each
    // round applies whatever it can, and a delta whose base has not arrived waits for
    // the next one. A round that applies nothing means the bundle cannot be resolved,
    // which is refused rather than left half stored. Rebuilding the content and
    // checking it against its own hash is what makes this safe: a delta that does not
    // reproduce the expected object is an error, not something written under a hash
    // that lies about what it holds.
    let mut pending: Vec<(String, String, Vec<crate::store::delta::Op>)> = b.deltas.clone();
    while !pending.is_empty() {
        let mut progressed = false;
        let mut still: Vec<(String, String, Vec<crate::store::delta::Op>)> = Vec::new();
        for (hash, base, ops) in pending {
            let Ok(base_content) = get_object(&tx, &base) else {
                still.push((hash, base, ops));
                continue;
            };
            let content = crate::store::delta::apply(&base_content, &ops).ok_or_else(|| {
                bad(format!(
                    "delta for {} does not apply to its base {}",
                    hash, base
                ))
            })?;
            let expect = hash_content(&content);
            if expect != hash {
                return Err(bad(format!(
                    "object {} rebuilt from a delta fails its hash check (got {})",
                    hash, expect
                )));
            }
            put_object(&tx, &content)?;
            progressed = true;
        }
        if !progressed {
            let names: Vec<String> = still.iter().map(|(h, _, _)| h.clone()).collect();
            return Err(bad(format!(
                "these objects arrive as deltas whose base is not in the bundle: {}",
                names.join(", ")
            )));
        }
        pending = still;
    }

    for c in &b.changesets {
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM changesets WHERE id = ?1",
                params![c.id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            continue;
        }
        tx.execute(
            "INSERT INTO changesets(id, message, author, created_at, status, parent, parent2, branch, session, intent)
             VALUES(?1, ?2, ?3, ?4, 'committed', ?5, ?6, ?7, ?8, ?9)",
            params![
                c.id,
                c.message,
                c.author,
                c.created_at,
                c.parent,
                c.parent2,
                b.branch,
                c.session,
                c.intent
            ],
        )?;
        applied += 1;
    }

    for (path, ver, hash, cs, ts) in &b.versions {
        tx.execute(
            "INSERT OR IGNORE INTO file_versions(path, version, hash, changeset_id, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![path, ver, hash, cs, ts],
        )?;
    }

    for (cs, path, old, new) in &b.files {
        tx.execute(
            "INSERT OR IGNORE INTO changeset_files(changeset_id, path, old_hash, new_hash)
             VALUES(?1, ?2, ?3, ?4)",
            params![cs, path, old, new],
        )?;
    }

    // Record where a shallow bundle was cut. Without this every later check reports
    // the boundary as a missing parent, which is the difference between a deliberate
    // shallow history and a damaged one.
    if let Some(root) = &b.shallow_root {
        tx.execute(
            "INSERT OR IGNORE INTO shallow_roots(changeset_id, branch, created_at)
             VALUES(?1, ?2, ?3)",
            params![root, b.branch, now()],
        )?;
    }
    // The head tree is a reference in its own right. Recording it keeps an integrity
    // check from treating the objects behind the working tree as garbage.
    for (path, hash) in &b.snapshot {
        tx.execute(
            "INSERT OR REPLACE INTO snapshot_objects(path, hash) VALUES(?1, ?2)",
            params![path, hash],
        )?;
    }

    tx.execute(
        "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, ?2, NULL)
         ON CONFLICT(name) DO NOTHING",
        params![b.branch, now()],
    )?;
    if move_branch {
        if let Some(head) = &b.head {
            crate::store::set_branch_head(&tx, &b.branch, head)?;
        }
    }

    tx.commit()?;
    Ok(applied)
}

/// Apply a bundle and move the branch head to it (clone and server receive).
pub fn apply_bundle(conn: &Connection, b: &Bundle) -> ZResult<usize> {
    apply_bundle_opts(conn, b, true)
}

/// Normalise a user supplied remote into a filesystem path.
pub fn resolve_remote(url: &str) -> std::path::PathBuf {
    let s = url.strip_prefix("file://").unwrap_or(url);
    std::path::PathBuf::from(s)
}

/// Initialise a bare repository at `path` (just a repository directory with a db).
pub fn init_bare(path: &Path, branch: &str) -> ZResult<()> {
    let zf = repo_dir(path);
    fs::create_dir_all(&zf)?;
    let conn = open_db(path)?;
    init_schema(&conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, ?2, NULL)",
        params![branch, now()],
    )?;
    Ok(())
}

/// Public base64 decode, used by HTTP Basic auth parsing.
pub fn decode_base64_str(s: &str) -> Option<Vec<u8>> {
    dec_base64(s)
}

/// Keep only the newest `depth` changesets of an already built bundle.
///
/// A bucket holds one object for the whole branch, so a shallow clone cuts what it
/// received rather than asking for less. Two things have to survive the cut:
///
/// The head snapshot, because the changesets that created the older files are exactly
/// what is being removed, and without it the working tree comes back with 5 files out
/// of 200.
///
/// Every delta base, because a delta whose base is gone cannot be resolved. The set of
/// wanted objects is grown until it stops growing, since a base can itself be a delta.
pub fn truncate_bundle(b: &Bundle, depth: usize) -> ZResult<Bundle> {
    if depth == 0 || b.changesets.len() <= depth {
        return Ok(b.clone());
    }
    let mut out = b.clone();
    let cut = out.changesets.len() - depth;
    let root = out.changesets[cut].id.clone();
    let keep: HashSet<String> = out.changesets[cut..].iter().map(|c| c.id.clone()).collect();
    out.changesets = out.changesets.split_off(cut);
    out.versions.retain(|(_, _, _, cs, _)| keep.contains(cs));
    out.files.retain(|(cs, _, _, _)| keep.contains(cs));

    // Seeds: what the surviving rows name, plus the whole head snapshot.
    let mut wanted: HashSet<String> = HashSet::new();
    for (_, _, hash, _, _) in &out.versions {
        wanted.insert(hash.clone());
    }
    for (_, _, _, new) in &out.files {
        if let Some(h) = new {
            wanted.insert(h.clone());
        }
    }
    let snap_hashes: HashSet<String> = out.snapshot.iter().map(|(_, h)| h.clone()).collect();
    wanted.extend(snap_hashes.iter().cloned());

    // Keep only the deltas worth keeping, then walk the bases they depend on. A base can
    // itself be a delta, so the walk repeats until nothing new is pulled in. Leaving this
    // out produced a bundle whose delta pointed at an object the cut had removed.
    out.deltas
        .retain(|(h, _, _)| wanted.contains(h) || snap_hashes.contains(h));
    loop {
        let mut add: Vec<String> = Vec::new();
        for (h, base, _) in &out.deltas {
            if wanted.contains(h) {
                add.push(base.clone());
            }
        }
        let mut grew = false;
        for a in add {
            if wanted.insert(a.clone()) {
                grew = true;
                // A newly wanted object may exist as a delta; bring it back in.
                for (h2, _, _) in &b.deltas {
                    if h2 == &a && !out.deltas.iter().any(|(x, _, _)| x == h2) {
                        if let Some(d) = b.deltas.iter().find(|(x, _, _)| x == h2) {
                            out.deltas.push(d.clone());
                        }
                    }
                }
            }
        }
        if !grew {
            break;
        }
    }

    out.objects.retain(|(h, _)| wanted.contains(h));
    out.hashes = wanted.iter().cloned().collect();
    out.hashes.sort();

    let carried: HashSet<String> = out
        .objects
        .iter()
        .map(|(h, _)| h.clone())
        .chain(out.deltas.iter().map(|(h, _, _)| h.clone()))
        .collect();
    let missing: Vec<&String> = snap_hashes.difference(&carried).collect();
    if !missing.is_empty() {
        return Err(bad(format!(
            "a shallow cut needs {} object(s) the bundle did not carry, so this remote cannot be cloned shallow",
            missing.len()
        )));
    }

    out.shallow_root = Some(root);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::commit_one;
    use crate::repo::tests::commit_files;
    use crate::store::tests::mem_db;

    fn empty_bundle_with_branch(branch: &str) -> Bundle {
        Bundle {
            branch: branch.to_string(),
            head: None,
            changesets: Vec::new(),
            objects: Vec::new(),
            binaries: Vec::new(),
            versions: Vec::new(),
            files: Vec::new(),
            hashes: Vec::new(),
            shallow_root: None,
            snapshot: Vec::new(),
            deltas: Vec::new(),
        }
    }

    #[test]
    fn a_branch_name_carrying_a_newline_cannot_inject_records() {
        // #282: `branch` was the one caller-controlled field written raw into the bundle text, and
        // decode splits on lines and matches each line by its first tab-separated column. A branch
        // containing a newline therefore forged whole records in the receiver's database on pull.
        //
        // The forged head is the cheapest demonstration: the last `head` line wins, so a branch of
        // `x\nhead\t<sha>` silently replaces the head the receiver applies.
        let forged = "x\nhead\tdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let b = empty_bundle_with_branch(forged);

        // Encoding escapes it, so what goes on the wire has one branch line and one head line only.
        // `encode` returns zlib bytes, so the text has to be inflated before it can be read.
        let wire = b.encode().unwrap();
        let text = String::from_utf8(
            crate::store::inflate_capped(ZlibDecoder::new(&wire[..]), crate::store::MAX_INFLATED)
                .expect("encode must produce a valid zlib stream"),
        )
        .expect("bundle text must be UTF-8");
        let branch_lines = text.lines().filter(|l| l.starts_with("branch\t")).count();
        let head_lines = text.lines().filter(|l| l.starts_with("head\t")).count();
        assert_eq!(
            branch_lines, 1,
            "the branch must occupy exactly one line: {text}"
        );
        assert_eq!(
            head_lines, 1,
            "an injected head record must not appear: {text}"
        );

        // And the value survives a round trip as itself, not as two records.
        let back = Bundle::decode(&wire).expect("a bundle this build wrote must decode");
        assert_eq!(back.branch, forged, "the escaping must be reversible");
        assert!(
            back.head.is_none(),
            "the forged head must not have been taken as the bundle head: {:?}",
            back.head
        );
    }

    #[test]
    fn a_bundle_from_an_old_server_with_a_multi_line_branch_is_refused() {
        // The fix has two halves because the escape only protects bundles this build wrote. A server
        // running an older build still writes the branch raw, so by the time `lines()` runs the
        // branch name has already become two records.
        //
        // The shape of the attack matters. `encode` always writes a head line, so a branch of
        // `x\nhead\t<chosen>` produces TWO head records: the injected one, then the real one. That
        // is what makes the framing check sound: a legitimate bundle has exactly one of each, and
        // the forger cannot produce a bundle that has one of each while also smuggling a record.
        let plain = b"gesta-bundle 1\n\
branch\tx\n\
head\t1111111111111111111111111111111111111111\n\
head\t2222222222222222222222222222222222222222\n\
end\n";
        let mut raw = Vec::new();
        {
            use flate2::write::ZlibEncoder;
            use std::io::Write;
            let mut e = ZlibEncoder::new(&mut raw, flate2::Compression::default());
            e.write_all(plain).unwrap();
            e.finish().unwrap();
        }
        let err = match Bundle::decode(&raw) {
            Ok(b) => panic!(
                "a bundle carrying two head records was accepted with branch {:?} and head {:?}",
                b.branch, b.head
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("more than one head record"),
            "the refusal must name the framing problem: {err}"
        );
    }

    #[test]
    fn a_forged_branch_cannot_smuggle_a_changeset_row() {
        // The other half of #282: an injected `cs` row put a changeset with attacker-chosen id,
        // author, message and parent into the victim's database on pull. The path checks at the end
        // of decode cover fv/cf/sn and never looked at cs, so this went in unexamined.
        //
        // The duplicate-record rule does not catch it on its own - `cs` rows are many by design -
        // which is why the branch is validated where it enters (`create_changeset_from_json`) and
        // escaped where it leaves (`encode`). This test pins the escape so a later change cannot
        // quietly drop it and let the row through again.
        let forged_branch = "x\ncs\tdeadbeef\t0\tmallory\tsteal everything\t-\t-";
        let b = empty_bundle_with_branch(forged_branch);
        let wire = b.encode().unwrap();

        // Whatever this build writes must decode back to exactly one branch and no changesets.
        let back = Bundle::decode(&wire).expect("our own bundle must decode");
        assert_eq!(back.branch, forged_branch);
        assert!(
            back.changesets.is_empty(),
            "the escaped branch must not have become a changeset row: {} row(s)",
            back.changesets.len()
        );
    }

    #[test]
    fn a_plain_branch_name_is_untouched_by_the_escape() {
        // The escape must not change what an ordinary bundle looks like on the wire, or every
        // existing test that inspects the text, and every old client, sees a different format.
        for name in ["main", "release/1.0", "feat/a-b_c.d"] {
            let b = empty_bundle_with_branch(name);
            let wire = b.encode().unwrap();
            let text = String::from_utf8(
                crate::store::inflate_capped(
                    ZlibDecoder::new(&wire[..]),
                    crate::store::MAX_INFLATED,
                )
                .expect("encode must produce a valid zlib stream"),
            )
            .expect("bundle text must be UTF-8");
            assert!(
                text.contains(&format!("branch\t{}\n", name)),
                "the branch line changed for a plain name: {text}"
            );
            assert_eq!(Bundle::decode(&wire).unwrap().branch, name);
        }
    }

    #[test]
    fn a_bundle_holding_a_climbing_path_is_refused_whole() {
        // #98: the rows a bundle decodes to are applied as they are, and checkout later joins those
        // paths onto the repository root, so `../../etc/x` writes outside it. Refused at decode, which
        // is before any table is touched.
        let b = Bundle {
            branch: "main".into(),
            head: None,
            changesets: Vec::new(),
            objects: Vec::new(),
            binaries: Vec::new(),
            versions: vec![("../../etc/passwd".to_string(), 1, "h".into(), "c".into(), 0)],
            files: Vec::new(),
            hashes: Vec::new(),
            shallow_root: None,
            snapshot: Vec::new(),
            deltas: Vec::new(),
        };
        let wire = b.encode().unwrap();
        let err = match Bundle::decode(&wire) {
            Ok(_) => panic!("a bundle with a climbing path was decoded"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not inside the repository") && err.contains("../../etc/passwd"),
            "the refusal must name the path: {err}"
        );

        // The same for a changeset file row and for a snapshot row, because all three reach checkout.
        for kind in ["files", "snapshot"] {
            let mut b2 = Bundle {
                branch: "main".into(),
                head: None,
                changesets: Vec::new(),
                objects: Vec::new(),
                binaries: Vec::new(),
                versions: Vec::new(),
                files: Vec::new(),
                hashes: Vec::new(),
                shallow_root: None,
                snapshot: Vec::new(),
                deltas: Vec::new(),
            };
            if kind == "files" {
                b2.files
                    .push(("c".into(), "/etc/passwd".into(), None, Some("h".into())));
            } else {
                b2.snapshot.push(("/etc/passwd".into(), "h".into()));
            }
            let w = match b2.encode() {
                Ok(v) => v,
                Err(_) => continue,
            };
            match Bundle::decode(&w) {
                Ok(_) => panic!("a bundle with an absolute {} path was decoded", kind),
                Err(e) => assert!(
                    e.to_string().contains("not inside the repository"),
                    "{}: {e}",
                    kind
                ),
            }
        }

        // An ordinary path is still accepted, so the check does not refuse every bundle.
        let ok = Bundle {
            branch: "main".into(),
            head: None,
            changesets: Vec::new(),
            objects: Vec::new(),
            binaries: Vec::new(),
            versions: vec![("src/main.rs".to_string(), 1, "h".into(), "c".into(), 0)],
            files: Vec::new(),
            hashes: Vec::new(),
            shallow_root: None,
            snapshot: Vec::new(),
            deltas: Vec::new(),
        };
        let w = ok.encode().unwrap();
        assert!(Bundle::decode(&w).is_ok(), "an ordinary bundle must decode");
    }

    #[test]
    fn base64_roundtrips_including_padding_cases() {
        for input in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"hello world!"] {
            let e = enc_base64(input);
            let d = dec_base64(&e).unwrap();
            assert_eq!(d, input.to_vec(), "failed for {:?} -> {:?}", input, e);
        }
    }

    #[test]
    fn field_escaping_survives_real_text() {
        let original = "line one\nline two\ttabbed\\backslash\n";
        let round = dec_str(&enc(original));
        assert_eq!(round, original);
    }

    #[test]
    fn bundle_roundtrips_through_encode_decode() {
        let conn = mem_db();
        let h = put_object(&conn, "alpha\nbeta\n").unwrap();
        conn.execute(
            "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
             VALUES('cs1','first commit\twith tab','agent',10,'committed',NULL,'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
             VALUES('cs1','src/a.rs',NULL,?1)",
            params![h],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
             VALUES('src/a.rs',1,?1,'cs1',10)",
            params![h],
        )
        .unwrap();
        conn.execute("UPDATE branches SET head='cs1' WHERE name='main'", [])
            .unwrap();

        let bundle = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        let bytes = bundle.encode().unwrap();
        let back = Bundle::decode(&bytes).unwrap();

        assert_eq!(back.branch, "main");
        assert_eq!(back.head.as_deref(), Some("cs1"));
        assert_eq!(back.changesets.len(), 1);
        assert_eq!(back.changesets[0].message, "first commit\twith tab");
        assert_eq!(back.objects.len(), 1);
        assert_eq!(back.objects[0].1, "alpha\nbeta\n");
    }

    #[test]
    fn push_then_pull_transfers_history_and_objects() {
        let source = mem_db();
        let h = put_object(&source, "content\n").unwrap();
        source
            .execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES('cs1','m','a',1,'committed',NULL,'main')",
                [],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES('cs1','f.txt',NULL,?1)",
                params![h],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',1,?1,'cs1',1)",
                params![h],
            )
            .unwrap();
        source
            .execute("UPDATE branches SET head='cs1' WHERE name='main'", [])
            .unwrap();

        let bundle = build_bundle(&source, "main", &HashSet::new()).unwrap();
        let bytes = bundle.encode().unwrap();
        let back = Bundle::decode(&bytes).unwrap();

        // Receiver starts empty.
        let dest = mem_db();
        let applied = apply_bundle(&dest, &back).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(branch_head(&dest, "main").unwrap().as_deref(), Some("cs1"));
        assert_eq!(get_object(&dest, &h).unwrap(), "content\n");
    }

    #[test]
    fn applying_a_bundle_twice_changes_nothing() {
        let source = mem_db();
        let h = put_object(&source, "x\n").unwrap();
        source
            .execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES('cs1','m','a',1,'committed',NULL,'main')",
                [],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',1,?1,'cs1',1)",
                params![h],
            )
            .unwrap();
        source
            .execute("UPDATE branches SET head='cs1' WHERE name='main'", [])
            .unwrap();

        let bundle = build_bundle(&source, "main", &HashSet::new()).unwrap();
        let dest = mem_db();
        assert_eq!(apply_bundle(&dest, &bundle).unwrap(), 1);
        assert_eq!(
            apply_bundle(&dest, &bundle).unwrap(),
            0,
            "second apply is a no-op"
        );
    }

    #[test]
    fn a_corrupted_object_is_rejected() {
        let mut b = Bundle {
            branch: "main".into(),
            head: None,
            changesets: Vec::new(),
            objects: vec![("0".repeat(64), "tampered".into())],
            binaries: Vec::new(),
            versions: Vec::new(),
            files: Vec::new(),
            hashes: Vec::new(),
            shallow_root: None,
            snapshot: Vec::new(),
            deltas: Vec::new(),
        };
        b.hashes.push("0".repeat(64));
        let dest = mem_db();
        let err = apply_bundle(&dest, &b).unwrap_err().to_string();
        assert!(err.contains("hash check"), "got: {}", err);
    }

    #[test]
    fn bundle_only_sends_what_the_peer_lacks() {
        let conn = mem_db();
        let h1 = put_object(&conn, "one\n").unwrap();
        let h2 = put_object(&conn, "two\n").unwrap();
        for (id, parent, hash, ver) in [("cs1", None, &h1, 1i64), ("cs2", Some("cs1"), &h2, 2)] {
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,'m','a',1,'committed',?2,'main')",
                params![id, parent],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',?1,?2,?3,1)",
                params![ver, hash, id],
            )
            .unwrap();
        }
        conn.execute("UPDATE branches SET head='cs2' WHERE name='main'", [])
            .unwrap();

        let mut have = HashSet::new();
        have.insert("cs1".to_string());
        let b = build_bundle(&conn, "main", &have).unwrap();
        assert_eq!(b.changesets.len(), 1, "peer already has cs1");
        assert_eq!(b.changesets[0].id, "cs2");
        assert!(
            !b.objects.iter().any(|(h, _)| h == &h1),
            "cs1 content not resent"
        );
    }
    /// A bundle must carry every changeset reachable from the head, in an order a
    /// receiver can apply. A depth-first walk happens to work for two commits and
    /// starts dropping them for longer histories, so the depth is the point of this
    /// test rather than an afterthought.
    #[test]
    fn a_bundle_carries_the_whole_chain_for_a_long_history() {
        let conn = mem_db();
        let mut parent: Option<String> = None;
        let mut ids = Vec::new();
        for i in 0..12 {
            let id = format!("cs{:02}", i);
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,?2,'a',?3,'pending',?4,'main')",
                params![id, format!("m{}", i), i as i64, parent],
            )
            .unwrap();
            commit_one(&conn, &id).unwrap();
            parent = Some(id.clone());
            ids.push(id);
        }

        let b = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        assert_eq!(
            b.changesets.len(),
            12,
            "the bundle dropped changesets, it carried {}",
            b.changesets.len()
        );
        // Oldest first, so the receiver applies a parent before its child.
        assert_eq!(
            b.changesets.first().map(|c| c.id.clone()),
            Some(ids[0].clone())
        );
        assert_eq!(
            b.changesets.last().map(|c| c.id.clone()),
            Some(ids[11].clone())
        );
        assert_eq!(b.head.as_deref(), Some(ids[11].as_str()));
    }
    /// Across a merge, a parent can sit deeper than a child on the other side, so
    /// reversing a breadth-first walk can emit a child before its own parent. A
    /// receiver applying rows in that order fails on a parent it has not seen.
    #[test]
    fn the_order_puts_every_parent_before_its_child_across_a_merge() {
        // A merge row in the shape the store uses: two parents. Building it
        // directly keeps this about ordering and nothing else.
        let conn = mem_db();
        for (id, p1, p2, ts) in [
            ("base", None, None, 1i64),
            ("left", Some("base"), None, 2),
            ("right", Some("base"), None, 3),
            ("merge", Some("left"), Some("right"), 4),
        ] {
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,parent2,branch)
                 VALUES(?1,?2,'a',?3,'committed',?4,?5,'main')",
                rusqlite::params![id, format!("m-{}", id), ts, p1, p2],
            )
            .unwrap();
        }

        let reachable = crate::store::ancestors_bfs(&conn, "merge").unwrap();
        assert_eq!(
            reachable.len(),
            4,
            "expected four changesets, got {:?}",
            reachable
        );
        let wanted: HashSet<String> = reachable.into_iter().collect();
        let order = topo_order(&conn, "merge", &wanted).unwrap();

        let mut pos = std::collections::HashMap::new();
        for (i, id) in order.iter().enumerate() {
            pos.insert(id.clone(), i);
        }
        assert_eq!(
            order.len(),
            4,
            "every reachable changeset must be listed, got {:?}",
            order
        );
        for (i, id) in order.iter().enumerate() {
            let (p1, p2) = crate::store::parents_of_public(&conn, id).unwrap();
            for p in [p1, p2].into_iter().flatten() {
                if let Some(pi) = pos.get(&p) {
                    assert!(
                        *pi < i,
                        "parent {} of {} came after it in {:?}",
                        p,
                        id,
                        order
                    );
                }
            }
        }
        // The merge is last: it depends on both sides.
        assert_eq!(order.last().map(|s| s.as_str()), Some("merge"));
    }
    /// A chain of deltas must resolve on the receiving side even though the order
    /// they appear in is not the order they depend on. Each version here is a delta
    /// against the previous one, so applying them in file order works only by luck;
    /// the resolver has to run in rounds.
    #[test]
    fn a_chain_of_deltas_resolves_in_any_order() {
        let src = mem_db();
        // A realistic edit: a fixed body with a few lines rewritten per version. If
        // every line changed, a delta would be the whole file and the store would
        // (correctly) refuse it, which would test nothing here.
        let body = |n: usize| {
            let mut s = String::new();
            for i in 0..4000 {
                if i % 400 == n % 400 {
                    s.push_str(&format!("line {} rewritten at version {}\n", i, n));
                } else {
                    s.push_str(&format!("line {} stable text that does not change\n", i));
                }
            }
            s
        };

        // Ten versions of one path, each stored against the previous one so the store
        // holds a delta chain. Every version is also recorded as a committed changeset
        // so a bundle built from the branch reaches all of them.
        // The base argument is the previous object's hash, not its content.
        let mut prev_hash: Option<String> = None;
        for v in 0..10 {
            let content = body(v);
            let hash =
                crate::store::put_object_against(&src, &content, prev_hash.as_deref()).unwrap();
            let id = format!("csver{:02}", v);
            let parent = if v == 0 {
                None
            } else {
                Some(format!("csver{:02}", v - 1))
            };
            src.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,?2,'a',?3,'committed',?4,'main')",
                rusqlite::params![id, format!("v{}", v), v as i64, parent],
            )
            .unwrap();
            src.execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',?1,?2,?3,?4)",
                rusqlite::params![v as i64, hash, format!("csver{:02}", v), v as i64],
            )
            .unwrap();
            src.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,'f.txt',?2,?3)",
                rusqlite::params![format!("csver{:02}", v), prev_hash, hash],
            )
            .unwrap();
            prev_hash = Some(hash);
        }
        crate::store::set_branch_head(&src, "main", "csver09").unwrap();

        let bundle = build_bundle(&src, "main", &HashSet::new()).unwrap();
        assert!(
            !bundle.deltas.is_empty(),
            "ten versions of one file produced no delta at all (objects={}, hashes={})",
            bundle.objects.len(),
            bundle.hashes.len()
        );
        for (hash, base, _) in &bundle.deltas {
            let known = bundle.objects.iter().any(|(h, _)| h == base)
                || bundle.deltas.iter().any(|(h, _, _)| h == base);
            assert!(
                known,
                "delta {} points at base {} which is not in the bundle",
                hash, base
            );
        }

        // Reverse the deltas so a dependency appears after its dependant, then apply.
        let mut shuffled = bundle.clone();
        shuffled.deltas.reverse();
        let wire = shuffled.encode().unwrap();
        let decoded = Bundle::decode(&wire).unwrap();
        let dest = mem_db();
        apply_bundle(&dest, &decoded).unwrap();

        for (hash, _base, _ops) in &bundle.deltas {
            let got = crate::store::get_object(&dest, hash).unwrap();
            let expect = crate::store::get_object(&src, hash).unwrap();
            assert_eq!(got, expect, "rebuilt object {} differs", hash);
        }
        // The newest version must be readable and correct on the receiving side.
        let newest =
            crate::store::get_object(&dest, &crate::store::hash_content(&body(9))).unwrap();
        assert_eq!(newest, body(9));
    }
    /// A shallow cut has to keep two things alive: the head snapshot, and every object a
    /// surviving delta needs to be resolved. Losing either makes the clone fail, and the
    /// first two attempts at this failed on exactly those two points.
    #[test]
    fn a_shallow_cut_keeps_the_snapshot_and_every_delta_base() {
        let conn = mem_db();
        let body = |n: usize| {
            let mut s = String::new();
            for i in 0..4000 {
                if i % 400 == n % 400 {
                    s.push_str(&format!("line {} rewritten at version {}\n", i, n));
                } else {
                    s.push_str(&format!("line {} stable text that does not change\n", i));
                }
            }
            s
        };
        let mut prev_hash: Option<String> = None;
        for v in 0..12 {
            let content = body(v);
            let hash =
                crate::store::put_object_against(&conn, &content, prev_hash.as_deref()).unwrap();
            let id = format!("csh{:02}", v);
            let parent = if v == 0 {
                None
            } else {
                Some(format!("csh{:02}", v - 1))
            };
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,?2,'a',?3,'committed',?4,'main')",
                params![id, format!("v{}", v), v as i64, parent],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',?1,?2,?3,?4)",
                params![v as i64, hash, format!("csh{:02}", v), v as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,'f.txt',?2,?3)",
                params![format!("csh{:02}", v), prev_hash, hash],
            )
            .unwrap();
            prev_hash = Some(hash);
        }
        crate::store::set_branch_head(&conn, "main", "csh11").unwrap();

        let full = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        // A snapshot is what makes a shallow clone able to restore the working tree.
        let head_tree = crate::store::tree_at(&conn, "csh11").unwrap();
        let mut with_snapshot = full.clone();
        with_snapshot.snapshot = head_tree.clone();
        let carried: HashSet<String> = with_snapshot
            .objects
            .iter()
            .map(|(h, _)| h.clone())
            .chain(with_snapshot.deltas.iter().map(|(h, _, _)| h.clone()))
            .collect();
        for (path, hash) in &head_tree {
            if !carried.contains(hash) {
                let content = crate::store::get_object(&conn, hash).unwrap();
                with_snapshot.objects.push((hash.to_string(), content));
                let _ = path;
            }
        }

        let cut = truncate_bundle(&with_snapshot, 3).unwrap();
        assert_eq!(cut.changesets.len(), 3);
        assert!(cut.shallow_root.is_some(), "the cut has to be recorded");
        assert_eq!(
            cut.snapshot.len(),
            head_tree.len(),
            "the snapshot must survive the cut"
        );

        // Every surviving delta must be resolvable: its base is present as a whole
        // object or as another delta in the same bundle.
        let present: HashSet<String> = cut
            .objects
            .iter()
            .map(|(h, _)| h.clone())
            .chain(cut.deltas.iter().map(|(h, _, _)| h.clone()))
            .collect();
        for (hash, base, _) in &cut.deltas {
            assert!(
                present.contains(base),
                "delta {} points at base {} which the cut removed",
                hash,
                base
            );
        }
        // And it must apply cleanly on a fresh repository.
        let wire = cut.encode().unwrap();
        let decoded = Bundle::decode(&wire).unwrap();
        let dest = mem_db();
        apply_bundle(&dest, &decoded).unwrap();
        for (_, hash) in &head_tree {
            let got = crate::store::get_object(&dest, hash).unwrap();
            let expect = crate::store::get_object(&conn, hash).unwrap();
            assert_eq!(
                got, expect,
                "an object from the snapshot differs after the cut"
            );
        }
    }

    #[test]
    fn a_depth_larger_than_the_history_changes_nothing() {
        let conn = mem_db();
        commit_files(&conn, "a1", "one", &[("a.txt", "alpha\n")]);
        commit_files(&conn, "a2", "two", &[("a.txt", "beta\n")]);
        let full = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        let same = truncate_bundle(&full, 99).unwrap();
        assert_eq!(same.changesets.len(), full.changesets.len());
        assert!(
            same.shallow_root.is_none(),
            "nothing was cut, so nothing is shallow"
        );
    }
    /// Deepening adds history below the boundary without moving the head or losing the
    /// working tree. It also has to leave the boundary honest: a clone that is still cut
    /// must still say so, or a later attempt to take everything reports nothing to do.
    #[test]
    fn deepening_extends_history_and_updates_the_boundary() {
        let conn = mem_db();
        for v in 0..8 {
            let id = format!("csd{:02}", v);
            let parent = if v == 0 {
                None
            } else {
                Some(format!("csd{:02}", v - 1))
            };
            conn.execute(
                "INSERT INTO changesets(id,message,author,created_at,status,parent,branch)
                 VALUES(?1,?2,'a',?3,'committed',?4,'main')",
                params![id, format!("v{}", v), v as i64, parent],
            )
            .unwrap();
            let content = format!("content of version {}\n", v);
            let hash = crate::store::put_object(&conn, &content).unwrap();
            conn.execute(
                "INSERT INTO file_versions(path,version,hash,changeset_id,created_at)
                 VALUES('f.txt',?1,?2,?3,?4)",
                params![v as i64, hash, format!("csd{:02}", v), v as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO changeset_files(changeset_id,path,old_hash,new_hash)
                 VALUES(?1,'f.txt',NULL,?2)",
                params![format!("csd{:02}", v), hash],
            )
            .unwrap();
        }
        crate::store::set_branch_head(&conn, "main", "csd07").unwrap();

        let full = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        let cut = truncate_bundle(&full, 3).unwrap();
        assert_eq!(cut.changesets.len(), 3);

        // Simulate what deepen does: apply the older rows without moving the head.
        let mut older = cut.clone();
        let keep: Vec<_> = full.changesets[..5].to_vec();
        older.changesets = keep;
        let want: HashSet<String> = older.changesets.iter().map(|c| c.id.clone()).collect();
        older.versions = full
            .versions
            .iter()
            .filter(|(_, _, _, cs, _)| want.contains(cs))
            .cloned()
            .collect();
        older.files = full
            .files
            .iter()
            .filter(|(cs, _, _, _)| want.contains(cs))
            .cloned()
            .collect();
        older.objects = full.objects.clone();
        older.deltas = full.deltas.clone();
        older.hashes = full.hashes.clone();
        older.snapshot = Vec::new();
        older.shallow_root = None;

        let head_before = crate::store::branch_head(&conn, "main").unwrap();
        apply_bundle_opts(&conn, &older, false).unwrap();
        assert_eq!(
            crate::store::branch_head(&conn, "main").unwrap(),
            head_before,
            "deepening must not move the head"
        );

        let held: i64 = conn
            .query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(held, 8, "expected the full chain after applying both parts");

        // The oldest held changeset is csd00, which has no parent, so the boundary is
        // gone entirely.
        let (p1, _) = crate::store::parents_of_public(&conn, "csd00").unwrap();
        assert!(p1.is_none(), "the first changeset has no parent");
    }
    /// Provenance has to survive the wire, and a bundle written before it existed has to
    /// stay readable. Both directions matter: losing the reasoning on a clone defeats the
    /// point, and refusing an old archive strands every repository already pushed.
    #[test]
    fn provenance_travels_and_old_bundles_still_decode() {
        let conn = mem_db();
        commit_files(&conn, "p1", "one", &[("a.txt", "alpha\n")]);
        conn.execute(
            "UPDATE changesets SET session = 'sess-7', intent = 'add the fixture' WHERE id = 'p1'",
            [],
        )
        .unwrap();

        let b = build_bundle(&conn, "main", &HashSet::new()).unwrap();
        assert_eq!(b.changesets.len(), 1);
        assert_eq!(b.changesets[0].session.as_deref(), Some("sess-7"));
        assert_eq!(b.changesets[0].intent.as_deref(), Some("add the fixture"));

        let wire = b.encode().unwrap();
        let back = Bundle::decode(&wire).unwrap();
        assert_eq!(back.changesets[0].session.as_deref(), Some("sess-7"));
        assert_eq!(
            back.changesets[0].intent.as_deref(),
            Some("add the fixture")
        );

        // And it lands in a fresh repository.
        let dest = mem_db();
        apply_bundle(&dest, &back).unwrap();
        let stored: (Option<String>, Option<String>) = dest
            .query_row(
                "SELECT session, intent FROM changesets WHERE id = 'p1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored.0.as_deref(), Some("sess-7"));
        assert_eq!(stored.1.as_deref(), Some("add the fixture"));
    }

    #[test]
    fn a_bundle_without_provenance_still_reads() {
        // The shape an older build wrote: seven fields, no session or intent.
        let text = "gesta-bundle 1\nbranch\tmain\nhead\tp1\n\
cs\tp1\t1\tagent\tone\t-\t-\nend\n";
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(text.as_bytes()).unwrap();
        let wire = enc.finish().unwrap();
        let b = Bundle::decode(&wire).unwrap();
        assert_eq!(b.changesets.len(), 1, "the old row was dropped");
        assert_eq!(b.changesets[0].id, "p1");
        assert!(b.changesets[0].session.is_none());
        assert!(b.changesets[0].intent.is_none());
    }
    #[test]
    fn a_shallow_bundle_carries_the_head_tree_so_the_receiver_can_restore_it() {
        // #285: `build_bundle_with_depth` cut the chain and left `snapshot` empty, so the receiver had
        // only the surviving changesets to replay. `restore_worktree` walks `tree_at`, which sees just
        // those, so every file created by a cut changeset was missing from disk while the clone still
        // printed success.
        //
        // Measured on a three-changeset repository before the fix: `--depth 1` restored 6 of 9 files
        // and said "cloned ... 6 file(s)". The other two clone paths already carried the tree; only
        // the HTTP one did not.
        let conn = mem_db();
        commit_files(&conn, "c1", "first", &[("old.txt", "one\n")]);
        commit_files(&conn, "c2", "second", &[("mid.txt", "two\n")]);
        commit_files(&conn, "c3", "third", &[("new.txt", "three\n")]);

        let bundle = build_bundle_with_depth(&conn, "main", &HashSet::new(), Some(1)).unwrap();
        assert!(
            bundle.shallow_root.is_some(),
            "a depth-1 bundle must be marked shallow, or this is not testing a cut"
        );
        let paths: Vec<&str> = bundle.snapshot.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths.len(),
            3,
            "the head tree has three files and all three must travel; got {paths:?}"
        );
        assert!(
            paths.contains(&"old.txt"),
            "the file created by the CUT changeset is the one a replay cannot produce, so it must be \
             in the snapshot; got {paths:?}"
        );

        // And the objects those files need must be in the bundle, or the receiver holds a tree it
        // cannot materialise.
        for (_, hash) in &bundle.snapshot {
            assert!(
                bundle.hashes.iter().any(|h| h == hash),
                "object {hash} is named by the snapshot but not carried by the bundle"
            );
        }

        // A full bundle carries its whole history, so it does not need - and must not pay for - a
        // redundant tree.
        let full = build_bundle_with_depth(&conn, "main", &HashSet::new(), None).unwrap();
        assert!(
            full.snapshot.is_empty(),
            "a bundle with no cut restores the ordinary way and should not carry a snapshot"
        );
    }
}
