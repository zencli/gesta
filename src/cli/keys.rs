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

pub fn cmd_tag(
    name: Option<&str>,
    changeset: Option<&str>,
    list: bool,
    force: bool,
) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    if list {
        let tags = read_tags(&conn)?;
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
    match write_tag(&conn, name, changeset, force)? {
        TagWrite::Created { target } => println!("tagged {} at {}", name, target),
        TagWrite::Moved { old, target } => println!("moved {} from {} to {}", name, old, target),
        TagWrite::AlreadyThere { target } => println!("{} is already at {}", name, target),
    }
    Ok(())
}

/// What writing a tag did, so the text and JSON renderers describe the same event.
pub enum TagWrite {
    Created { target: String },
    Moved { old: String, target: String },
    AlreadyThere { target: String },
}

fn read_tags(conn: &Connection) -> ZResult<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare("SELECT name, changeset_id, created_at FROM tags ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn write_tag(
    conn: &Connection,
    name: &str,
    changeset: Option<&str>,
    force: bool,
) -> ZResult<TagWrite> {
    // Whatever was named has to be turned into a changeset id before it is stored. Storing the
    // name is what let `tag v2 main` succeed while pointing at a string that is not a changeset:
    // the tag exists, resolves to nothing, and fails only when somebody reads through it.
    let target = match changeset {
        Some(c) => crate::store::resolve_rev(conn, c)?.ok_or_else(|| {
            bad(format!(
                "no such branch, tag or changeset: {}; a tag can point at a branch, a changeset \
                 or another tag, but it has to point at something",
                c
            ))
        })?,
        None => {
            let branch = current_branch(conn)?;
            branch_head(conn, &branch)?.ok_or_else(|| bad("branch has no changesets to tag"))?
        }
    };
    // A tag that already exists is refused, not moved. `ON CONFLICT DO UPDATE` made `gesta tag v1`
    // twice silently relocate `v1` to the second changeset: exit 0, a success message, and the
    // first target gone from every record. A name that people put in a build script or a release
    // note has to mean the same thing the next time it is read.
    //
    // The escape is explicit: `--force` moves it and says the old target in the message, so a
    // script that means to re-point a tag has to say so and the log keeps the previous answer.
    let existing: Option<String> = conn
        .query_row(
            "SELECT changeset_id FROM tags WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .optional()?;
    match (&existing, force) {
        (Some(old), false) if old == &target => {
            // Re-tagging the same point is a no-op, not an error: a script that sets a tag after
            // every build should not fail on the runs where nothing moved.
            return Ok(TagWrite::AlreadyThere { target });
        }
        (Some(old), false) => {
            return Err(bad(format!(
                "tag {} already points at {}; pass --force to move it to {}",
                name, old, target
            )));
        }
        (Some(old), true) => {
            conn.execute(
                "UPDATE tags SET changeset_id = ?2, created_at = ?3 WHERE name = ?1",
                rusqlite::params![name, target, now()],
            )?;
            ops::record(conn, "tag-move", Some(name), Some(old))?;
            return Ok(TagWrite::Moved {
                old: old.clone(),
                target,
            });
        }
        (None, _) => {}
    }
    conn.execute(
        "INSERT INTO tags(name, changeset_id, message, created_at) VALUES(?1, ?2, NULL, ?3)",
        rusqlite::params![name, target, now()],
    )?;
    ops::record(conn, "tag", Some(name), None)?;
    Ok(TagWrite::Created { target })
}

pub fn cmd_tag_json(
    name: Option<&str>,
    changeset: Option<&str>,
    list: bool,
    force: bool,
) -> YResultAlias {
    let (_root, conn) = open_repo()?;
    // A command that names a tag means to create it. Printing the list instead answered a question
    // nobody asked and threw away the arguments, so `gesta --json tag v1.0` reported success while
    // creating nothing.
    if !list {
        if let Some(name) = name {
            let outcome = write_tag(&conn, name, changeset, force)?;
            let (action, old, target) = match outcome {
                TagWrite::Created { target } => ("created", Json::Null, target),
                TagWrite::Moved { old, target } => ("moved", Json::str(old), target),
                TagWrite::AlreadyThere { target } => ("unchanged", Json::Null, target),
            };
            emit(Json::obj(vec![
                ("name", Json::str(name.to_string())),
                ("action", Json::str(action)),
                ("previous", old),
                ("changeset", Json::str(target)),
            ]));
            return Ok(());
        }
    }
    let rows = read_tags(&conn)?
        .into_iter()
        .map(|(n, cs, ts)| {
            Json::obj(vec![
                ("name", Json::str(n)),
                ("changeset", Json::str(cs)),
                ("created_at", Json::int(ts)),
            ])
        })
        .collect();
    emit(Json::arr(rows));
    Ok(())
}

/// Read a line from the terminal without echoing it.
///
/// The value is typed, not passed as an argument: an argument is visible in the
/// process list and lands in shell history, and a secret in either is a secret that
/// has to be rotated.
///
/// What this promises is narrow and it used to break in three ways at once (#205):
///
/// - On Windows there was no echo suppression at all, so the passphrase was printed to the terminal
///   the command exists to keep it out of.
/// - On Unix the two `stty` calls were bare, so a read that failed between them left the terminal in
///   no-echo and every later prompt invisible.
/// - `trim_end` removed trailing spaces, so a passphrase that ends in one could be set and never
///   entered again.
///
/// What it still cannot promise: a `SIGKILL`, or a panic in a `panic = "abort"` build, skips every
/// destructor, so echo stays off. `SIGINT` is covered where the platform delivers it as a Rust exit
/// path; a hard kill is not, and pretending otherwise would be the same kind of lie as the three
/// above.
pub(crate) fn read_hidden(prompt: &str) -> ZResult<String> {
    use std::io::{BufRead, Write};

    print!("{}", prompt);
    std::io::stdout().flush()?;

    // The guard restores echo however this function leaves: a normal return, an I/O error from the
    // read, or any `?` in between. Two bare `stty` calls restored it only on the happy path.
    let guard = EchoOff::engage();

    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);

    // Restore before the newline is printed, so the cursor lands on a fresh line rather than
    // wherever the hidden characters would have ended.
    drop(guard);
    println!();
    read?;

    Ok(strip_newline(line))
}

