//! Command handlers.
//!
//! Kept out of `main.rs` so the entry point stays a thin dispatcher, and split into one module per
//! question a reader arrives with rather than by file size. A command's module is the answer to
//! "where would I look for this", and that is the only grouping that survives a reorganisation.
//!
//! `main.rs` reaches every handler through the re-exports below, so splitting a module moves code
//! without changing a call site.

pub mod backup;
pub mod branch;
pub mod brief;
pub mod changeset;
pub mod cli_remote;
pub mod cmd_history;
pub mod cmd_ops;
pub mod cmd_repo;
pub mod git_bridge;
pub mod gitcmds;
pub mod jsonutil;
pub mod keys;
pub mod review;
pub mod subcmd;
pub mod tree;

// The handlers, re-exported so `cli::cmd_status` keeps working wherever it is named. A reader who
// wants the body follows the module; a caller does not have to know which module it landed in.
pub use backup::*;
pub use branch::*;
pub use changeset::*;
pub use cmd_history::*;
pub use cmd_ops::*;
pub use cmd_repo::*;
pub use gitcmds::*;
pub use keys::*;
pub use review::*;
pub use subcmd::*;
pub use tree::*;

// Helpers that one module defines and another calls. Declared here rather than repeated in each
// module's `use super::{...}`, because `use super::*` then reaches them and a new module does not have
// to know which file a helper landed in.
pub(crate) use jsonutil::{emit, emit_object, envelope};
pub(crate) use keys::read_hidden;

use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

// The modules reach these through `use super::*`, so the import list lives here rather than twelve
// times over. It is the set the command handlers actually share: opening the repository, the store
// calls every command makes, and the error constructors.
use crate::repo;
use crate::repo::history;
use crate::repo::merge;
use crate::store::walk;
use crate::store::{
    self, branch_head, current_branch, get_object, hash_at_point, init_schema, open_db,
    reachable_from, set_state,
};
use crate::support::json::Json;
use crate::support::ops;
use crate::transport::mcp;
use crate::{bad, fmt_ts, normalise, now, open_repo, repo_dir, repo_root, ZResult};

/// How a search was asked for, so the two entry points share one shape.
///
/// Gathered into a struct rather than passed as eight arguments: the call sites read as
/// `Search { pattern: "x", history: true, ..Default::default() }`, and a new flag does not change
/// every signature that carries it.
#[derive(Default, Clone)]
pub struct Search {
    pub pattern: String,
    pub history: bool,
    pub ignore_case: bool,
    pub word: bool,
    pub regex: bool,
    pub path: Option<String>,
    pub limit: i64,
    pub files_with_matches: bool,
    /// Cut each matching line to this many characters. Zero keeps whole lines.
    pub max_line: usize,
}

/// Shorthand for the JSON command return type.
pub(crate) type YResultAlias = ZResult<()>;

/// The version of every JSON document this program prints.
///
/// An agent reads this output and branches on its field names. Renaming a field without saying so
/// breaks a caller silently, which is the worst kind of break: it keeps running and does the wrong
/// thing. The version goes on every object so a caller can refuse one it does not know.
pub const SCHEMA_VERSION: &str = "gesta@1";

/// Set once anything has been printed as JSON, so a later failure does not append a second object to
/// a document that already went out.
pub static JSON_PRINTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod pending_tests {
    /// A pending changeset is a snapshot of an earlier moment. If the file moved on before it was
    /// committed, committing it stores content that does not describe the disk, and nothing else
    /// would say so.
    #[test]
    fn a_pending_changeset_that_no_longer_matches_the_disk_is_reported() {
        let conn = crate::store::tests::mem_db();
        let dir = std::env::temp_dir().join(format!("gesta-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
        crate::repo::tests::commit_files(&conn, "p1", "base", &[("f.txt", "v1\n")]);
        crate::store::set_branch_head(&conn, "main", "p1").unwrap();
        crate::store::set_state(&conn, "current_branch", "main").unwrap();

        // Snapshot a change without committing it.
        std::fs::write(dir.join("f.txt"), "v2\n").unwrap();
        let cs = crate::repo::create_changeset(&conn, &dir, "snapshot", "a")
            .unwrap()
            .expect("there is a change to snapshot");

        // Matching the disk, so nothing is stale yet.
        let clean = crate::cli::pending_out_of_date(&conn, &dir, 0).unwrap();
        assert!(
            clean.is_empty(),
            "a fresh snapshot must not be reported: {:?}",
            clean
        );

        // The file moves on before the commit.
        std::fs::write(dir.join("f.txt"), "v3\n").unwrap();
        let stale = crate::cli::pending_out_of_date(&conn, &dir, 0).unwrap();
        assert_eq!(stale.len(), 1, "the stale snapshot was not found");
        assert_eq!(stale[0].0, cs.id);
        assert_eq!(stale[0].1, vec!["f.txt".to_string()]);

        // Deleting the file is also a mismatch.
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        let gone = crate::cli::pending_out_of_date(&conn, &dir, 0).unwrap();
        assert_eq!(
            gone.len(),
            1,
            "a file removed after the snapshot was not reported"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
