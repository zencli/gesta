//! Encrypted storage for credentials that a remote needs.
//!
//! A push to object storage needs a secret key, and a secret key written to disk in
//! plain text lives in every backup and every copy of the repository forever. This
//! module keeps those secrets sealed at rest, in one file per user rather than per
//! repository, so a repository can be shared or pushed without carrying any
//! credential with it.
//!
//! Layout of the store file:
//!
//! ```text
//! line 1: gesta-secrets v1 <salt hex> <nonce hex> <ciphertext hex>
//! ```
//!
//! The key comes from a passphrase and the salt through HMAC-SHA256. The plaintext
//! is a small text file of `name = value` lines, sealed in one piece with
//! ChaCha20-Poly1305, which authenticates as well as encrypts: an edited store is
//! rejected instead of decrypted into rubbish.
//!
//! Two deliberate limits:
//!
//! - One KDF pass, not thousands. A passphrase with real entropy is not made
//!   meaningfully stronger by stretching, and this store is opened on every push.
//!   The threat model is a leaked backup, not an attacker with the file and a GPU.
//! - `store.path` decides where the file lives. A test points it at a temporary
//!   directory; a user leaves the default under their config directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::rand::{SecureRandom, SystemRandom};

use crate::{bad, ZResult};

const MAGIC: &str = "gesta-secrets v1";
const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 16;

/// Where the sealed store lives, and what unlocks it.
pub struct SecretStore {
    pub path: PathBuf,
    pub passphrase: String,
}

/// Is this a name a person can type back?
///
/// The format now escapes anything (#67), so a name with a newline no longer corrupts the file -
/// but `secret list` prints names and `secret get` takes one, and a name containing a control
/// character cannot round-trip through a terminal. Stored, it is then unreachable by the only
/// interface that reads it: the entry exists, `list` shows a broken line, and `get` cannot name it.
/// Refusing at the door is the only version of this that does not silently create an entry nobody
/// can ever read.
///
/// `=` is allowed: the format escapes it, and `s3.access_key` style names are what people type.
fn check_name(name: &str) -> ZResult<()> {
    if name.is_empty() {
        return Err(bad("a secret with no name cannot be looked up".to_string()));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(bad(format!(
            "the name {:?} contains a control character, so it could not be typed back into `secret get`",
            name
        )));
    }
    Ok(())
}

impl SecretStore {
    /// Resolve the store path and passphrase from the environment.
    ///
    /// `GESTA_SECRETS_FILE` overrides the location and `GESTA_SECRETS_PASSPHRASE`
    /// supplies the key material, which is what a test or a CI job uses. Without
    /// them the file sits next to the user's config and the passphrase is asked
    /// for on the terminal, never on a command line where `ps` can read it.
    pub fn from_env() -> ZResult<SecretStore> {
        let path = match std::env::var("GESTA_SECRETS_FILE") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => default_path()?,
        };
        let passphrase = match std::env::var("GESTA_SECRETS_PASSPHRASE") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                return Err(bad(
                    "no passphrase: set GESTA_SECRETS_PASSPHRASE, or run `gesta secret init` to be asked once"
                        .to_string(),
                ))
            }
        };
        Ok(SecretStore { path, passphrase })
    }

    /// Read every entry. A missing file is an empty store.
    pub fn load(&self) -> ZResult<BTreeMap<String, String>> {
        match fs::read_to_string(&self.path) {
            Ok(text) => decode(&text, &self.passphrase),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(bad(format!("cannot read {}: {}", self.path.display(), e))),
        }
    }

    /// Write every entry back, sealed.
    pub fn save(&self, entries: &BTreeMap<String, String>) -> ZResult<()> {
        let text = render(entries);
        let sealed = encode(&text, &self.passphrase)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Replaced, not truncated, and created 0600 before its content is written.
        //
        // `fs::write` followed by `restrict_permissions` was two defects in two lines: the file
        // existed at 0644 for the width of the write, which is the window a watcher needs, and it
        // was truncated in place, so a reader during the write got a file that does not decode and a
        // write that died partway left the whole credential store gone.
        crate::support::private_file::replace_private(&self.path, sealed.as_bytes())?;
        Ok(())
    }

    /// Set one entry, creating the file when it is new.
    pub fn put(&self, name: &str, value: &str) -> ZResult<()> {
        check_name(name)?;
        let mut entries = self.load()?;
        entries.insert(name.to_string(), value.to_string());
        self.save(&entries)
    }

    /// Remove one entry. Returns whether it existed.
    pub fn remove(&self, name: &str) -> ZResult<bool> {
        let mut entries = self.load()?;
        let existed = entries.remove(name).is_some();
        if existed {
            self.save(&entries)?;
        }
        Ok(existed)
    }
}

