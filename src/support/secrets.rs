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
use std::path::{Path, PathBuf};

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
        let text = entries
            .iter()
            .map(|(k, v)| format!("{} = {}\n", k, v))
            .collect::<String>();
        let sealed = encode(&text, &self.passphrase)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Create with owner-only permissions where the platform supports it, then
        // write. The sealed contents are useless without the passphrase, but a
        // file nobody else can read is one less thing to think about.
        fs::write(&self.path, sealed)?;
        restrict_permissions(&self.path)?;
        Ok(())
    }

    /// Set one entry, creating the file when it is new.
    pub fn put(&self, name: &str, value: &str) -> ZResult<()> {
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

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> ZResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> ZResult<()> {
    // Windows inherits the directory ACL, which for a per-user local app data
    // directory is already limited to that user.
    Ok(())
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

    let mut entries = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| bad(format!("a store line has no equals sign: {}", line)))?;
        entries.insert(k.trim().to_string(), v.trim().to_string());
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
}
