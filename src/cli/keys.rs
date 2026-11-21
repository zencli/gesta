//! The keys commands.

use super::*;

/// Create a signing key and print the public half.
pub fn cmd_key_generate(path: Option<&str>) -> ZResult<()> {
    let path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::transport::sign::default_key_path(),
    };
    let key = crate::transport::sign::SigningKey::generate(&path)?;
    // Recorded here as well as on rotation: a repository that never rotates still has to be able to
    // answer "which keys were ours", and a record that starts at the first rotation answers for half
    // the history.
    if let Ok((root, _)) = open_repo() {
        crate::transport::keyring::record_active(
            &root,
            &key.public_hex(),
            &key.path,
            crate::now(),
        )?;
    }
    println!("created a signing key: {}", key.path.display());
    println!("public key: {}", key.public_hex());
    println!();
    println!("Give that public key to anyone who should verify your changesets.");
    Ok(())
}

/// Replace the active signing key, archiving the one in use.
///
/// Nothing is re-signed. A signature names the public key it was made with, so an old changeset keeps
/// verifying against the old key for as long as that public key is known. The archive exists for the
/// case a person cannot reconstruct: a signature is disputed and the private half has to be found.
pub fn cmd_key_rotate(path: Option<&str>, dry_run: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let target = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::transport::sign::default_key_path(),
    };

    // What is active now, from the record rather than from the file: a key made with `--path` lives
    // somewhere else, and rotating the default path would leave the real key in place and sign with
    // the wrong one for as long as nobody looked.
    let current = crate::transport::keyring::active(&root)?;
    let current_path = current
        .as_ref()
        .map(|r| std::path::PathBuf::from(&r.path))
        .unwrap_or_else(|| target.clone());

    if dry_run {
        println!("would archive: {}", current_path.display());
        println!("would create:  {}", target.display());
        if let Some(r) = &current {
            println!(
                "the key in use is {}",
                &r.public_hex[..16.min(r.public_hex.len())]
            );
        } else {
            println!("no key is recorded as active");
        }
        return Ok(());
    }

    // The old key moves aside before the new one is made, so a crash between the two steps leaves an
    // archived key and no active one rather than two keys claiming the same filename.
    let mut archived = None;
    if current_path.exists() {
        let public = match crate::transport::sign::SigningKey::load(&current_path) {
            Ok(k) => k.public_hex(),
            Err(_) => crate::transport::keyring::active(&root)?
                .map(|r| r.public_hex)
                .unwrap_or_default(),
        };
        if !public.is_empty() {
            archived = Some(crate::transport::keyring::archive(&current_path, &public)?);
        }
    }

    let key = crate::transport::sign::SigningKey::generate(&target)?;
    crate::transport::keyring::record_active(&root, &key.public_hex(), &key.path, crate::now())?;

    crate::support::audit::local::key_rotated(
        &root,
        "local",
        &key.public_hex(),
        archived
            .as_ref()
            .map(|p| p.display().to_string())
            .as_deref(),
    );
    if let Some(a) = archived {
        println!("archived the old key to {}", a.display());
    }
    println!("created a new signing key: {}", key.path.display());
    println!("public key: {}", key.public_hex());
    println!();
    println!("Changesets signed with the old key still verify: a signature names the key it was made with.");
    println!("Give the new public key to anyone who should verify your changesets.");
    Ok(())
}

/// Show the keys this repository has used.
pub fn cmd_key_list() -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let records = crate::transport::keyring::read_records(&root)?;
    if records.is_empty() {
        println!("no signing keys recorded for this repository");
        println!("create one with: gesta key generate");
        return Ok(());
    }
    for r in records {
        let state = match r.retired_at {
            None => "active".to_string(),
            // A unix second, printed as one. There is no date formatter in this tree and a
            // rotation is read once, so a number that can be compared is enough.
            Some(t) => format!("retired at {} (unix)", t),
        };
        let present = if std::path::Path::new(&r.path).exists() {
            ""
        } else {
            "  (file is gone)"
        };
        println!(
            "{}  {}  {}{}",
            &r.public_hex[..16.min(r.public_hex.len())],
            state,
            r.path,
            present
        );
    }
    Ok(())
}

/// Sign a pending changeset with the local key.
pub fn cmd_sign(id: &str, key_path: Option<&str>) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let (message, author, parent, status): (String, String, Option<String>, String) = conn
        .query_row(
            "SELECT message, author, parent, status FROM changesets WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| {
            crate::support::error::Error::not_found(format!("no such changeset: {}", id))
        })?;
    if status == "committed" {
        return Err(bad(format!(
            "{} is already committed: signing after the fact signs a different history than the one anybody else has",
            id
        )));
    }
    let path = match key_path {
        Some(p) => std::path::PathBuf::from(p),
        None => crate::transport::sign::default_key_path(),
    };
    let key = crate::transport::sign::SigningKey::load(&path)?;
    let signature = key.sign_changeset(id, &message, &author, parent.as_deref());
    conn.execute(
        "UPDATE changesets SET signature = ?2, signer = ?3 WHERE id = ?1",
        rusqlite::params![id, signature, key.public_hex()],
    )?;
    println!("signed {} with {}", id, key.public_hex());
    Ok(())
}

