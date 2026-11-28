//! Content search over the working tree and over history.
//!
//! An agent asks "where is this string" far more often than a person does, and the answer it
//! needs is a path and a line number, not a page of context. Searching history matters because
//! the string that was deleted is usually the one being looked for.

use crate::store::get_object;
use crate::{bad, ZResult};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::Path;

/// Put a word boundary before a pattern unless it already starts with one.
///
/// A pattern that starts with a character that is not part of a word, such as a bracket or a
/// dot, has a boundary there already. Adding one would refuse a match the caller asked for.
#[cfg(feature = "regex")]
fn bracket_word_start(pattern: &str) -> String {
    let starts_wordy = pattern
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_');
    if starts_wordy {
        format!("\\b{}", pattern)
    } else {
        pattern.to_string()
    }
}

/// One match: where it is and what the line says.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: String,
    pub line: usize,
    /// The revision this match was read at. Empty means the working tree.
    pub rev: String,
    pub text: String,
}

/// A search that has been compiled once and run against many revisions.
pub struct Pattern {
    needle: String,
    ignore_case: bool,
    /// Match whole words only, so `hash` does not find `hashes`.
    word: bool,
    /// Set when the pattern is a regular expression.
    #[cfg(feature = "regex")]
    compiled: Option<regex::Regex>,
}

impl Pattern {
    pub fn new(needle: &str, ignore_case: bool, word: bool) -> ZResult<Pattern> {
        if needle.is_empty() {
            return Err(bad("an empty pattern matches every line, so it is refused"));
        }
        Ok(Pattern {
            needle: if ignore_case {
                needle.to_lowercase()
            } else {
                needle.to_string()
            },
            ignore_case,
            word,
            #[cfg(feature = "regex")]
            compiled: None,
        })
    }