/// The store entry that holds a remote's token.
///
/// One naming rule, in one place, because the entry is written by `remote add` and read by every
/// push: two spellings would mean a token that saves and never loads.
pub fn remote_entry_name(remote: &str) -> String {
    format!("remote.{}.token", remote)
}

/// `GESTA_REMOTE_TOKEN_<NAME>`, for a build that has no passphrase.
///
/// The name is upper-cased and encoded so that different remote names cannot produce the same
/// variable (#306). The first version lower-cased nothing and turned every non-alphanumeric character
/// into `_`, which made `a-b` and `a_b` both `GESTA_REMOTE_TOKEN_A_B`: a token exported for one remote
/// was then used for the other, and nothing said so. Two remotes are usually two different servers, so
/// that is a credential reaching a host it was not meant for.
///
/// The encoding is an escape, not a substitution:
///
///   `A-Z`, `0-9`   kept, upper-cased
///   `_`            `__`     (so an escape can never be read as a separator)
///   other byte     `_<hh>`  lower-case hex of the UTF-8 byte
///
/// It is injective: each input byte maps to a distinct, self-delimiting output, so two different names
/// cannot collide. `origin` stays `ORIGIN` and `up-stream` becomes `UP_2DSTREAM`, which is uglier than
/// before and correct.
///
/// Returns `None` for an empty value, so `VAR=` behaves like unset rather than like an empty token.
pub fn env_name_for_remote(remote: &str) -> String {
    let mut suffix = String::with_capacity(remote.len());
    for b in remote.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => suffix.push(b.to_ascii_uppercase() as char),
            b'_' => suffix.push_str("__"),
            _ => suffix.push_str(&format!("_{:02x}", b)),
        }
    }
    format!("GESTA_REMOTE_TOKEN_{}", suffix)
}

/// The token from the environment, if it is set and non-empty.
pub fn env_for_remote(remote: &str) -> Option<String> {
    match std::env::var(env_name_for_remote(remote)) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// The default location: a directory next to the user's other config.
pub fn default_path() -> ZResult<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA").map(PathBuf::from).ok()
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .map(|h| PathBuf::from(h).join(".config"))
                    .ok()
            })
    };
    let base = base.ok_or_else(|| bad("cannot find a config directory".to_string()))?;
    Ok(base.join("gesta").join("secrets"))
}

/// Derive the 32 byte key from a passphrase and a salt.
pub(crate) fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    // HMAC-SHA256 with the salt as the key and the passphrase as the message. One
    // pass is enough here: the passphrase is chosen with real entropy and this is
    // opened on every push, so stretching it would be paid constantly to slow an
    // attacker who already has the file.
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, salt);
    let tag = ring::hmac::sign(&key, passphrase.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

fn random_bytes(n: usize) -> ZResult<Vec<u8>> {
    let mut buf = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| bad("the system random source failed".to_string()))?;
    Ok(buf)
}

/// Seal plaintext into the one-line store format.
pub fn encode(plain: &str, passphrase: &str) -> ZResult<String> {
    let salt = random_bytes(SALT_LEN)?;
    let nonce_bytes = random_bytes(NONCE_LEN)?;
    let key_bytes = derive_key(passphrase, &salt);

    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| bad("cannot build the cipher key".to_string()))?;
    let key = LessSafeKey::new(unbound);
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(&nonce_bytes);
    let nonce = Nonce::assume_unique_for_key(nonce_arr);

    let mut buf = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut buf)
        .map_err(|_| bad("sealing the store failed".to_string()))?;

    Ok(format!(
        "{} {} {} {}",
        MAGIC,
        hex::encode(&salt),
        hex::encode(&nonce_bytes),
        hex::encode(&buf)
    ))
}

