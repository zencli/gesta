//! Reading and writing Git repositories.
//!
//! Git stores four object kinds, each zlib compressed, named by the SHA-1 of
//! `"<kind> <size>\0<body>"`. A repository is that object store plus refs, and a commit names a
//! tree and its parents. Nothing else is needed to move content in and out, which is why this is
//! written against the documented format rather than by calling the git binary: a bridge that
//! shells out cannot run where git is not installed, and that is exactly the machine an agent
//! works on.
//!
//! What this does not do: packfiles. Loose objects are readable and writable, and `git gc`
//! gives a repository that only has packs back its loose form. A pack reader is a different
//! piece of work and is not pretended to be here.

use crate::bad;
use crate::ZResult;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The four kinds of object git stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Tag => "tag",
        }
    }

    fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "blob" => Kind::Blob,
            "tree" => Kind::Tree,
            "commit" => Kind::Commit,
            "tag" => Kind::Tag,
            _ => return None,
        })
    }
}

/// A Git object: its kind and its body.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub kind: Kind,
    pub body: Vec<u8>,
}

/// A tree entry: a mode, a name and the object it points at.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub mode: String,
    pub name: String,
    pub hash: String,
}

/// A commit, as far as anything here needs one.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_time: i64,
    pub timezone: String,
    pub message: String,
}

// SHA-1. Git names objects by it, so a bridge cannot avoid it. Written here rather than pulled in
// because this is the only use: the tree, the commit and the blob are hashed over a few hundred
// bytes, and the alternative is another dependency for one call.
/// The same digest as `sha1`, as bytes. A pack's trailing checksum is raw bytes, and converting a
/// hex string back to them to append it would be two conversions for one value.
pub fn sha1_raw(data: &[u8]) -> [u8; 20] {
    let h = sha1(data);
    let mut out = [0u8; 20];
    for i in 0..20 {
        let byte = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0);
        out[i] = byte;
    }
    out
}

fn sha1(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            let c = *chunk;
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    h.iter().map(|x| format!("{:08x}", x)).collect()
}

/// The name git gives an object: the SHA-1 of its header and body.
pub fn object_hash(kind: Kind, body: &[u8]) -> String {
    let header = format!("{} {}\0", kind.as_str(), body.len());
    let mut full = header.into_bytes();
    full.extend_from_slice(body);
    sha1(&full)
}

/// Where a repository keeps its objects and refs.
pub struct Repo {
    /// `.git` for a normal repository, or the path a worktree's git dir points at.
    pub git_dir: PathBuf,
}

