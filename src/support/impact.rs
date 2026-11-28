//! What a change breaks outside the files it touched.
//!
//! A diff shows what was edited. It cannot show what depended on what was edited, and the
//! failures that hurt most are the ones where a change looks complete and an untouched file
//! stops working. Renaming a field is the smallest example: two files change and fifteen keep
//! reading the old name.
//!
//! This searches for the names a change removed. A name that still appears in a file the change
//! did not touch is a caller that was not updated, and that is a finding with a file and a line
//! number rather than a warning about a whole diff.
//!
//! What it can see:
//!
//! - An identifier the change deleted from a line it also changed.
//! - A quoted string that disappeared, which is how a JSON key or an event name is written.
//!
//! What it cannot see, stated so a reader knows the limit: a name reached through a computed
//! key, a dynamic import, a string built by concatenation, or a rename that keeps the old name
//! alive in an alias. Those need a type system or a running program.

use crate::ZResult;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// The line with any secret in it masked, when the secret scanner is compiled in.
///
/// `leak` lives behind the `regex` feature and `impact` does not, so the call has to be conditional
/// or a build without that feature stops compiling - which is what the gate step "the build without
/// optional features compiles" caught (#324). Without the scanner there is no definition of what a
/// secret looks like, so the honest fallback withholds the line rather than printing it and hoping:
/// quoting the source line is a convenience, and leaking the value it mentions is not a price worth
/// paying for it (#302).
#[cfg(feature = "regex")]
fn redacted_line(line: String) -> String {
    crate::support::leak::redact_line(&line)
}

#[cfg(not(feature = "regex"))]
fn redacted_line(_line: String) -> String {
    String::from("<line withheld: built without the secret scanner>")
}

/// One reference to a name the change removed, in a file the change did not touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stale {
    /// The identifier or string that no longer exists in the file that defined it.
    pub name: String,
    /// Where the old name still appears.
    pub path: String,
    pub line: usize,
    /// The line itself, trimmed, so a reader can judge without opening anything.
    pub text: String,
    /// The file the change removed the name from.
    pub removed_from: String,
}

/// Names that are too common to be evidence.
///
/// An identifier like `id` or `name` appears in every file for reasons unrelated to any one
/// change, so reporting them would bury the real findings under noise and teach a reader to
/// ignore the output.
const NOISE: &[&str] = &[
    "self",
    "this",
    "data",
    "value",
    "result",
    "item",
    "items",
    "index",
    "name",
    "id",
    "key",
    "true",
    "false",
    "null",
    "none",
    "undefined",
    "string",
    "number",
    "object",
    "array",
    "error",
    "message",
    "response",
    "request",
    "config",
    "options",
    "args",
    "params",
    "args",
    "len",
    "type",
    "new",
    "default",
    "main",
    "test",
    "tests",
    "expect",
    "assert",
    "print",
    "println",
    "return",
    "import",
    "export",
    "from",
    "function",
    "class",
    "struct",
    "enum",
    "impl",
    "const",
    "static",
    "public",
    "private",
    "void",
    "int",
    "bool",
    "str",
    "vec",
    "some",
    "ok",
    "err",
    "map",
    "filter",
];

/// Identifiers in a line: runs of letters, digits and underscores, at least three characters.
fn identifiers(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in line.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            if cur.len() >= 3 {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    if cur.len() >= 3 {
        out.push(cur);
    }
    out
}

/// Quoted strings in a line, single or double. A JSON key travels as one of these.
fn quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars();
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == '\\' {
                    // Skip the escape so a quote inside a string does not end it.
                    cur.push(ch);
                    if let Some(next) = chars.next() {
                        cur.push(next);
                    }
                    continue;
                }
                if ch == q {
                    if cur.len() >= 3 {
                        out.push(cur.clone());
                    }
                    cur.clear();
                    quote = None;
                } else {
                    cur.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    cur.clear();
                }
            }
        }
    }
    out
}

fn is_noise(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    NOISE.contains(&lower.as_str()) || name.len() < 4
}

/// What a search could not see, counted rather than guessed at.
///
/// A review that finds nothing reads as "nothing to find". That is a different claim from "nothing
/// I can see", and the difference decides whether a reader trusts a clean result. The two shapes
/// that stay invisible are a name assembled at run time and a name reached through another name,
/// so counting the places those could hide turns a silent gap into a stated one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlindSpots {
    /// Lines that build a string, so a key they produce is not a name this search can match.
    pub computed_keys: usize,
    /// Lines that re-export or alias, so a name can reach a file through a second name.
    pub indirections: usize,
    /// Files whose content was read; the denominator for the two counts above.
    pub files_read: usize,
}

