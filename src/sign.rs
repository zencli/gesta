//! Signed changesets.
//!
//! A commit message says who wrote a change. Without a signature that claim is whatever the
//! author typed, which is fine on one machine and worthless the moment a recorded history is
//! shared. Ed25519 over the changeset's own identity gives a verifier something to check that
//! does not depend on trusting the writer.
//!
//! What is signed is the changeset id plus its message, author and parent. The id already
//! commits to the tree, so signing it covers the content without walking the tree again.
//!
//! ```text
//! $ gesta key generate
//! created a signing key: /home/me/.gesta/signing.key
//! public key: 3f2a...c1   add this to the people you want to verify
//!
//! $ gesta changeset create "fix the parser" --author me --sign
//! signed with 3f2a...c1
//! ```
//!
//! The private key is one file, mode 0600, and it never leaves the machine. This is not a
//! replacement for a hardware token: a compromised account defeats it exactly as it defeats an
//! ssh key, and the docs say so.

use crate::{bad, ZResult};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use std::path::{Path, PathBuf};

/// Where the signing key lives when no path is given.
pub fn default_key_path() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".gesta").join("signing.key")
}

/// The exact bytes a signature covers.
///
/// Every field that identifies the change is here, and the separator makes a field that shifts
/// from one position to another produce a different payload rather than the same one.
pub fn signed_payload(id: &str, message: &str, author: &str, parent: Option<&str>) -> Vec<u8> {
    format!(
        "gesta-changeset-v1\nid:{}\nparent:{}\nauthor:{}\nmessage:{}",
        id,
        parent.unwrap_or("-"),
        author,
        message
    )
    .into_bytes()
}

/// A key pair on disk: the private seed, hex encoded, three lines of context.
pub struct SigningKey {
    pair: Ed25519KeyPair,
    pub path: PathBuf,
}

impl SigningKey {
    /// Generate a new key and write it, refusing to overwrite an existing one.
    pub fn generate(path: &Path) -> ZResult<SigningKey> {
        if path.exists() {
            return Err(bad(format!(
                "{} already exists: move it aside rather than replacing it, or every signature it made becomes unverifiable",
                path.display()
            )));
        }
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| bad("the system random source refused to generate a key"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, pkcs8.as_ref())?;
        // The private key is the whole secret. A file the whole machine can read is not a secret.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|_| bad("the key just generated cannot be loaded"))?;
        Ok(SigningKey {
            pair,
            path: path.to_path_buf(),
        })
    }

    /// Load an existing key.
    pub fn load(path: &Path) -> ZResult<SigningKey> {
        let bytes = std::fs::read(path).map_err(|e| {
            bad(format!(
                "cannot read the signing key {}: {}. Run `gesta key generate` first",
                path.display(),
                e
            ))
        })?;
        let pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| {
            bad(format!(
                "{} is not an Ed25519 signing key this build can read",
                path.display()
            ))
        })?;
        Ok(SigningKey {
            pair,
            path: path.to_path_buf(),
        })
    }

    /// The public half, hex, which is what a verifier needs.
    pub fn public_hex(&self) -> String {
        hex::encode(self.pair.public_key().as_ref())
    }

    /// Sign a changeset's identity.
    pub fn sign_changeset(
        &self,
        id: &str,
        message: &str,
        author: &str,
        parent: Option<&str>,
    ) -> String {
        let payload = signed_payload(id, message, author, parent);
        hex::encode(self.pair.sign(&payload).as_ref())
    }
}

/// Check a signature against a public key.
pub fn verify(
    public_hex: &str,
    id: &str,
    message: &str,
    author: &str,
    parent: Option<&str>,
    signature_hex: &str,
) -> ZResult<()> {
    let public =
        hex::decode(public_hex.trim()).map_err(|_| bad("the public key is not valid hex"))?;
    let signature =
        hex::decode(signature_hex.trim()).map_err(|_| bad("the signature is not valid hex"))?;
    let payload = signed_payload(id, message, author, parent);
    UnparsedPublicKey::new(&ring::signature::ED25519, &public)
        .verify(&payload, &signature)
        .map_err(|_| {
            bad(format!(
                "signature does not match changeset {} as recorded: the message, the author or the parent was changed after signing",
                id
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_key(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gesta-key-{}-{}", std::process::id(), name))
    }

    #[test]
    fn a_signature_verifies_against_the_key_that_made_it() {
        let path = temp_key("round");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "a message", "alice", Some("csparent"));
        verify(
            &key.public_hex(),
            "csabc",
            "a message",
            "alice",
            Some("csparent"),
            &sig,
        )
        .expect("a signature made by this key must verify");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_message_breaks_the_signature() {
        let path = temp_key("tamper-msg");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "the real message", "alice", None);
        let err = verify(
            &key.public_hex(),
            "csabc",
            "a different message",
            "alice",
            None,
            &sig,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not match"), "unhelpful message: {}", err);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_author_breaks_the_signature() {
        let path = temp_key("tamper-author");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "m", "alice", None);
        assert!(verify(&key.public_hex(), "csabc", "m", "mallory", None, &sig).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_parent_breaks_the_signature() {
        let path = temp_key("tamper-parent");
        let _ = std::fs::remove_file(&path);
        let key = SigningKey::generate(&path).unwrap();
        let sig = key.sign_changeset("csabc", "m", "alice", Some("csp1"));
        assert!(verify(&key.public_hex(), "csabc", "m", "alice", Some("csp2"), &sig).is_err());
        // Also a parent that was removed.
        assert!(verify(&key.public_hex(), "csabc", "m", "alice", None, &sig).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn another_key_does_not_verify_it() {
        let a = temp_key("other-a");
        let b = temp_key("other-b");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
        let ka = SigningKey::generate(&a).unwrap();
        let kb = SigningKey::generate(&b).unwrap();
        let sig = ka.sign_changeset("csabc", "m", "alice", None);
        assert!(
            verify(&kb.public_hex(), "csabc", "m", "alice", None, &sig).is_err(),
            "a signature verified against the wrong key"
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn generating_over_an_existing_key_is_refused() {
        let path = temp_key("exists");
        let _ = std::fs::remove_file(&path);
        SigningKey::generate(&path).unwrap();
        // `match` rather than `unwrap_err`: a SigningKey holds a private key, so it must not
        // implement Debug, and `expect` would need one to print the value it was handed.
        match SigningKey::generate(&path) {
            Ok(_) => panic!("replacing a key silently would strand every signature it made"),
            Err(e) => {
                let text = e.to_string();
                assert!(
                    text.contains("already exists"),
                    "unhelpful message: {}",
                    text
                );
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_payload_separates_its_fields() {
        // A message that shifts into the author position must not produce the same bytes.
        let a = signed_payload("cs1", "x\nauthor:bob", "alice", None);
        let b = signed_payload("cs1", "x", "bob", None);
        assert_ne!(a, b);
    }
}
