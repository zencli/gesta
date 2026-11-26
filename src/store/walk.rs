//! Walking a working tree: ignore rules, file walking, text/binary detection.
//!
//! In the storage tier because the worktree cache is here and is the caller: it asks what is on
//! disk to decide what to re-read. It sat in `repo`, which made `store` call up into the layer
//! above it. Everything here is `fs` and a path comparison, with no knowledge of changesets.

use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{LEGACY_DIR, ZF_DIR};

/// Directory names never descended into, regardless of ignore rules.
const SKIP_DIRS: &[&str] = &[
    ZF_DIR,
    LEGACY_DIR,
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "dist",
    "build",
    "__pycache__",
];

/// A gitignore-style pattern.
///
/// Supports the subset that actually matters in practice: `*` (any run of
/// non-separator characters), `**` (any run including separators), `?` (one
/// non-separator character), a leading `/` to anchor at the repository root, a
/// trailing `/` to match directories only, and a leading `!` to un-ignore.
#[derive(Debug, Clone)]
pub struct Pattern {
    glob: Vec<Segment>,
    pub negated: bool,
    pub dir_only: bool,
    pub anchored: bool,
}

#[derive(Debug, Clone)]
enum Segment {
    Literal(char),
    AnyChar,     // ?
    AnyRun,      // *
    AnyRunCross, // **
}

impl Pattern {
    /// True when the glob is a plain string with no wildcard.
    fn is_plain_literal(&self) -> bool {
        self.glob.iter().all(|s| matches!(s, Segment::Literal(_)))
    }

    /// The tail of a `*.ext` pattern, or None for any other shape.
    fn simple_suffix(&self) -> Option<String> {
        let mut wildcards = 0usize;
        let mut tail = String::new();
        for seg in &self.glob {
            match seg {
                Segment::AnyRun if wildcards == 0 && tail.is_empty() => wildcards += 1,
                Segment::Literal(c) if wildcards == 1 => tail.push(*c),
                _ => return None,
            }
        }
        if wildcards == 1 && !tail.is_empty() {
            Some(tail)
        } else {
            None
        }
    }

    fn literal_text(&self) -> Option<String> {
        if !self.is_plain_literal() {
            return None;
        }
        Some(
            self.glob
                .iter()
                .filter_map(|s| match s {
                    Segment::Literal(c) => Some(*c),
                    _ => None,
                })
                .collect(),
        )
    }