impl BlindSpots {
    /// Whether the search had anything it could not see.
    pub fn any(&self) -> bool {
        self.computed_keys > 0 || self.indirections > 0
    }
}

/// Count the places where a name could be built or forwarded.
///
/// This walks the same tree the search walks, so the two answers describe one pass over one set of
/// files. A count taken from a different set would be a claim about a different question.
pub fn blind_spots(conn: &Connection, tree: &[(String, String)]) -> BlindSpots {
    // Named so the checks below read as shapes rather than as escaped quote soup.
    const QUOTE: char = '"';
    let mut out = BlindSpots::default();
    for (_, hash) in tree {
        let Ok(body) = crate::store::get_object(conn, hash) else {
            continue;
        };
        out.files_read += 1;
        for line in body.lines() {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with('#') || t.starts_with('*') {
                continue;
            }
            // A concatenation or a template placeholder is how a key gets built from parts.
            // A quote-shaped string built by glue is the same shape as a template literal, so
            // both are counted: the point is that a key was assembled, not how.
            let glue_before = t.contains(&format!("+ {}", QUOTE));
            let glue_after = t.contains(&format!("{} +", QUOTE));
            let template = t.contains("`${");
            let formatted = t.contains("format!(");
            if glue_before || glue_after || template || formatted {
                out.computed_keys += 1;
            }
            // A re-export or a destructuring alias gives one name two spellings.
            if t.contains("export * from")
                || t.contains("module.exports =")
                || t.contains("} = require(")
                || t.contains(" as ")
                    && (t.contains("import") || t.contains("const {") || t.contains("let {"))
            {
                out.indirections += 1;
            }
        }
    }
    out
}