/// Check a changeset's signature.
pub fn cmd_verify(id: Option<&str>, key_hex: Option<&str>) -> ZResult<()> {
    let (root, conn) = open_repo()?;
    let id = match id {
        Some(i) => i.to_string(),
        None => {
            let branch = current_branch(&conn)?;
            branch_head(&conn, &branch)?.ok_or_else(|| {
                Box::new(crate::support::error::Error::new(
                    crate::support::error::Code::NotFound,
                    "the branch has no changesets to verify",
                )) as Box<dyn std::error::Error>
            })?
        }
    };
    /// What a verification needs: the fields the signature covered, plus the signature itself.
    type SignedRow = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<SignedRow> = conn
        .query_row(
            "SELECT message, author, parent, signature, signer FROM changesets WHERE id = ?1",
            rusqlite::params![&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (message, author, parent, signature, signer) = row.ok_or_else(|| {
        crate::support::error::Error::not_found(format!("no such changeset: {}", id))
    })?;
    let signature =
        signature.ok_or_else(|| bad(format!("{} carries no signature: nothing to verify", id)))?;
    let public = match key_hex {
        Some(k) => k.to_string(),
        None => signer.ok_or_else(|| {
            bad(format!(
                "{} records no signing key, so pass --key with the public key to check against",
                id
            ))
        })?,
    };
    crate::transport::sign::verify(
        &public,
        &id,
        &message,
        &author,
        parent.as_deref(),
        &signature,
    )?;
    println!("{} verifies against {}", id, public);
    // The maths checking out answers "was this signed by that key", not "is that key ours".
    // Any signature verifies against its own public half, so a key this repository has never
    // used is worth naming: it means the changeset arrived from somewhere else.
    if !crate::transport::keyring::was_ours(&root, &public)? {
        if let Some(a) = crate::transport::keyring::active(&root)? {
            if a.public_hex != public {
                println!(
                    "note: {} is not a key this repository has recorded; the active one is {}",
                    &public[..16.min(public.len())],
                    &a.public_hex[..16.min(a.public_hex.len())]
                );
            }
        }
    }
    Ok(())
}

pub fn cmd_tag(name: Option<&str>, changeset: Option<&str>, list: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if list {
        let tags: Vec<(String, String, i64)> = {
            let mut stmt =
                conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if tags.is_empty() {
            println!("no tags");
        }
        for (n, cs, ts) in tags {
            println!("{:<20} {} {}", n, cs, fmt_ts(ts));
        }
        return Ok(());
    }
    let name =
        name.ok_or_else(|| bad("a tag needs a name: `gesta tag <name> [--changeset REV]`"))?;

    // Whatever was named has to be turned into a changeset id before it is stored. Storing the
    // name is what let `tag v2 main` succeed while pointing at a string that is not a changeset:
    // the tag exists, resolves to nothing, and fails only when somebody reads through it.
    let target = match changeset {
        Some(c) => crate::store::resolve_rev(&conn, c)?.ok_or_else(|| {
            bad(format!(
                "no such branch, tag or changeset: {}; a tag can point at a branch, a changeset \
                 or another tag, but it has to point at something",
                c
            ))
        })?,
        None => {
            let branch = current_branch(&conn)?;
            branch_head(&conn, &branch)?.ok_or_else(|| bad("branch has no changesets to tag"))?
        }
    };
    conn.execute(
        "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, NULL, ?3)
         ON CONFLICT(name) DO UPDATE SET changeset_id = excluded.changeset_id",
        rusqlite::params![name, target, now()],
    )?;
    ops::record(&conn, "tag", Some(name), None)?;
    println!("tagged {} at {}", name, target);
    Ok(())
}

pub fn cmd_tag_json() -> YResultAlias {
    let (_root, conn) = open_repo()?;
    let mut stmt = conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok(Json::obj(vec![
            ("name", Json::str(r.get::<_, String>(0)?)),
            ("changeset", Json::str(r.get::<_, String>(1)?)),
            ("created_at", Json::int(r.get::<_, i64>(2)?)),
        ]))
    })?;
    emit(Json::arr(rows.collect::<rusqlite::Result<Vec<_>>>()?));
    Ok(())
}

/// Read a line from the terminal without echoing it.
///
/// The value is typed, not passed as an argument: an argument is visible in the
/// process list and lands in shell history, and a secret in either is a secret that
/// has to be rotated.
pub(crate) fn read_hidden(prompt: &str) -> ZResult<String> {
    use std::io::{BufRead, Write};

    print!("{}", prompt);
    std::io::stdout().flush()?;

    #[cfg(unix)]
    {
        // stty is present wherever a terminal is, and turning echo off for one read
        // is the one thing this needs from it.
        let _ = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
        println!();
        #[allow(clippy::needless_return)] // the other cfg branch returns below
        return Ok(line.trim_end().to_string());
    }

    #[cfg(not(unix))]
    {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim_end().to_string())
    }
}
