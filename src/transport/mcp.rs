//! MCP server over stdio.
//!
//! Speaks JSON-RPC 2.0 on stdin/stdout, one message per line, which is what the
//! stdio transport of the Model Context Protocol expects. Tools are declared once
//! in `tool_definitions` and dispatched in `call_tool`, so adding a tool means
//! touching both in one place.

use std::io::{BufRead, Write};
use std::path::Path;

use rusqlite::Connection;

use crate::store::{branch_head, current_branch, get_object, hash_at_point};
use crate::support::json::{self, field, Json};
use crate::ZResult;
use crate::{repo, repo::history, store};

/// Protocol version this server implements.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Tool definitions, in a fixed order so clients can cache the list.
pub fn tool_definitions() -> Json {
    Json::arr(vec![
        tool(
            "gesta_status",
            "Repository state: current branch, head changeset, pending changesets and the files that differ from the head.",
            Json::obj(vec![("type", Json::str("object")), ("properties", Json::obj(vec![]))]),
        ),
        tool(
            "gesta_log",
            "List changesets, newest first.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("limit", Json::obj(vec![("type", Json::str("integer")), ("description", Json::str("How many changesets to return, default 20."))])),
                        ("branch", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Branch name, default the current branch."))])),
                    ]),
                ),
            ]),
        ),
        tool(
            "gesta_ls",
            "List the files a revision holds, so a path can be named without guessing. Use this before reading or editing anything: it answers what exists at that point.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("rev", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Branch, tag or changeset. Default the current branch."))])),
                        ("path", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Only paths under this prefix."))])),
                    ]),
                ),
            ]),
        ),
        tool(
            "gesta_read_file",
            "Read a file from the repository without checking it out. Pass a changeset or branch to read an older version.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("path", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Repository relative path."))])),
                        ("rev", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Changeset id or branch name. Defaults to the current branch head."))])),
                    ]),
                ),
                ("required", Json::arr(vec![Json::str("path")])),
            ]),
        ),
        tool(
            "gesta_blame",
            "Line ownership for a file: which changeset and author introduced each line.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![("path", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Repository relative path."))]))]),
                ),
                ("required", Json::arr(vec![Json::str("path")])),
            ]),
        ),
        tool(
            "gesta_diff",
            "Structured diff for one changeset, or for the working tree against the current branch.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("changeset", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Changeset id. Omit to diff the working tree."))])),
                    ]),
                ),
            ]),
        ),
        tool(
            "gesta_merge_conflicts",
            "Report what merging a branch would conflict on, with both sides and line numbers, without touching anything.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("source", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Branch to merge from."))])),
                        ("target", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Branch to merge into, default the current branch."))])),
                    ]),
                ),
                ("required", Json::arr(vec![Json::str("source")])),
            ]),
        ),
        tool(
            "gesta_brief",
            "Everything a session needs before its first edit, in one call: branch, head, whether a rebase or merge stopped, pending changesets, stale snapshots, stashes and how many files differ from the head. Start here. Read only: to act, call gesta_fix.",
            Json::obj(vec![
                ("type", Json::str("object")),
                ("properties", Json::obj(vec![])),
            ]),
        ),
        tool(
            "gesta_fix",
            "Do the safe part of what gesta_brief reports: commit a snapshot that matches the disk exactly, and drop one the disk has moved past. Refuses a stopped rebase, an unresolved conflict, or uncommitted work no snapshot describes, naming the command instead. Set stash to true to set uncommitted work aside rather than refuse it.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![(
                        "stash",
                        Json::obj(vec![
                            ("type", Json::str("boolean")),
                            ("description", Json::str("Set uncommitted work aside as a stash instead of refusing it.")),
                        ]),
                    )]),
                ),
            ]),
        ),
        tool(
            "gesta_grep",
            "Find text. With history true it searches every recorded version, including text that was deleted, and returns the changeset that holds each match.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("pattern", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Text to find."))])),
                        ("history", Json::obj(vec![("type", Json::str("boolean")), ("description", Json::str("Search every recorded version instead of the files on disk."))])),
                        ("word", Json::obj(vec![("type", Json::str("boolean")), ("description", Json::str("Match whole words only."))])),
                        ("ignore_case", Json::obj(vec![("type", Json::str("boolean")), ("description", Json::str("Case insensitive."))])),
                        ("path", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Only paths starting with this prefix."))])),
                    ]),
                ),
                ("required", Json::arr(vec![Json::str("pattern")])),
            ]),
        ),
        tool(
            "gesta_create_changeset",
            "Snapshot the working tree as a pending changeset. Nothing is committed until gesta_commit.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![
                        ("message", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Changeset message."))])),
                        ("author", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Author name, default agent."))])),
                    ]),
                ),
                ("required", Json::arr(vec![Json::str("message")])),
            ]),
        ),
        tool(
            "gesta_commit",
            "Commit a pending changeset and move the branch head.",
            Json::obj(vec![
                ("type", Json::str("object")),
                (
                    "properties",
                    Json::obj(vec![("id", Json::obj(vec![("type", Json::str("string")), ("description", Json::str("Changeset id returned by gesta_create_changeset."))]))]),
                ),
                ("required", Json::arr(vec![Json::str("id")])),
            ]),
        ),
    ])
}

