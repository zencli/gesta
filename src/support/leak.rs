//! Secret scanning, for the moment before a secret leaves the machine.
//!
//! A version control tool is where a leaked credential is most expensive: the commit is the record,
//! and a record that has to be rewritten is a record nobody can trust. This module answers one
//! question at commit time, on the content that is about to be stored: does this look like a
//! credential, and if so, which rule says so.
//!
//! The design follows three things learned from tools that have done this longer.
//!
//! **A regex engine is not enough, and it is not the wrong place to start.** A pattern recognises
//! the shapes that are already known, which is most of the value; the entropy check on the captured
//! secret is what separates `AKIAIOSFODNN7EXAMPLE` from a real access key of the same shape. Both
//! signals are used here, and a rule can require either or both.
//!
//! **A finding needs a fingerprint that survives a change of line number.** The tool that reports
//! `file:line` produces findings that move every time somebody edits the file above them, and the
//! allowlist entry that suppressed the finding yesterday stops matching today. The fingerprint here
//! is the rule id, the path and a digest of the secret itself, so the same secret in the same file
//! keeps one identity across edits.
//!
//! **Fixtures are the false positives that matter.** This repository's own tests hold placeholder
//! keys, and a scanner that refuses to let the project commit its own test suite is a scanner
//! nobody keeps. Allowlists are part of the format, not an afterthought: a path glob, a rule id, or
//! a stopword that appears inside the matched secret.
//!
//! What is deliberately absent: network validation (a request that has a real key authenticate is a
//! request made from a machine that did not ask for it), archive traversal, and decoding chains.
//! Each is a program doing work on content on behalf of a commit, and the cost of being wrong is a
//! commit that succeeded while sending traffic somewhere.

use std::fmt;
use std::path::Path;

/// One rule: a shape, and the conditions under which a match is a secret.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Stable identifier, used in the fingerprint and in an allowlist.
    pub id: String,
    /// The pattern, compiled once when the rule set is built.
    pattern: regex::Regex,
    /// Which capture group holds the secret. `0` is the whole match.
    pub secret_group: usize,
    /// Minimum Shannon entropy of the secret, in bits per character. `None` disables the check.
    pub entropy: Option<f64>,
    /// How many characters a match must have before the entropy test means anything.
    ///
    /// Entropy over three characters is noise. Below this length the check is skipped rather than
    /// failed, because a rule whose secret is short is a rule that should not have asked for
    /// entropy at all.
    pub min_entropy_len: usize,
}

impl Rule {
    /// Build a rule, compiling the pattern once.
    pub fn new(
        id: &str,
        pattern: &str,
        secret_group: usize,
        entropy: Option<f64>,
    ) -> Result<Rule, String> {
        let compiled = regex::Regex::new(pattern)
            .map_err(|e| format!("rule {} has an invalid pattern: {}", id, e))?;
        Ok(Rule {
            id: id.to_string(),
            pattern: compiled,
            secret_group,
            entropy,
            min_entropy_len: 20,
        })
    }

    /// The secret inside a match, or nothing when the group did not participate.
    fn secret_of<'a>(&self, caps: &regex::Captures<'a>) -> Option<&'a str> {
        caps.get(self.secret_group)
            .map(|m| m.as_str())
            .or_else(|| caps.get(0).map(|m| m.as_str()))
    }
}

/// A suppression, so the fixtures a project needs do not block its own commits.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    /// Path globs whose content is never reported.
    pub paths: Vec<String>,
    /// Rule ids that never report.
    pub rules: Vec<String>,
    /// Words that mark a match as a placeholder when they appear in the secret itself.
    pub stopwords: Vec<String>,
}

impl Allowlist {
    /// Whether a path is exempt entirely.
    pub fn skips_path(&self, path: &str) -> bool {
        self.paths.iter().any(|p| glob(p, path))
    }

    /// Whether a rule is switched off.
    pub fn skips_rule(&self, id: &str) -> bool {
        self.rules.iter().any(|r| r == id)
    }