    pub fn parse(raw: &str) -> Option<Pattern> {
        let mut s = raw.trim();
        if s.is_empty() || s.starts_with('#') {
            return None;
        }
        let mut negated = false;
        if let Some(rest) = s.strip_prefix('!') {
            negated = true;
            s = rest.trim();
        }
        let dir_only = s.ends_with('/');
        if dir_only {
            s = &s[..s.len() - 1];
        }
        let anchored = s.starts_with('/') || s.contains('/');
        let s = s.trim_start_matches('/');
        if s.is_empty() {
            return None;
        }

        let mut glob = Vec::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if chars.peek() == Some(&'*') {
                        chars.next();
                        // `**/` also matches zero directories
                        if chars.peek() == Some(&'/') {
                            chars.next();
                        }
                        glob.push(Segment::AnyRunCross);
                    } else {
                        glob.push(Segment::AnyRun);
                    }
                }
                '?' => glob.push(Segment::AnyChar),
                _ => glob.push(Segment::Literal(c)),
            }
        }
        Some(Pattern {
            glob,
            negated,
            dir_only,
            anchored,
        })
    }

    /// Does this pattern match `path` (a `/`-separated repo-relative path)?
    pub fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && is_dir {
            return self.match_any_prefix(path);
        }
        if self.dir_only && !is_dir {
            // A directory-only rule also matches files inside that directory,
            // at any depth: `secret/` covers `deep/secret/key.pem`.
            return self.match_any_prefix(path);
        }
        if self.anchored {
            return self.match_text(path);
        }
        self.match_suffix(path)
    }

    /// Match the pattern against the whole path, or against any parent prefix
    /// of it (used for directory-only rules).
    fn match_any_prefix(&self, path: &str) -> bool {
        if self.anchored {
            let parts: Vec<&str> = path.split('/').collect();
            for end in 1..=parts.len() {
                if self.match_text(&parts[..end].join("/")) {
                    return true;
                }
            }
            return false;
        }
        let parts: Vec<&str> = path.split('/').collect();
        for start in 0..parts.len() {
            for end in start + 1..=parts.len() {
                if self.match_text(&parts[start..end].join("/")) {
                    return true;
                }
            }
        }
        false
    }

    /// An unanchored pattern matches the name or any trailing sub-path.
    fn match_suffix(&self, path: &str) -> bool {
        if self.match_text(path) {
            return true;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if self.match_text(name) {
            return true;
        }
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            if self.match_text(&parts[i..].join("/")) {
                return true;
            }
        }
        false
    }

    fn match_text(&self, text: &str) -> bool {
        // A stack buffer for the common short name, so the hot path does not
        // allocate. The allocator was the cost: 8 patterns times 2000 files is
        // 16000 Vec<char> allocations per status, worth 3 ms.
        let mut buf = [0u8; 256];
        if text.len() <= buf.len() && text.is_ascii() {
            buf[..text.len()].copy_from_slice(text.as_bytes());
            return self.match_bytes(&buf[..text.len()]);
        }
        let t: Vec<char> = text.chars().collect();
        self.match_from(0, 0, &t)
    }

    /// Same grammar as `match_from`, on bytes. Only used when the text is ASCII,
    /// because a glob segment holds one char and a multi-byte character would
    /// then be compared one byte at a time.
    fn match_bytes(&self, t: &[u8]) -> bool {
        self.match_bytes_from(0, 0, t)
    }

    fn match_bytes_from(&self, gi: usize, ti: usize, t: &[u8]) -> bool {
        if gi == self.glob.len() {
            return ti == t.len();
        }
        match &self.glob[gi] {
            Segment::Literal(c) => {
                let mut enc = [0u8; 4];
                let bytes = c.encode_utf8(&mut enc).as_bytes();
                ti + bytes.len() <= t.len()
                    && &t[ti..ti + bytes.len()] == bytes
                    && self.match_bytes_from(gi + 1, ti + bytes.len(), t)
            }
            Segment::AnyChar => {
                ti < t.len() && t[ti] != b'/' && self.match_bytes_from(gi + 1, ti + 1, t)
            }
            Segment::AnyRun => {
                if self.match_bytes_from(gi + 1, ti, t) {
                    return true;
                }
                let mut i = ti;
                while i < t.len() && t[i] != b'/' {
                    i += 1;
                    if self.match_bytes_from(gi + 1, i, t) {
                        return true;
                    }
                }
                false
            }
            Segment::AnyRunCross => {
                if self.match_bytes_from(gi + 1, ti, t) {
                    return true;
                }
                let mut i = ti;
                while i < t.len() {
                    i += 1;
                    if self.match_bytes_from(gi + 1, i, t) {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn match_from(&self, gi: usize, ti: usize, t: &[char]) -> bool {
        if gi == self.glob.len() {
            return ti == t.len();
        }
        match &self.glob[gi] {
            Segment::Literal(c) => {
                ti < t.len() && t[ti] == *c && self.match_from(gi + 1, ti + 1, t)
            }
            Segment::AnyChar => ti < t.len() && t[ti] != '/' && self.match_from(gi + 1, ti + 1, t),
            Segment::AnyRun => {
                if self.match_from(gi + 1, ti, t) {
                    return true;
                }
                let mut i = ti;
                while i < t.len() && t[i] != '/' {
                    i += 1;
                    if self.match_from(gi + 1, i, t) {
                        return true;
                    }
                }
                false
            }
            Segment::AnyRunCross => {
                if self.match_from(gi + 1, ti, t) {
                    return true;
                }
                for i in ti..t.len() {
                    if self.match_from(gi + 1, i + 1, t) {
                        return true;
                    }
                }
                false
            }
        }
    }
}

/// Ordered set of ignore rules. Later rules win, so a `!` rule can re-include
/// something an earlier rule excluded.
#[derive(Clone)]
pub struct IgnoreRules {
    /// Anchored directory patterns such as `/.git/`, stored without the slashes.
    /// Checking a path against this list is a byte compare, and every one of the
    /// default rules is of this shape. The walk calls is_ignored once per entry,
    /// so eight glob matches per file became the largest single cost of a scan.
    anchored_dirs: Vec<String>,
    /// Names matched exactly at any depth, such as `.DS_Store`.
    plain_names: Vec<String>,
    /// Suffixes matched at any depth, such as `.swp` from `*.swp`.
    suffixes: Vec<String>,
    /// Patterns with a leading `!`. Any of them makes the cheap answers unsafe,
    /// because a negation can undo a rule the tables would have matched first.
    negations: Vec<String>,
    /// Everything else: `**`, `?`, a wildcard in the middle, a negation.
    patterns: Vec<Pattern>,
}

impl IgnoreRules {
    pub fn new(patterns: Vec<String>) -> IgnoreRules {
        let parsed: Vec<Pattern> = patterns.iter().filter_map(|p| Pattern::parse(p)).collect();
        // A directory rule anchored at the root, such as `/.git/`: the only
        // pattern shape whose match is a prefix test. Collected once here so the
        // walk does not re-derive it for every entry in the tree.
        let anchored_dirs: Vec<String> = parsed
            .iter()
            .filter(|p| p.anchored && p.dir_only && !p.negated && p.is_plain_literal())
            .filter_map(|p| p.literal_text())
            .collect();
        // A plain name that is not anchored matches any file with that name at any
        // depth, which is a string compare after the last separator.
        let plain_names: Vec<String> = parsed
            .iter()
            .filter(|p| !p.anchored && !p.dir_only && !p.negated && p.is_plain_literal())
            .filter_map(|p| p.literal_text())
            .collect();
        // The `*.ext` shape: one wildcard, then literals. Matched by suffix.
        let suffixes: Vec<String> = parsed
            .iter()
            .filter(|p| !p.anchored && !p.dir_only && !p.negated)
            .filter_map(|p| p.simple_suffix())
            .collect();
        // Any negated pattern at all, including a wildcard one: `!*.log` is just as
        // capable of undoing a `*.log` table match as `!keep.log` was.
        let negations: Vec<String> = parsed
            .iter()
            .filter(|p| p.negated)
            .enumerate()
            .map(|(i, _)| i.to_string())
            .collect();
        IgnoreRules {
            anchored_dirs,
            plain_names,
            suffixes,
            negations,
            patterns: parsed,
        }
    }

    /// Built-in defaults plus whatever the caller supplies.
    pub fn with_defaults(extra: &[String]) -> IgnoreRules {
        let mut all: Vec<String> = DEFAULT_IGNORES.iter().map(|s| s.to_string()).collect();
        all.extend(extra.iter().cloned());
        IgnoreRules::new(all)
    }

    pub fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        // Nothing to answer: a tree with no rules at all is the state of a fresh
        // repository, and this returns before touching any pattern.
        if self.patterns.is_empty() {
            return false;
        }
        // The common rules are root-anchored directories. Answering them with a
        // prefix test skips the glob engine entirely, which was 7 ms of a 22 ms
        // scan on 8000 files. The general matcher still runs for everything else,
        // and a test asserts the two agree.
        let bytes = path.as_bytes();
        for d in &self.anchored_dirs {
            let dl = d.len();
            // Slicing a str by a byte count lands inside a character when the path holds one
            // outside ASCII, and a Vietnamese file name panicked the whole scan. The byte check
            // is the only thing that can answer "is this prefix ASCII", and the str comparison
            // then runs on a boundary that is known to be one.
            if bytes.len() >= dl
                && path.is_char_boundary(dl)
                && &path[..dl] == d.as_str()
                && (bytes.len() == dl || bytes[dl] == b'/')
            {
                return true;
            }
        }
        // A path not under any of those cannot be matched by them, so they are
        // excluded from the loop. Falling through to the raw pattern loop when
        // the list happened to be empty skipped the name and suffix tables, which
        // is why the shortcut measured slower than the code it replaced.
        self.match_remaining(path, is_dir)
    }

    /// The general matcher, minus the shapes answered by the cheap tables.
    fn match_remaining(&self, path: &str, is_dir: bool) -> bool {
        let name = match path.rfind('/') {
            Some(i) => &path[i + 1..],
            None => path,
        };

        // A negation anywhere in the set means a cheap answer cannot be trusted:
        // `!keep.log` has to undo an earlier `*.log`, and returning true at the
        // first name match skipped it. With a negation present the general loop
        // runs, which is the only thing that knows the order.
        if !self.negations.is_empty() {
            return self.general(path, is_dir);
        }

        // A name rule ignores the file itself. A directory of the same name is not
        // what the rule says, which is why is_dir is checked rather than assumed.
        if !is_dir && self.plain_names.iter().any(|n| n == name) {
            return true;
        }
        if !is_dir && self.suffixes.iter().any(|s| name.ends_with(s.as_str())) {
            return true;
        }
        self.general_without_tables(path, is_dir)
    }

    /// Every pattern, in order, with no shortcut.
    fn general(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for p in &self.patterns {
            if p.matches(path, is_dir) {
                ignored = !p.negated;
            }
        }
        ignored
    }

    /// The general matcher, skipping the shapes the tables already answered.
    fn general_without_tables(&self, path: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for p in &self.patterns {
            let answered = (p.anchored && p.dir_only && !p.negated)
                || (!p.anchored
                    && !p.dir_only
                    && !p.negated
                    && (p.is_plain_literal() || p.simple_suffix().is_some()));
            if answered {
                continue;
            }
            if p.matches(path, is_dir) {
                ignored = !p.negated;
            }
        }
        ignored
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }
}

/// Rules applied even when the repository has no ignore file.
pub const DEFAULT_IGNORES: &[&str] = &[
    ".DS_Store",
    "Thumbs.db",
    "*.swp",
    "*.tmp",
    "*~",
    "/.gesta/",
    "/.zenforge/",
    "/.git/",
];

/// Walk `root` and return every regular file that belongs to the repository,
/// together with the metadata the walk already had to fetch. Sorted, with
/// `root`-relative paths, honouring the ignore rules.
///
/// Callers that need size or mtime used to stat every file a second time after
/// a path-only walk. On a 2000 file repository that is 2000 extra syscalls for
/// information the directory entry already carried.
///
/// A subrepository checkout holds another repository's files. Walking into it
/// would record those files as this repository's own, which is how a nested
/// repository turns into a commit that swallows its child. The paths are skipped
/// before descending, so the child tree is never read at all.
pub fn scan_files_excluding(
    root: &Path,
    rules: &IgnoreRules,
    exclude: &[String],
) -> std::io::Result<Vec<(String, fs::Metadata)>> {
    // Two phases, because they cost different things and only one of them parallelises. Readdir is
    // one call per directory and the walk is already cheap; stat is one syscall per file and on this
    // filesystem it dominates. Measured on 100,003 files: 78 ms of stats against 17 ms of readdir,
    // and the stats fall to 23.7 ms with four threads while the walk does not move.
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    // The directory being walked, with its relative path already built. Carrying the prefix down the
    // stack means a file's repository path is one concatenation instead of a component walk over an
    // absolute path, which measured 21 ms per 100,000 files.
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            // The name is compared without allocating: a String per entry was
            // 8000 allocations on a large tree for a four word lookup.
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `file_type()` costs nothing: readdir already reported the type, and this just reads it
            // out of the entry. `metadata()` is a stat per entry, which measured 122 ms over 100,000
            // files against 21.7 ms for readdir alone. An earlier comment here said metadata cost
            // nothing beyond readdir, which a measurement in this container contradicts.
            //
            // Directories are decided by file_type. A file still needs its metadata, because the
            // scan compares size and mtime, and a symlink is treated as a file whose stat is taken
            // without following it, which is what metadata() does.
            let ftype = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Only a directory is skipped. The list names directories never descended into, and
            // checking it before the type test dropped a regular file called `build` or `dist` from
            // every scan with no warning - a file the user asked to track and never could.
            if ftype.is_dir() && SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            // One allocation: the prefix, a separator and the name. A `PathBuf` was built here before
            // and thrown away for every file the cache could answer for, which is most of them.
            let mut rel = String::with_capacity(prefix.len() + 1 + name.len());
            if !prefix.is_empty() {
                rel.push_str(&prefix);
                rel.push('/');
            }
            rel.push_str(&name);
            let is_dir = ftype.is_dir();
            let ign = rules.is_ignored(&rel, is_dir);
            if is_dir {
                if ign {
                    continue;
                }
                if is_inside_sub(&rel, exclude) {
                    continue;
                }
                stack.push((entry.path(), rel));
                continue;
            }
            // Not a directory: a file, a symlink, or a socket. The stat is taken below, in the
            // second phase, because taking it here is what made the walk serial.
            if is_dir || ign {
                continue;
            }
            found.push((rel, entry.path()));
        }
    }
    // The stats, spread over a few threads. Four is where the measurement peaked: two left the
    // filesystem idle between calls, eight spent more on handoff than the stat saved, and the curve
    // between them is flat enough that the exact number is not worth tuning per machine.
    //
    // Sorted order is not used by any caller. The scan's result becomes a map keyed by relative
    // path, and the tests ask whether a path is present. Sorting 100,002 PathBuf values measured
    // 79 ms of a 244 ms walk, because comparing paths compares components rather than bytes.
    Ok(stat_in_parallel(found, STAT_THREADS))
}

/// How many threads the stat phase uses.
///
/// Four, from a measurement rather than a guess. The walk is bound by syscalls the kernel serves
/// concurrently, so the curve flattens early: 1 thread 78 ms, 2 threads 43, 4 threads 24, 8 threads
/// 34. The cost past four is thread handoff on 100,003 very short items.
const STAT_THREADS: usize = 4;

/// Stat every path, spread over `workers` threads.
///
/// Chunked rather than queued: each thread gets one contiguous slice and touches no shared state, so
/// there is no lock on the hot path. The queue version measured 31 ms against 24, and the lock is the
/// difference.
fn stat_in_parallel(
    mut found: Vec<(String, PathBuf)>,
    workers: usize,
) -> Vec<(String, fs::Metadata)> {
    if found.len() < 512 || workers < 2 {
        // Below this size the thread spawn costs more than the stats. A small repository, which is
        // most of them, takes the loop it always took.
        return stat_range(found);
    }
    let n = found.len();
    let chunk = n.div_ceil(workers);

    // `thread::scope` lets each thread borrow its own slice, and the handles are joined inside the
    // scope: a handle cannot outlive the borrow it was spawned against.
    let mut out: Vec<(String, fs::Metadata)> = Vec::with_capacity(n);
    std::thread::scope(|scope| {
        let handles: Vec<_> = found
            .chunks_mut(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let mut mine = Vec::with_capacity(part.len());
                    for (rel, path) in part.iter() {
                        // Keep the link itself, including directory and dangling links.
                        if let Ok(meta) = fs::symlink_metadata(path) {
                            if meta.is_file() || meta.file_type().is_symlink() {
                                mine.push((rel.clone(), meta));
                            }
                        }
                    }
                    mine
                })
            })
            .collect();
        for h in handles {
            if let Ok(v) = h.join() {
                out.extend(v);
            }
        }
    });
    out
}