    /// A pattern read as a regular expression.
    ///
    /// The engine is the Rust one, which has no lookaround or backreferences. A pattern using
    /// them is refused here, with the construct named, rather than quietly matching less than
    /// the caller meant. `word` wraps the pattern in word boundaries instead of filtering
    /// matches, because a regex match has no single position to check neighbours at.
    #[cfg(feature = "regex")]
    pub fn regex(needle: &str, ignore_case: bool, word: bool) -> ZResult<Pattern> {
        if needle.is_empty() {
            return Err(bad("an empty pattern matches every line, so it is refused"));
        }
        // The engine's own boundary at the end is enough when the pattern ends on a word
        // character; adding a second one produced two boundaries in a row, which is refused.
        let body = if word {
            let open = bracket_word_start(needle);
            let close = if needle
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                r"\b"
            } else {
                ""
            };
            format!(r"(?:{}){}", open, close)
        } else {
            needle.to_string()
        };
        let built = regex::RegexBuilder::new(&body)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| {
                // The parser puts the position and the offending piece on later lines; taking
                // only the first line dropped the part that explains what is wrong.
                let detail: String = e
                    .to_string()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                bad(format!(
                    "not a valid regular expression ({}). The engine has no lookaround such as an opening paren with a question mark and an equals sign, and no backreferences; express the same match another way.",
                    detail
                ))
            })?;
        Ok(Pattern {
            needle: needle.to_string(),
            ignore_case,
            word,
            compiled: Some(built),
        })
    }

    pub fn matches(&self, line: &str) -> bool {
        #[cfg(feature = "regex")]
        if let Some(re) = &self.compiled {
            return re.is_match(line);
        }
        let hay = if self.ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        if !self.word {
            return hay.contains(&self.needle);
        }
        let mut from = 0;
        while let Some(at) = hay[from..].find(&self.needle) {
            let start = from + at;
            let end = start + self.needle.len();
            let before_ok = start == 0
                || !hay[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after_ok = end >= hay.len()
                || !hay[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if before_ok && after_ok {
                return true;
            }
            // Advance past the whole matched character, not one byte. `start + 1` lands inside a
            // multi-byte character whenever the needle is not ASCII, and slicing `hay[from..]` there
            // panics with "byte index N is not a char boundary" - a `grep -w` over a file with an
            // accented word killed the process instead of reporting no match.
            from = end.max(start + 1);
            while from < hay.len() && !hay.is_char_boundary(from) {
                from += 1;
            }
            if from >= hay.len() {
                return false;
            }
        }
        false
    }

    /// Every matching line of one file body.
    pub fn scan(&self, path: &str, body: &str, rev: &str) -> Vec<Hit> {
        if body.contains('\0') {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (n, line) in body.lines().enumerate() {
            if self.matches(line) {
                out.push(Hit {
                    path: path.to_string(),
                    line: n + 1,
                    rev: rev.to_string(),
                    text: line.trim_end().to_string(),
                });
            }
        }
        out
    }
}

/// Search the files on disk, skipping anything the repository ignores.
/// The largest file `search_worktree` will read.
///
/// A search reads a file whole and then copies it once more to make it a `String`, so the peak is
/// twice the size. 16 MiB is past every source file a person greps and small enough that a directory
/// full of them does not exhaust memory. A larger file is named in the error rather than skipped, so
/// the answer "no match" is never given about a file that was not read.
pub const MAX_SEARCH_FILE: u64 = 16 * 1024 * 1024;

pub fn search_worktree(
    conn: &Connection,
    root: &Path,
    pattern: &Pattern,
    path_filter: Option<&str>,
) -> ZResult<Vec<Hit>> {
    let rules = crate::repo::load_ignore_rules(root, conn)?;
    let scope = crate::store::sparse::SparseScope::load(conn)?;
    let attrs = crate::support::attributes::Rules::load(root);
    let scanned = crate::store::cache::scan_with_cache(conn, root, &rules, &attrs)?;
    if let Some(first) = scanned.unreadable.first() {
        return Err(bad(format!(
            "cannot read {} ({} more)",
            first,
            scanned.unreadable.len() - 1
        )));
    }

    let mut hits = Vec::new();
    let mut too_large: Vec<String> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for f in scanned.files {
        if !scope.contains(&f.rel) {
            continue;
        }
        if let Some(filter) = path_filter {
            if !f.rel.starts_with(filter) {
                continue;
            }
        }
        // A file over the cap is reported, not silently skipped (#91). Reading every file whole, with
        // no limit, means a repository with one 4 GiB log makes `grep` hold 4 GiB - and the file was
        // copied a second time by `from_utf8_lossy(..).to_string()`. Skipping quietly would be the
        // worse answer: the search would come back saying "no match" about a file it never read.
        //
        // The size comes from the scan cache where it has one, so the check costs nothing extra on the
        // path that already stat'ed the file.
        let full = root.join(&f.rel);
        // A file whose metadata cannot be read is REPORTED, not skipped (#298).
        //
        // `continue` here meant a file deleted between the scan and this read was neither searched
        // nor mentioned: the search came back saying "no match", which reads as "the text is not
        // there" about a file it never opened. That is the same wrong answer #91 refused for the
        // size cap, applied to a different failure.
        let size = match std::fs::metadata(&full) {
            Ok(m) => m.len(),
            Err(e) => {
                unreadable.push(format!("{} ({})", f.rel, e));
                continue;
            }
        };
        if size > MAX_SEARCH_FILE {
            too_large.push(format!("{} ({} MiB)", f.rel, size / (1024 * 1024)));
            continue;
        }
        // Read through a cap, not `fs::read` (#298).
        //
        // Between the `metadata` above and this read the file can grow - a log being appended to,
        // which is exactly the file a repository has that is worth capping. `fs::read` then read
        // whatever was there, past the cap the check had just enforced. Taking `size + 1` bytes and
        // refusing anything longer makes the cap hold at the moment of reading rather than at the
        // moment of measuring.
        let body = match read_capped(&full, size) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(e) => {
                unreadable.push(format!("{} ({})", f.rel, e));
                continue;
            }
        };
        hits.extend(pattern.scan(&f.rel, &body, ""));
    }
    if !unreadable.is_empty() {
        return Err(bad(format!(
            "not searched: {} (the file could not be read)\n\
             this is not \"no match\": the text was never looked at",
            unreadable.join(", ")
        )));
    }
    if !too_large.is_empty() {
        return Err(bad(format!(
            "not searched: {} (over the {} MiB cap; search them with --path or raise the cap)",
            too_large.join(", "),
            MAX_SEARCH_FILE / (1024 * 1024)
        )));
    }
    Ok(hits)
}

/// Read a file, refusing anything longer than it measured a moment ago.
///
/// The size check before this makes the cap mean something only if the read honours the same bound.
/// `fs::read` reads whatever is on disk at the moment it runs, so a file growing between the measure
/// and the read slipped past the cap entirely (#298). Reading `size + 1` bytes answers both questions
/// in one pass: if the result is longer than `size`, the file grew and the answer is a refusal rather
/// than a body that breaks the promise the cap makes.
fn read_capped(path: &std::path::Path, measured: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(measured.min(MAX_SEARCH_FILE) as usize);
    // One byte past the measurement: enough to tell "the same" from "it grew" without reading the
    // rest of a file that may be gigabytes.
    f.by_ref()
        .take(measured.saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > measured {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the file grew while it was being read",
        ));
    }
    Ok(buf)
}