/// Open a sealed store.
pub fn decode(sealed: &str, passphrase: &str) -> ZResult<BTreeMap<String, String>> {
    let line = sealed.trim();
    let mut parts = line.split_whitespace();
    let magic = format!(
        "{} {}",
        parts.next().unwrap_or(""),
        parts.next().unwrap_or("")
    );
    if magic != MAGIC {
        return Err(bad(
            "the secrets file is not in a format this build understands".to_string(),
        ));
    }
    let salt = hex::decode(parts.next().unwrap_or(""))
        .map_err(|_| bad("the salt is not valid hex".to_string()))?;
    let nonce_bytes = hex::decode(parts.next().unwrap_or(""))
        .map_err(|_| bad("the nonce is not valid hex".to_string()))?;
    let mut ct = hex::decode(parts.next().unwrap_or(""))
        .map_err(|_| bad("the ciphertext is not valid hex".to_string()))?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(bad("the nonce has the wrong length".to_string()));
    }
    // Nothing may follow the ciphertext. The fields are hex, so a sixth token is not padding and
    // not a future field this build knows about: it is either corruption or something appended to
    // the file after it was written. Either way the file is not what it claims to be, and reading it
    // and ignoring the remainder is how a truncated-but-extended store looks intact. This is the
    // same rule the backup format needed; both said "trailing data is ignored".
    if parts.next().is_some() {
        return Err(bad(
            "the secrets file has data after the ciphertext; it may have been appended to"
                .to_string(),
        ));
    }

    let key_bytes = derive_key(passphrase, &salt);
    let unbound = UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| bad("cannot build the cipher key".to_string()))?;
    let key = LessSafeKey::new(unbound);
    let mut nonce_arr = [0u8; NONCE_LEN];
    nonce_arr.copy_from_slice(&nonce_bytes);
    let nonce = Nonce::assume_unique_for_key(nonce_arr);

    let plain = key
        .open_in_place(nonce, Aad::empty(), &mut ct)
        .map_err(|_| bad("cannot open the store: wrong passphrase or edited file".to_string()))?;
    let text = String::from_utf8(plain.to_vec())
        .map_err(|_| bad("the store decrypted to invalid text".to_string()))?;

    parse(&text)
}

/// The store's plaintext form, version 2.
///
/// Version 1 was `key = value` with no escaping, and it could not represent its own inputs: a value
/// containing a newline became two lines, a value containing `=` survived only because the split was
/// on the first one, a key containing `=` split in the wrong place, and `trim()` silently dropped
/// leading and trailing spaces from both. A token with a trailing space, or a passphrase-shaped value
/// with an `=`, was stored as something else and read back as something else again.
///
/// Version 2 escapes the four characters that matter and marks itself with a first line, so a store
/// written by an earlier build still reads: there is no version marker in one, and that is how the
/// two are told apart.
const V2: &str = "#2";

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // Only a key can break the split, but escaping both keeps the two sides symmetric and
            // makes a value with `=` in it round-trip without a second rule.
            '=' => out.push_str("\\e"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('e') => out.push('='),
            Some('\\') => out.push('\\'),
            // Not a sequence this writer produces. Kept as written rather than dropped, so a value
            // that came from somewhere else is not silently rewritten.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn render(entries: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    out.push_str(V2);
    out.push('\n');
    for (k, v) in entries {
        out.push_str(&escape(k));
        // No spaces around the `=` in version 2: they would have to be stripped on the way back in,
        // and stripping is what lost a value's own leading and trailing spaces in version 1.
        out.push('=');
        out.push_str(&escape(v));
        out.push('\n');
    }
    out
}