fn tool(name: &str, description: &str, schema: Json) -> Json {
    Json::obj(vec![
        ("name", Json::str(name)),
        ("description", Json::str(description)),
        ("inputSchema", schema),
    ])
}

/// Run the server on stdin/stdout until the client closes the stream.
pub fn serve_stdio(root: &Path) -> ZResult<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut lines = stdin.lock().lines();

    while let Some(Ok(line)) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let response = handle_line(root, line);
        if let Some(r) = response {
            writeln!(stdout, "{}", r.render())?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC message. `None` means "no response" (a notification).
pub fn handle_line(root: &Path, line: &str) -> Option<Json> {
    let req = match json::parse(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Json::Null,
                -32700,
                &format!("parse error: {}", e),
            ));
        }
    };

    let id = field(&req, "id").cloned().unwrap_or(Json::Null);
    let method = match field(&req, "method").and_then(json::as_str) {
        Some(m) => m.to_string(),
        None => {
            // No method: a response to something we never sent. Ignore it.
            return None;
        }
    };

    // A request without an id is a notification, and must not be answered.
    let is_notification = field(&req, "id").is_none();

    let result = match method.as_str() {
        "initialize" => Ok(initialize_result()),
        "notifications/initialized" => return None,
        // Cache hints, because a tool catalog is sent with every request.
        //
        // A client that cannot cache the catalog pays for all of it on every model call, including
        // the tools it never invokes. The list here changes only when the binary changes, so the
        // TTL is long and the scope is private: two processes of the same build agree, and a
        // client is free to keep the answer for as long as it holds the connection.
        // One call that answers what the server can do, for a client that wants to know before it
        // asks for anything. The spec makes it optional; a client that skips it costs one extra
        // round trip on its first real call, and a client that uses it can size its own budget.
        "server/discover" => Ok(Json::obj(vec![
            ("protocolVersion", Json::str(PROTOCOL_VERSION)),
            (
                "serverInfo",
                Json::obj(vec![
                    ("name", Json::str("gesta")),
                    ("version", Json::str(env!("CARGO_PKG_VERSION"))),
                ]),
            ),
            (
                "capabilities",
                Json::obj(vec![(
                    "tools",
                    Json::obj(vec![("listChanged", Json::Bool(false))]),
                )]),
            ),
            (
                "instructions",
                Json::str(
                    "A version-control server. Read `gesta_brief` first in a new session: it reports \
                     the branch, the head, what is pending, and what the repository is waiting for, \
                     in one call. `gesta_status` reports state; `gesta_brief` reports what to do next.",
                ),
            ),
        ])),
        "tools/list" => Ok(Json::obj(vec![
            ("tools", tool_definitions()),
            ("ttlMs", Json::int(3_600_000)),
            ("cacheScope", Json::str("private")),
        ])),
        "tools/call" => {
            let params = field(&req, "params").cloned().unwrap_or(Json::Null);
            call_tool(root, &params)
        }
        "ping" => Ok(Json::obj(vec![])),
        other => Err((-32601, format!("method not found: {}", other))),
    };

    if is_notification {
        return None;
    }

    Some(match result {
        Ok(value) => Json::obj(vec![
            ("jsonrpc", Json::str("2.0")),
            ("id", id),
            ("result", value),
        ]),
        Err((code, msg)) => error_response(id, code, &msg),
    })
}

