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
