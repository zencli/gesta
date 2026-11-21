//! `.gitattributes`, to the extent a mixed team needs it.
//!
//! On a team with a Windows checkout and a Unix one, the same file arrives with different line
//! endings. Stored as-is, every checkout looks like it changed every line, the diff is useless, and
//! a merge conflicts on content nobody edited. The fix is the same one git uses: normalise on the
//! way in, decide the line ending on the way out, and say which files are not text at all.
//!
//! Three attributes are honoured, because those are the ones that change what a commit contains:
//!
//! - `text` normalises line endings to LF in the object.
//! - `-text` or `binary` stores the file byte for byte, and no normalisation happens.
//! - `eol=lf` and `eol=crlf` decide what is written to the working tree.
//!
//! `text=auto` is treated as `text`: a file that is not valid UTF-8 is stored raw anyway, because
//! normalising bytes that are not text corrupts them.
//!
//! What is deliberately absent: filters, diff drivers, merge drivers, and `working-tree-encoding`.
//! Each is a hook into a program, and a rule that runs a program on every file is a different
//! feature with its own trust question. This module only decides bytes.

use std::collections::HashMap;
use std::path::Path;

/// What the rules say about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Text {
    /// Normalise to LF in the object; the working tree gets `eol`.
    Yes,
    /// Store byte for byte.
    No,
    /// No rule matched. The default is git's: normalise text, leave binary alone.
    Unspecified,
}

/// One parsed rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The pattern as written, for a message that explains which rule decided.
    pub pattern: String,
    /// Which paths it matches.
    matcher: Matcher,
    pub text: Option<Text>,
    /// The line ending to write to the working tree, if the rule names one.
    pub eol: Option<Eol>,
}

/// Which line ending a checkout gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

/// How a pattern matches, kept as an enum so a pattern is not re-parsed per file.
#[derive(Debug, Clone)]
enum Matcher {
    /// No slash: matches the file name at any depth, like `.gitignore`.
    Name(String),
    /// A slash: matched against the path from the repository root.
    Path(String),
    /// Every path.
    All,
}

impl Matcher {
    fn matches(&self, path: &str) -> bool {
        match self {
            Matcher::All => true,
            Matcher::Path(p) => glob(p, path),
            Matcher::Name(p) => {
                let name = path.rsplit('/').next().unwrap_or(path);
                glob(p, name)
            }
        }
    }
}

/// A tiny glob: `*` matches within a segment, `**` crosses segments, `?` one character.
///
/// Written here rather than pulled in, because the full pattern language buys nothing here and every
/// dependency in a storage tool is a thing an operator has to trust.
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_at(&p, 0, &t, 0)
}