impl Repo {
    /// Find the repository that contains `start`, or `start/.git` itself.
    pub fn open(start: &Path) -> ZResult<Repo> {
        // A bare repository is its own git directory: it has `objects` and `refs` directly, and
        // no `.git` inside. Checking for that first is what lets an import read a bare clone or
        // a staging repository `git init --bare` just made.
        if start.join("objects").is_dir() && start.join("refs").is_dir() {
            return Ok(Repo {
                git_dir: start.to_path_buf(),
            });
        }
        // Four things reach this function and only two of them are a directory: a repository, a
        // path inside one, a bundle file, and a path that does not exist. "no git repository at or
        // above X" is true for all four and useless for two, because a bundle is a single file that
        // holds a whole history and the reader has to clone it first. Say which case this is.
        if start.exists() && !start.is_dir() {
            let name = start
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.ends_with(".bundle") || name.ends_with(".pack") {
                return Err(bad(format!(
                    "{} is a file, not a repository. A git bundle holds a whole history in one file, so it has to be cloned before it can be read:\n  git clone {} <some-directory>\n  gesta git import <some-directory>",
                    start.display(),
                    start.display()
                )));
            }
            return Err(bad(format!(
                "{} is a file, not a directory. This command takes a git repository directory (a checkout, or a bare repository)",
                start.display()
            )));
        }
        if !start.exists() {
            return Err(bad(format!(
                "{} does not exist, so there is no repository to read",
                start.display()
            )));
        }

        let mut cur = start.to_path_buf();
        loop {
            let candidate = cur.join(".git");
            if candidate.is_dir() {
                return Ok(Repo { git_dir: candidate });
            }
            // A worktree's `.git` is a file naming the real directory.
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate)?;
                if let Some(rest) = text.trim().strip_prefix("gitdir:") {
                    let p = cur.join(rest.trim());
                    return Ok(Repo { git_dir: p });
                }
            }
            if !cur.pop() {
                return Err(bad(format!(
                    "no git repository at or above {}",
                    start.display()
                )));
            }
        }
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        // Loose objects are split by the first two characters, which keeps any one directory to
        // a workable number of entries.
        self.git_dir
            .join("objects")
            .join(&hash[..2])
            .join(&hash[2..])
    }

    /// Write an object and return its name. An object already present is not rewritten.
    pub fn write_object(&self, kind: Kind, body: &[u8]) -> ZResult<String> {
        let hash = object_hash(kind, body);
        let path = self.object_path(&hash);
        if path.exists() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let header = format!("{} {}\0", kind.as_str(), body.len());
        let mut raw = header.into_bytes();
        raw.extend_from_slice(body);
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw)?;
        let packed = enc.finish()?;
        // Written to a temporary name and moved, so a reader never sees half an object.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, &packed)?;
        std::fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    /// Read an object by name.
    pub fn read_object(&self, hash: &str) -> ZResult<Object> {
        let path = self.object_path(hash);
        if !path.exists() {
            // Everything can be inside a packfile instead. A repository that has been through
            // `git gc` holds no loose objects at all, and refusing it made an import of any real
            // repository impossible.
            return self.read_packed(hash);
        }
        let packed = std::fs::read(&path)?;
        let mut dec = flate2::read::ZlibDecoder::new(&packed[..]);
        let mut raw = Vec::new();
        dec.read_to_end(&mut raw)?;
        let nul = raw
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad(format!("object {} has no header", hash)))?;
        let header = String::from_utf8_lossy(&raw[..nul]).to_string();
        let (k, size) = header.split_once(' ').ok_or_else(|| {
            bad(format!(
                "object {} has a malformed header: {}",
                hash, header
            ))
        })?;
        let kind = Kind::parse(k)
            .ok_or_else(|| bad(format!("object {} has an unknown kind: {}", hash, k)))?;
        let body = raw[nul + 1..].to_vec();
        let want: usize = size
            .parse()
            .map_err(|_| bad(format!("object {} declares a bad size: {}", hash, size)))?;
        if body.len() != want {
            return Err(bad(format!(
                "object {} declares {} bytes and holds {}",
                hash,
                want,
                body.len()
            )));
        }
        Ok(Object { kind, body })
    }

    /// Read an object out of whichever pack holds it.
    fn read_packed(&self, hash: &str) -> ZResult<Object> {
        let packs = crate::packfile::Pack::open_all(&self.git_dir)?;
        for pack in &packs {
            if let Some(found) = pack.read(hash)? {
                let kind = match found.kind {
                    crate::packfile::Kind::Commit => Kind::Commit,
                    crate::packfile::Kind::Tree => Kind::Tree,
                    crate::packfile::Kind::Blob => Kind::Blob,
                    crate::packfile::Kind::Tag => Kind::Tag,
                    // A resolved object is never a delta: the reader hands back the base's kind.
                    crate::packfile::Kind::OfsDelta | crate::packfile::Kind::RefDelta => {
                        return Err(bad(format!("object {} resolved to a delta", hash)))
                    }
                };
                return Ok(Object {
                    kind,
                    body: found.data,
                });
            }
        }
        Err(Box::new(crate::error::Error::new(
            crate::error::Code::NotFound,
            format!(
                "object {} is in neither a loose object nor a packfile here ({} pack(s) searched)",
                hash,
                packs.len()
            ),
        )))
    }

    /// Write a ref, creating it if needed.
    pub fn write_ref(&self, name: &str, hash: &str) -> ZResult<()> {
        let path = self.git_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, format!("{}\n", hash))?;
        Ok(())
    }

    /// Every branch, by short name.
    pub fn branches(&self) -> ZResult<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        // `git gc` moves branch refs out of `refs/heads/` and into `packed-refs`, so a repository
        // that has been packed once has no ref files at all. Reading only the directory made an
        // import of a real repository report "no branches" while every commit was present.
        let packed = self.git_dir.join("packed-refs");
        if let Ok(text) = std::fs::read_to_string(&packed) {
            for line in text.lines() {
                // The header starts with `#`, and a peeled tag line with `^`.
                if line.starts_with('#') || line.starts_with('^') || line.trim().is_empty() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let (Some(sha), Some(name)) = (parts.next(), parts.next()) else {
                    continue;
                };
                if let Some(branch) = name.strip_prefix("refs/heads/") {
                    out.insert(branch.to_string(), sha.to_string());
                }
            }
        }
        // Loose refs win: a branch written after the last pack is the current one.
        let heads = self.git_dir.join("refs").join("heads");
        collect_refs(&heads, &heads, &mut out)?;
        Ok(out)
    }
}

fn collect_refs(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> ZResult<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_refs(base, &path, out)?;
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            let text = text.trim();
            if text.is_empty() || text.starts_with("ref:") {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(base) {
                let name = rel.to_string_lossy().replace('\\', "/");
                out.insert(name, text.to_string());
            }
        }
    }
    Ok(())
}

// Trees.