    /// Whether a matched secret is a placeholder.
    ///
    /// Case-insensitive, because `EXAMPLE`, `example` and `Example` are the same intent and a
    /// stopword that only matched one of them would let the others through.
    pub fn is_stopword(&self, secret: &str) -> bool {
        let lower = secret.to_ascii_lowercase();
        self.stopwords
            .iter()
            .any(|w| lower.contains(&w.to_ascii_lowercase()))
    }
}

/// One match, with everything needed to explain it and to recognise it again.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub rule: String,
    pub path: String,
    /// 1-based.
    pub line: usize,
    /// The matched line, truncated, because a report is read by a person and a log is not a place
    /// to put a secret.
    pub preview: String,
    /// The secret with all but its first four characters replaced. A report that prints the secret
    /// is a report that leaks it again.
    pub redacted: String,
    /// Stable across edits above the finding: rule, path, digest of the secret.
    pub fingerprint: String,
    /// Shannon entropy of the secret, when it was measured.
    pub entropy: Option<f64>,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}  {}  {}",
            self.path, self.line, self.rule, self.redacted
        )
    }
}

/// The rule set, ready to scan with.
#[derive(Debug, Clone)]
pub struct Scanner {
    pub rules: Vec<Rule>,
    pub allowlist: Allowlist,
    /// Files larger than this are skipped rather than scanned.
    ///
    /// A minified bundle is a megabyte on one line. Scanning it is a regex run over a megabyte with
    /// a line number that means nothing, and the secrets that matter are not in build output.
    pub max_bytes: usize,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            rules: default_rules(),
            // Placeholders shipped by providers and by every tutorial: reporting the key printed in
            // a provider's own documentation would teach a person to ignore this check on the first
            // day, and a check people ignore is worse than no check.
            allowlist: Allowlist {
                paths: Vec::new(),
                rules: Vec::new(),
                stopwords: default_stopwords(),
            },
            max_bytes: 1_000_000,
        }
    }
}