fn glob_at(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' => {
                // `**` crosses path separators, a single `*` does not.
                let double = pi + 1 < p.len() && p[pi + 1] == '*';
                let start = if double { pi + 2 } else { pi + 1 };
                // A `**/` at the start also matches nothing at all, which is what makes
                // `**/build` match `build`.
                if double && start < p.len() && p[start] == '/' && glob_at(p, start + 1, t, ti) {
                    return true;
                }
                for end in ti..=t.len() {
                    if glob_at(p, start, t, end) {
                        return true;
                    }
                    // A single star never swallows a separator.
                    if !double && end < t.len() && t[end] == '/' {
                        return false;
                    }
                }
                return false;
            }
            '?' => {
                if ti >= t.len() || t[ti] == '/' {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

/// The rules a repository carries, in the order they were written.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    pub rules: Vec<Rule>,
    /// The path the rules were read from, so a message can name the file.
    pub source: Option<String>,
}

impl Rules {
    /// Read `.gitattributes` at the repository root, and `.gesta/attributes` if it exists.
    ///
    /// The second file is the project's own name for the same thing, so a repository that predates
    /// this support does not have to rename anything, and a repository that wants one file for both
    /// tools keeps working.
    pub fn load(root: &Path) -> Self {
        let mut out = Rules::default();
        for name in [".gitattributes", ".gesta/attributes"] {
            let path = root.join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let before = out.rules.len();
                    out.parse(&text);
                    if out.rules.len() > before && out.source.is_none() {
                        out.source = Some(name.to_string());
                    }
                }
                Err(_) => continue,
            }
        }
        out
    }

    /// Parse attribute lines. Later rules win, which is how git reads them.
    pub fn parse(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let pattern = match parts.next() {
                Some(p) => p,
                None => continue,
            };
            let mut text_attr = None;
            let mut eol = None;
            let mut saw_any = false;
            for attr in parts {
                saw_any = true;
                match attr {
                    "text" | "text=auto" => text_attr = Some(Text::Yes),
                    "-text" | "binary" => text_attr = Some(Text::No),
                    "eol=lf" => {
                        eol = Some(Eol::Lf);
                        if text_attr.is_none() {
                            // Naming an eol implies the file is text; git treats it that way and a
                            // rule that says otherwise is a rule nobody means.
                            text_attr = Some(Text::Yes);
                        }
                    }
                    "eol=crlf" => {
                        eol = Some(Eol::Crlf);
                        if text_attr.is_none() {
                            text_attr = Some(Text::Yes);
                        }
                    }
                    // A `!attr` unsets it, which is how a file opts back out of a broad rule.
                    "!text" => text_attr = Some(Text::Unspecified),
                    _ => continue,
                }
            }
            if !saw_any {
                // A pattern with no attributes is a declaration of interest, not a rule.
                continue;
            }
            let matcher = if pattern == "*" {
                Matcher::All
            } else if pattern.contains('/') {
                Matcher::Path(pattern.trim_start_matches('/').to_string())
            } else {
                Matcher::Name(pattern.to_string())
            };
            self.rules.push(Rule {
                pattern: pattern.to_string(),
                matcher,
                text: text_attr,
                eol,
            });
        }
    }

    /// What the rules say about a path, taken from the last rule that matches and mentions it.
    pub fn lookup(&self, path: &str) -> (Text, Option<Eol>, Option<&str>) {
        let mut text = Text::Unspecified;
        let mut eol = None;
        let mut which = None;
        for rule in &self.rules {
            if !rule.matcher.matches(path) {
                continue;
            }
            if let Some(t) = rule.text {
                text = t;
                which = Some(rule.pattern.as_str());
            }
            if rule.eol.is_some() {
                eol = rule.eol;
                if which.is_none() {
                    which = Some(rule.pattern.as_str());
                }
            }
        }
        (text, eol, which)
    }

    /// Whether the rules are empty, which is the common case and worth not paying for.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Whether a body is text, by the same test git uses in spirit: no NUL byte in the first bytes.
///
/// This is a heuristic and it is the honest one for a storage tool: a NUL byte in content is the
/// only signal available without knowing what produced the file.
pub fn is_binary(body: &[u8]) -> bool {
    body.iter().take(8000).any(|b| *b == 0)
}

/// Replace CRLF with LF.
pub fn to_lf(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\r' && i + 1 < body.len() && body[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out
}

/// Replace LF with CRLF, leaving an existing CRLF alone.
pub fn to_crlf(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + body.len() / 32);
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\r' && i + 1 < body.len() && body[i + 1] == b'\n' {
            out.push(b'\r');
            out.push(b'\n');
            i += 2;
        } else if body[i] == b'\n' {
            out.push(b'\r');
            out.push(b'\n');
            i += 1;
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out
}

/// What to store, and what to write back, for one path.
///
/// Returned as one value so a caller cannot take the store decision and forget the checkout one:
/// normalising on the way in and not on the way out turns every file into a whole-file diff for
/// anybody on Windows, which is the problem the feature exists to solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub normalise: bool,
    pub checkout: Option<Eol>,
}

impl Plan {
    /// The bytes an object holds for this body.
    pub fn stored(&self, body: &[u8]) -> Vec<u8> {
        if self.normalise && !is_binary(body) {
            to_lf(body)
        } else {
            body.to_vec()
        }
    }

    /// The bytes the working tree gets for this object body.
    pub fn checked_out(&self, body: &[u8]) -> Vec<u8> {
        if self.normalise && !is_binary(body) {
            match self.checkout {
                Some(Eol::Crlf) => to_crlf(body),
                Some(Eol::Lf) | None => body.to_vec(),
            }
        } else {
            body.to_vec()
        }
    }
}

/// Decide the plan for a path from the rules alone.
///
/// There is no platform default, and that is a decision rather than an omission. A default that
/// guessed CRLF on Windows would rewrite every file in a repository that never asked for it, and
/// the person who found out would be the one whose next commit touched every line. A repository
/// with no rules behaves as it did before: normalise on the way in, leave the working tree alone.
/// A platform that wants CRLF says so in the rule file, where it can be read, reviewed and moved.
pub fn plan_for(rules: &Rules, path: &str) -> Plan {
    let (text, eol, _) = rules.lookup(path);
    match text {
        Text::Yes => Plan {
            normalise: true,
            checkout: eol,
        },
        Text::No => Plan {
            normalise: false,
            checkout: None,
        },
        // No rule names this path. Normalising is still right, because it is what makes a file
        // written with either ending produce one object.
        Text::Unspecified => Plan {
            normalise: true,
            checkout: eol,
        },
    }
}