/// Parse a tree body into its entries. Git sorts entries by name with directories treated as
/// ending in a slash, and the order matters because it is part of the hash.
pub fn parse_tree(body: &[u8]) -> ZResult<Vec<Entry>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let space = body[i..]
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| bad("tree entry has no mode separator"))?
            + i;
        let mode = String::from_utf8_lossy(&body[i..space]).to_string();
        let nul = body[space + 1..]
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad("tree entry has no name terminator"))?
            + space
            + 1;
        let name = String::from_utf8_lossy(&body[space + 1..nul]).to_string();
        if nul + 21 > body.len() {
            return Err(bad("tree entry is shorter than its hash"));
        }
        let hash = body[nul + 1..nul + 21]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        out.push(Entry { mode, name, hash });
        i = nul + 21;
    }
    Ok(out)
}

/// Build a tree body from entries, sorted the way git sorts them.
pub fn build_tree(mut entries: Vec<Entry>) -> Vec<u8> {
    // A directory sorts as if its name ended in a slash, so it comes after any file sharing its
    // prefix. Sorting on the key git uses is what makes the hash match.
    entries.sort_by(|a, b| {
        let ka = sort_key(a);
        let kb = sort_key(b);
        ka.cmp(&kb)
    });
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        for i in 0..20 {
            let byte = u8::from_str_radix(&e.hash[i * 2..i * 2 + 2], 16).unwrap_or(0);
            out.push(byte);
        }
    }
    out
}

fn sort_key(e: &Entry) -> Vec<u8> {
    let mut k = e.name.as_bytes().to_vec();
    if e.mode == "40000" {
        k.push(b'/');
    }
    k
}

// Commits.

/// Parse a commit body. Only the fields a bridge needs are read; anything else is ignored, which
/// is what keeps a commit written by another tool readable here.
pub fn parse_commit(body: &[u8]) -> ZResult<Commit> {
    let text = String::from_utf8_lossy(body).to_string();
    let (head, message) = match text.split_once("\n\n") {
        Some((h, m)) => (h.to_string(), m.to_string()),
        None => (text.clone(), String::new()),
    };
    let mut tree = String::new();
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut author_time = 0;
    let mut timezone = "+0000".to_string();
    for line in head.lines() {
        if let Some(v) = line.strip_prefix("tree ") {
            tree = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("parent ") {
            parents.push(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("author ") {
            // "Name <mail> 1700000000 +0700"
            author = v.to_string();
            if let Some((rest, tz)) = v.rsplit_once(' ') {
                timezone = tz.trim().to_string();
                if let Some((_, ts)) = rest.rsplit_once(' ') {
                    author_time = ts.trim().parse().unwrap_or(0);
                }
            }
        }
    }
    Ok(Commit {
        tree,
        parents,
        author,
        author_time,
        timezone,
        message,
    })
}

/// Build a commit body. `author_name` and `author_mail` are kept apart from the time so a commit
/// exported from here has the same shape a person's does.
pub fn build_commit(
    tree: &str,
    parents: &[String],
    author_name: &str,
    author_mail: &str,
    time: i64,
    timezone: &str,
    message: &str,
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&format!("tree {}\n", tree));
    for p in parents {
        out.push_str(&format!("parent {}\n", p));
    }
    let who = format!("{} <{}> {} {}", author_name, author_mail, time, timezone);
    out.push_str(&format!("author {}\n", who));
    out.push_str(&format!("committer {}\n", who));
    out.push('\n');
    out.push_str(message);
    if !message.ends_with('\n') {
        out.push('\n');
    }
    out.into_bytes()
}

/// Split a git author line into its name and mail, for reporting.
pub fn author_parts(line: &str) -> (String, String) {
    match (line.find('<'), line.find('>')) {
        (Some(a), Some(b)) if b > a => (
            line[..a].trim().to_string(),
            line[a + 1..b].trim().to_string(),
        ),
        _ => (line.trim().to_string(), String::new()),
    }
}

impl Repo {
    /// The commit a branch points at, read the way git stores it.
    ///
    /// Restored as a method because a server needs it: the earlier version was removed as dead code
    /// when only a test called it, and the protocol needs exactly this lookup to answer a clone.
    pub fn read_head(&self, branch: &str) -> ZResult<Option<String>> {
        let loose = self.git_dir.join("refs").join("heads").join(branch);
        if let Ok(text) = std::fs::read_to_string(&loose) {
            let t = text.trim();
            if !t.is_empty() {
                return Ok(Some(t.to_string()));
            }
        }
        let packed = std::fs::read_to_string(self.git_dir.join("packed-refs")).unwrap_or_default();
        for line in packed.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            if let Some((hash, name)) = line.split_once(' ') {
                if name.trim() == format!("refs/heads/{}", branch) {
                    return Ok(Some(hash.trim().to_string()));
                }
            }
        }
        Ok(None)
    }

    /// Every object in this repository, as (hex sha, type code, body).
    ///
    /// Walks loose objects only. A repository this code wrote never has a pack, and reading a pack
    /// here would duplicate what `packfile.rs` already does for git repositories it did not write.
    pub fn all_objects(&self) -> ZResult<Vec<(String, u8, Vec<u8>)>> {
        let mut out = Vec::new();
        let objects = self.git_dir.join("objects");
        let dirs = match std::fs::read_dir(&objects) {
            Ok(d) => d,
            Err(_) => return Ok(out),
        };
        for entry in dirs.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // `info` and `pack` sit next to the two-character object folders.
            if name.len() != 2 {
                continue;
            }
            let inner = match std::fs::read_dir(entry.path()) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for f in inner.flatten() {
                let rest = f.file_name().to_string_lossy().to_string();
                let sha = format!("{}{}", name, rest);
                if let Ok(obj) = self.read_object(&sha) {
                    out.push((sha, kind_code(&obj.kind), obj.body));
                }
            }
        }
        Ok(out)
    }
}