/// Words that mark a match as a placeholder rather than a credential.
///
/// A stopword is matched against the secret itself, case-insensitively. It is a blunt instrument
/// and it is the right one here: the cost of a placeholder that slips through is a refusal somebody
/// has to allowlist once, and the cost of a real key that is suppressed is a leaked key.
pub fn default_stopwords() -> Vec<String> {
    [
        "example",
        "sample",
        "placeholder",
        "changeme",
        "dummy",
        "fake",
        "redacted",
        "xxxx",
        "your_",
        "your-",
        "test_key",
        "notarealkey",
        "000000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Scanner {
    /// Read `.gesta/secret-allowlist` if it exists and merge it into the allowlist.
    ///
    /// One line per entry, the same shape as an ignore file: `path <glob>`, `rule <id>`,
    /// `stopword <word>`. A format that needs no parser library and that a person can read in an
    /// editor, because the file a project maintains by hand should not need a tool to inspect.
    pub fn load_allowlist(&mut self, root: &Path) {
        // Two files, in this order: `.gesta/secret-allowlist` is local to one checkout, and
        // `.secret-allowlist` is committed with the project. Without the second one a fresh clone
        // has no allowlist at all, so its own fixtures read as credentials and the scanner refuses
        // the project it ships with.
        for path in [
            root.join(".gesta").join("secret-allowlist"),
            root.join(".secret-allowlist"),
        ] {
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            self.merge_allowlist_text(&text);
        }
        self.warn_if_the_scanner_is_neutered(root);
    }

    /// Say so when the allowlist has turned the scanner off.
    ///
    /// The committed `.secret-allowlist` arrives with a clone, so a repository that lists every rule
    /// id disables the commit-time check for everyone who works in it, and the refusal that would
    /// have caught a real key never fires. Nothing about committing that file looks like an act; it
    /// is a text file in a diff. The scan therefore says how many rules are off, every time, on
    /// stderr, because a check that is silently not running is worse than one that is absent.
    fn warn_if_the_scanner_is_neutered(&self, root: &Path) {
        let total = self.rules.len();
        let off = self
            .rules
            .iter()
            .filter(|r| self.allowlist.skips_rule(&r.id))
            .count();
        if total == 0 || off == 0 {
            return;
        }
        if off == total {
            eprintln!(
                "gesta: the secret scanner is off. Every one of its {} rules is named in the \
                 allowlist under {}. If that was not deliberate, the list is a file that arrived \
                 with the clone, and the commit-time check has not been running.",
                total,
                root.display()
            );
        } else {
            eprintln!(
                "gesta: the secret scanner is running {} of {} rules; {} are disabled by the \
                 allowlist under {}",
                total - off,
                total,
                off,
                root.display()
            );
        }
    }

    fn merge_allowlist_text(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (kind, value) = match line.split_once(char::is_whitespace) {
                Some((k, v)) => (k, v.trim()),
                None => continue,
            };
            if value.is_empty() {
                continue;
            }
            match kind {
                "path" => self.allowlist.paths.push(value.to_string()),
                "rule" => self.allowlist.rules.push(value.to_string()),
                "stopword" => self.allowlist.stopwords.push(value.to_string()),
                _ => continue,
            }
        }
    }

    /// Scan one file's text.
    ///
    /// Per line, not over the whole body: a rule that matches across a newline finds shapes that
    /// span two unrelated lines, and the line number a reader needs is the one the match starts on.
    pub fn scan_text(&self, path: &str, body: &str) -> Vec<Finding> {
        if self.allowlist.skips_path(path) || body.len() > self.max_bytes {
            return Vec::new();
        }
        // A body with a NUL byte is binary, and a regex over it produces matches that are not text.
        if body.as_bytes().iter().take(8000).any(|b| *b == 0) {
            return Vec::new();
        }

        let mut out = Vec::new();
        for (i, line) in body.lines().enumerate() {
            for rule in &self.rules {
                if self.allowlist.skips_rule(&rule.id) {
                    continue;
                }
                for caps in rule.pattern.captures_iter(line) {
                    let secret = match rule.secret_of(&caps) {
                        Some(s) if !s.is_empty() => s,
                        _ => continue,
                    };
                    if self.allowlist.is_stopword(secret) {
                        continue;
                    }
                    let ent = if secret.len() >= rule.min_entropy_len {
                        Some(shannon(secret))
                    } else {
                        None
                    };
                    if let (Some(need), Some(got)) = (rule.entropy, ent) {
                        if got < need {
                            continue;
                        }
                    }
                    out.push(Finding {
                        rule: rule.id.clone(),
                        path: path.to_string(),
                        line: i + 1,
                        preview: preview_of(line.trim(), secret),
                        redacted: redact(secret),
                        fingerprint: fingerprint(&rule.id, path, secret),
                        entropy: ent,
                    });
                }
            }
        }
        out
    }
}

/// Shannon entropy in bits per character.
///
/// This is the measurement gitleaks reports beside a finding, and it is the one thing that tells a
/// placeholder from a key without calling a service: `AKIAIOSFODNN7EXAMPLE` is repetitive, a real
/// key is not.
pub fn shannon(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
    for b in s.bytes() {
        *counts.entry(b).or_insert(0) += 1;
    }
    let len = s.len() as f64;
    let mut h = 0.0;
    for (_b, n) in counts {
        let p = n as f64 / len;
        h -= p * p.log2();
    }
    h
}

/// Replace every secret a line contains with its redacted form.
///
/// For text that ends up in a report rather than in a scan: a finding quotes the source line it came
/// from, and a line that mentions a deleted name can also hold the value of the thing being deleted
/// (`api_key = "9f8e..."`). Reporting the line unredacted put the secret in the report, which is a
/// worse place for it than the file was (#302).
///
/// Runs the same rules the scanner runs, so a secret the scanner would flag is a secret this
/// removes - one definition of "secret", not two. A line with no finding comes back unchanged, and
/// everything else keeps its shape so the reader can still see what the line was.
pub fn redact_line(line: &str) -> String {
    let scanner = Scanner::default();
    let mut out = line.to_string();
    for rule in &scanner.rules {
        // Collect first: the replacements shift later offsets, and rewriting while iterating the
        // matches of the same rule would move the ones that have not been handled yet.
        let hits: Vec<String> = rule
            .pattern
            .captures_iter(line)
            .filter_map(|c| rule.secret_of(&c).map(|s| s.to_string()))
            .filter(|s| !s.is_empty() && !scanner.allowlist.is_stopword(s))
            .collect();
        for secret in hits {
            out = out.replace(&secret, &redact(&secret));
        }
    }
    out
}