/// The attribute string a rule set produces for display, keyed by path.
///
/// Exists so `attributes why <path>` can answer with the same function the writer uses. A second
/// implementation that agrees today drifts tomorrow, and then the tool explains a decision it did
/// not make.
pub fn explain(rules: &Rules, path: &str) -> HashMap<String, String> {
    let (text, eol, which) = rules.lookup(path);
    let mut out = HashMap::new();
    out.insert(
        "text".to_string(),
        match text {
            Text::Yes => "set".to_string(),
            Text::No => "unset".to_string(),
            Text::Unspecified => "unspecified".to_string(),
        },
    );
    out.insert(
        "eol".to_string(),
        match eol {
            Some(Eol::Lf) => "lf".to_string(),
            Some(Eol::Crlf) => "crlf".to_string(),
            None => "unspecified".to_string(),
        },
    );
    out.insert(
        "rule".to_string(),
        which
            .map(|w| w.to_string())
            .unwrap_or_else(|| "(none)".to_string()),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Rules {
        let mut r = Rules::default();
        r.parse(text);
        r
    }

    #[test]
    fn a_name_pattern_matches_at_any_depth() {
        let r = rules("*.txt text eol=lf");
        assert_eq!(r.lookup("a.txt").1, Some(Eol::Lf));
        assert_eq!(r.lookup("deep/dir/b.txt").1, Some(Eol::Lf));
        assert_eq!(r.lookup("c.md").1, None);
    }

    #[test]
    fn a_pattern_with_a_slash_anchors_at_the_root() {
        let r = rules("build/*.js binary");
        assert_eq!(r.lookup("build/a.js").0, Text::No);
        // A nested build directory is not what the rule said.
        assert_eq!(r.lookup("src/build/a.js").0, Text::Unspecified);
    }

    #[test]
    fn a_later_rule_wins() {
        let r = rules("*.txt text eol=lf\nspecial.txt -text");
        assert_eq!(r.lookup("a.txt").0, Text::Yes);
        assert_eq!(r.lookup("special.txt").0, Text::No);
    }

    #[test]
    fn naming_an_eol_implies_text() {
        let r = rules("*.sh eol=crlf");
        assert_eq!(r.lookup("run.sh").0, Text::Yes);
        assert_eq!(r.lookup("run.sh").1, Some(Eol::Crlf));
    }

    #[test]
    fn a_double_star_crosses_directories() {
        let r = rules("**/generated/** -text");
        assert_eq!(r.lookup("a/generated/x.js").0, Text::No);
        assert_eq!(r.lookup("generated/x.js").0, Text::No);
    }

    #[test]
    fn a_binary_body_is_never_normalised() {
        let plan = Plan {
            normalise: true,
            checkout: Some(Eol::Crlf),
        };
        let png = b"\x89PNG\r\n\x1a\n\x00\x00".to_vec();
        assert_eq!(
            plan.stored(&png),
            png,
            "a NUL byte means the body is not text"
        );
        assert_eq!(plan.checked_out(&png), png);
    }

    #[test]
    fn text_normalises_on_the_way_in_and_back_out() {
        let plan = Plan {
            normalise: true,
            checkout: Some(Eol::Crlf),
        };
        let crlf = b"a\r\nb\r\n".to_vec();
        assert_eq!(plan.stored(&crlf), b"a\nb\n".to_vec());
        assert_eq!(plan.checked_out(b"a\nb\n"), b"a\r\nb\r\n".to_vec());
    }

    #[test]
    fn no_rules_means_normalise_and_leave_the_tree_alone() {
        let plan = plan_for(&Rules::default(), "a.txt");
        assert!(plan.normalise);
        // Nothing is written back, so a repository without rules does not rewrite anybody's files.
        assert_eq!(plan.checkout, None);
    }

    #[test]
    fn a_rule_for_another_path_does_not_decide_this_one() {
        let r = rules("*.sh eol=lf");
        assert_eq!(plan_for(&r, "a.txt").checkout, None);
        assert_eq!(plan_for(&r, "run.sh").checkout, Some(Eol::Lf));
    }

    #[test]
    fn a_comment_and_a_lone_pattern_are_not_rules() {
        let r = rules("# a comment\n*.md");
        assert!(r.is_empty());
    }

    #[test]
    fn explain_names_the_rule_that_decided() {
        let r = rules("*.txt text eol=lf");
        let e = explain(&r, "a.txt");
        assert_eq!(e["text"], "set");
        assert_eq!(e["eol"], "lf");
        assert_eq!(e["rule"], "*.txt");
    }
}
