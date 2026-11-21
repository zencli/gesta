//! The subcommands that own their own argument tree: sub, submodule, hook.

use super::*;

pub fn cmd_sub(sub: crate::SubCmd, json: bool) -> ZResult<()> {
    use crate::SubCmd;
    match sub {
        SubCmd::Add { path, remote } => {
            let (_root, conn) = open_repo()?;
            let s = crate::repo::subrepo::add(&conn, &path, &remote)?;
            println!("added subrepository {} -> {}", s.path, s.remote);
            println!(
                "next: place a checkout there, then `gesta sub pin {}`",
                s.path
            );
            Ok(())
        }
        SubCmd::List => {
            let (root, conn) = open_repo()?;
            let all = crate::repo::subrepo::list(&conn)?;
            if json {
                let parts: Vec<String> = all
                    .iter()
                    .map(|s| {
                        format!(
                            "{{\"path\":\"{}\",\"remote\":\"{}\",\"pinned\":\"{}\",\"present\":{}}}",
                            s.path,
                            s.remote,
                            s.pinned,
                            crate::repo::subrepo::is_present(&root, s)
                        )
                    })
                    .collect();
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if all.is_empty() {
                println!("no subrepositories");
                return Ok(());
            }
            for s in &all {
                // A missing checkout is stated, not hidden: a caller that acts on
                // the pin needs to know the directory is not there.
                let state = if crate::repo::subrepo::is_present(&root, s) {
                    "present"
                } else {
                    "missing"
                };
                let pin = if s.pinned.is_empty() {
                    "unpinned".to_string()
                } else {
                    s.pinned.clone()
                };
                println!("{}  {}  {}  {}", s.path, pin, state, s.remote);
            }
            Ok(())
        }
        SubCmd::Pin { path } => {
            let (root, conn) = open_repo()?;
            let s = crate::repo::subrepo::get(&conn, &path)?
                .ok_or_else(|| bad(format!("{} is not a subrepository", path)))?;
            let head = crate::repo::subrepo::child_head(&root, &s).ok_or_else(|| {
                bad(format!(
                    "{} has no repository checkout, so there is nothing to pin",
                    s.path
                ))
            })?;
            let s = crate::repo::subrepo::pin(&conn, &path, &head)?;
            println!("pinned {} at {}", s.path, s.pinned);
            Ok(())
        }
        SubCmd::Remove { path } => {
            let (_root, conn) = open_repo()?;
            if crate::repo::subrepo::remove(&conn, &path)? {
                println!("removed subrepository {}", path);
            } else {
                println!("{} was not a subrepository", path);
            }
            Ok(())
        }
    }
}

/// Handle `gesta submodule`.
pub fn cmd_submodule(sub: crate::SubmoduleCmd, json: bool) -> ZResult<()> {
    use crate::SubmoduleCmd;
    match sub {
        SubmoduleCmd::Add { path, url, branch } => {
            let (root, _conn) = open_repo()?;
            let mut subs = crate::repo::submodule::read_gitmodules(&root)?;
            if subs.contains_key(&path) {
                return Err(bad(format!("submodule {} already exists", path)));
            }
            subs.insert(
                path.clone(),
                crate::repo::submodule::Submodule {
                    path: path.clone(),
                    url: url.clone(),
                    branch,
                    commit: None,
                },
            );
            crate::repo::submodule::write_gitmodules(&root, &subs)?;
            println!("added submodule {} -> {}", path, url.clone());
            println!("run `gesta submodule init` to clone it");
            Ok(())
        }
        SubmoduleCmd::Init => {
            let (root, _conn) = open_repo()?;
            let cloned = crate::repo::submodule::submodule_init(&root)?;
            if cloned.is_empty() {
                println!("all submodules already initialized");
            } else {
                for c in cloned {
                    println!("cloned {}", c);
                }
            }
            Ok(())
        }
        SubmoduleCmd::Update => {
            let (root, _conn) = open_repo()?;
            let updated = crate::repo::submodule::submodule_update(&root)?;
            if updated.is_empty() {
                println!("all submodules up to date");
            } else {
                for u in updated {
                    println!("updated {}", u);
                }
            }
            Ok(())
        }
        SubmoduleCmd::Status => {
            let (root, _conn) = open_repo()?;
            let statuses = crate::repo::submodule::submodule_status(&root)?;
            if json {
                let parts: Vec<String> = statuses
                    .iter()
                    .map(|s| format!(
                        "{{\"path\":\"{}\",\"commit\":\"{}\",\"pinned\":{},                        \"dirty\":{},                        \"missing\":{}}}",
                        s.path,
                        s.commit,
                        s.pinned.as_ref().map(|p| format!("\"{}\"", p)).unwrap_or("null".to_string()),
                        s.dirty,
                        s.missing
                    ))
                    .collect();
                println!("[{}]", parts.join(","));
                return Ok(());
            }
            if statuses.is_empty() {
                println!("no submodules");
                return Ok(());
            }
            for s in &statuses {
                let state = if s.missing {
                    "missing"
                } else if s.dirty {
                    "dirty"
                } else if !crate::repo::submodule::matches_pin(s.pinned.as_deref(), &s.commit) {
                    "behind"
                } else {
                    "clean"
                };
                let pin = s.pinned.as_deref().unwrap_or("unpinned");
                println!("{}  {}  {}  {}", s.path, s.commit, pin, state);
            }
            Ok(())
        }
        SubmoduleCmd::Sync => {
            let (root, _conn) = open_repo()?;
            let subs = crate::repo::submodule::read_gitmodules(&root)?;
            for sub in subs.values() {
                println!("{}  {}", sub.path, sub.url);
            }
            Ok(())
        }
    }
}

pub fn cmd_hook(sub: crate::HookCmd, json: bool) -> ZResult<()> {
    use crate::HookCmd;
    match sub {
        HookCmd::List => {
            let (root, _conn) = open_repo()?;
            for (name, installed) in crate::repo::hooks::list(&root) {
                println!(
                    "{}  {}",
                    if installed { "installed" } else { "absent   " },
                    name
                );
            }
            Ok(())
        }
        HookCmd::Install { name, file } => {
            let (root, _conn) = open_repo()?;
            let body = match file {
                Some(path) => std::fs::read_to_string(&path)
                    .map_err(|e| bad(format!("cannot read {}: {}", path, e)))?,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };
            if body.trim().is_empty() {
                return Err(bad("a hook needs a body: pass --file or write it on stdin"));
            }
            let path = crate::repo::hooks::install(&root, &name, &body)?;
            println!("installed {} at {}", name, path.display());
            Ok(())
        }
        HookCmd::Status { json_status } => cmd_hook_status(json_status || json),
        HookCmd::Remove { name } => {
            let (root, _conn) = open_repo()?;
            if crate::repo::hooks::remove(&root, &name)? {
                println!("removed {}", name);
            } else {
                println!("{} was not installed", name);
            }
            Ok(())
        }
    }
}

/// Report whether each hook still matches what was installed.
///
/// A hook runs with the user's rights. The interesting event is a hook that changed after it was
/// installed, because that is either a deliberate edit somebody should see or an edit nobody
/// asked for. Both deserve to be visible before the next commit runs it.
pub fn cmd_hook_status(json: bool) -> ZResult<()> {
    let (root, _conn) = open_repo()?;
    let states = crate::repo::hooks::status(&root)?;
    if json {
        let mut items = Vec::new();
        for s in &states {
            items.push(crate::support::json::Json::obj(vec![
                ("name", crate::support::json::Json::str(s.name.clone())),
                ("installed", crate::support::json::Json::Bool(s.installed)),
                ("changed", crate::support::json::Json::Bool(s.changed())),
                (
                    "recorded",
                    match &s.recorded {
                        Some(h) => crate::support::json::Json::str(h.clone()),
                        None => crate::support::json::Json::Null,
                    },
                ),
                (
                    "current",
                    match &s.current {
                        Some(h) => crate::support::json::Json::str(h.clone()),
                        None => crate::support::json::Json::Null,
                    },
                ),
            ]));
        }
        emit_object(vec![
            ("hooks", crate::support::json::Json::Arr(items)),
            (
                "changed",
                crate::support::json::Json::int(
                    states.iter().filter(|s| s.changed()).count() as i64
                ),
            ),
        ]);
        return Ok(());
    }
    for s in &states {
        if !s.installed {
            println!("absent     {}", s.name);
        } else if s.changed() {
            println!("changed    {}", s.name);
        } else if s.recorded.is_some() {
            println!("unchanged  {}", s.name);
        } else {
            println!("unrecorded {}", s.name);
        }
    }
    let changed = states.iter().filter(|s| s.changed()).count();
    if changed > 0 {
        println!("{} hook(s) changed since installation", changed);
    }
    Ok(())
}