/// Keep the first four characters and the length, so a report identifies a secret without being one.
pub fn redact(secret: &str) -> String {
    let visible = 4.min(secret.chars().count());
    let head: String = secret.chars().take(visible).collect();
    format!(
        "{}{}",
        head,
        "*".repeat(secret.chars().count().saturating_sub(visible))
    )
}

/// A stable identity for a finding that does not move when the file is edited above it.
///
/// The line number is deliberately absent: it is the field that changes, and an allowlist keyed on
/// it stops working the first time somebody adds an import.
pub fn fingerprint(rule: &str, path: &str, secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    let digest = h.finalize();
    format!("{}:{}:{}", rule, path, hex::encode(&digest[..8]))
}

/// The line that matched, with the secret itself replaced, then truncated.
///
/// Truncating the line was the whole of the first version and it removed nothing: a line short
/// enough to fit inside 160 characters arrived complete, secret and all. The rule for this field is
/// the same as the rule for `redacted` - a report that prints the secret is a report that leaks it
/// again - so every occurrence of the secret in the line is replaced before the length is considered.
fn preview_of(line: &str, secret: &str) -> String {
    let shown = redact(secret);
    let hidden = if secret.is_empty() {
        line.to_string()
    } else {
        line.replace(secret, &shown)
    };
    truncate(&hidden, 160)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{}...", cut)
}

/// The shared glob, so this file and `attributes.rs` cannot drift (#300).
///
/// The local copy recursed afresh for every split point at each `*`, so a pattern with several stars
/// against a long path was exponential: a committed allowlist file is attacker-controlled by a clone,
/// which is what made the cost reachable from data rather than from an argument. The one in `paths`
/// memoizes on `(pattern index, text index)`, which bounds the work at the product of the two lengths.
fn glob(pattern: &str, text: &str) -> bool {
    crate::support::paths::glob_match(pattern, text)
}