fn kind_code(k: &Kind) -> u8 {
    match k {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SHA-1 of "abc" is published, and a hashing mistake here would name every object
    /// wrongly while looking fine.
    #[test]
    fn the_sha1_matches_the_published_vectors() {
        assert_eq!(sha1(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    /// An empty blob's name is the one every git repository has, so it is a real cross check
    /// against another implementation rather than against this one.
    #[test]
    fn an_empty_blob_has_the_name_git_gives_it() {
        assert_eq!(
            object_hash(Kind::Blob, b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn a_blob_round_trips_through_a_repository() {
        let dir = std::env::temp_dir().join(format!("gesta-git-blob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let repo = Repo::open(&dir).unwrap();

        let hash = repo.write_object(Kind::Blob, b"hello\n").unwrap();
        assert_eq!(hash, "ce013625030ba8dba906f756967f9e9ca394464a");
        let back = repo.read_object(&hash).unwrap();
        assert_eq!(back.kind, Kind::Blob);
        assert_eq!(back.body, b"hello\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_the_same_object_twice_keeps_one_file() {
        let dir = std::env::temp_dir().join(format!("gesta-git-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let repo = Repo::open(&dir).unwrap();
        let a = repo.write_object(Kind::Blob, b"same\n").unwrap();
        let b = repo.write_object(Kind::Blob, b"same\n").unwrap();
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tree_round_trips_and_keeps_its_order() {
        let entries = vec![
            Entry {
                mode: "100644".into(),
                name: "b.txt".into(),
                hash: "ce013625030ba8dba906f756967f9e9ca394464a".into(),
            },
            Entry {
                mode: "40000".into(),
                name: "a".into(),
                hash: "ce013625030ba8dba906f756967f9e9ca394464a".into(),
            },
        ];
        let body = build_tree(entries);
        let back = parse_tree(&body).unwrap();
        assert_eq!(back.len(), 2);
        // A directory named "a" sorts before a file named "b".
        assert_eq!(back[0].name, "a");
        assert_eq!(back[0].mode, "40000");
        assert_eq!(back[1].name, "b.txt");
    }

    #[test]
    fn a_commit_keeps_its_tree_parents_and_author() {
        let tree = "ce013625030ba8dba906f756967f9e9ca394464a";
        let body = build_commit(
            tree,
            &["0123456789012345678901234567890123456789".to_string()],
            "Agent",
            "agent@example.com",
            1700000000,
            "+0700",
            "a message\n",
        );
        let c = parse_commit(&body).unwrap();
        assert_eq!(c.tree, tree);
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author_time, 1700000000);
        assert_eq!(c.timezone, "+0700");
        assert_eq!(c.message, "a message\n");
        let (name, mail) = author_parts(&c.author);
        assert_eq!(name, "Agent");
        assert_eq!(mail, "agent@example.com");
    }

    #[test]
    fn a_commit_with_no_parent_still_parses() {
        let body = build_commit(
            "ce013625030ba8dba906f756967f9e9ca394464a",
            &[],
            "A",
            "a@b.c",
            1,
            "+0000",
            "root\n",
        );
        let c = parse_commit(&body).unwrap();
        assert!(c.parents.is_empty());
        assert_eq!(c.message, "root\n");
    }

    #[test]
    fn a_missing_object_says_why_rather_than_pretending() {
        let dir = std::env::temp_dir().join(format!("gesta-git-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let repo = Repo::open(&dir).unwrap();
        let err = repo
            .read_object("0000000000000000000000000000000000000000")
            .unwrap_err()
            .to_string();
        assert!(err.contains("packfile"), "unhelpful message: {}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