/// Find names the change removed that still appear somewhere it did not touch.
///
/// `changed` is every path the change edited, so a name still present in one of those is not a
/// finding: the author may have updated it there. `tree` is the repository at the point being
/// reviewed, which is what the untouched files are read from.
pub fn stale_references(
    conn: &Connection,
    changed: &[(String, Vec<String>, Vec<String>)],
    tree: &[(String, String)],
) -> ZResult<Vec<Stale>> {
    // Names removed from a line the change also rewrote.
    let mut removed: HashMap<String, String> = HashMap::new();
    let mut still_defined: HashSet<String> = HashSet::new();
    for (path, old, new) in changed {
        for line in new {
            for name in identifiers(line).into_iter().chain(quoted(line)) {
                still_defined.insert(name);
            }
        }
        for line in old {
            for name in identifiers(line).into_iter().chain(quoted(line)) {
                if is_noise(&name) {
                    continue;
                }
                removed.insert(name, path.clone());
            }
        }
    }
    // A name that reappears in the new side of the same change was not really removed.
    for name in &still_defined {
        removed.remove(name);
    }
    if removed.is_empty() {
        return Ok(Vec::new());
    }

    let changed_paths: HashSet<&str> = changed.iter().map(|(p, _, _)| p.as_str()).collect();
    let mut out = Vec::new();
    for (path, hash) in tree {
        if changed_paths.contains(path.as_str()) {
            continue;
        }
        // Only text. A binary file has no lines and reading it as one would fail anyway.
        let Ok(body) = crate::store::get_object(conn, hash) else {
            continue;
        };
        for (n, line) in body.lines().enumerate() {
            for name in identifiers(line).into_iter().chain(quoted(line)) {
                if let Some(from) = removed.get(&name) {
                    out.push(Stale {
                        name,
                        path: path.clone(),
                        line: n + 1,
                        // Redacted, because a finding quotes the source line and that line can
                        // hold more than the name it matched (#302). The scanner and the report share
                        // one definition of what a secret is, so nothing the scan would flag is
                        // printed here in the clear.
                        text: redacted_line(line.trim().chars().take(160).collect::<String>()),
                        removed_from: from.clone(),
                    });
                }
            }
        }
    }
    // One finding per name and file is enough: a name used five times in one file is one
    // unfinished caller, and repeating it five times hides the next file.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    out.retain(|s| seen.insert((s.name.clone(), s.path.clone())));
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_only_in_the_wrong_place_is_a_finding() {
        let changed = vec![(
            "src/api.rs".to_string(),
            vec!["let payload = build(userName);".to_string()],
            vec!["let payload = build(user_name);".to_string()],
        )];
        let body = "fn render() {\n    let n = record.userName;\n}\n".to_string();
        let conn = crate::store::tests::mem_db();
        let hash = crate::store::put_object(&conn, &body).unwrap();
        let tree = vec![("src/view.rs".to_string(), hash)];
        let stale = stale_references(&conn, &changed, &tree).unwrap();
        assert_eq!(
            stale.len(),
            1,
            "expected one stale reference, got {:?}",
            stale
        );
        assert_eq!(stale[0].name, "userName");
        assert_eq!(stale[0].path, "src/view.rs");
        assert_eq!(stale[0].line, 2);
        assert_eq!(stale[0].removed_from, "src/api.rs");
    }

    #[test]
    fn a_name_still_used_in_a_changed_file_is_not_a_finding() {
        let changed = vec![(
            "src/api.rs".to_string(),
            vec!["call(userName)".to_string()],
            vec!["call(user_name)".to_string()],
        )];
        let body = "call(user_name)\n".to_string();
        let conn = crate::store::tests::mem_db();
        let hash = crate::store::put_object(&conn, &body).unwrap();
        let tree = vec![("src/api.rs".to_string(), hash)];
        assert!(stale_references(&conn, &changed, &tree).unwrap().is_empty());
    }

    #[test]
    fn a_json_key_removed_from_one_file_is_found_in_another() {
        let changed = vec![(
            "src/feed.rs".to_string(),
            vec![r#"json!({"authorName": a})"#.to_string()],
            vec![r#"json!({"author": a})"#.to_string()],
        )];
        let body = r#"let name = item["authorName"];"#.to_string();
        let conn = crate::store::tests::mem_db();
        let hash = crate::store::put_object(&conn, &body).unwrap();
        let tree = vec![("src/view.js".to_string(), hash)];
        let stale = stale_references(&conn, &changed, &tree).unwrap();
        assert!(
            stale.iter().any(|s| s.name == "authorName"),
            "a removed JSON key still read elsewhere must be reported: {:?}",
            stale
        );
    }

    #[test]
    fn common_words_do_not_bury_the_real_finding() {
        // `value` and `data` are removed here and appear everywhere, which must not produce a
        // wall of noise that hides `userId`.
        let changed = vec![(
            "src/a.rs".to_string(),
            vec!["let value = data.userId;".to_string()],
            vec!["let v = d.user_id;".to_string()],
        )];
        let body = "let x = value;\nlet y = data;\nlet z = cfg.userId;\n".to_string();
        let conn = crate::store::tests::mem_db();
        let hash = crate::store::put_object(&conn, &body).unwrap();
        let tree = vec![("src/b.rs".to_string(), hash)];
        let stale = stale_references(&conn, &changed, &tree).unwrap();
        assert!(
            stale.iter().all(|s| s.name != "value" && s.name != "data"),
            "noise was reported: {:?}",
            stale
        );
        assert!(stale.iter().any(|s| s.name == "userId"), "{:?}", stale);
    }

    #[test]
    fn one_finding_per_name_and_file() {
        let changed = vec![(
            "src/a.rs".to_string(),
            vec!["oldField".to_string()],
            vec!["new_field".to_string()],
        )];
        let body = "a.oldField\nb.oldField\nc.oldField\n".to_string();
        let conn = crate::store::tests::mem_db();
        let hash = crate::store::put_object(&conn, &body).unwrap();
        let tree = vec![("src/b.rs".to_string(), hash)];
        let stale = stale_references(&conn, &changed, &tree).unwrap();
        assert_eq!(
            stale.len(),
            1,
            "a name used three times is one unfinished file"
        );
    }

    #[test]
    fn a_binary_file_is_skipped_not_failed() {
        let changed = vec![(
            "src/a.rs".to_string(),
            vec!["oldName".to_string()],
            vec!["newName".to_string()],
        )];
        let conn = crate::store::tests::mem_db();
        let hash = crate::store::put_object_bytes(&conn, &[0u8, 1, 2, 3]).unwrap();
        let tree = vec![("img.png".to_string(), hash)];
        assert!(stale_references(&conn, &changed, &tree).unwrap().is_empty());
    }
}