/// The rules shipped with the tool.
///
/// The shapes are prefix-anchored where the provider documents a prefix, because that is what makes
/// a rule worth having: a prefix plus a length is a key, and a prefix plus anything is a word. Where
/// no prefix is documented, the rule needs a nearby keyword or an entropy floor, and both are
/// stated rather than implied.
pub fn default_rules() -> Vec<Rule> {
    let specs: &[(&str, &str, usize, Option<f64>)] = &[
        // Cloud provider access keys have a documented prefix and a fixed length.
        (
            "aws-access-key-id",
            r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b",
            0,
            Some(3.0),
        ),
        (
            "aws-secret-access-key",
            r#"(?i)aws_secret_access_key\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})"#,
            1,
            Some(3.5),
        ),
        // GitHub tokens: prefix plus length is the whole signal.
        // `ghp_` covers a personal token. GitHub also issues `ghu_` (a user-to-server token from an
        // OAuth app), `ghs_` (server-to-server), `ghr_` (a refresh token) and `gho_` (an OAuth access
        // token), and none of those matched here (#301). They are the same secret in a different
        // envelope, and a scanner that catches one and not the others reports a clean tree that is
        // not clean.
        ("github-pat", r"\bgh[pousr]_[A-Za-z0-9]{36}\b", 0, Some(3.0)),
        (
            "github-fine-grained",
            r"\bgithub_pat_[A-Za-z0-9_]{82}\b",
            0,
            Some(3.0),
        ),
        ("gitlab-pat", r"\bglpat-[A-Za-z0-9_-]{20}\b", 0, Some(3.0)),
        // Payment providers, where the prefix is documented and the cost of a leak is money.
        (
            "stripe-secret",
            r"\b(?:sk|rk)_live_[A-Za-z0-9]{24,}\b",
            0,
            Some(3.0),
        ),
        (
            "stripe-restricted",
            r"\brk_live_[A-Za-z0-9]{24,}\b",
            0,
            Some(3.0),
        ),
        // Messaging and mail.
        (
            "slack-token",
            r"\bxox[baprs]-[A-Za-z0-9-]{10,72}\b",
            0,
            Some(3.0),
        ),
        (
            "slack-webhook",
            r"https://hooks\.slack\.com/services/T[A-Za-z0-9_]+/B[A-Za-z0-9_]+/[A-Za-z0-9_]+",
            0,
            Some(3.0),
        ),
        (
            "sendgrid-key",
            r"\bSG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}\b",
            0,
            Some(3.0),
        ),
        // Model providers.
        (
            // Two shapes, and only the legacy one used to be here. `T3BlbkFJ` is base64 for
            // "OpenAI", which the legacy keys carry and the newer project keys do not: those are
            // `sk-proj-` followed by a long base62 tail, and they passed the scanner uncaught (#301).
            //
            // Written as one alternation rather than a second rule so a single finding names the
            // whole thing, and anchored on the token boundary so a longer string that happens to
            // contain a key is still one finding, not a prefix.
            "openai-key",
            r"\b(?:sk-[A-Za-z0-9]{20}T3BlbkFJ[A-Za-z0-9]{20}|sk-proj-[A-Za-z0-9_\-]{32,})\b",
            0,
            Some(3.0),
        ),
        (
            "anthropic-key",
            r"\bsk-ant-[A-Za-z0-9_-]{20,}\b",
            0,
            Some(3.0),
        ),
        // Private keys are a shape no entropy check improves on.
        (
            "private-key",
            r"-----BEGIN (?:RSA |EC |OPENSSH |PGP |DSA )?PRIVATE KEY(?: BLOCK)?-----",
            0,
            None,
        ),
        // A connection string with a password in it.
        (
            "url-with-password",
            r"(?i)\b[a-z][a-z0-9+.-]*://[^\s:/@]+:([^\s@/]{6,})@[^\s/]+",
            1,
            Some(2.5),
        ),
        // The generic case, which needs a keyword to be worth running at all.
        (
            "generic-secret-assignment",
            r#"(?i)\b(?:api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|password|passwd)\s*[=:]\s*['"]?([A-Za-z0-9_\-+/=]{16,})"#,
            1,
            Some(3.5),
        ),
    ];

    let mut out = Vec::new();
    for (id, pattern, group, entropy) in specs {
        match Rule::new(id, pattern, *group, *entropy) {
            Ok(r) => out.push(r),
            // A shipped rule that does not compile is a bug in this file, and a scan that silently
            // skipped it would report "no secrets" with a rule missing. It is reported instead, and
            // the rest of the rules still run.
            Err(e) => eprintln!("secret rule {} did not compile: {}", id, e),
        }
    }
    out
}