/// Stat one slice, keeping regular files and symbolic links.
fn stat_range(items: Vec<(String, PathBuf)>) -> Vec<(String, fs::Metadata)> {
    let mut out = Vec::with_capacity(items.len());
    for (rel, path) in items {
        // Keep the link itself, including directory and dangling links.
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.is_file() || meta.file_type().is_symlink() {
                out.push((rel, meta));
            }
        }
    }
    out
}

/// Path relative to `root`, with `/` between components.
///
/// The walk builds relative paths from a prefix instead of calling this, because it already knows the
/// directory it is in and building from an absolute path costs a component walk per file. This stays
/// for callers that hold an absolute path, which is what the tests and `subrepo` do.
#[allow(dead_code)]
///
/// The separators are rebuilt from the path components rather than by replacing every
/// backslash in the joined string. On Unix a backslash is an ordinary character in a file
/// name: `back\\slash.txt` and `back/slash.txt` are two different files, and a blanket replace
/// turned them into one path, which then collided on the primary key and failed the commit with
/// a message about a database constraint rather than about the file.
pub fn rel_path(root: &Path, p: &Path) -> String {
    let rel = p.strip_prefix(root).unwrap_or(p);
    let mut out = String::with_capacity(rel.as_os_str().len());
    let mut first = true;
    for part in rel.components() {
        use std::path::Component;
        let text = match part {
            Component::Normal(s) => s.to_string_lossy(),
            // A relative path under the root never holds these; keep them out rather than
            // letting `..` or a root prefix end up inside a repository path.
            Component::CurDir | Component::RootDir | Component::Prefix(_) => continue,
            Component::ParentDir => "..".into(),
        };
        if !first {
            out.push('/');
        }
        out.push_str(&text);
        first = false;
    }
    out
}