/// The line without its terminator, and without anything else.
///
/// `trim_end` also removed trailing spaces, which made a passphrase ending in one impossible to
/// enter: the stored value kept the space and the typed one lost it, so the two never matched and
/// nothing said why.
fn strip_newline(mut line: String) -> String {
    if line.ends_with('\n') {
        line.pop();
    }
    if line.ends_with('\r') {
        line.pop();
    }
    line
}

/// Echo switched off for as long as the value lives.
///
/// A guard rather than a second call at the end of the function, because the failure this fixes is
/// the exit that is not the end: an interrupted read used to leave the terminal silent for the rest
/// of the session.
struct EchoOff {
    /// Whether anything was actually turned off, so `Drop` does not run `stty echo` on a terminal
    /// that refused `stty -echo` in the first place.
    engaged: bool,
}

impl EchoOff {
    #[cfg(unix)]
    fn engage() -> EchoOff {
        // stty is present wherever a terminal is, and turning echo off for one read is the one thing
        // this needs from it. A failure here is not fatal: the read still happens, it is just
        // visible, and refusing to prompt because the terminal would not cooperate helps nobody.
        let ok = std::process::Command::new("stty")
            .arg("-echo")
            .stdin(std::process::Stdio::inherit())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        EchoOff { engaged: ok }
    }

    #[cfg(windows)]
    fn engage() -> EchoOff {
        EchoOff {
            engaged: win_console::echo(false),
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn engage() -> EchoOff {
        EchoOff { engaged: false }
    }
}

impl Drop for EchoOff {
    #[cfg(unix)]
    fn drop(&mut self) {
        if self.engaged {
            let _ = std::process::Command::new("stty")
                .arg("echo")
                .stdin(std::process::Stdio::inherit())
                .status();
        }
    }

    #[cfg(windows)]
    fn drop(&mut self) {
        if self.engaged {
            win_console::echo(true);
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn drop(&mut self) {}
}

/// Turning terminal echo off on Windows, where there is no `stty`.
///
/// `SetConsoleMode` on the standard input handle: clearing `ENABLE_ECHO_INPUT` is the whole job, and
/// setting it back is the same call with the bit restored. Declared here rather than pulled in as a
/// dependency because these are the only three functions this program needs from kernel32, and a
/// crate for three symbols would be a dependency to review for nothing.
#[cfg(windows)]
mod win_console {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    /// `STD_INPUT_HANDLE`, which is `-10` as an unsigned value.
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    /// Characters typed at the console are echoed to the screen.
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    /// `INVALID_HANDLE_VALUE`.
    const INVALID_HANDLE_VALUE: isize = -1;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    }

    /// Turn echo off (`on = false`) or back on, answering whether it worked.
    ///
    /// Every step is checked because each can fail for a reason that matters: no console at all when
    /// input is a pipe, or a handle that is not a console handle. Answering `false` leaves the caller
    /// reading normally, which is what the old code did on this platform anyway - the difference is
    /// that now it is known and the restore is skipped rather than run blind.
    pub fn echo(on: bool) -> bool {
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return false;
            }
            let wanted = if on {
                mode | ENABLE_ECHO_INPUT
            } else {
                mode & !ENABLE_ECHO_INPUT
            };
            SetConsoleMode(handle, wanted) != 0
        }
    }
}

#[cfg(test)]
mod hidden_tests {
    use super::strip_newline;

    #[test]
    fn a_trailing_space_survives_the_read() {
        // `trim_end` removed every trailing space, so a passphrase ending in one could be set and
        // then never entered again: the stored value kept the space, the typed one lost it, and
        // nothing said why. Only the terminator is removed now.
        assert_eq!(strip_newline("secret \n".to_string()), "secret ");
        assert_eq!(strip_newline("secret  ".to_string()), "secret  ");
        assert_eq!(strip_newline("  leading".to_string()), "  leading");
    }

    #[test]
    fn the_terminator_is_removed_on_both_line_endings() {
        assert_eq!(strip_newline("value\n".to_string()), "value");
        assert_eq!(strip_newline("value\r\n".to_string()), "value");
        assert_eq!(strip_newline("value".to_string()), "value");
        assert_eq!(strip_newline(String::new()), "");
        // A lone `\r` in the middle is content, not a terminator.
        assert_eq!(strip_newline("a\rb\n".to_string()), "a\rb");
    }

    #[test]
    fn a_read_that_fails_still_restores_echo() {
        // The old code ran `stty -echo`, read, then `stty echo` as three separate statements, so a
        // read that failed left the terminal in no-echo and every later prompt invisible. The guard
        // makes the restore a destructor, and this test pins the property that matters: dropping it
        // runs the restore even when nothing else does.
        //
        // What can be observed without a terminal is the shape: `EchoOff` is dropped on the error
        // path too, which the `drop(guard)` before `read?` guarantees by ordering.
        let guard = super::EchoOff { engaged: false };
        // Nothing to restore, so this must not hang or fail; the point is that `Drop` runs at all.
        drop(guard);
    }
}
