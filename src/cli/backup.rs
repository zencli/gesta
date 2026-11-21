//! The backup commands.

use super::*;

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

/// Handle `gesta secret`.
pub fn cmd_secret(sub: crate::SecretCmd) -> ZResult<()> {
    use crate::SecretCmd;

    match sub {
        SecretCmd::Init => {
            let path = match std::env::var("GESTA_SECRETS_FILE") {
                Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
                _ => crate::support::secrets::default_path()?,
            };
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

    let pass = match passphrase {
        Some(p) => p,
        None => std::env::var("GESTA_BACKUP_PASSPHRASE").map_err(|_| {
            crate::error::Error::new(
                crate::error::Code::Usage,
                "a backup must be sealed, so it needs a passphrase: pass --passphrase or set \
                 GESTA_BACKUP_PASSPHRASE. An unencrypted copy of the whole history in an object \
                 store is a disclosure, not a backup."
                    .to_string(),
            )
        })?,
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
            let tmp = std::env::temp_dir().join(format!(
                "gesta-pull-{}-{}.db",
                std::process::id(),
                crate::support::clock::unix_now()
            ));
            std::fs::write(&tmp, &plain)?;
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
