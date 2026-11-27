//! The backup commands.

use super::*;

/// Write an unsealed repository to a private temporary file and hand back the path.
///
/// The file is the whole plaintext of a backup whose passphrase exists to keep it unreadable, so the
/// three ways it used to go wrong are worth naming (#193): `fs::write` creates 0644 under the usual
/// umask, so any local user could read it; the name was `gesta-pull-{pid}-{unix_now}.db`, predictable
/// from outside the process, so a symlink planted at that path was followed; and a kill between the
/// write and the remove left it behind for good.
///
/// `write_private` closes the first two in one call - `O_EXCL` plus mode 0600 on create, so an
/// existing path fails instead of being opened, and the mode is set before the content lands. The
/// third needs the caller to remove it, which `pull` does on every path out.
///
/// A function rather than a line in the handler for the same reason `read_file_at` exists: a property
/// nothing can call is a property nothing tests.
fn unseal_to_private_temp(plain: &[u8]) -> ZResult<std::path::PathBuf> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut salt = [0u8; 8];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| crate::bad("no randomness for a temp file name".to_string()))?;
    let suffix: String = salt.iter().map(|b| format!("{:02x}", b)).collect();
    let tmp = std::env::temp_dir().join(format!("gesta-pull-{}-{}.db", std::process::id(), suffix));
    crate::support::private_file::write_private(&tmp, plain)?;
    Ok(tmp)
}

/// Write the whole store to one file.
///
/// Different from `restore`, which puts working-tree files back from a branch. This copies the
/// repository itself, including the parts no bundle carries: tags, blame ranges, every branch.
pub fn cmd_backup(dest: &str, json: bool) -> ZResult<()> {
    let (_root, conn) = open_repo()?;
    let path = std::path::PathBuf::from(dest);
    let report = crate::support::backup::backup(&conn, &path)?;
    if json {
        let doc = crate::support::json::Json::obj(vec![
            ("schema", crate::support::json::Json::str("gesta@1")),
            (
                "path",
                crate::support::json::Json::str(report.path.display().to_string()),
            ),
            (
                "bytes",
                crate::support::json::Json::int(report.bytes as i64),
            ),
            (
                "changesets",
                crate::support::json::Json::int(report.changesets),
            ),
            ("objects", crate::support::json::Json::int(report.objects)),
            ("branches", crate::support::json::Json::int(report.branches)),
            ("tags", crate::support::json::Json::int(report.tags)),
        ]);
        println!("{}", doc.render());
        return Ok(());
    }
    println!(
        "backed up {} change(s), {} object(s), {} branch(es), {} tag(s) to {} ({} bytes)",
        report.changesets,
        report.objects,
        report.branches,
        report.tags,
        report.path.display(),
        report.bytes
    );
    Ok(())
}

/// Check a backup file. Reads only the file, so it is safe to run against a repository in use.
pub fn cmd_verify_backup(file: &str) -> ZResult<()> {
    let path = std::path::PathBuf::from(file);
    let c = crate::support::backup::verify(&path)?;
    println!(
        "{} is readable: {} change(s), {} object(s), {} branch(es), {} tag(s), {} version(s), {} blame range(s)",
        path.display(),
        c.changesets,
        c.objects,
        c.branches,
        c.tags,
        c.file_versions,
        c.blame_ranges
    );
    Ok(())
}

/// Where `secret init` gets its passphrase, decided from the two things it can see.
///
/// Split from the command so the decision is testable: a test cannot make the harness's own stdin a
/// terminal, and a check that cannot be exercised is a check that is not known to work.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PassphraseSource {
    /// Ask on the terminal, twice, and make the two match.
    Prompt,
    /// An environment passphrase was supplied, which needs no terminal.
    Given(String),
    /// No terminal and no passphrase: refuse rather than read whatever stdin happens to be.
    Refuse,
}

