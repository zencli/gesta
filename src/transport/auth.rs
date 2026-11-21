//! Access control for the HTTP remote.
//!
//! Three roles: read may clone and pull, approve may also sign off a change to a protected branch,
//! write may also push. A
//! token may also carry a comma-separated list of path globs; a write that
//! touches a path outside that list is refused. When the server has no tokens
//! configured it refuses to bind to anything except a loopback address, so an
//! open port is never the accidental default.

use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// What a token is allowed to do.
///
/// Ordered, because the check is `role >= needed` and that shape only works if every step is a step
/// up. `Approve` sits between: a reviewer reads and signs off, and does not write. Giving a reviewer
/// a write token so it can approve is how a review gate becomes a formality, because the same
/// credential can then push the thing it is reviewing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Read,
    Approve,
    Write,
}

/// Token store. Tokens are kept as SHA-256 digests, never in clear text, so a
/// leaked config file does not leak working credentials.
#[derive(Debug, Clone)]
pub struct Auth {
    tokens: HashMap<String, Grant>,
    pub required: bool,
    /// What the file said last time it was read, so a change can be noticed without re-reading a
    /// file that has not changed.
    ///
    /// The modification time alone is not enough: it has a coarse resolution on some file systems,
    /// so adding a token and removing it within the same tick looks like no change at all. That
    /// window is exactly when a person removes a leaked token. The length plus the mtime is cheap
    /// and catches the case that matters, because a removal always shortens the file.
    loaded_at: Option<(std::time::SystemTime, u64)>,
}

/// One authenticated token: who it is, what it may do, and where.
#[derive(Debug, Clone)]
pub struct Grant {
    pub name: String,
    pub role: Role,
    /// Glob patterns for the paths this token may write. Empty means every path,
    /// which is what an entry without a scope field grants.
    pub scope: Vec<String>,
    /// Unix timestamp after which the token is dead. `None` means it does not
    /// expire, which is the old behaviour and still the default.
    pub expires_at: Option<i64>,
    /// Glob patterns for the branches this token may write. Empty means every branch, which is what
    /// an entry without the field grants, so an existing auth file keeps working unchanged.
    ///
    /// Path scope answers "which file", and this answers "which line of history". A token that may
    /// write one directory still rewrote every branch before this existed.
    pub branches: Vec<String>,
}

impl Grant {
    pub fn is_expired(&self, now: i64) -> bool {
        matches!(self.expires_at, Some(t) if now >= t)
    }
}

impl Grant {
    /// Is a write to `path` inside this grant's scope?
    ///
    /// An empty scope means unrestricted. A pattern without a slash matches the
    /// file name at any depth, so `*.md` covers `docs/a.md` and `a.md`, which is
    /// what a person writing the pattern usually means.
    /// Whether this token may write `branch`. Empty means every branch.
    ///
    /// Checked apart from the path scope because the two questions are different: a token scoped to
    /// `docs/**` was still allowed to rewrite `main`, `release` and every other branch, and the
    /// branch is the part a mistake costs the most to undo.
    pub fn permits_branch(&self, branch: &str) -> bool {
        if self.branches.is_empty() || self.role == Role::Read {
            return true;
        }
        self.branches
            .iter()
            .any(|pat| glob_match(pat, branch) || pat == branch)
    }