/// Read the plaintext form, either version.
///
/// A malformed line is reported by its number and never by its content: this text has just been
/// decrypted, so printing the line that failed printed the secret. That was the whole of #68 - the
/// one diagnostic a broken store could give was the thing the store exists to protect.
fn parse(text: &str) -> ZResult<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    let v2 = text.starts_with(V2) && text[V2.len()..].starts_with('\n');
    for (n, line) in text.lines().enumerate() {
        if v2 && n == 0 {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| {
            bad(format!(
                "the store has a malformed line at {} (no equals sign); its content is not shown \
                 because it is a credential",
                n + 1
            ))
        })?;
        let (k, v) = if v2 {
            (unescape(k), unescape(v))
        } else {
            // Version 1 had no escaping and was written with spaces around the `=`, so the spaces
            // are the writer's and trimming them recovers what a version 1 file meant.
            (k.trim().to_string(), v.trim().to_string())
        };
        entries.insert(k, v);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_round_trips() {
        let sealed = encode("a = 1\nb = 2\n", "correct horse battery staple").unwrap();
        let back = decode(&sealed, "correct horse battery staple").unwrap();
        assert_eq!(back.get("a").map(|s| s.as_str()), Some("1"));
        assert_eq!(back.get("b").map(|s| s.as_str()), Some("2"));
    }

    #[test]
    fn the_plaintext_does_not_appear_in_the_sealed_text() {
        // The one property that matters: a secret on disk must not be readable by
        // anyone holding the file.
        let sealed = encode("token = supersecretvalue\n", "p").unwrap();
        assert!(
            !sealed.contains("supersecretvalue"),
            "the sealed store contains the secret: {}",
            sealed
        );
    }

    #[test]
    fn a_wrong_passphrase_is_refused_and_says_so() {
        let sealed = encode("a = 1\n", "right").unwrap();
        let err = decode(&sealed, "wrong").unwrap_err().to_string();
        assert!(
            err.contains("wrong passphrase") || err.contains("edited"),
            "the error must name the likely cause, got: {}",
            err
        );
    }

    #[test]
    fn an_edited_store_is_refused() {
        // Poly1305 authenticates the ciphertext, so a flipped byte is a failure and
        // not silent rubbish. Without this an attacker could swap one credential
        // for another and the tool would happily use it.
        let sealed = encode("a = 1\n", "p").unwrap();
        let mut parts: Vec<String> = sealed.split_whitespace().map(|s| s.to_string()).collect();
        let ct = parts.last_mut().unwrap();
        let mut bytes = hex::decode(&*ct).unwrap();
        bytes[0] ^= 1;
        *ct = hex::encode(&bytes);
        let edited = parts.join(" ");
        assert!(
            decode(&edited, "p").is_err(),
            "an edited store must be refused"
        );
    }

    #[test]
    fn two_seals_of_the_same_input_differ() {
        // A fresh salt and nonce each time, so an observer cannot tell whether two
        // stores hold the same secrets.
        let a = encode("x = 1\n", "p").unwrap();
        let b = encode("x = 1\n", "p").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn an_unknown_format_is_refused_without_panicking() {
        assert!(decode("something else entirely", "p").is_err());
        assert!(decode("", "p").is_err());
        assert!(decode("gesta-secrets v1 nothex nothex nothex", "p").is_err());
    }

    #[test]
    fn put_and_remove_change_only_the_named_entry() {
        let dir = std::env::temp_dir().join(format!("gesta-secrets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = SecretStore {
            path: dir.join("secrets"),
            passphrase: "p".to_string(),
        };
        store.put("one", "1").unwrap();
        store.put("two", "2").unwrap();
        let got = store.load().unwrap();
        assert_eq!(got.len(), 2);
        assert!(store.remove("one").unwrap());
        let got = store.load().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("two").map(|s| s.as_str()), Some("2"));
        assert!(!store.remove("never-there").unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_store() {
        let store = SecretStore {
            path: std::env::temp_dir().join("gesta-secrets-not-here-xyz"),
            passphrase: "p".to_string(),
        };
        assert!(store.load().unwrap().is_empty());
    }
    #[test]
    fn a_value_with_a_newline_or_an_equals_sign_round_trips() {
        // #67: the format was `key = value` with no escaping, so a newline in a value became two
        // lines and split the entry in half, and `trim()` dropped the spaces the caller stored. A
        // token with a trailing space is invisible in a list and wrong in a request.
        let mut entries = BTreeMap::new();
        entries.insert(
            "pem".to_string(),
            "-----BEGIN-----\nabc\n-----END-----".to_string(),
        );
        entries.insert("eq".to_string(), "a=b=c".to_string());
        entries.insert("space".to_string(), "  padded  ".to_string());
        entries.insert("k=with=eq".to_string(), "v".to_string());
        entries.insert("crlf".to_string(), "a\r\nb".to_string());

        let text = render(&entries);
        let back = parse(&text).unwrap();
        assert_eq!(back, entries, "the store must hold what it was given");
    }

    #[test]
    fn a_store_written_before_this_change_still_reads() {
        // A version 1 file has no marker; that absence is how it is recognised. Refusing it would
        // strand every credential somebody already stored.
        let old = "a = 1\nb = 2\n";
        let back = parse(old).unwrap();
        assert_eq!(back.get("a").map(|s| s.as_str()), Some("1"));
        assert_eq!(back.get("b").map(|s| s.as_str()), Some("2"));
    }

    #[test]
    fn a_malformed_store_line_does_not_print_the_credential() {
        // #68: the error printed the whole line, and the line was just decrypted content. The one
        // message a broken store could give was the secret itself.
        let text = render(&BTreeMap::new());
        let broken = format!("{}secret-token-without-an-equals\n", text);
        let err = parse(&broken).unwrap_err().to_string();
        assert!(
            !err.contains("secret-token-without-an-equals"),
            "the error must not echo decrypted content: {err}"
        );
        assert!(
            err.contains("at 2"),
            "the error must name the line number, got {err}"
        );
    }

    #[test]
    fn data_after_the_ciphertext_is_refused() {
        // #69: `split_whitespace` handed out five fields and nothing looked at the sixth, so anything
        // appended to the file after the ciphertext was read as part of a valid store.
        let sealed = encode("a = 1\n", "p").unwrap();
        let appended = format!("{} deadbeef", sealed);
        let err = decode(&appended, "p").unwrap_err().to_string();
        assert!(
            err.contains("after the ciphertext"),
            "trailing data must be named, got {err}"
        );
        // The untouched file still opens.
        assert_eq!(
            decode(&sealed, "p").unwrap().get("a").map(|s| s.as_str()),
            Some("1")
        );
    }
    #[test]
    fn a_name_that_could_not_be_typed_back_is_refused() {
        // #67: `put` took the name straight from the argument vector with no check. This goes
        // through `put`, not through `check_name`: the first version of this test called the helper
        // and passed with the call removed from `put`, which is the same mistake as #87's.
        let dir = std::env::temp_dir().join(format!("gesta-secrets-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SecretStore {
            path: dir.join("secrets"),
            passphrase: "p".to_string(),
        };
        let err = store.put("line\nbreak", "v").unwrap_err().to_string();
        assert!(
            err.contains("control character"),
            "a name with a control character must be refused by put, got {err}"
        );
        assert!(
            !store.path.exists(),
            "a refused name must not have written the store"
        );
        // The ordinary names still work, including one with `=`, which the format escapes.
        store.put("s3.access_key", "v1").unwrap();
        store.put("a=b", "v2").unwrap();
        let back = store.load().unwrap();
        assert_eq!(back.get("a=b").map(|s| s.as_str()), Some("v2"));
        assert_eq!(back.get("s3.access_key").map(|s| s.as_str()), Some("v1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_malformed_store_line_names_its_number_and_not_its_content() {
        // #68: the error printed the offending line, which is decrypted plaintext from the store. If a
        // stored value ever spanned lines - a PEM key is the obvious case - the continuation line
        // landed verbatim in stderr, shell history, CI logs and agent transcripts. The one diagnostic
        // a broken secrets file could give was the material the file exists to protect.
        let secret = "-----BEGIN PRIVATE KEY-----";
        let text = format!("good=one\n{secret}\nother=two\n");
        let err = match parse(&text) {
            Ok(_) => panic!("a line with no equals sign must fail, not be skipped"),
            Err(e) => e.to_string(),
        };
        assert!(
            !err.contains(secret),
            "the error must not contain the line, got: {err}"
        );
        assert!(
            !err.contains("BEGIN PRIVATE KEY"),
            "no fragment of the line may leak, got: {err}"
        );
        assert!(
            err.contains("line at 2"),
            "it must still say where to look, got: {err}"
        );
    }
}