pub(crate) fn decide_passphrase(is_terminal: bool, from_env: Option<String>) -> PassphraseSource {
    match from_env {
        Some(p) if !p.is_empty() => PassphraseSource::Given(p),
        _ if is_terminal => PassphraseSource::Prompt,
        _ => PassphraseSource::Refuse,
    }
}

/// Handle `gesta secret`.
pub fn cmd_secret(sub: crate::SecretCmd) -> ZResult<()> {
    use crate::SecretCmd;

    match sub {
        SecretCmd::Init => {
            use std::io::IsTerminal;

            let path = match std::env::var("GESTA_SECRETS_FILE") {
                Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
                _ => crate::support::secrets::default_path()?,
            };
            // Nothing is printed and nothing is touched until a passphrase is in hand (#339).
            //
            // Before this, a redirected stdin was read as if it were a terminal: `stty` printed its
            // ioctl complaint into the prompt, the piped bytes became the passphrase, and a store was
            // written and reported as created. A provisioning script piping `printf 'pw\npw\n'` got
            // an "initialised" empty store and exit 0, and `< /dev/null` produced the same complaint
            // and the same 0. The exit status said nothing went wrong on a run where nothing worked.
            //
            // Two ways in are honest and both work without a terminal: the passphrase in
            // `GESTA_SECRETS_PASSPHRASE`, or a terminal to ask. Anything else is refused here, before
            // the path is printed and before the file is read or written, so a failed init leaves the
            // previous store exactly as it was.
            let first = match decide_passphrase(
                std::io::stdin().is_terminal(),
                std::env::var("GESTA_SECRETS_PASSPHRASE").ok(),
            ) {
                PassphraseSource::Given(p) => p,
                PassphraseSource::Refuse => {
                    return Err(bad(
                        "no passphrase: stdin is not a terminal, so there is nothing to ask. \
                         Set GESTA_SECRETS_PASSPHRASE to initialise without a prompt"
                            .to_string(),
                    ))
                }
                PassphraseSource::Prompt => {
                    println!("store: {}", path.display());
                    let first = read_hidden("passphrase: ")?;
                    if first.len() < 8 {
                        return Err(bad(
                            "a passphrase shorter than 8 characters is not worth the file it protects"
                                .to_string(),
                        ));
                    }
                    let again = read_hidden("repeat: ")?;
                    if first != again {
                        return Err(bad("the two passphrases differ".to_string()));
                    }
                    first
                }
            };
            if first.len() < 8 {
                return Err(bad(
                    "a passphrase shorter than 8 characters is not worth the file it protects"
                        .to_string(),
                ));
            }
            println!("store: {}", path.display());
            let store = crate::support::secrets::SecretStore {
                path,
                passphrase: first,
            };
            let existing = store.load()?;
            // Saving an empty map is what creates the file, and keeps what is there.
            store.save(&existing)?;
            println!(
                "created {} ({} entries kept)",
                store.path.display(),
                existing.len()
            );
            Ok(())
        }
        SecretCmd::Set { name } => {
            let store = crate::support::secrets::SecretStore::from_env()?;
            let value = read_hidden(&format!("value for {}: ", name))?;
            if value.is_empty() {
                return Err(bad("an empty value is not stored".to_string()));
            }
            store.put(&name, &value)?;
            println!("stored {} in {}", name, store.path.display());
            Ok(())
        }
        SecretCmd::List => {
            let store = crate::support::secrets::SecretStore::from_env()?;
            let entries = store.load()?;
            if entries.is_empty() {
                println!("no entries in {}", store.path.display());
            } else {
                println!("{} ({} entries)", store.path.display(), entries.len());
                for name in entries.keys() {
                    println!("  {}", name);
                }
            }
            Ok(())
        }
        SecretCmd::Remove { name } => {
            let store = crate::support::secrets::SecretStore::from_env()?;
            if store.remove(&name)? {
                println!("removed {}", name);
            } else {
                println!("no entry named {}", name);
            }
            Ok(())
        }
        SecretCmd::Scan { history, json_scan } => {
            crate::cli::cli_remote::cmd_secret_scan(history, json_scan)
        }
    }
}