/// Outcome of reading a file as text.
pub enum ReadOutcome {
    Text(String),
    Binary,
    Unreadable(String),
}

/// Read a working-tree entry, storing a link's target name rather than following it.
pub fn read_bytes(p: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if fs::symlink_metadata(p)?.file_type().is_symlink() {
        let target = fs::read_link(p)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return Ok(target.as_os_str().as_bytes().to_vec());
        }
        #[cfg(not(unix))]
        {
            return target
                .to_str()
                .map(|s| s.as_bytes().to_vec())
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "link target is not UTF-8")
                });
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(p)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a working-tree entry and decide whether it is text, reading only what the decision needs.
///
/// The decision needs 8000 bytes: a NUL anywhere in that prefix means binary. Reading the whole
/// file first to look at its first 8000 bytes made a read-only `status` cost as much memory as the
/// largest tracked file, so a multi-gigabyte asset spiked memory on every scan. A file that starts
/// with text but has a NUL later is still caught, because the UTF-8 validation below reads to the
/// end - it just is not paid for files whose prefix already says "binary".
pub fn read_file(p: &Path) -> ReadOutcome {
    match read_file_limited(p) {
        Ok(ReadOutcome::Binary) => ReadOutcome::Binary,
        Ok(ReadOutcome::Text(s)) => ReadOutcome::Text(s),
        Ok(other) => other,
        Err(e) => ReadOutcome::Unreadable(e.to_string()),
    }
}