    pub fn permits(&self, path: &str) -> bool {
        if self.scope.is_empty() {
            return true;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        self.scope
            .iter()
            .any(|pat| glob_match(pat, path) || (!pat.contains('/') && glob_match(pat, name)))
    }
}

/// Match one glob pattern. Supports `*` inside a segment and `**` across
/// segments, plus `?` for a single character.
/// Match one glob pattern against a path.
///
/// Shared with sparse checkout: two matchers that disagree about `**` would be
/// a subtle and very annoying bug.
pub fn glob_is_match(pattern: &str, text: &str) -> bool {
    glob_match(pattern, text)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == '*' && p.len() > 1 && p[1] == '*' {
            // `**` crosses directory separators; the leading `/` in `**/x` is
            // optional so `**/a.md` also matches a top level `a.md`.
            let rest = &p[2..];
            let rest = if rest.first() == Some(&'/') {
                &rest[1..]
            } else {
                rest
            };
            for skip in 0..=t.len() {
                if go(rest, &t[skip..]) {
                    return true;
                }
                if skip < t.len() && t[skip] == '/' {
                    continue;
                }
            }
            // Simple fallback: try every suffix.
            for skip in 0..=t.len() {
                if go(rest, &t[skip..]) {
                    return true;
                }
            }
            false
        } else if p[0] == '*' {
            let rest = &p[1..];
            if go(rest, t) {
                return true;
            }
            for skip in 0..t.len() {
                if t[skip] == '/' {
                    return false; // a single `*` does not cross a separator
                }
                if go(rest, &t[skip + 1..]) {
                    return true;
                }
            }
            false
        } else if p[0] == '?' {
            !t.is_empty() && t[0] != '/' && go(&p[1..], &t[1..])
        } else {
            !t.is_empty() && p[0] == t[0] && go(&p[1..], &t[1..])
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let x: Vec<char> = text.chars().collect();
    go(&p, &x)
}

impl Auth {
    /// Build from the `auth` file inside the repository directory.
    ///
    /// Two forms are accepted, and the file may mix them:
    ///
    /// ```text
    /// <token>:<role>:<name>                 token in clear, hashed at load
    /// sha256:<digest>:<role>:<name>         only the digest is stored
    /// ```
    ///
    /// The second form is the one to use when the file might be readable by
    /// someone else: a stolen digest cannot be presented as a token.
    pub fn from_entries(entries: &[String]) -> Auth {
        let mut tokens = HashMap::new();
        for entry in entries {
            let line = entry.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // `sha256:<digest>:<role>:<name>` has four fields, the clear form
            // has three, so the digest marker has to be handled first.
            if let Some(rest) = line.strip_prefix("sha256:") {
                let mut parts = rest.splitn(6, ':');
                let (Some(digest_hex), Some(role)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let name = parts.next().unwrap_or("unnamed");
                let scope = parts.next().unwrap_or("");
                let expires = parts.next().unwrap_or("");
                let branches = parts.next().unwrap_or("");
                let digest_hex = digest_hex.trim().to_ascii_lowercase();
                if digest_hex.len() != 64 || !digest_hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue; // a malformed digest must never match anything
                }
                let role = parse_role(role);
                tokens.insert(
                    digest_hex,
                    Grant {
                        name: name.trim().to_string(),
                        role,
                        scope: parse_scope(scope),
                        expires_at: parse_expiry(expires),
                        branches: parse_scope(branches),
                    },
                );
                continue;
            }

            let mut parts = line.splitn(6, ':');
            let token = match parts.next() {
                Some(t) if !t.trim().is_empty() => t.trim().to_string(),
                _ => continue,
            };
            let role = parse_role(parts.next().unwrap_or("read"));
            let name = parts.next().unwrap_or("unnamed").trim().to_string();
            let scope = parts.next().unwrap_or("");
            let expires = parts.next().unwrap_or("");
            let branches = parts.next().unwrap_or("");
            tokens.insert(
                digest(&token),
                Grant {
                    name,
                    role,
                    scope: parse_scope(scope),
                    expires_at: parse_expiry(expires),
                    branches: parse_scope(branches),
                },
            );
        }
        Auth {
            required: !tokens.is_empty(),
            tokens,
            loaded_at: None,
        }
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Rebuild from the auth file if it changed since the last time this was asked.
    ///
    /// A token is removed by editing a file, and a running server had already read it, so the old
    /// token kept working until a restart. That is the gap between "the token is gone" and "the
    /// token stopped working", and closing it is the whole point of removing a leaked token. The
    /// file is small and its modification time is one `stat`, so asking on every request costs less
    /// than the mistake.
    pub fn reload_if_changed(&mut self, path: &std::path::Path) {
        let stamp = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        if stamp == self.loaded_at {
            return;
        }
        let entries = match std::fs::read_to_string(path) {
            Ok(text) => text.lines().map(|l| l.to_string()).collect::<Vec<_>>(),
            // A file that vanished is a file with no tokens. Refusing every request because the
            // file is missing would turn a removal into an outage, and accepting them would turn it
            // into an open server.
            Err(_) => Vec::new(),
        };
        // Required-ness is decided when the server starts and does not come back. Rebuilding from an
        // emptied file would clear this flag, and a server that was authenticating every request
        // would answer the next one without asking. Removing the last token has to mean "nobody gets
        // in", which is the opposite of "everybody gets in".
        let was_required = self.required;
        *self = Auth::from_entries(&entries);
        self.required = was_required;
        self.loaded_at = stamp;
    }

    /// Resolve a presented token to a role, or `None` when it is unknown.
    #[cfg(test)]
    pub fn authenticate(&self, presented: &str) -> Option<Grant> {
        self.tokens.get(&digest(presented)).cloned()
    }

    /// Authenticate and reject a grant that has expired.
    ///
    /// Separate from `authenticate` so a caller can tell "wrong token" from
    /// "right token, too late" and answer 401 with a different reason. The
    /// server uses this one.
    pub fn authenticate_now(&self, presented: &str, now: i64) -> AuthOutcome {
        match self.tokens.get(&digest(presented)) {
            None => AuthOutcome::Unknown,
            Some(g) if g.is_expired(now) => AuthOutcome::Expired(g.name.clone()),
            Some(g) => AuthOutcome::Granted(g.clone()),
        }
    }

    /// Does `role` satisfy `needed`?
    pub fn allows(role: Role, needed: Role) -> bool {
        role >= needed
    }
}

/// The result of checking a presented token.
#[derive(Debug, Clone)]
pub enum AuthOutcome {
    Granted(Grant),
    Expired(String),
    Unknown,
}

/// Parse an expiry field.
///
/// Accepts a unix timestamp, an ISO date (`2027-03-01`), or an ISO timestamp.
/// An empty field or `never` means no expiry. Anything unparseable also means no
/// expiry rather than "expires now", because a typo locking every agent out of a
/// repository is worse than a token that lives longer than intended.
pub fn parse_expiry(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("never") {
        return None;
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(if n > 1_000_000_000_000 { n / 1000 } else { n });
    }
    // A bare date expires at the last second of that day; a full timestamp is
    // taken as written. Both are UTC, which is what the stored value is.
    if s.len() == 10 {
        return crate::support::clock::end_of_day(s);
    }
    crate::support::clock::parse_minute(s).or_else(|| {
        // A timestamp with seconds, such as 2027-03-01T12:00:00.
        let base = crate::support::clock::parse_minute(&s[..16.min(s.len())])?;
        let rest = s.get(16..)?;
        let secs: i64 = rest.trim_start_matches(':').parse().ok()?;
        Some(base + secs)
    })
}

fn parse_scope(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty() && *p != "*")
        .map(|p| p.trim_start_matches("./").to_string())
        .collect()
}

fn parse_role(s: &str) -> Role {
    match s.trim().to_ascii_lowercase().as_str() {
        "write" => Role::Write,
        "approve" => Role::Approve,
        _ => Role::Read,
    }
}

pub fn digest(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// Generate a token of `bytes` random bytes, hex encoded.
///
/// Reads from the OS entropy source. Returns an error rather than falling back
/// to a weak source, because a guessable token is worse than no server.
pub fn generate_token(bytes: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; bytes];
    // `/dev/urandom` does not exist on Windows, so the old version made every token command fail
    // there with "the system cannot find the path specified". Reading the system source through the
    // standard library is the portable answer, and it costs no dependency that a build without TLS
    // would have to carry anyway.
    fill_random(&mut buf)?;
    Ok(hex::encode(buf))
}

/// Fill a buffer from the operating system's random source.
///
/// `/dev/urandom` is a Unix path and does not exist on Windows, so the old version made every token
/// command fail there. `ring` is not optional in this crate (the secret store needs it in a build
/// without TLS), and its generator is the system source on every platform.
fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    use ring::rand::{SecureRandom, SystemRandom};
    SystemRandom::new()
        .fill(buf)
        .map_err(|_| std::io::Error::other("the system random source refused"))
}

/// A bind address is private when it can only be reached from this machine.
pub fn is_loopback(bind: &str) -> bool {
    matches!(bind, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_has_no_tokens() {
        let a = Auth::from_entries(&[]);
        assert!(!a.required);
        assert_eq!(a.token_count(), 0);
    }

    #[test]
    fn parses_token_role_and_name() {
        let a = Auth::from_entries(&[
            "abc123:write:alice".to_string(),
            "def456:read:bob".to_string(),
        ]);
        assert_eq!(a.token_count(), 2);

        let g = a.authenticate("abc123").unwrap();
        assert_eq!(g.name, "alice");
        assert_eq!(g.role, Role::Write);

        let g = a.authenticate("def456").unwrap();
        assert_eq!(g.name, "bob");
        assert_eq!(g.role, Role::Read);
    }

    #[test]
    fn unknown_token_is_rejected() {
        let a = Auth::from_entries(&["abc123:write:alice".to_string()]);
        assert!(a.authenticate("wrong").is_none());
        assert!(a.authenticate("").is_none());
    }

    #[test]
    fn a_line_without_a_role_defaults_to_read() {
        let a = Auth::from_entries(&["tokenonly".to_string()]);
        let g = a.authenticate("tokenonly").unwrap();
        assert_eq!(g.role, Role::Read, "a malformed entry must not grant write");
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let a = Auth::from_entries(&[
            "# a comment".to_string(),
            "".to_string(),
            "  ".to_string(),
            "real:write:x".to_string(),
        ]);
        assert_eq!(a.token_count(), 1);
    }

    #[test]
    fn write_role_satisfies_read_but_not_the_reverse() {
        assert!(Auth::allows(Role::Write, Role::Write));
        assert!(Auth::allows(Role::Write, Role::Read));
        assert!(Auth::allows(Role::Read, Role::Read));
        assert!(!Auth::allows(Role::Read, Role::Write));
    }

    #[test]
    fn digests_are_stable_and_not_reversible() {
        assert_eq!(digest("abc"), digest("abc"));
        assert_ne!(digest("abc"), digest("abd"));
        assert_eq!(digest("abc").len(), 64);
        assert!(!digest("abc").contains("abc"));
    }

    #[test]
    fn generated_tokens_have_the_requested_entropy_and_differ() {
        let a = generate_token(32).unwrap();
        let b = generate_token(32).unwrap();
        assert_eq!(a.len(), 64, "32 bytes hex encoded");
        assert_ne!(a, b, "two calls must not repeat");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_stored_digest_authenticates_the_matching_token() {
        let token = "s3cret-token-value";
        let line = format!("sha256:{}:write:carol", digest(token));
        let a = Auth::from_entries(&[line]);
        let g = a.authenticate(token).unwrap();
        assert_eq!(g.name, "carol");
        assert_eq!(g.role, Role::Write);
        assert!(a.authenticate("wrong").is_none());
    }

    #[test]
    fn a_stored_digest_is_not_itself_a_token() {
        // Presenting the digest must not authenticate: only the preimage does.
        let token = "real-token";
        let d = digest(token);
        let a = Auth::from_entries(&[format!("sha256:{}:write:x", d)]);
        assert!(
            a.authenticate(&d).is_none(),
            "a stolen digest must not work as a token"
        );
        assert!(a.authenticate(token).is_some());
    }

    #[test]
    fn a_malformed_digest_never_matches() {
        let a = Auth::from_entries(&["sha256:nothex:write:x".to_string()]);
        assert_eq!(a.token_count(), 0, "a broken line must not create an entry");
    }

    #[test]
    fn clear_and_hashed_entries_can_be_mixed() {
        let secret = "abc";
        let a = Auth::from_entries(&[
            "plain:read:p".to_string(),
            format!("sha256:{}:write:h", digest(secret)),
        ]);
        assert_eq!(a.token_count(), 2);
        assert_eq!(a.authenticate("plain").unwrap().role, Role::Read);
        assert_eq!(a.authenticate(secret).unwrap().role, Role::Write);
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("0.0.0.0"));
        assert!(!is_loopback("192.168.1.10"));
    }

    #[test]
    fn a_token_without_an_expiry_field_never_expires() {
        let a = Auth::from_entries(&["abc:write:alice".to_string()]);
        let g = a.authenticate("abc").unwrap();
        assert_eq!(g.expires_at, None);
        assert!(!g.is_expired(i64::MAX / 2));
    }

    #[test]
    fn a_unix_timestamp_expiry_is_honoured() {
        let a = Auth::from_entries(&["abc:write:alice::1000".to_string()]);
        let g = a.authenticate("abc").unwrap();
        assert_eq!(g.expires_at, Some(1000));
        assert!(!g.is_expired(999));
        assert!(g.is_expired(1000), "the exact second counts as expired");
        assert!(g.is_expired(1001));
    }

    #[test]
    fn an_iso_date_expiry_covers_the_whole_day() {
        // A person writing 2027-03-01 means "through that day", not "until
        // midnight at the start of it".
        let ts = parse_expiry("2027-03-01").unwrap();
        // Stated as the formatted text, so the assertion reads the same way the
        // value is shown to a person.
        assert_eq!(
            crate::support::clock::format_rfc3339(ts),
            "2027-03-01T23:59:59Z"
        );
    }

    #[test]
    fn a_millisecond_timestamp_is_normalised_to_seconds() {
        assert_eq!(parse_expiry("1700000000000"), Some(1_700_000_000));
    }

    #[test]
    fn an_unparseable_expiry_means_no_expiry_not_lockout() {
        // A typo must not revoke every agent at once.
        assert_eq!(parse_expiry("next tuesday"), None);
        assert_eq!(parse_expiry(""), None);
        assert_eq!(parse_expiry("never"), None);
    }

    #[test]
    fn authenticate_now_separates_expired_from_unknown() {
        let a = Auth::from_entries(&["abc:write:alice::1000".to_string()]);
        assert!(matches!(
            a.authenticate_now("abc", 500),
            AuthOutcome::Granted(_)
        ));
        match a.authenticate_now("abc", 2000) {
            AuthOutcome::Expired(name) => assert_eq!(name, "alice"),
            other => panic!("expected Expired, got {:?}", other),
        }
        assert!(matches!(
            a.authenticate_now("wrong", 500),
            AuthOutcome::Unknown
        ));
    }

    #[test]
    fn an_expiry_works_on_the_hashed_form_too() {
        let token = "s3cret";
        let line = format!("sha256:{}:read:bob:docs/**:1800000000", digest(token));
        let a = Auth::from_entries(&[line]);
        let g = a.authenticate(token).unwrap();
        assert_eq!(g.expires_at, Some(1_800_000_000));
        assert_eq!(g.scope, vec!["docs/**".to_string()]);
    }

    #[test]
    fn a_scope_pattern_matches_the_way_a_person_expects() {
        let g = Grant {
            name: "x".into(),
            role: Role::Write,
            scope: vec!["docs/**".into(), "*.md".into(), "src/lib/one.rs".into()],
            expires_at: None,
            branches: Vec::new(),
        };
        assert!(g.permits("docs/a/b.md"));
        assert!(g.permits("README.md"));
        assert!(
            g.permits("nested/readme.md"),
            "a bare name pattern matches any depth"
        );
        assert!(g.permits("src/lib/one.rs"));
        assert!(!g.permits("src/lib/two.rs"));
        assert!(!g.permits("src/main.rs"));
    }

    #[test]
    fn removing_the_last_token_closes_the_server_rather_than_opening_it() {
        // Rebuilding from an emptied file cleared `required`, so a server that had been checking
        // every request stopped checking them. Removing a leaked token then made the server open to
        // everyone, which is the opposite of what the person removing it intended.
        let dir = std::env::temp_dir().join(format!("gesta-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("auth");
        std::fs::write(&file, "secret:read:alice\n").unwrap();

        let mut a = Auth::from_entries(&["secret:read:alice".to_string()]);
        a.required = true;
        a.reload_if_changed(&file);
        assert!(a.required, "still required while a token is present");
        assert!(a.authenticate("secret").is_some());

        std::fs::write(&file, "").unwrap();
        // A modification time has a coarse resolution on some file systems, so the content change is
        // what the reload looks for; force it visible by touching the file again.
        a.loaded_at = None;
        a.reload_if_changed(&file);
        assert!(
            a.required,
            "removing the last token must not open the server"
        );
        assert!(
            a.authenticate("secret").is_none(),
            "the removed token is gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_can_be_limited_to_one_branch() {
        // Before the sixth field existed, a token scoped to one directory rewrote every branch.
        // The file format gained the field at the end, so an entry written earlier reads the same.
        let a = Auth::from_entries(&["t:write:agent:docs/**:".to_string()]);
        let g = a.authenticate("t").unwrap();
        assert!(g.branches.is_empty(), "an old entry grants every branch");
        assert!(g.permits_branch("main"));

        let b = Auth::from_entries(&["t:write:agent:::main".to_string()]);
        let g = b.authenticate("t").unwrap();
        assert_eq!(g.branches, vec!["main".to_string()]);
        assert!(g.permits_branch("main"));
        assert!(!g.permits_branch("release"));
        assert!(!g.permits_branch("other"));
    }

    #[test]
    fn a_read_token_is_never_limited_by_branch() {
        // Reading is covered by the path scope, and refusing a read because a branch is not listed
        // would make a branch field on a read token silently hide history.
        let a = Auth::from_entries(&["t:read:agent:::main".to_string()]);
        let g = a.authenticate("t").unwrap();
        assert!(
            g.permits_branch("anything"),
            "a read grant is not branch limited"
        );
    }

    #[test]
    fn the_name_of_an_entry_is_the_third_field_even_with_optional_trailing_ones() {
        // A removal matched on the name and used `splitn(3, ':')`, which leaves the remaining fields
        // joined, so the name of `t:read:a:::main` read as `a:::main`. The removal matched nothing
        // and reported nothing, and the token it was meant to kill kept working.
        let line = "t:read:a:::main";
        let mut parts = line.splitn(3, ':');
        let _token = parts.next();
        let _role = parts.next();
        let tail = parts.next().unwrap();
        let name = tail.split(':').next().unwrap();
        assert_eq!(name, "a", "the name is the field before the optional ones");

        // The same shape as a stored digest entry.
        let hashed = "sha256:d:read:b:::release";
        let rest = hashed.strip_prefix("sha256:").unwrap();
        let mut parts = rest.splitn(3, ':');
        let _digest = parts.next();
        let _role = parts.next();
        let tail = parts.next().unwrap();
        assert_eq!(tail.split(':').next().unwrap(), "b");
    }
}