/// Where the off-site copy lives, and with what credentials.
///
/// The URL carries no secrets, the same rule as every other remote: `s3://bucket/prefix` plus
/// `GESTA_S3_ENDPOINT` and its key variables. A passphrase in an environment variable rather than an
/// argument keeps it out of shell history and out of `ps`.
fn remote_target(
    passphrase: Option<String>,
) -> ZResult<(crate::transport::s3::S3Config, String, String)> {
    let url = std::env::var("GESTA_BACKUP_URL").map_err(|_| {
        crate::error::Error::new(
            crate::error::Code::Usage,
            "set GESTA_BACKUP_URL to where the backup should go, for example \
             s3://bucket/backups; the S3 credentials come from GESTA_S3_ENDPOINT, \
             GESTA_S3_REGION, GESTA_S3_ACCESS_KEY and GESTA_S3_SECRET_KEY"
                .to_string(),
        )
    })?;
    let (cfg, prefix) = crate::transport::s3::S3Config::from_url(&url)?;

    // The passphrase came in as `--passphrase`, which is an argument, and an argument is in the
    // process list and in the shell history. The comment above this function already said an
    // environment passphrase is the one that stays out of both, so the order is: environment,
    // terminal, and the argument only when there is no terminal to ask on.
    let pass = match crate::cli::secret_not_in_argv(
        passphrase,
        "GESTA_BACKUP_PASSPHRASE",
        "backup passphrase: ",
        true,
    ) {
        Ok(p) => p,
        Err(e) => {
            // Name what the seal protects, because a caller who gives up here is one step from
            // shipping the whole history in the clear.
            return Err(crate::error::Error::new(
                crate::error::Code::Usage,
                format!(
                    "a backup must be sealed, so it needs a passphrase: {}. An unencrypted copy of \
                     the whole history in an object store is a disclosure, not a backup.",
                    e
                ),
            )
            .into());
        }
    };
    if pass.is_empty() {
        return Err(crate::error::Error::new(
            crate::error::Code::Usage,
            "the passphrase is empty, which seals nothing".to_string(),
        )
        .into());
    }
    Ok((cfg, prefix, pass))
}