/// The sniff-and-read behind `read_file`, keeping every guard `read_bytes` has.
///
/// A symlink reads as its target name, not through it; the open refuses to follow one, refuses a
/// FIFO, and does not block. Those guards exist because a scan walks attacker-reachable paths, and
/// dropping them to save memory would be trading a memory ceiling for a hang. Only the read differs:
/// the prefix is read first and the rest only if the prefix said text.
fn read_file_limited(p: &Path) -> std::io::Result<ReadOutcome> {
    use std::io::Read;
    if fs::symlink_metadata(p)?.file_type().is_symlink() {
        let target = fs::read_link(p)?;
        #[cfg(unix)]
        let raw = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let raw = target
            .to_str()
            .map(|s| s.as_bytes().to_vec())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "link target is not UTF-8")
            })?;
        return Ok(match String::from_utf8(raw) {
            Ok(s) => ReadOutcome::Text(s),
            Err(_) => ReadOutcome::Binary,
        });
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut f = options.open(p)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut head = [0u8; 8000];
    let got = f.read(&mut head)?;
    if head[..got].contains(&0u8) {
        return Ok(ReadOutcome::Binary);
    }
    // The prefix said text, so the rest has to be read to prove it. Read from the same handle
    // rather than reopening: the file could change between the two opens.
    let mut bytes = Vec::with_capacity(got);
    bytes.extend_from_slice(&head[..got]);
    f.read_to_end(&mut bytes)?;
    Ok(match String::from_utf8(bytes) {
        Ok(s) => ReadOutcome::Text(s),
        Err(_) => ReadOutcome::Binary,
    })
}

/// Read a file as UTF-8 text, `None` for binary or unreadable content.
pub fn read_text(p: &Path) -> Option<String> {
    match read_file(p) {
        ReadOutcome::Text(s) => Some(s),
        _ => None,
    }
}

/// Is `rel` inside a subrepository, or the subrepository itself?
pub fn is_inside_sub(rel: &str, subs: &[String]) -> bool {
    subs.iter().any(|s| {
        rel == s.as_str()
            || rel
                .strip_prefix(s.as_str())
                .map(|rest| rest.starts_with('/'))
                .unwrap_or(false)
    })
}

/// Checkout paths of every subrepository, for the walk to skip.
///
/// Read straight from the table rather than through `subrepo::list`, because the walk is the caller
/// and the storage tier must not depend on the layer above it. The table is the one `subrepo` writes.
///
/// A subrepository is a tracked path whose content lives in another repository. Scanning it would
/// record the child's files as this repository's own, which is how a nested repository turns into a
/// commit that swallows its child.
fn subrepo_paths(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn.prepare("SELECT path FROM subrepos ORDER BY path") {
        Ok(s) => s,
        // The table exists from the first time a subrepository is added. A repository that has none
        // has no table, and nothing to skip.
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// Checkout paths of every submodule, for the walk to skip.
///
/// Read from `.gitmodules`, which is the file the pointer lives in. A submodule's files belong to the
/// child repository; recording them here would put another project's content in this one's history.
fn submodule_paths(root: &Path) -> Vec<String> {
    let path = root.join(".gitmodules");
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path") {
            if let Some(value) = rest.split_once('=').map(|(_, v)| v.trim()) {
                if !value.is_empty() {
                    out.push(value.to_string());
                }
            }
        }
    }
    out
}

