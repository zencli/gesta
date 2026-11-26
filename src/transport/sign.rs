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
/// Every field is length-prefixed, and the reason is a forgery that the separator alone does not
/// stop (#162). The earlier payload was `...\nauthor:<author>\nmessage:<message>`, so an author of
/// `x\nmessage: <forged>` produced the same bytes as the pair `("x", "<forged>")`. A signature made
/// over one (author, message) verified for the other, which is the one thing a signed payload
/// exists to prevent. Rejecting `\n` in an author would close that particular spelling, but the
/// message is free-form by design and could shift the same way, so the encoding has to be
/// unambiguous on its own rather than rely on every future caller validating every field.
///
/// A length prefix is unambiguous for any bytes whatsoever: `author:5:alice` can only be read one
/// way, whatever the field holds.
pub fn signed_payload(id: &str, message: &str, author: &str, parent: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"gesta-changeset-v2\n");
    for (name, value) in [
        ("id", id),
        ("parent", parent.unwrap_or("-")),
        ("author", author),
        ("message", message),
    ] {
        out.extend_from_slice(name.as_bytes());
        out.push(b':');
        out.extend_from_slice(value.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out
}

/// A key pair on disk: the private seed, hex encoded, three lines of context.
pub struct SigningKey {
    pair: Ed25519KeyPair,
    pub path: PathBuf,
}

impl SigningKey {
    /// Generate a new key and write it, refusing to overwrite an existing one.
    pub fn generate(path: &Path) -> ZResult<SigningKey> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| bad("the system random source refused to generate a key"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // One call, not `exists()` then `write()` (#161). The check and the write were two
        // operations, so two concurrent generates both passed the check and the second overwrote the
        // first - a key lost with no error to either caller. And `fs::write` creates 0644 under the
        // usual umask, with the mode fixed up afterwards, so the key was readable by any local
        // process for the width of the write. `create_new` with mode 0600 at creation answers
        // "does it exist" and "make it mine" in the same syscall.
        match crate::support::private_file::write_new_private(path, pkcs8.as_ref()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(bad(format!(
                    "{} already exists: move it aside rather than replacing it, or every signature it made becomes unverifiable",
                    path.display()
                )));
            }
            Err(e) => return Err(e.into()),
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

/// A payload in the pre-#162 encoding, for signatures made before this change.
///
/// This format is only unambiguous under one condition, and `legacy_payload` is the only caller:
/// `id`, `parent` and `author` must contain no newline. With that held, the text up to
/// `message:` is fixed by those three fields alone and everything after it is the message, so two
/// different tuples cannot produce the same bytes. The forgery #162 describes needs a newline in
/// the author to work, which means it cannot be spelled in an accepted legacy payload - so falling
/// back to v1 does not reopen the hole that dropping v1 was meant to close.
///
/// The fallback exists because signatures are stored without a version, and refusing every
/// signature made before the fix would leave repositories unable to verify their own history.
fn legacy_payload(id: &str, message: &str, author: &str, parent: Option<&str>) -> Option<Vec<u8>> {
    let parent = parent.unwrap_or("-");
    for field in [id, parent, author] {
        if field.contains('\n') {
            return None;
        }
    }
    Some(
        format!(
            "gesta-changeset-v1\nid:{}\nparent:{}\nauthor:{}\nmessage:{}",
            id, parent, author, message
        )
        .into_bytes(),
    )
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
    let key = UnparsedPublicKey::new(&ring::signature::ED25519, &public);
    if key
        .verify(&signed_payload(id, message, author, parent), &signature)
        .is_ok()
    {
        return Ok(());
    }
    // A signature made before #162 covers the old encoding. Accepting it here keeps that history
    // verifiable, and the newline guard inside `legacy_payload` keeps the fallback from accepting
    // the one pair that made the old encoding ambiguous.
    if let Some(old) = legacy_payload(id, message, author, parent) {
        if key.verify(&old, &signature).is_ok() {
            return Ok(());
        }
    }
    Err(bad(format!(
        "signature does not match changeset {} as recorded: the message, the author or the parent was changed after signing",
        id
    )))
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

    #[test]
    fn an_author_carrying_a_field_separator_cannot_forge_another_pair() {
        // #162: with the old `author:{}\nmessage:{}` encoding these two produced identical bytes,
        // so a signature over the first verified for the second.
        //
        // The pair is the whole point, and a first attempt at this test used the wrong one: it put
        // the separator in the author with an empty message, which does NOT collide, because the
        // forged text has to land where the real message begins. The colliding pair moves the
        // separator to the end of the message instead - `message:` is the last field, so text
        // appended to it, plus the `\nmessage:` that the author brings, reconstructs the other
        // spelling exactly.
        let legit_author = "x";
        let legit_message = " transfer everything\nmessage:";
        let claimed_author = "x\nmessage: transfer everything";
        let claimed_message = "";

        let legit = signed_payload("cs1", legit_message, legit_author, None);
        let claimed = signed_payload("cs1", claimed_message, claimed_author, None);
        assert_ne!(
            legit, claimed,
            "an author containing a separator must not encode like a different (author, message) pair"
        );

        // The same shape with the two fields the other way round, so the test is about the encoding
        // and not about which field happens to be last.
        let a2 = signed_payload("cs1", "alice\nmessage:evil", "", None);
        let b2 = signed_payload("cs1", "", "alice\nmessage:evil", None);
        assert_ne!(a2, b2);
    }

    #[test]
    fn a_signature_made_before_the_fix_still_verifies() {
        // History signed under the v1 encoding has to keep verifying, or every old signature in
        // every repository becomes unusable the moment this fix lands.
        let dir = temp_key("legacy");
        let key = SigningKey::generate(&dir).unwrap();
        let old = legacy_payload("cs1", "fix the bug", "alice", Some("cs0")).unwrap();
        let sig = hex::encode(key.pair.sign(&old).as_ref());
        verify(
            &key.public_hex(),
            "cs1",
            "fix the bug",
            "alice",
            Some("cs0"),
            &sig,
        )
        .expect("a v1 signature must still verify");
    }

    #[test]
    fn the_legacy_fallback_refuses_the_ambiguous_pair() {
        // The fallback must not hand back the very collision #162 is about. An author with a
        // newline is not a shape a legacy signature can be checked against, so `legacy_payload`
        // declines it instead of guessing which reading was meant.
        assert!(legacy_payload("cs1", "", "x\nmessage: evil", None).is_none());
        assert!(legacy_payload("cs1", "m", "alice", Some("c\ns0")).is_none());
        assert!(legacy_payload("c\ns1", "m", "alice", None).is_none());
        assert!(legacy_payload("cs1", "m", "alice", None).is_some());
    }
    #[test]
    fn a_generated_key_is_created_unreadable_not_made_so_afterwards() {
        // #161, and the first version of this test was a lesson in what does not work.
        //
        // It asserted on the mode of the finished file. That test passes against the old code too -
        // `fs::write` then `set_permissions(0600)` ends at 0600, so the only thing it proved was
        // that the fix-up ran. The window between the two calls is real and invisible from the end
        // state, which is the whole point: you cannot observe a gap by looking at what comes after
        // it (A60).
        //
        // What can be observed is the property that removes the gap. `create_new` with mode 0600
        // makes the file exist and private in the same syscall, so a second create cannot succeed
        // and no reader sees a wider file. The mode is still asserted - it is the outcome a person
        // cares about - but the race test above is what makes the claim, and this one would pass
        // either way, so it is stated as the weaker check it is.
        let d = std::env::temp_dir().join(format!("zf-key-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("signing.key");

        SigningKey::generate(&path).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the key ended at {mode:o}, not 600");
        }
        let key = SigningKey::load(&path).unwrap();
        assert_eq!(key.path, path);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrent_generates_against_one_path_produce_exactly_one_key() {
        // The half of #161 a sequential test cannot reach. `if path.exists() { refuse }` followed by
        // `write` is a check-then-act: two threads can both pass the check, and then both write, so
        // the loser's key vanishes with no error to either caller - and any signature the loser made
        // stops verifying. Racing them is the only way to see it.
        //
        // Sixteen threads on a barrier, so they start together rather than one after another. With
        // `create_new` exactly one wins by construction; it is not a matter of timing.
        let d = std::env::temp_dir().join(format!("zf-key-race2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("contended.key");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                SigningKey::generate(&path).is_ok()
            }));
        }
        let wins = handles
            .into_iter()
            .filter(|_| true)
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            wins, 1,
            "{wins} of 16 racing generates reported success on one path; the loser's key was \
             overwritten and neither caller was told"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_generates_cannot_both_win() {
        // The other half of #161: `if path.exists() { refuse }` then `write` let two callers both
        // pass the check, and the second overwrote the first's key with no error to either. With
        // `create_new` exactly one call succeeds and the other gets AlreadyExists.
        let d = std::env::temp_dir().join(format!("zf-key-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("contended.key");

        let first = SigningKey::generate(&path);
        assert!(first.is_ok(), "the first generate must succeed");
        let winner = std::fs::read(&path).unwrap();

        let second = SigningKey::generate(&path);
        assert!(
            second.is_err(),
            "a second generate on the same path reported success and replaced a key that signatures \
             already depend on"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            winner,
            "the losing call rewrote the file"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_refusal_for_an_existing_key_still_says_why() {
        // The message is the whole value of the refusal: "already exists" without the reason reads
        // like a bug, and somebody moves the file aside without knowing what it costs.
        let d = std::env::temp_dir().join(format!("zf-key-msg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("k");
        SigningKey::generate(&path).unwrap();
        let err = match SigningKey::generate(&path) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a second generate on the same path succeeded"),
        };
        assert!(
            err.contains("unverifiable"),
            "the refusal does not explain why replacing the key is worse than failing: {err}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