/// Whether a path is worth reading at all.
///
/// A lock file is a hash list, a minified bundle is one long line, and an image is binary. Reading
/// them costs the scan and produces nothing but noise.
pub fn worth_scanning(path: &str) -> bool {
    const SKIP_EXT: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf", ".zip", ".gz", ".tar", ".xz",
        ".7z", ".mp3", ".mp4", ".mov", ".woff", ".woff2", ".ttf", ".eot", ".so", ".dylib", ".dll",
        ".exe", ".bin", ".wasm", ".class", ".jar", ".pyc", ".lock",
    ];
    let lower = path.to_ascii_lowercase();
    if SKIP_EXT.iter().any(|e| lower.ends_with(e)) {
        return false;
    }
    // The lock files that do not carry an extension.
    !matches!(
        lower.rsplit('/').next().unwrap_or(""),
        "package-lock.json" | "yarn.lock" | "cargo.lock" | "poetry.lock" | "composer.lock"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner() -> Scanner {
        Scanner::default()
    }

    #[test]
    fn an_aws_key_is_found_and_redacted() {
        let s = scanner();
        let got = s.scan_text("a.py", "AWS_KEY = \"AKIA2E4F6G8H0J2K4L6M\"\n");
        assert_eq!(got.len(), 1, "one finding, got {:?}", got);
        assert_eq!(got[0].rule, "aws-access-key-id");
        assert_eq!(got[0].line, 1);
        assert!(got[0].redacted.starts_with("AKIA"));
        assert!(
            !got[0].redacted.contains("2E4F"),
            "the secret is not repeated in the report"
        );
    }

    #[test]
    fn the_documented_placeholder_key_is_not_reported() {
        // This is the key in the provider's own documentation, and reporting it would be the same
        // mistake as reporting the string "password" in a comment about passwords.
        let s = scanner();
        let got = s.scan_text("doc.md", "AKIAIOSFODNN7EXAMPLE\n");
        assert!(got.is_empty(), "the placeholder was reported: {:?}", got);
    }

    #[test]
    fn a_stopword_is_matched_whatever_its_case() {
        let s = scanner();
        assert!(s.scan_text("a.txt", "AKIAIOSFODNN7ExAmPlE\n").is_empty());
    }

    #[test]
    fn a_low_entropy_key_of_the_right_shape_is_refused() {
        let mut s = Scanner::default();
        s.allowlist.stopwords.clear();
        // Twenty characters, all the same letter: the shape matches, the entropy does not.
        let got = s.scan_text("a.txt", "AAAAAAAAAAAAAAAAAAAAAAAA\n");
        assert!(
            got.is_empty(),
            "a repetitive string passed the entropy floor: {:?}",
            got
        );
    }

    #[test]
    fn a_private_key_header_is_found() {
        let s = scanner();
        let got = s.scan_text("k.pem", "-----BEGIN RSA PRIVATE KEY-----\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].rule, "private-key");
    }

    #[test]
    fn the_private_key_fixture_in_this_repository_is_allowlisted() {
        // The test suite holds a PEM header, and the project has to be able to commit its own tests.
        let mut s = Scanner::default();
        s.allowlist.paths.push("src/tls.rs".to_string());
        assert!(s
            .scan_text("src/tls.rs", "-----BEGIN PRIVATE KEY-----\\n")
            .is_empty());
        // The same text outside that file is still a finding.
        assert_eq!(
            s.scan_text("other.rs", "-----BEGIN PRIVATE KEY-----").len(),
            1
        );
    }

    #[test]
    fn a_connection_string_password_is_caught() {
        let s = scanner();
        let got = s.scan_text(
            "c.py",
            "url = \"postgres://admin:hunter2hunter2@db.internal/app\"\n",
        );
        assert_eq!(got.len(), 1, "got {:?}", got);
        assert_eq!(got[0].rule, "url-with-password");
    }

    #[test]
    fn a_generic_assignment_needs_a_keyword() {
        let s = scanner();
        // A long random-looking value with no keyword near it is not reported, because a rule that
        // fires on every base64 blob reports every file.
        assert!(s
            .scan_text("a.txt", "value = \"Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA\"\n")
            .is_empty());
        let got = s.scan_text("b.txt", "api_key = \"9f8e7d6c5b4a39281706f5e4d3c2b1a0\"\n");
        assert_eq!(got.len(), 1, "got {:?}", got);
    }

    #[test]
    fn a_binary_body_is_skipped() {
        let s = scanner();
        let body = String::from_utf8_lossy(b"\x00\x01AKIA2E4F6G8H0J2K4L6M\x00").to_string();
        assert!(s.scan_text("a.bin", &body).is_empty());
    }

    #[test]
    fn a_path_that_is_a_lock_file_is_not_worth_reading() {
        assert!(!worth_scanning("package-lock.json"));
        assert!(!worth_scanning("Cargo.lock"));
        assert!(!worth_scanning("img/logo.png"));
        assert!(worth_scanning("src/main.rs"));
        assert!(worth_scanning("config/.env.example"));
    }

    #[test]
    fn the_fingerprint_survives_a_change_of_line_and_of_column() {
        let a = fingerprint("aws-access-key-id", "a.py", "AKIA2E4F6G8H0J2K4L6M");
        let b = fingerprint("aws-access-key-id", "a.py", "AKIA2E4F6G8H0J2K4L6M");
        assert_eq!(a, b);
        // A different file, rule or secret is a different finding.
        assert_ne!(
            a,
            fingerprint("aws-access-key-id", "b.py", "AKIA2E4F6G8H0J2K4L6M")
        );
        assert_ne!(a, fingerprint("github-pat", "a.py", "AKIA2E4F6G8H0J2K4L6M"));
    }

    #[test]
    fn entropy_separates_a_key_from_a_word() {
        // The same length, and only one of them is a credential.
        assert!(shannon("correcthorsebatterystaple") < shannon("9f8e7d6c5b4a39281706f5e4d3c2b1a0"));
        assert_eq!(shannon(""), 0.0);
        assert_eq!(shannon("aaaa"), 0.0);
    }

    #[test]
    fn a_rule_that_cannot_compile_is_reported_rather_than_dropped() {
        let got = Rule::new("broken", "(unclosed", 0, None);
        assert!(got.is_err());
        assert!(
            got.unwrap_err().contains("broken"),
            "the message names the rule"
        );
    }

    #[test]
    fn an_allowlist_file_is_read_without_a_parser_library() {
        let dir = std::env::temp_dir().join(format!("gesta-al-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        std::fs::write(
            dir.join(".gesta").join("secret-allowlist"),
            "# comment\npath docs/**\nrule generic-secret-assignment\nstopword placeholder\n",
        )
        .unwrap();
        let mut s = Scanner::default();
        s.load_allowlist(&dir);
        assert!(s.allowlist.skips_path("docs/deep/a.md"));
        assert!(s.allowlist.skips_rule("generic-secret-assignment"));
        assert!(s.allowlist.is_stopword("MY-PLACEHOLDER-KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_shipped_rule_compiles() {
        let rules = default_rules();
        assert!(rules.len() >= 14, "got {} rules", rules.len());
        for r in &rules {
            assert!(!r.id.is_empty());
        }
    }

    #[test]
    fn a_finding_carries_the_line_it_starts_on() {
        let s = scanner();
        let body = "one\ntwo\nkey = \"AKIA2E4F6G8H0J2K4L6M\"\nfour\n";
        let got = s.scan_text("a.py", body);
        assert_eq!(got[0].line, 3);
    }
    #[test]
    fn the_preview_of_a_finding_does_not_hold_the_secret() {
        // #74: `preview` was `truncate(line, 160)`, and 160 characters do not remove a secret - a
        // short line fits whole. The field's own doc says a log is not a place to put a secret.
        // Nothing printed it yet, which is why this was latent, and latent is how it would have
        // stayed until somebody added `--verbose` or a JSON field.
        // A made-up value in the shape the shipped rule matches, the same one as the entropy test.
        const FAKE: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a0";
        let s = scanner();
        let body = format!("api_key = \"{}\"\n", FAKE);
        let got = s.scan_text("b.txt", &body);
        assert_eq!(
            got.len(),
            1,
            "the shipped rule must match, or this tests nothing"
        );
        let f = &got[0];
        assert!(
            !f.preview.contains(FAKE),
            "the preview holds the secret: {}",
            f.preview
        );
        assert!(
            f.preview.contains("api_key"),
            "the preview must still say where in the line: {}",
            f.preview
        );
        assert!(!f.redacted.contains(FAKE));
        assert!(
            !f.to_string().contains(FAKE),
            "Display must not leak it either"
        );
    }

    #[test]
    fn a_committed_allowlist_that_turns_the_scanner_off_says_so() {
        // #77: `.secret-allowlist` is committed with the project and arrives with a clone. A repo
        // that lists every rule id silently disables the commit-time check for everyone working in
        // it, and the refusal that would have caught a real key never fires - with nothing said.
        //
        // stderr is read by re-running this test as a child with `--nocapture`; a test cannot read
        // its own stderr.
        if std::env::var("GESTA_LEAK_OFF_PROBE").is_ok() {
            let dir = std::env::temp_dir().join(format!("gesta-leak-off-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let mut text = String::new();
            for r in default_rules() {
                text.push_str(&format!("rule {}\n", r.id));
            }
            std::fs::write(dir.join(".secret-allowlist"), text).unwrap();
            let mut s = scanner();
            s.load_allowlist(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "support::leak::tests::a_committed_allowlist_that_turns_the_scanner_off_says_so",
                "--exact",
                "--nocapture",
            ])
            .env("GESTA_LEAK_OFF_PROBE", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("the secret scanner is off"),
            "a scanner disabled by a committed file must say so, stderr was: {err}"
        );
    }

    #[test]
    fn a_partial_allowlist_says_how_many_rules_are_left() {
        // The quieter case: a rule or two disabled is legitimate and common, but the operator should
        // still be able to see it without reading the allowlist.
        if std::env::var("GESTA_LEAK_PART_PROBE").is_ok() {
            let dir = std::env::temp_dir().join(format!("gesta-leak-part-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let first = default_rules()[0].id.clone();
            std::fs::write(dir.join(".secret-allowlist"), format!("rule {}\n", first)).unwrap();
            let mut s = scanner();
            s.load_allowlist(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "support::leak::tests::a_partial_allowlist_says_how_many_rules_are_left",
                "--exact",
                "--nocapture",
            ])
            .env("GESTA_LEAK_PART_PROBE", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("running") && err.contains("1 are disabled"),
            "a partial allowlist must be visible, stderr was: {err}"
        );
        assert!(
            !err.contains("the secret scanner is off"),
            "one disabled rule is not the scanner being off: {err}"
        );
    }
    #[test]
    fn a_project_key_is_found_and_so_is_every_github_prefix() {
        // #301: the openai rule required `T3BlbkFJ`, which is base64 for "OpenAI" and only the legacy
        // keys carry it. `sk-proj-...` therefore passed the scanner uncaught. The GitHub rule was
        // `ghp_`/`gho_`/`github_pat_` only, missing `ghu_`, `ghs_` and `ghr_` - the same secret in a
        // different envelope, and a scan that catches one and not the others calls a dirty tree clean.
        let s = Scanner::default();

        // A project key: `sk-proj-` and a long base62 tail, no `T3BlbkFJ`.
        let proj = format!("OPENAI_API_KEY=sk-proj-{}", "a1B2c3D4e5F6".repeat(4));
        let found = s.scan_text("x.py", &proj);
        assert!(
            found.iter().any(|f| f.rule == "openai-key"),
            "sk-proj- keys must be found, got: {:?}",
            found.iter().map(|f| &f.rule).collect::<Vec<_>>()
        );

        // The legacy shape still matches. The body has to look random: a run of one repeated
        // character is below the entropy floor, which is deliberate - `aaaa...` in a test fixture is
        // not a secret, and flagging it would be the noise the entropy check exists to remove.
        let legacy = format!(
            "sk-{}T3BlbkFJ{}",
            "aB3dE5gH7jK9mN1pQ3sT", "uV5xY7zA9cD1eF3gH5jK"
        );
        let found = s.scan_text("y.py", &legacy);
        assert!(
            found.iter().any(|f| f.rule == "openai-key"),
            "legacy keys still match"
        );

        // Every GitHub prefix, in the shape GitHub issues them.
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let line = format!("token: {prefix}{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
            let found = s.scan_text("z.yml", &line);
            assert!(
                found.iter().any(|f| f.rule == "github-pat"),
                "{prefix} must be caught, got: {:?}",
                found.iter().map(|f| &f.rule).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_reported_line_has_its_secrets_redacted() {
        // #302: `Stale.text` carried the first 160 characters of the source line with no redaction, so
        // a line that mentions a deleted name and also holds a secret value put the secret in the
        // report - a worse place for it than the file was.
        let line = format!(
            "api_key = \"sk-proj-{}\"  # uses old_helper",
            "a1B2c3D4e5F6".repeat(4)
        );
        let redacted = redact_line(&line);

        assert!(
            !redacted.contains("sk-proj-a1B2c3D4"),
            "the key must not appear in the clear: {redacted}"
        );
        assert!(
            redacted.contains("old_helper"),
            "the rest of the line survives, so the reader can still see what it said: {redacted}"
        );
        assert!(
            redacted.contains("****"),
            "and the secret is marked where it was: {redacted}"
        );

        // A line with nothing to hide comes back untouched.
        let plain = "call(old_helper)";
        assert_eq!(redact_line(plain), plain);
    }
}