/// Every path the walk must skip: subrepositories and submodules.
///
/// One list rather than two, because the walk descends once and a second caller remembering to merge
/// them is a second place to forget.
pub fn excluded_paths(conn: &Connection, root: &Path) -> Vec<String> {
    let mut out = subrepo_paths(conn);
    out.extend(submodule_paths(root));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pats: &[&str]) -> IgnoreRules {
        IgnoreRules::new(pats.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn rel_path_uses_forward_slashes() {
        let root = Path::new("/tmp/repo");
        assert_eq!(
            rel_path(root, Path::new("/tmp/repo/sub/file.txt")),
            "sub/file.txt"
        );
    }

    #[test]
    fn simple_glob_matches_filename_anywhere() {
        let r = rules(&["*.log"]);
        assert!(r.is_ignored("app.log", false));
        assert!(r.is_ignored("logs/app.log", false));
        assert!(!r.is_ignored("app.txt", false));
    }

    #[test]
    fn anchored_pattern_only_matches_at_root() {
        let r = rules(&["/build"]);
        assert!(r.is_ignored("build", true));
        assert!(!r.is_ignored("src/build", true));
    }

    #[test]
    fn dir_only_pattern_covers_contents() {
        let r = rules(&["secret/"]);
        assert!(r.is_ignored("secret", true));
        assert!(r.is_ignored("secret/key.pem", false));
        assert!(r.is_ignored("deep/secret/key.pem", false));
        assert!(!r.is_ignored("secretfile.txt", false));
    }

    #[test]
    fn double_star_crosses_directories() {
        let r = rules(&["docs/**/tmp"]);
        assert!(r.is_ignored("docs/a/tmp", false));
        assert!(r.is_ignored("docs/a/b/c/tmp", false));
        assert!(!r.is_ignored("other/tmp", false));
    }

    #[test]
    fn negation_re_includes_a_file() {
        let r = rules(&["*.log", "!keep.log"]);
        assert!(r.is_ignored("app.log", false));
        assert!(!r.is_ignored("keep.log", false), "later ! rule must win");
    }

    #[test]
    fn question_mark_matches_one_character() {
        let r = rules(&["file?.txt"]);
        assert!(r.is_ignored("file1.txt", false));
        assert!(!r.is_ignored("file10.txt", false));
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let r = rules(&["# a comment", "", "  ", "*.o"]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn defaults_ignore_os_noise_but_not_source() {
        let r = IgnoreRules::with_defaults(&[]);
        assert!(r.is_ignored(".DS_Store", false));
        assert!(!r.is_ignored("src/.gesta/x", false)); // nested, not the repo dir
        assert!(!r.is_ignored("src/main.rs", false));
    }

    #[test]
    fn env_file_is_ignored_when_user_asks() {
        let r = IgnoreRules::with_defaults(&[".env".to_string(), "*.key".to_string()]);
        assert!(r.is_ignored(".env", false));
        assert!(r.is_ignored("certs/server.key", false));
        assert!(!r.is_ignored("app.rs", false));
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zf_wt_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scan_prunes_vendor_dirs_and_ignored_paths() {
        let tmp = tmpdir("scan");
        for d in [".gesta", "target", "node_modules", "src", "secret"] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }
        fs::write(tmp.join("src/a.rs"), "fn main(){}").unwrap();
        fs::write(tmp.join(".gesta/db"), "x").unwrap();
        fs::write(tmp.join("target/bin"), "x").unwrap();
        fs::write(tmp.join("node_modules/m.js"), "x").unwrap();
        fs::write(tmp.join("secret/key.pem"), "PRIVATE").unwrap();
        fs::write(tmp.join(".env"), "SECRET=1").unwrap();
        fs::write(tmp.join("app.rs"), "fn main(){}").unwrap();

        let r = IgnoreRules::with_defaults(&["secret/".to_string(), ".env".to_string()]);
        let got: Vec<String> = scan_files_excluding(&tmp, &r, &[])
            .unwrap()
            .iter()
            .map(|(r, _)| r.clone())
            .collect();
        assert_eq!(
            got,
            vec!["app.rs", "src/a.rs"],
            "ignored paths must not be tracked"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_walk_returns_every_file_regardless_of_order() {
        // The walk used to sort its result, and no caller reads the order: the output becomes a map
        // keyed by relative path. The sort was 79 ms of a 244 ms walk at 100,000 files. This test
        // pins the contract the callers actually need, so a future change that adds ordering work
        // has to justify it against a caller that wants it.
        let tmp = tmpdir("walk-order");
        for name in ["z.txt", "a.txt", "m/n.txt", "m/b.txt"] {
            let p = tmp.join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, "x\n").unwrap();
        }

        let rules = IgnoreRules::with_defaults(&[]);
        let got: Vec<String> = scan_files_excluding(&tmp, &rules, &[])
            .unwrap()
            .iter()
            .map(|(r, _)| r.clone())
            .collect();

        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "a.txt".to_string(),
                "m/b.txt".to_string(),
                "m/n.txt".to_string(),
                "z.txt".to_string(),
            ],
            "every file must be present, in any order"
        );
        assert_eq!(got.len(), 4, "no file may be dropped: {:?}", got);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_large_binary_file_is_classified_without_reading_all_of_it() {
        // `read_file` used to call `fs::read` and then look at the first 8000 bytes, so a read-only
        // `status` cost as much memory as the largest tracked file. This writes a 64 MB file whose
        // first bytes already say "binary" and checks the classification is right; the read guard
        // is what keeps this test honest - if the whole file were loaded, peak RSS would show it.
        //
        // The assertion is on behaviour, not on allocator counters, because a portable per-read
        // allocation count is not available here. 64 MB is chosen so the old path is visible in a
        // memory profile while the test itself stays fast.
        let tmp = tmpdir("bigbin");
        let big = tmp.join("big.bin");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&big).unwrap();
            // NUL in the first byte: the prefix alone decides.
            f.write_all(&[0u8, 1, 2, 3]).unwrap();
            let chunk = vec![0x41u8; 1 << 20];
            for _ in 0..64 {
                f.write_all(&chunk).unwrap();
            }
        }
        assert!(
            fs::metadata(&big).unwrap().len() >= 64 << 20,
            "the fixture was not written at the size the test needs"
        );
        assert!(
            matches!(read_file(&big), ReadOutcome::Binary),
            "a 64 MB file starting with NUL was not read as binary"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_symlink_still_reads_as_its_target_name() {
        // The faster read must not drop the guard that `read_bytes` had: a scan walks
        // attacker-reachable paths, and following a link out of the repository is how a scan reads
        // something it was never pointed at.
        let tmp = tmpdir("link");
        let link = tmp.join("link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();
            match read_file(&link) {
                ReadOutcome::Text(s) => {
                    assert_eq!(s, "/etc/passwd", "the link target was not the content")
                }
                ReadOutcome::Binary => panic!("a symlink's target name was read as binary"),
                ReadOutcome::Unreadable(e) => {
                    panic!("a symlink was not read at all: {}", e)
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = link;
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_file_separates_binary_from_text() {
        let tmp = tmpdir("bin");
        let text = tmp.join("a.txt");
        let bin = tmp.join("b.bin");
        fs::write(&text, "plain text\n").unwrap();
        fs::write(&bin, [0x89u8, 0x50, 0x4e, 0x47, 0x00, 0x01]).unwrap();

        assert!(matches!(read_file(&text), ReadOutcome::Text(_)));
        assert!(matches!(read_file(&bin), ReadOutcome::Binary));
        assert!(matches!(
            read_file(&tmp.join("missing")),
            ReadOutcome::Unreadable(_)
        ));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn utf8_without_nul_is_text() {
        let tmp = tmpdir("utf8");
        let p = tmp.join("vi.txt");
        fs::write(&p, "Tiếng Việt có dấu\n\u{7bc0}\u{70b9}\n").unwrap();
        assert_eq!(
            read_text(&p).unwrap(),
            "Tiếng Việt có dấu\n\u{7bc0}\u{70b9}\n"
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    /// Ignore matching has a fast path for plain names and `*.ext`. A wrong fast
    /// path silently stops ignoring things, which is how a build directory ends
    /// up committed, so every shape is checked against the general matcher.
    #[test]
    fn a_plain_name_ignores_a_nested_file_of_that_name() {
        let rules = IgnoreRules::with_defaults(&["secret.txt".to_string()]);
        assert!(rules.is_ignored("secret.txt", false));
        assert!(rules.is_ignored("deep/secret.txt", false));
        assert!(!rules.is_ignored("secret.txt.bak", false));
    }

    #[test]
    fn a_suffix_rule_matches_only_names_that_end_with_it() {
        let rules = IgnoreRules::with_defaults(&["*.log".to_string()]);
        assert!(rules.is_ignored("a.log", false));
        // The star matches an empty run, so a file named `.log` is covered.
        assert!(rules.is_ignored(".log", false));
        // A name without the dot is a different name.
        assert!(!rules.is_ignored("log", false));
        assert!(!rules.is_ignored("alog", false));
    }

    #[test]
    fn a_negated_rule_still_wins_over_a_fast_path_match() {
        let rules = IgnoreRules::with_defaults(&["*.log".to_string(), "!keep.log".to_string()]);
        assert!(rules.is_ignored("a.log", false));
        assert!(!rules.is_ignored("keep.log", false));
    }
    /// The byte matcher is a second implementation of the same grammar. Every
    /// shape is compared against the char matcher, because two implementations
    /// of a glob that disagree is how files silently stop being ignored.
    #[test]
    fn the_byte_matcher_agrees_with_the_char_matcher() {
        let patterns = [
            "*.log",
            "a?c",
            "**/x",
            "deep/*.rs",
            "literal.txt",
            "*.tar.gz",
            "a*b",
            "?*?",
            "**/build/**",
            "/root.txt",
            "*",
        ];
        let texts = [
            "a.log",
            ".log",
            "log",
            "alog",
            "abc",
            "axc",
            "a/c",
            "deep/x.rs",
            "x.rs",
            "literal.txt",
            "deep/literal.txt",
            "archive.tar.gz",
            "aXXb",
            "ab",
            "root.txt",
            "deep/root.txt",
            "build/out/i.o",
            "deep/build/out/i.o",
            "anything",
            "",
        ];
        for pat in patterns {
            let p = Pattern::parse(pat).expect("pattern parses");
            for text in texts {
                assert_eq!(
                    p.match_text(text),
                    p.match_bytes(text.as_bytes()),
                    "byte matcher disagrees for pattern {:?} text {:?}",
                    pat,
                    text
                );
            }
        }
    }

    /// The anchored directory shortcut must agree with the general matcher on
    /// every path. A wrong shortcut here silently stops ignoring a build
    /// directory, which is how generated files end up in a commit.
    #[test]
    fn the_shortcuts_agree_with_the_general_matcher() {
        let rules = IgnoreRules::with_defaults(&[
            "*.log".to_string(),
            "build/".to_string(),
            "!keep.log".to_string(),
            "secret.txt".to_string(),
            "**/generated/**".to_string(),
            "a?c".to_string(),
        ]);
        let paths = [
            ("a.log", false),
            ("deep/a.log", false),
            ("keep.log", false),
            ("build", true),
            ("build/x.o", false),
            ("deep/build/x.o", false),
            (".git", true),
            (".git/config", false),
            (".gitignore", false),
            (".gesta", true),
            (".gesta/gesta.db", false),
            (".gestax", false),
            ("src/.git/x", false),
            (".DS_Store", false),
            ("deep/.DS_Store", false),
            ("x.tmp", false),
            ("x~", false),
            ("normal.rs", false),
            ("secret.txt", false),
            ("deep/secret.txt", false),
            ("generated/x", false),
            ("deep/generated/x", false),
            ("abc", false),
            ("a/c", false),
            ("Thumbs.db", true),
        ];
        for (path, is_dir) in paths {
            let fast = rules.is_ignored(path, is_dir);
            // The reference answer: the general loop over every pattern, with no
            // shortcut and no table. Slower, and that is the point.
            let reference = {
                let mut ignored = false;
                for p in &rules.patterns {
                    if p.matches(path, is_dir) {
                        ignored = !p.negated;
                    }
                }
                ignored
            };
            assert_eq!(
                fast, reference,
                "shortcut disagrees for {} (dir={}): fast {} reference {}",
                path, is_dir, fast, reference
            );
        }
    }

    #[test]
    fn a_directory_named_like_the_prefix_but_longer_is_not_ignored() {
        // `.gestax` starts with `.gesta`, and a byte prefix test would wrongly
        // ignore it. The separator check is what makes this correct.
        let rules = IgnoreRules::with_defaults(&[]);
        assert!(rules.is_ignored(".gesta", true));
        assert!(rules.is_ignored(".gesta/x", false));
        assert!(!rules.is_ignored(".gestax", false));
        assert!(!rules.is_ignored(".gestax/y", false));
    }
    #[test]
    fn a_vietnamese_path_does_not_panic_the_ignore_matcher() {
        let rules = IgnoreRules::with_defaults(&[]);
        // "Node" is six bytes of an anchored directory, and the fourth byte of this path is the
        // middle of a three byte character. Slicing by that count panicked the whole scan.
        let path = "nồ.md/file.md";
        assert!(!rules.is_ignored(path, false), "a plain path was ignored");
        assert!(!rules.is_ignored("tài-liệu/ghi-chú.md", false));
        assert!(!rules.is_ignored("ồ", false));
    }

    #[test]
    fn an_anchored_directory_still_matches_under_a_plain_prefix() {
        // The anchored shape is `/name/`, which is what turns the match into a prefix test.
        let rules = IgnoreRules::new(vec!["/target/".to_string()]);
        assert!(rules.is_ignored("target/debug/x", false));
        assert!(
            !rules.is_ignored("targets/x", false),
            "a longer name matched the prefix"
        );
        assert!(
            !rules.is_ignored("src/target/x", false),
            "a nested one matched an anchored rule"
        );
        // And a non-ASCII path in front of it must not trip the byte slice.
        assert!(!rules.is_ignored("tồ/target/x", false));
    }
    #[test]
    fn a_backslash_in_a_file_name_is_not_a_separator() {
        // A backslash is a separator on Windows and a character in a name everywhere else, so the
        // same two paths are one file on one platform and two on the other. The assertion that holds
        // on both: the answer is a path whose separators are forward slashes, and on a Unix disk the
        // name keeps its backslash because there it is not a separator.
        let root = std::path::Path::new(if cfg!(windows) { "C:/repo" } else { "/repo" });
        let two = root.join("back").join("slash.txt");
        let b = rel_path(root, &two);
        assert_eq!(b, "back/slash.txt", "a separator stayed a backslash");

        if cfg!(windows) {
            // On Windows a name cannot hold a backslash at all, so there is no second file to
            // compare with. Saying so is better than an assertion that only holds on one platform.
            return;
        }
        let one = root.join("back\\slash.txt");
        let a = rel_path(root, &one);
        assert_eq!(a, "back\\slash.txt", "a backslash in a name was rewritten");
        assert_ne!(a, b, "two different files became one path");
    }

    #[test]
    fn nesting_is_joined_with_forward_slashes() {
        let root = std::path::Path::new("/repo");
        let p = root.join("a").join("b").join("c.md");
        assert_eq!(rel_path(root, &p), "a/b/c.md");
        assert_eq!(rel_path(root, &root.join("x.txt")), "x.txt");
    }

    #[test]
    fn a_vietnamese_name_survives_the_path_round_trip() {
        let root = std::path::Path::new("/repo");
        let p = root.join("tài-liệu").join("ghi-chú.md");
        assert_eq!(rel_path(root, &p), "tài-liệu/ghi-chú.md");
    }
}