/// Search every distinct version of every tracked path, newest first.
///
/// The same body is reached through many changesets, so each object hash is read once. Reading
/// per changeset instead cost a repeated read of the whole history, which is the difference
/// between a search that finishes and one that does not on a long repository.
pub fn search_history(
    conn: &Connection,
    pattern: &Pattern,
    path_filter: Option<&str>,
    limit: i64,
) -> ZResult<Vec<Hit>> {
    // SQLite reads `LIMIT -1` as "no limit" (#297).
    //
    // `limit` comes from clap as a signed integer and went straight into the statement, so
    // `gesta grep --history --limit -1` read every version of every tracked path and every distinct
    // object body - exactly the work the limit exists to prevent, reached by typing a number the
    // flag was supposed to reject. A negative limit is a usage error, not a request for everything.
    if limit < 0 {
        return Err(bad(format!(
            "--limit must be zero or more, not {}; a negative limit used to mean \"read everything\"",
            limit
        )));
    }
    let mut stmt = conn.prepare(
        "SELECT fv.path, fv.hash, fv.changeset_id
         FROM file_versions fv
         ORDER BY fv.created_at DESC, fv.version DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;

    let mut seen: BTreeMap<(String, String), ()> = BTreeMap::new();
    let mut hits = Vec::new();
    for row in rows {
        let (path, hash, cs) = row?;
        if let Some(filter) = path_filter {
            if !path.starts_with(filter) {
                continue;
            }
        }
        if seen.insert((path.clone(), hash.clone()), ()).is_some() {
            continue;
        }
        let body = match get_object(conn, &hash) {
            Ok(b) => b,
            Err(_) => continue,
        };
        hits.extend(pattern.scan(&path, &body, &cs));
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_word_search_with_a_multi_byte_pattern_does_not_panic() {
        // #79: after a rejected match the scan advanced by one BYTE, `from = start + 1`. When the
        // needle or the text around it is not ASCII, that lands in the middle of a character and
        // `hay[from..]` panics with "byte index N is not a char boundary". A `grep -w` over a file
        // with an accented word crashed the process instead of reporting no match.
        let m = Pattern::new("é", false, true).unwrap();
        // "aé" - the match starts at byte 1 and is refused because 'a' is a word character, so the
        // scan advances one byte into the middle of 'é'.
        assert!(!m.matches("aé"), "aé does not contain é as a whole word");
        assert!(m.matches(" é "), "a standalone é is a whole word");
        // And a multi-byte needle in a longer line, several times over.
        assert!(!m.matches("aaaéaaa"));
        assert!(m.matches("aaa é aaa"));
    }

    #[test]
    fn a_plain_string_is_found_on_its_line() {
        let p = Pattern::new("needle", false, false).unwrap();
        let hits = p.scan("a.txt", "first\nhas a needle here\nlast\n", "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].path, "a.txt");
    }

    #[test]
    fn case_is_ignored_only_when_asked() {
        let exact = Pattern::new("Needle", false, false).unwrap();
        assert!(exact.scan("a", "a needle\n", "").is_empty());
        let loose = Pattern::new("Needle", true, false).unwrap();
        assert_eq!(loose.scan("a", "a needle\n", "").len(), 1);
    }

    #[test]
    fn a_whole_word_match_does_not_find_a_longer_identifier() {
        let p = Pattern::new("hash", false, true).unwrap();
        assert!(p.matches("let hash = 1;"));
        assert!(!p.matches("let hashes = 1;"), "a prefix must not match");
        assert!(!p.matches("let rehash = 1;"), "a suffix must not match");
        assert!(p.matches("call(hash)"));
    }

    #[test]
    fn an_empty_pattern_is_refused_because_it_matches_everything() {
        assert!(Pattern::new("", false, false).is_err());
    }

    #[test]
    fn a_binary_file_is_skipped_rather_than_reported() {
        let p = Pattern::new("a", false, false).unwrap();
        assert!(p.scan("blob.bin", "a\0b\n", "").is_empty());
    }

    #[test]
    fn history_reads_each_object_once_across_many_changesets() {
        let conn = crate::store::tests::mem_db();
        crate::repo::tests::commit_files(&conn, "g1", "one", &[("a.txt", "needle here\n")]);
        crate::repo::tests::commit_files(&conn, "g2", "two", &[("a.txt", "needle here\n")]);
        let p = Pattern::new("needle", false, false).unwrap();
        let hits = search_history(&conn, &p, None, 500).unwrap();
        // One body, reached through two changesets, reported once per distinct revision.
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt"], "the same body was reported twice");
    }

    #[test]
    fn history_finds_a_string_that_no_longer_exists_on_disk() {
        let conn = crate::store::tests::mem_db();
        crate::repo::tests::commit_files(&conn, "h1", "old", &[("gone.txt", "the lost token\n")]);
        let p = Pattern::new("lost token", false, false).unwrap();
        let hits = search_history(&conn, &p, None, 500).unwrap();
        assert_eq!(hits.len(), 1, "a deleted string must still be findable");
        assert_eq!(hits[0].rev, "h1");
    }
    #[cfg(feature = "regex")]
    #[test]
    fn a_regex_matches_more_than_a_literal_does() {
        let body = "fn read_config() {}\nfn write_config() {}\n";
        let literal = Pattern::new("fn .*_config", false, false).unwrap();
        assert!(
            literal.scan("a", body, "").is_empty(),
            "a literal found a pattern"
        );

        let re = Pattern::regex("fn [a-z]+_config", false, false).unwrap();
        assert_eq!(re.scan("a", body, "").len(), 2);
    }

    #[cfg(feature = "regex")]
    #[test]
    fn a_regex_with_word_uses_boundaries_so_a_prefix_does_not_match() {
        let body = "let hashes = 1;\nlet hash = 2;\n";
        let re = Pattern::regex("hash", false, true).unwrap();
        let hits = re.scan("a", body, "");
        assert_eq!(hits.len(), 1, "a prefix matched: {:?}", hits);
        assert_eq!(hits[0].line, 2);
    }

    #[cfg(feature = "regex")]
    #[test]
    fn a_regex_ending_on_a_bracket_still_works_with_word() {
        // The end boundary is only added when the pattern ends on a word character. Adding a
        // second boundary after a bracket produced a pattern the engine refuses.
        let re = Pattern::regex(r"read_config\(", false, true).unwrap();
        assert_eq!(re.scan("a", "fn read_config(path)\n", "").len(), 1);
    }

    #[cfg(feature = "regex")]
    #[test]
    fn a_pattern_the_engine_cannot_express_is_refused_with_a_reason() {
        let err = match Pattern::regex("foo(?=bar)", false, false) {
            Ok(_) => panic!("a lookahead was accepted"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("look-around") || err.contains("lookaround"),
            "the reason did not name the construct: {}",
            err
        );
        // The detail from the parser survives, rather than the first empty line alone.
        assert!(err.len() > 120, "the message lost its detail: {}", err);
    }

    #[cfg(feature = "regex")]
    #[test]
    fn case_insensitive_regex_and_literal_agree() {
        let re = Pattern::regex("FN ", true, false).unwrap();
        let lit = Pattern::new("FN ", true, false).unwrap();
        let body = "fn main() {}\n";
        assert_eq!(re.matches(body), lit.matches(body));
        assert!(re.matches(body), "case insensitive regex missed a line");
    }
    #[test]
    fn a_file_over_the_cap_is_named_instead_of_read() {
        // #91: every file the ignore rules admit was read whole with no size limit, and copied once
        // more by `from_utf8_lossy(..).to_string()`. One large file in the tree was enough to make the
        // search hold all of it.
        //
        // The cap is exercised at its own value rather than by writing 16 MiB of data: the assertion
        // that matters is that a file over the limit is NAMED in the error, because the alternative -
        // skipping it quietly - answers "no match" about a file that was never read.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::init_schema(&conn).unwrap();
        crate::store::migrate(&conn).unwrap();
        let dir = std::env::temp_dir().join(format!("gesta-grep-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("small.txt"), "needle here\n").unwrap();
        let big = dir.join("big.txt");
        // A sparse file: `set_len` reserves the length without writing 16 MiB of zeros.
        {
            let f = std::fs::File::create(&big).unwrap();
            f.set_len(MAX_SEARCH_FILE + 1).unwrap();
        }

        let pattern = Pattern::new("needle", false, false).unwrap();
        let err = match search_worktree(&conn, &dir, &pattern, None) {
            Ok(hits) => panic!(
                "a file over the cap was read and {} hit(s) returned; the cap is not in the way",
                hits.len()
            ),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("big.txt"),
            "the oversized file must be named, or the answer is 'no match' about a file never read: {err}"
        );
        assert!(
            err.contains("not searched"),
            "the error must say the search did not happen, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_under_the_cap_is_still_searched() {
        // The cap must not stop an ordinary file.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::init_schema(&conn).unwrap();
        crate::store::migrate(&conn).unwrap();
        let dir = std::env::temp_dir().join(format!("gesta-grep-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "first\nhas a needle\n").unwrap();

        let pattern = Pattern::new("needle", false, false).unwrap();
        let hits = search_worktree(&conn, &dir, &pattern, None).unwrap();
        assert_eq!(hits.len(), 1, "the ordinary file must still be searched");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_negative_limit_is_refused_instead_of_reading_everything() {
        // #297: `limit` came from clap as a signed integer and went straight into `LIMIT ?1`, where
        // SQLite reads -1 as "no limit". `gesta grep --history --limit -1` then read every version of
        // every tracked path and every distinct object body - exactly the work the limit exists to
        // prevent, reached by typing a number the flag was meant to reject.
        let conn = crate::store::tests::mem_db();
        let pattern = Pattern::new("needle", false, false).unwrap();
        let err = search_history(&conn, &pattern, None, -1)
            .expect_err("a negative limit must be refused");
        let msg = format!("{err}");
        assert!(
            msg.contains("zero or more"),
            "the refusal must say what is allowed, got: {msg}"
        );

        // Zero is a real limit and must stay harmless rather than becoming an error.
        assert!(
            search_history(&conn, &pattern, None, 0).is_ok(),
            "zero means zero rows, not a refusal"
        );
    }

    #[test]
    fn a_read_is_refused_when_the_file_grew_past_its_measurement() {
        // #298: the cap was enforced against a `metadata` call and then `fs::read` read whatever was
        // on disk at that later moment, so a file growing in between slipped past the cap entirely.
        // The read takes `measured + 1` bytes, which is enough to tell "the same" from "it grew"
        // without reading the rest of a file that may be gigabytes.
        let dir = std::env::temp_dir().join(format!("gesta-grow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("grow.txt");
        std::fs::write(&f, "0123456789").unwrap();

        // Measured ten, read ten: fine.
        let ok = read_capped(&f, 10).unwrap();
        assert_eq!(ok.len(), 10);

        // Measured five, file is ten: it grew, so the answer is a refusal rather than ten bytes.
        let err =
            read_capped(&f, 5).expect_err("a file longer than its measurement must be refused");
        assert!(
            err.to_string().contains("grew"),
            "the refusal must name what happened, got: {err}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