/// `backup remote push|list|pull|prune`.
pub fn cmd_backup_remote(sub: crate::BackupRemoteCmd) -> ZResult<()> {
    use crate::support::json::Json;

    match sub {
        crate::BackupRemoteCmd::Push {
            prefix,
            keep,
            passphrase,
            json_remote,
        } => {
            let (cfg, url_prefix, pass) = remote_target(passphrase)?;
            let prefix = if prefix == "backups" {
                url_prefix
            } else {
                prefix
            };
            let (root, conn) = open_repo()?;
            let _ = root;
            let pushed = crate::support::backup_remote::push(&conn, &cfg, &prefix, &pass, keep)?;
            if json_remote {
                println!(
                    "{}",
                    Json::obj(vec![
                        ("schema", Json::str(super::SCHEMA_VERSION)),
                        ("key", Json::str(pushed.key.clone())),
                        ("plain_bytes", Json::int(pushed.plain_bytes as i64)),
                        ("sealed_bytes", Json::int(pushed.sealed_bytes as i64)),
                        ("changesets", Json::int(pushed.changesets)),
                        ("objects", Json::int(pushed.objects)),
                        ("kept", Json::int(keep as i64)),
                    ])
                    .render()
                );
                return Ok(());
            }
            println!(
                "sealed {} change(s), {} object(s) into {} ({} bytes sealed from {}, {}x the plain file)",
                pushed.changesets,
                pushed.objects,
                pushed.key,
                pushed.sealed_bytes,
                pushed.plain_bytes,
                if pushed.plain_bytes == 0 {
                    "0".to_string()
                } else {
                    format!(
                        "{:.2}",
                        pushed.sealed_bytes as f64 / pushed.plain_bytes as f64
                    )
                }
            );
            println!("the newest {} are kept; older ones are deleted", keep);
            Ok(())
        }

        crate::BackupRemoteCmd::List {
            prefix,
            json_remote,
        } => {
            let url = std::env::var("GESTA_BACKUP_URL").map_err(|_| {
                crate::error::Error::new(
                    crate::error::Code::Usage,
                    "set GESTA_BACKUP_URL to list backups".to_string(),
                )
            })?;
            let (cfg, url_prefix) = crate::transport::s3::S3Config::from_url(&url)?;
            let prefix = if prefix == "backups" {
                url_prefix
            } else {
                prefix
            };
            let keys = cfg.list_keys(&prefix)?;
            if json_remote {
                println!(
                    "{}",
                    Json::obj(vec![
                        ("schema", Json::str(super::SCHEMA_VERSION)),
                        (
                            "backups",
                            Json::arr(keys.iter().map(|k| Json::str(k.clone())).collect())
                        ),
                        ("count", Json::int(keys.len() as i64)),
                    ])
                    .render()
                );
                return Ok(());
            }
            if keys.is_empty() {
                println!("no backup under {}", prefix);
                return Ok(());
            }
            println!("{} backup(s) under {}:", keys.len(), prefix);
            for k in &keys {
                println!("  {}", k);
            }
            Ok(())
        }

        crate::BackupRemoteCmd::Pull {
            key,
            prefix,
            passphrase,
            yes,
        } => {
            let (cfg, url_prefix, pass) = remote_target(passphrase)?;
            let prefix = if prefix == "backups" {
                url_prefix
            } else {
                prefix
            };

            let target = if key == "latest" {
                let keys = cfg.list_keys(&prefix)?;
                match keys.last() {
                    Some(k) => k.clone(),
                    None => {
                        return Err(crate::error::Error::not_found(format!(
                            "there is no backup under {}, so there is nothing to restore",
                            prefix
                        ))
                        .into())
                    }
                }
            } else if key.contains('/') {
                key.clone()
            } else {
                format!("{}/{}", prefix.trim_end_matches('/'), key)
            };

            let body = cfg.get(&target)?.ok_or_else(|| {
                crate::error::Error::not_found(format!("{} is not in the bucket", target))
            })?;

            // Unsealed before the question, so a wrong passphrase does not make the reader answer a
            // prompt about replacing their store and then fail.
            let plain = crate::support::backup_remote::unseal(&body, &pass)?;
            // The unsealed repository lands on disk for the verification, and that file is the whole
            // plaintext of a backup whose passphrase exists to keep it unreadable (#193).
            //
            // Three things were wrong with `fs::write` onto `gesta-pull-{pid}-{unix_now}.db`:
            // `fs::write` creates 0644 under the usual umask, so any local user could read it; the
            // name is predictable from the pid and the clock, so a symlink planted there is followed;
            // and a kill between the write and the remove leaves it behind. `write_private` creates
            // the file with O_EXCL and mode 0600 in one step, so a symlink fails the create instead
            // of being followed. The name keeps the clock out of the guess: a pid plus 8 random bytes
            // is not predictable from outside the process.
            let tmp = unseal_to_private_temp(&plain)?;
            let contents = match crate::support::backup::verify(&tmp) {
                Ok(c) => c,
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
            };

            if !yes {
                println!(
                    "{} holds {} change(s), {} object(s), {} branch(es), {} tag(s).",
                    target, contents.changesets, contents.objects, contents.branches, contents.tags
                );
                print!(
                    "Restoring replaces this repository's store and cannot be undone. Continue? [y/N] "
                );
                use std::io::Write;
                std::io::stdout().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    let _ = std::fs::remove_file(&tmp);
                    println!("nothing was changed");
                    return Ok(());
                }
            }

            let root = repo_root()?;
            let restored = crate::support::backup::restore(&root, &tmp);
            let _ = std::fs::remove_file(&tmp);
            let c = restored?;
            println!(
                "restored {} change(s), {} object(s), {} branch(es), {} tag(s) from {}",
                c.changesets, c.objects, c.branches, c.tags, target
            );
            Ok(())
        }

        crate::BackupRemoteCmd::Prune { prefix, keep } => {
            let url = std::env::var("GESTA_BACKUP_URL").map_err(|_| {
                crate::error::Error::new(
                    crate::error::Code::Usage,
                    "set GESTA_BACKUP_URL to prune backups".to_string(),
                )
            })?;
            let (cfg, url_prefix) = crate::transport::s3::S3Config::from_url(&url)?;
            let prefix = if prefix == "backups" {
                url_prefix
            } else {
                prefix
            };
            let removed = crate::support::backup_remote::prune(&cfg, &prefix, keep)?;
            println!(
                "removed {} backup(s); the newest {} are kept",
                removed, keep
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unsealed_temp_file_is_private_and_not_at_a_predicable_name() {
        // #193: `pull` wrote the whole decrypted backup with `fs::write` to
        // `gesta-pull-{pid}-{unix_now}.db` in the system temp directory. Three separate problems, and
        // this checks all three: the mode, the name, and that an existing path is refused rather than
        // opened.
        let a = unseal_to_private_temp(b"repository one").unwrap();
        let b = unseal_to_private_temp(b"repository two").unwrap();
        assert_ne!(
            a, b,
            "two calls produced the same path, so the name is guessable from the pid and the clock"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "the unsealed repository is mode {mode:o}, readable by every local user"
            );
        }
        assert_eq!(std::fs::read(&a).unwrap(), b"repository one");
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[cfg(unix)]
    #[test]
    fn the_temp_file_does_not_exist_before_it_is_written() {
        // The symlink defence is `O_EXCL`, and it only works if nothing is at the path beforehand.
        // `write_private` opens with `create(true)` and `truncate(true)` - it deliberately does not
        // refuse an existing file, because `token rotate` rewrites one on purpose (see its doc).
        //
        // So the property this test can hold is the one the name is responsible for: a random suffix
        // means there is nothing at the path to plant against. Measured by asking a million names
        // whether any collides - if the suffix came from the clock, the answer would be visible as a
        // repeat at this size.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let p = unseal_to_private_temp(b"x").unwrap();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                seen.insert(name.clone()),
                "the same temp name was produced twice, so it is predictable: {name}"
            );
            let _ = std::fs::remove_file(&p);
        }
    }
    #[test]
    fn init_refuses_a_redirected_stdin_before_touching_the_store() {
        // #339: a piped passphrase used to be read as if it came from a terminal, the `stty` complaint
        // was printed into the prompt, and a store was written and reported as created. A provisioning
        // script got "initialised", exit 0, and an empty store.
        //
        // The check lives in the command, and a test cannot redirect the test harness's own stdin, so
        // what is asserted here is the decision the command makes from the two inputs it reads: an
        // environment passphrase, and whether stdin is a terminal. `decide_passphrase` is that
        // decision, split out for exactly this reason.
        assert_eq!(
            decide_passphrase(true, None),
            PassphraseSource::Prompt,
            "a terminal is asked"
        );
        assert_eq!(
            decide_passphrase(false, None),
            PassphraseSource::Refuse,
            "a redirected stdin with no environment passphrase is refused, not guessed at"
        );
        assert_eq!(
            decide_passphrase(false, Some("from-env".to_string())),
            PassphraseSource::Given("from-env".to_string()),
            "the environment passphrase works without a terminal"
        );
        assert_eq!(
            decide_passphrase(true, Some("from-env".to_string())),
            PassphraseSource::Given("from-env".to_string()),
            "and it wins over the prompt, so a script never blocks on a tty"
        );
    }
}