fn error_response(id: Json, code: i64, message: &str) -> Json {
    Json::obj(vec![
        ("jsonrpc", Json::str("2.0")),
        ("id", id),
        (
            "error",
            Json::obj(vec![
                ("code", Json::Int(code)),
                ("message", Json::str(message)),
            ]),
        ),
    ])
}

fn initialize_result() -> Json {
    Json::obj(vec![
        ("protocolVersion", Json::str(PROTOCOL_VERSION)),
        (
            "capabilities",
            Json::obj(vec![(
                "tools",
                Json::obj(vec![("listChanged", Json::Bool(false))]),
            )]),
        ),
        (
            "serverInfo",
            Json::obj(vec![
                ("name", Json::str("gesta")),
                ("version", Json::str(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
    ])
}

type ToolError = (i64, String);

/// Prefix a tool uses to mark a failure as the caller's argument rather than the server's state.
///
/// A prefix rather than a second error type: the distinction has to survive twelve tool functions,
/// and what matters is the code the client receives, not the type this file uses to carry it. The
/// prefix is stripped before the message reaches the client, so it never appears in an error.
const PARAM_ERROR: &str = "params: ";

fn call_tool(root: &Path, params: &Json) -> Result<Json, ToolError> {
    let name = field(params, "name")
        .and_then(json::as_str)
        .ok_or((-32602, "tools/call needs a tool name".to_string()))?;
    let args = field(params, "arguments").cloned().unwrap_or(Json::Null);

    let conn = store::open_db(root).map_err(|e| (-32603, e.to_string()))?;
    store::init_schema(&conn).map_err(|e| (-32603, e.to_string()))?;

    // Tools are named for the project. A client configured before the rename
    // still sends the old prefix, so it is mapped onto the same handler rather
    // than failing with "unknown tool".
    let name = name
        .strip_prefix("zenforge_")
        .map(|rest| format!("gesta_{}", rest))
        .unwrap_or(name.to_string());
    let name = name.as_str();

    let value = match name {
        "gesta_brief" => tool_brief(&conn, root),
        "gesta_fix" => tool_fix(&conn, root, &args),
        "gesta_grep" => tool_grep(&conn, root, &args),
        "gesta_status" => tool_status(&conn),
        "gesta_log" => tool_log(&conn, &args),
        "gesta_read_file" => tool_read_file(&conn, &args),
        "gesta_ls" => tool_ls(&conn, &args),
        "gesta_blame" => tool_blame(&conn, &args),
        "gesta_diff" => tool_diff(&conn, root, &args),
        "gesta_merge_conflicts" => tool_merge_conflicts(&conn, root, &args),
        "gesta_create_changeset" => tool_create_changeset(&conn, root, &args),
        "gesta_commit" => tool_commit(&conn, &args),
        other => return Err((-32602, format!("unknown tool: {}", other))),
    }
    .map_err(|e| {
        // A missing or malformed argument is the caller's fault and JSON-RPC has a code for it. A
        // client that receives -32603 cannot tell "my request was wrong" from "the server broke",
        // and the two lead to opposite decisions: fix the call, or stop and report a fault.
        if e.starts_with(PARAM_ERROR) {
            (-32602, e.trim_start_matches(PARAM_ERROR).to_string())
        } else {
            (-32603, e)
        }
    })?;

    // Every tool result carries the schema version, so a client can refuse one it does not
    // know instead of reading fields by the wrong names and running on quietly wrong data.
    let value = match value {
        Json::Obj(mut m) => {
            m.entry("schema".to_string())
                .or_insert_with(|| Json::str(crate::cli::SCHEMA_VERSION));
            Json::Obj(m)
        }
        other => other,
    };

    // MCP wraps every tool result in a content array. The JSON is also returned
    // as text so a model that only reads `content` still sees the data.
    let text = value.render();
    Ok(Json::obj(vec![
        (
            "content",
            Json::arr(vec![Json::obj(vec![
                ("type", Json::str("text")),
                ("text", Json::str(text)),
            ])]),
        ),
        ("structuredContent", value),
        ("isError", Json::Bool(false)),
    ]))
}

fn arg_str(args: &Json, key: &str) -> Option<String> {
    field(args, key)
        .and_then(json::as_str)
        .map(|s| s.to_string())
}

fn tool_brief(conn: &Connection, root: &Path) -> Result<Json, String> {
    let b = crate::cli::brief::brief(conn, root).map_err(|e| e.to_string())?;
    Ok(crate::cli::brief::to_json(&b))
}

fn tool_fix(conn: &Connection, root: &Path, args: &Json) -> Result<Json, String> {
    let stash = field(args, "stash")
        .and_then(json::as_bool)
        .unwrap_or(false);
    let fixed = crate::cli::brief::fix_with(
        conn,
        root,
        crate::cli::brief::Options {
            stash_uncommitted: stash,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(crate::cli::brief::fixed_to_json(&fixed))
}

fn tool_grep(conn: &Connection, root: &Path, args: &Json) -> Result<Json, String> {
    let needle = field(args, "pattern")
        .and_then(json::as_str)
        .ok_or("gesta_grep needs a pattern".to_string())?;
    let history = field(args, "history")
        .and_then(json::as_bool)
        .unwrap_or(false);
    let word = field(args, "word").and_then(json::as_bool).unwrap_or(false);
    let ignore_case = field(args, "ignore_case")
        .and_then(json::as_bool)
        .unwrap_or(false);
    let path = field(args, "path").and_then(json::as_str);

    let pattern =
        crate::support::grep::Pattern::new(needle, ignore_case, word).map_err(|e| e.to_string())?;
    let hits = if history {
        crate::support::grep::search_history(conn, &pattern, path, 5000)
    } else {
        crate::support::grep::search_worktree(conn, root, &pattern, path)
    }
    .map_err(|e| e.to_string())?;

    Ok(Json::obj(vec![
        ("count", Json::int(hits.len() as i64)),
        (
            "hits",
            Json::arr(
                hits.iter()
                    .map(|h| {
                        Json::obj(vec![
                            ("path", Json::str(h.path.clone())),
                            ("line", Json::int(h.line as i64)),
                            ("rev", Json::str(h.rev.clone())),
                            ("text", Json::str(h.text.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
    ]))
}

fn tool_status(conn: &Connection) -> Result<Json, String> {
    let branch = current_branch(conn).map_err(|e| e.to_string())?;
    let head = branch_head(conn, &branch).map_err(|e| e.to_string())?;

    let pending: Vec<Json> = {
        let mut stmt = conn
            .prepare("SELECT id, message, author, created_at FROM changesets WHERE status='pending' ORDER BY created_at, rowid")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Json::obj(vec![
                    ("id", Json::str(r.get::<_, String>(0)?)),
                    ("message", Json::str(r.get::<_, String>(1)?)),
                    ("author", Json::str(r.get::<_, String>(2)?)),
                    ("created_at", Json::int(r.get::<_, i64>(3)?)),
                ]))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?
    };

    let tracked = {
        let mut stmt = conn
            .prepare("SELECT COUNT(DISTINCT path) FROM file_versions")
            .map_err(|e| e.to_string())?;
        stmt.query_row([], |r| r.get::<_, i64>(0)).unwrap_or(0)
    };

    Ok(Json::obj(vec![
        ("branch", Json::str(branch)),
        ("head", head.map(Json::str).unwrap_or(Json::Null)),
        ("pending_changesets", Json::arr(pending)),
        ("tracked_paths", Json::int(tracked)),
    ]))
}

fn tool_log(conn: &Connection, args: &Json) -> Result<Json, String> {
    let limit = field(args, "limit")
        .and_then(json::as_i64)
        .unwrap_or(20)
        .clamp(1, 1000);
    let mut stmt = conn
        .prepare(
            "SELECT id, message, author, created_at, status FROM changesets
             ORDER BY created_at DESC, rowid DESC LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([limit], |r| {
            Ok(Json::obj(vec![
                ("id", Json::str(r.get::<_, String>(0)?)),
                ("message", Json::str(r.get::<_, String>(1)?)),
                ("author", Json::str(r.get::<_, String>(2)?)),
                ("created_at", Json::int(r.get::<_, i64>(3)?)),
                ("status", Json::str(r.get::<_, String>(4)?)),
            ]))
        })
        .map_err(|e| e.to_string())?;
    let out = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    Ok(Json::arr(out))
}

fn tool_ls(conn: &Connection, args: &Json) -> Result<Json, String> {
    let rev = match arg_str(args, "rev") {
        Some(r) => r,
        None => current_branch(conn).map_err(|e| e.to_string())?,
    };
    let cs = crate::store::resolve_rev(conn, &rev)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no such branch, tag or changeset: {}", rev))?;
    let mut tree: Vec<(String, String)> = crate::store::tree_at(conn, &cs)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    tree.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(prefix) = arg_str(args, "path") {
        let p = crate::normalise(&prefix);
        let p = p.trim_end_matches('/');
        tree.retain(|(path, _)| path == p || path.starts_with(&format!("{}/", p)));
    }
    Ok(Json::obj(vec![
        (
            "files",
            Json::arr(tree.iter().map(|(p, _)| Json::str(p.clone())).collect()),
        ),
        ("count", Json::int(tree.len() as i64)),
        ("rev", Json::str(rev)),
    ]))
}

fn tool_read_file(conn: &Connection, args: &Json) -> Result<Json, String> {
    let path = arg_str(args, "path").ok_or("params: read_file needs a path")?;
    let rev = match arg_str(args, "rev") {
        Some(r) => r,
        None => current_branch(conn).map_err(|e| e.to_string())?,
    };
    let cs = crate::store::resolve_rev(conn, &rev)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no such branch, tag or changeset: {}", rev))?;
    let hash = hash_at_point(conn, &path, &cs)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{} does not exist at {}", path, rev))?;
    let content = get_object(conn, &hash).map_err(|e| e.to_string())?;
    Ok(Json::obj(vec![
        ("path", Json::str(path)),
        ("rev", Json::str(rev)),
        ("changeset", Json::str(cs)),
        ("hash", Json::str(hash)),
        ("content", Json::str(content)),
    ]))
}

fn tool_blame(conn: &Connection, args: &Json) -> Result<Json, String> {
    let path = arg_str(args, "path").ok_or("params: blame needs a path")?;
    let mut stmt = conn
        .prepare(
            "SELECT b.start_line, b.end_line, b.changeset_id, c.author FROM blame_ranges b
             LEFT JOIN changesets c ON c.id = b.changeset_id WHERE b.path = ?1 ORDER BY b.start_line",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([&path], |r| {
            Ok(Json::obj(vec![
                ("line", Json::int(r.get::<_, i64>(0)?)),
                ("changeset", Json::str(r.get::<_, String>(1)?)),
                ("author", Json::str(r.get::<_, String>(2)?)),
            ]))
        })
        .map_err(|e| e.to_string())?;
    let out = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    if out.is_empty() {
        return Err(format!("no blame data for {}", path));
    }
    Ok(Json::obj(vec![
        ("path", Json::str(path)),
        ("lines", Json::arr(out)),
    ]))
}

fn tool_diff(conn: &Connection, root: &Path, args: &Json) -> Result<Json, String> {
    match arg_str(args, "changeset") {
        Some(id) => {
            let files = repo::changeset_diff(conn, &id).map_err(|e| e.to_string())?;
            let renames = history::detect_renames(conn, &id, 0.5).unwrap_or_default();
            Ok(Json::obj(vec![
                ("changeset", Json::str(id)),
                (
                    "files",
                    Json::arr(
                        files
                            .into_iter()
                            .map(|(path, removed, added)| {
                                Json::obj(vec![
                                    ("path", Json::str(path)),
                                    (
                                        "removed",
                                        Json::arr(removed.into_iter().map(Json::str).collect()),
                                    ),
                                    (
                                        "added",
                                        Json::arr(added.into_iter().map(Json::str).collect()),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "renames",
                    Json::arr(
                        renames
                            .into_iter()
                            .map(|r| {
                                Json::obj(vec![
                                    ("from", Json::str(r.from)),
                                    ("to", Json::str(r.to)),
                                    ("similarity", Json::Float(r.similarity)),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]))
        }
        None => {
            let branch = current_branch(conn).map_err(|e| e.to_string())?;
            let d = repo::diff_worktree(conn, root, &branch).map_err(|e| e.to_string())?;
            Ok(Json::obj(vec![
                ("branch", Json::str(branch)),
                (
                    "added",
                    Json::arr(d.added.into_iter().map(Json::str).collect()),
                ),
                (
                    "modified",
                    Json::arr(d.modified.into_iter().map(Json::str).collect()),
                ),
                (
                    "deleted",
                    Json::arr(d.deleted.into_iter().map(Json::str).collect()),
                ),
                (
                    "skipped_binary",
                    Json::arr(d.skipped_binary.into_iter().map(Json::str).collect()),
                ),
                ("unchanged", Json::int(d.unchanged as i64)),
            ]))
        }
    }
}

fn tool_merge_conflicts(conn: &Connection, root: &Path, args: &Json) -> Result<Json, String> {
    let source = arg_str(args, "source").ok_or("params: merge_conflicts needs a source branch")?;
    let target = match arg_str(args, "target") {
        Some(t) => t,
        None => current_branch(conn).map_err(|e| e.to_string())?,
    };
    let reports = repo::merge_preview(conn, root, &source, &target).map_err(|e| e.to_string())?;
    Ok(crate::repo::conflict::reports_to_json(&reports))
}

fn tool_create_changeset(conn: &Connection, root: &Path, args: &Json) -> Result<Json, String> {
    let message = arg_str(args, "message").ok_or("params: create_changeset needs a message")?;
    let author = arg_str(args, "author").unwrap_or_else(|| "agent".to_string());
    match store::with_retry(|| repo::create_changeset(conn, root, &message, &author))
        .map_err(|e| e.to_string())?
    {
        None => Ok(Json::obj(vec![
            ("created", Json::Bool(false)),
            ("reason", Json::str("working tree matches the branch head")),
        ])),
        Some(cs) => Ok(Json::obj(vec![
            ("created", Json::Bool(true)),
            ("id", Json::str(cs.id)),
            (
                "files",
                Json::arr(
                    cs.files
                        .into_iter()
                        .map(|(path, old, _)| {
                            Json::obj(vec![
                                ("path", Json::str(path)),
                                (
                                    "kind",
                                    Json::str(if old.is_none() { "added" } else { "modified" }),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "deleted",
                Json::arr(cs.deleted.into_iter().map(Json::str).collect()),
            ),
        ])),
    }
}

fn tool_commit(conn: &Connection, args: &Json) -> Result<Json, String> {
    let id = arg_str(args, "id").ok_or("params: commit needs an id")?;
    let n = store::with_retry(|| repo::commit_one(conn, &id)).map_err(|e| e.to_string())?;
    let branch = current_branch(conn).map_err(|e| e.to_string())?;
    Ok(Json::obj(vec![
        ("id", Json::str(id)),
        ("committed", Json::Bool(n > 0)),
        ("files", Json::int(n as i64)),
        ("branch", Json::str(branch)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_repo(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zf_mcp_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        // The database lives inside the repository directory, so that directory has
        // to exist before open_db, exactly as `gesta init` would leave it.
        std::fs::create_dir_all(crate::repo_dir(&d)).unwrap();
        let conn = store::open_db(&d).unwrap();
        store::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO branches(name,head,created_at,from_branch) VALUES('main',NULL,0,NULL)",
            [],
        )
        .unwrap();
        store::set_state(&conn, "current_branch", "main").unwrap();
        d
    }

    fn rpc(root: &Path, text: &str) -> Json {
        handle_line(root, text).expect("expected a response")
    }

    #[test]
    fn initialize_reports_the_protocol_and_identity() {
        let d = tmp_repo("init");
        let r = rpc(
            &d,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        );
        let result = field(&r, "result").unwrap();
        assert_eq!(
            json::as_str(field(result, "protocolVersion").unwrap()),
            Some(PROTOCOL_VERSION)
        );
        let info = field(result, "serverInfo").unwrap();
        assert_eq!(json::as_str(field(info, "name").unwrap()), Some("gesta"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn tools_list_returns_every_tool_with_a_schema() {
        let d = tmp_repo("list");
        let r = rpc(&d, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = match field(field(&r, "result").unwrap(), "tools").unwrap() {
            Json::Arr(items) => items.clone(),
            _ => panic!("tools must be an array"),
        };
        assert!(
            tools.len() >= 7,
            "expected the full tool set, got {}",
            tools.len()
        );
        for t in &tools {
            assert!(field(t, "name").is_some());
            assert!(field(t, "inputSchema").is_some());
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_notification_gets_no_response() {
        let d = tmp_repo("notif");
        assert!(handle_line(
            &d,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        )
        .is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unknown_method_is_a_jsonrpc_error() {
        let d = tmp_repo("bad");
        let r = rpc(&d, r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#);
        let err = field(&r, "error").unwrap();
        assert_eq!(json::as_i64(field(err, "code").unwrap()), Some(-32601));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_json_is_reported_not_crashed() {
        let d = tmp_repo("malformed");
        let r = rpc(&d, "{not json");
        let err = field(&r, "error").unwrap();
        assert_eq!(json::as_i64(field(err, "code").unwrap()), Some(-32700));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn status_on_an_empty_repository() {
        let d = tmp_repo("status");
        let r = rpc(
            &d,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"gesta_status","arguments":{}}}"#,
        );
        let sc = field(field(&r, "result").unwrap(), "structuredContent").unwrap();
        assert_eq!(json::as_str(field(sc, "branch").unwrap()), Some("main"));
        assert!(matches!(field(sc, "head").unwrap(), Json::Null));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn create_then_commit_through_the_tool_interface() {
        let d = tmp_repo("commit");
        std::fs::write(d.join("a.txt"), "hello\n").unwrap();

        let r = rpc(
            &d,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"gesta_create_changeset","arguments":{"message":"first","author":"tester"}}}"#,
        );
        let sc = field(field(&r, "result").unwrap(), "structuredContent").unwrap();
        let id = json::as_str(field(sc, "id").unwrap()).unwrap().to_string();

        let call = format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{{"name":"gesta_commit","arguments":{{"id":"{}"}}}}}}"#,
            id
        );
        let r = rpc(&d, &call);
        let sc = field(field(&r, "result").unwrap(), "structuredContent").unwrap();
        assert!(matches!(field(sc, "committed").unwrap(), Json::Bool(true)));

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_file_returns_content_without_a_working_copy() {
        let d = tmp_repo("read");
        std::fs::write(d.join("a.txt"), "line one\n").unwrap();
        let conn = store::open_db(&d).unwrap();
        repo::create_changeset(&conn, &d, "add", "t").unwrap();
        let pending: String = conn
            .query_row(
                "SELECT id FROM changesets WHERE status='pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        repo::commit_one(&conn, &pending).unwrap();
        drop(conn);
        std::fs::remove_file(d.join("a.txt")).unwrap();

        let r = rpc(
            &d,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"gesta_read_file","arguments":{"path":"a.txt"}}}"#,
        );
        let sc = field(field(&r, "result").unwrap(), "structuredContent").unwrap();
        assert_eq!(
            json::as_str(field(sc, "content").unwrap()),
            Some("line one\n")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unknown_tool_is_rejected() {
        let d = tmp_repo("unknown");
        let r = rpc(
            &d,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        );
        assert!(field(&r, "error").is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_declared_tool_is_dispatchable() {
        // A tool listed but not dispatched would be a silent trap for a client.
        let declared: Vec<String> = match tool_definitions() {
            Json::Arr(items) => items
                .iter()
                .filter_map(|t| {
                    field(t, "name")
                        .and_then(json::as_str)
                        .map(|s| s.to_string())
                })
                .collect(),
            _ => Vec::new(),
        };
        let d = tmp_repo("dispatch");
        for name in declared {
            let call = format!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"{}","arguments":{{}}}}}}"#,
                name
            );
            let r = rpc(&d, &call);
            if let Some(err) = field(&r, "error") {
                let msg = json::as_str(field(err, "message").unwrap()).unwrap_or("");
                assert!(
                    !msg.contains("unknown tool"),
                    "{} is declared but not dispatched",
                    name
                );
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
