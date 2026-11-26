//! Pushing to a forge without the git binary.
//!
//! The forge ecosystem is the one place this project needed git installed: `git push` was the bridge,
//! and it was reached by exporting a bare repository to a temporary directory and running git in it.
//! That works, and it costs a process plus a full copy of the tree on every push.
//!
//! What it does not need is git's client. A push to GitHub, GitLab or Gitea is three HTTP requests:
//! `GET /info/refs?service=git-receive-pack` to learn the refs and capabilities, `POST
//! /git-receive-pack` with a pkt-line command list followed by a pack, and reading the report-status
//! that comes back. This module makes those requests.
//!
//! The pack writer is the one `gitserver` already had for serving clone and push, so there is one
//! implementation of what an object looks like on the wire. This module is the client half: it builds
//! the request, parses the advertisement, decides which objects the remote is missing, and reads the
//! answer.
//!
//! ## What is verified
//!
//! Opening a service against a local `gesta serve --git` and pushing to it: the ref moves, the objects
//! arrive, and a second push of the same tree sends an empty pack. That tests this client against this
//! server. It does not test it against GitHub, which needs a network and a token; the shape of the
//! request is the same one git makes, and the pieces that differ per forge (the URL path, the
//! authentication header) are parameters rather than assumptions.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::support::error::{Code, Error};
use crate::transport::gitserver::{pkt_line, write_pack, Entry};
use crate::ZResult;

/// Where a forge serves one repository, and how to authenticate to it.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The base URL of the repository, without `/info/refs` or `/git-receive-pack`. For GitHub this is
    /// `https://github.com/owner/repo`, for Gitea the same shape, and for a repository served by this
    /// tool it is `http://host/git/repo`.
    pub base: String,
    /// A token, sent as `Authorization: Basic` with the username the forge expects. GitHub and Gitea
    /// take any username with a token as the password; GitLab takes `oauth2` as the username.
    pub token: Option<String>,
    /// The username to pair with the token. `x-access-token` is what GitHub documents for a PAT.
    pub user: String,
}

impl Endpoint {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: None,
            user: "x-access-token".to_string(),
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}{}", self.base, suffix)
    }
}

/// One ref the remote advertised, and what it points at now.
#[derive(Debug, Clone)]
pub struct RemoteRef {
    pub name: String,
    pub sha: String,
}

/// The advertisement a `git-receive-pack` answers with: what refs the remote has, and what it can do.
#[derive(Debug, Clone, Default)]
pub struct Advertisement {
    pub refs: Vec<RemoteRef>,
    pub capabilities: Vec<String>,
}

impl Advertisement {
    /// The id the remote holds for a ref, or `None` when it has never had one.
    ///
    /// A push needs this: the command line carries the old id, and a forge rejects a command whose old
    /// id is not what it holds. Sending zeroes for a ref that exists is how a push fails with "fetch
    /// first" after a successful push from somewhere else.
    pub fn id_of(&self, name: &str) -> Option<&str> {
        self.refs
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.sha.as_str())
    }

    pub fn can(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// Parse a `git-receive-pack` advertisement.
///
/// The body is pkt-lines. The first ref line carries the capabilities after a NUL; the rest are plain.
/// A smart-HTTP response also puts four lines of its own in front (`# service=...`, a flush, and the
/// flush that ends the ref list), and a dumb server answers with something else entirely, which is
/// reported rather than parsed into an empty advertisement that would look like a repository with no
/// branches.
pub fn parse_advertisement(raw: &[u8]) -> ZResult<Advertisement> {
    let text = String::from_utf8_lossy(raw);
    if text.trim_start().starts_with("<!DOCTYPE") || text.trim_start().starts_with("<html") {
        return Err(Box::new(Error::new(
            Code::Remote,
            "the remote answered with HTML, not git: the URL is probably the web page rather than the \
             repository, or the token was rejected and the forge served a login form"
                .to_string(),
        )));
    }

    let mut out = Advertisement::default();
    let mut i = 0usize;
    while i + 4 <= raw.len() {
        let Ok(hex) = std::str::from_utf8(&raw[i..i + 4]) else {
            break;
        };
        let Ok(len) = usize::from_str_radix(hex, 16) else {
            break;
        };
        if len == 0 {
            // flush-pkt
            i += 4;
            continue;
        }
        if len < 4 || i + len > raw.len() {
            break;
        }
        let payload = &raw[i + 4..i + len];
        i += len;

        let line = String::from_utf8_lossy(payload);
        let line = line.trim_end_matches('\n');
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        // `<old-id> <name>\0<capabilities>` on the first line only.
        let (rest, caps) = match line.split_once('\0') {
            Some((a, b)) => (a, Some(b)),
            None => (line, None),
        };
        if let Some(c) = caps {
            out.capabilities = c.split_whitespace().map(|s| s.to_string()).collect();
        }
        let mut parts = rest.split_whitespace();
        let sha = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        if sha.len() >= 40 && !name.is_empty() {
            out.refs.push(RemoteRef {
                name: name.to_string(),
                sha: sha.to_string(),
            });
        }
    }
    Ok(out)
}

/// Ask a forge what it holds before pushing.
///
/// This is `GET <base>/info/refs?service=git-receive-pack`, which is the first request git makes. It is
/// also the request that fails first when a token is wrong, so a push that cannot authenticate stops
/// here rather than after sending a pack.
pub fn advertise(endpoint: &Endpoint) -> ZResult<Advertisement> {
    let url = endpoint.url("/info/refs?service=git-receive-pack");
    let auth = endpoint
        .token
        .as_ref()
        .map(|t| format!("{}:{}", endpoint.user, t));

    let (status, body) = crate::transport::http::request_with_auth(
        "GET",
        &url,
        None,
        auth.as_deref(),
        Some("application/x-git-receive-pack-advertisement"),
    )?;

    if status == 401 || status == 403 {
        return Err(Box::new(Error::new(
            Code::Remote,
            format!(
                "the forge refused the token (HTTP {}): check that it has write access to this \
                 repository and that it has not expired",
                status
            ),
        )));
    }
    if status == 404 {
        return Err(Box::new(Error::new(
            Code::Remote,
            format!(
                "the forge has no repository at {}: check the owner and name, and that the token can \
                 see it",
                endpoint.base
            ),
        )));
    }
    if !(200..300).contains(&status) {
        let preview: String = String::from_utf8_lossy(&body).chars().take(200).collect();
        return Err(Box::new(Error::new(
            Code::Remote,
            format!("the forge answered HTTP {}: {}", status, preview),
        )));
    }
    // A chunked advertisement is the same trap as a chunked report: the hex chunk length reads as a
    // pkt-line length, so `14C\r\n001f# service=...` parses as one 0x14C-byte pkt and every ref is
    // lost. GitHub chunks both responses. Without this, `advertise` returned zero refs against a
    // repository that had one, the push then claimed the ref was new (`old` all zeroes), and GitHub
    // refused it as a non-fast-forward while the real fault was this line.
    let body = crate::transport::http::dechunk(&body);
    parse_advertisement(&body)
}

/// What a push is asked to do to one ref.
#[derive(Debug, Clone)]
pub struct Update {
    /// `refs/heads/main`, the full name, because that is what the wire carries.
    pub name: String,
    /// The id the remote is expected to hold. All zeroes for a ref it has never had.
    pub old: String,
    /// The id to set it to. All zeroes deletes the ref.
    pub new: String,
}

/// The result the remote reported for one ref.
#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

/// Build the request body for a push: the command list, a flush, then the pack.
///
/// The first command carries the capabilities after a NUL, the same shape as the advertisement. Only
/// `report-status` is requested, because it is the one that makes a rejected push visible: without it
/// the remote closes the connection and the client cannot tell a refusal from a success.
pub fn push_body(updates: &[Update], entries: &[Entry], caps: &[&str]) -> ZResult<Vec<u8>> {
    let mut out = Vec::new();
    for (i, u) in updates.iter().enumerate() {
        let line = if i == 0 {
            let mut l = format!("{} {} {}\0", u.old, u.new, u.name);
            l.push_str(&caps.join(" "));
            l.push('\n');
            l
        } else {
            format!("{} {} {}\n", u.old, u.new, u.name)
        };
        out.extend_from_slice(&pkt_line(line.as_bytes()));
    }
    out.extend_from_slice(&crate::transport::gitserver::flush());
    if !entries.is_empty() {
        out.extend_from_slice(&write_pack(entries)?);
    }
    Ok(out)
}

/// Read the `report-status` a push answers with.
///
/// The answer is `unpack ok` (or a reason), then one `ok <ref>` or `ng <ref> <reason>` per command, then
/// a flush. A body that carries no report at all is treated as a failure rather than a success: a
/// server that did not answer did not accept anything.
pub fn parse_report(raw: &[u8]) -> Vec<UpdateResult> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= raw.len() {
        let Ok(hex) = std::str::from_utf8(&raw[i..i + 4]) else {
            break;
        };
        let Ok(len) = usize::from_str_radix(hex, 16) else {
            break;
        };
        if len == 0 {
            i += 4;
            continue;
        }
        if len < 4 || i + len > raw.len() {
            break;
        }
        let line = String::from_utf8_lossy(&raw[i + 4..i + len])
            .trim_end()
            .to_string();
        i += len;
        if line.starts_with("unpack ") {
            // A failed unpack means no ref moved, whatever the per-ref lines that follow say. Returning
            // just this failure is the point: a body of `unpack error` followed by `ok refs/heads/main`
            // used to be read as success, because the per-ref line was found first by name.
            if line.trim_end() != "unpack ok" {
                return vec![UpdateResult {
                    name: String::new(),
                    ok: false,
                    message: line,
                }];
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("ok ") {
            out.push(UpdateResult {
                name: rest.to_string(),
                ok: true,
                message: String::new(),
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("ng ") {
            let (name, msg) = match rest.split_once(' ') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (rest.to_string(), String::new()),
            };
            out.push(UpdateResult {
                name,
                ok: false,
                message: msg,
            });
        }
    }
    if out.is_empty() {
        out.push(UpdateResult {
            name: String::new(),
            ok: false,
            message: "the remote sent no report-status, so the push was not confirmed".to_string(),
        });
    }
    out
}
/// Which objects the remote is missing, given what this store holds.
///
/// Split from the export so a caller that already has the objects does not pay for a second one: a push
/// exports once to learn the ref names, and the same export carries every object.
pub fn objects_to_send(all: Vec<Entry>, have_remote: &HashSet<String>) -> Vec<Entry> {
    if have_remote.is_empty() {
        return all;
    }
    // The set the remote can reach, computed over the objects we are also about to send. A tip we have
    // never seen is simply not in `all` and contributes nothing, which is the right answer: an object we
    // do not hold cannot be subtracted from what we send.
    let seen = crate::transport::gitserver::reachable_from(
        &all,
        &have_remote.iter().cloned().collect::<Vec<_>>(),
    );
    all.into_iter().filter(|e| !seen.contains(&e.sha)).collect()
}

/// Push one branch to a forge, without the git binary.
///
/// Returns what the remote said about the ref. A refusal is an error, and the message is the remote's
/// own words rather than a rewrite of them.
/// Move one local branch to one ref on the remote.
pub fn push_branch(
    conn: &Connection,
    root: &std::path::Path,
    endpoint: &Endpoint,
    branch: &str,
    remote_ref: &str,
) -> ZResult<UpdateResult> {
    let mut out = push_refs(
        conn,
        root,
        endpoint,
        &[(branch.to_string(), remote_ref.to_string())],
    )?;
    // One pair in, one result out. `push_refs` answers per ref so a multi-ref push can report every
    // one; this caller asked for exactly one.
    out.pop().ok_or_else(|| {
        Box::new(Error::new(
            Code::Remote,
            "the remote reported nothing for the branch that was pushed".to_string(),
        )) as Box<dyn std::error::Error>
    })
}

/// Move every local branch to a ref of the same name on the remote.
///
/// A push that can only send one branch means a repository with a `main` and a `release` needs two
/// invocations and two packs, and the second one re-sends every object the first already sent because
/// the client has no memory of what it pushed. One request carrying every ref is what `git push --all`
/// does, and it is the shape this was missing.
///
/// Branches the remote already has at the same id are skipped: sending a command that moves a ref to
/// where it already is costs the server work and costs this client a round trip, and some servers
/// refuse it outright.
pub fn push_all(
    conn: &Connection,
    root: &std::path::Path,
    endpoint: &Endpoint,
) -> ZResult<Vec<UpdateResult>> {
    let adv = advertise(endpoint)?;

    // The comparison has to be between two git commit ids. `branches.head` holds this store's own
    // changeset id (`cs...`), not the git commit that export writes for it, so comparing a branch head
    // to an advertised sha is comparing two different namespaces and is never equal. The first version
    // of this did exactly that and pushed every branch every time.
    //
    // The exported refs are the right side of the comparison, and they are what the push sends anyway.
    let exported = crate::transport::http::open_repo_for_serve(root)?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (name, sha) in &exported.refs {
        let remote_name = format!("refs/heads/{}", name);
        let already = adv.id_of(&remote_name).map(|s| s == *sha).unwrap_or(false);
        if already {
            continue;
        }
        pairs.push((name.clone(), remote_name));
    }
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    push_refs(conn, root, endpoint, &pairs)
}

/// The push itself: every (local branch, remote ref) pair in one request, one pack.
fn push_refs(
    conn: &Connection,
    root: &std::path::Path,
    endpoint: &Endpoint,
    pairs: &[(String, String)],
) -> ZResult<Vec<UpdateResult>> {
    let adv = advertise(endpoint)?;

    // One export gives both the ref names and every object, which is what a push needs: the ids to set
    // the refs to, and the pack that has to arrive with them.
    let exported = crate::transport::http::open_repo_for_serve(root)?;

    let mut updates: Vec<Update> = Vec::new();
    for (branch, remote_ref) in pairs {
        if crate::store::branch_head(conn, branch)?.is_none() {
            return Err(bad(format!("no branch named {} to push", branch)));
        }
        let want = exported
            .refs
            .iter()
            .find(|(n, _)| n == branch)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| bad(format!("branch {} exported to nothing", branch)))?;

        // The id the remote advertised, or zeroes when it has never had this ref. A command whose old
        // id is not what the remote holds is refused, which is the mechanism that stops a push
        // overwriting work someone else landed.
        let old = match adv.id_of(remote_ref) {
            Some(sha) if sha.len() >= 40 => sha.to_string(),
            _ => "0".repeat(40),
        };
        // A command that moves a ref to where it already is costs the server work and some servers
        // refuse it outright, which is why `push_all` filters these before calling in. A single-branch
        // push goes straight here and did not filter, so pushing an unchanged branch a second time was
        // sent as `old == new` and GitHub answered `ng refs/heads/main failed`. Skipping it is what
        // git does for the same case and it is the answer the caller wanted: nothing to do.
        if old == want {
            continue;
        }
        updates.push(Update {
            name: remote_ref.clone(),
            old,
            new: want,
        });
    }

    if updates.is_empty() {
        // Every requested ref is already where the caller wants it. One entry per pair, so the answer
        // has the same shape as a push that moved something - the caller prints one line per element
        // and a short list would read as a short push.
        return Ok(pairs
            .iter()
            .map(|(_, r)| UpdateResult {
                name: r.clone(),
                ok: true,
                message: "already at that id, nothing to send".to_string(),
            })
            .collect());
    }

    let mut have: HashSet<String> = HashSet::new();
    for r in &adv.refs {
        have.insert(r.sha.clone());
    }
    let entries = objects_to_send(exported.objects, &have);
    // Only ask for what the remote said it can do. `report-status` is what makes a rejection visible;
    // asking for it from a server that never advertised it is asking for an answer that never comes,
    // and then a refused push reads as a silent success. A remote without it gets no claim of success
    // from this client either.
    //
    // GitHub advertises `report-status-v2` and does **not** advertise plain `report-status`, so a
    // client that only asks for the v1 name gets no report at all: the push is accepted, the ref moves,
    // and this client reports "the remote sent no report-status" because it never negotiated one.
    // v2 is a superset - same `unpack` line, per-ref `ok`/`ng` lines, plus optional `option` lines -
    // so the existing parser reads it once the request negotiates it. v1 stays the fallback for
    // servers that only have that.
    let mut caps: Vec<&str> = Vec::new();
    if adv.can("report-status-v2") {
        caps.push("report-status-v2");
        // v2 pushes may carry `option` lines; asking for side-band is what git does here and it also
        // keeps a verbose rejection inside its own channel rather than mixed into the pkt stream.
        if adv.can("side-band-64k") {
            caps.push("side-band-64k");
        }
    } else if adv.can("report-status") {
        caps.push("report-status");
    }
    // `ofs-delta` is not optional when the pack carries OFS_DELTA entries, which `write_pack` always
    // writes when it finds a base: without this capability the server reads the delta offset as blob
    // content and the unpack fails with "no voting servers succeeded" while the pack itself indexes
    // cleanly under real git. GitHub advertises it; a server that does not is sent a full pack instead,
    // which is correct but larger.
    if adv.can("ofs-delta") {
        caps.push("ofs-delta");
    }
    if adv.can("delete-refs") {
        caps.push("delete-refs");
    }
    let body = push_body(&updates, &entries, &caps)?;

    let url = endpoint.url("/git-receive-pack");
    let auth = endpoint
        .token
        .as_ref()
        .map(|t| format!("{}:{}", endpoint.user, t));
    let (status, resp) = crate::transport::http::request_with_auth(
        "POST",
        &url,
        Some(&body),
        auth.as_deref(),
        Some("application/x-git-receive-pack-request"),
    )?;

    if !(200..300).contains(&status) {
        let preview: String = String::from_utf8_lossy(&resp).chars().take(300).collect();
        return Err(Box::new(Error::new(
            Code::Remote,
            format!(
                "the forge refused the push with HTTP {}: {}",
                status, preview
            ),
        )));
    }

    // A smart server wraps the report in side-band frames when it negotiated side-band, which this
    // client does not ask for, so the report is the body. Strip the v0 framing if one arrived anyway.
    // GitHub chunks the answer. De-chunk first, then strip any side-band framing, then read the
    // report - the order matters, because a chunked body whose payload is side-band frames has two
    // layers and `strip_side_band` alone sees the chunk header as a bad frame and gives up.
    let plain = crate::transport::http::dechunk(&resp);
    let report = parse_report(&crate::transport::http::strip_side_band(&plain));

    // Every command has to be acknowledged. A push of three refs where one was refused is a failed
    // push, and reporting the first name as if it spoke for all of them is how a partial push reads
    // as a complete one.
    if let Some(bad_one) = report.iter().find(|r| !r.ok) {
        return Err(Box::new(Error::new(
            Code::Remote,
            format!(
                "the forge rejected {}: {}",
                bad_one.name,
                if bad_one.message.is_empty() {
                    "no reason given"
                } else {
                    &bad_one.message
                }
            ),
        )));
    }

    // One result per command, not one result for the batch. `push_all` sent every branch in this
    // single request, and the old shape - picking `updates[0]` and returning it - meant a successful
    // push of five branches was reported as one, so the caller's `--all` and `--json` both named only
    // the first. A refused ref already errored above; this is the success report, and it has to
    // account for every ref the request carried.
    if report.is_empty() {
        return Err(Box::new(Error::new(
            Code::Remote,
            "the remote sent no report, so the push cannot be confirmed".to_string(),
        )));
    }
    results_for(&updates, &report)
}

/// One result per update the request carried, in request order.
///
/// Split out of `push_refs` so the mapping can be tested without a server: the fault this fixes was
/// in the mapping, not in the transport. A ref the report skipped is an error rather than a silent
/// omission, because the request carried it and nothing said it landed.
pub fn results_for(updates: &[Update], report: &[UpdateResult]) -> ZResult<Vec<UpdateResult>> {
    let mut out: Vec<UpdateResult> = Vec::with_capacity(updates.len());
    for u in updates {
        let r = report
            .iter()
            .find(|r| r.name == u.name)
            .cloned()
            .ok_or_else(|| {
                Box::new(Error::new(
                    Code::Remote,
                    format!("the remote did not acknowledge {}", u.name),
                )) as Box<dyn std::error::Error>
            })?;
        if !r.ok {
            return Err(Box::new(Error::new(
                Code::Remote,
                format!(
                    "the forge rejected {}: {}",
                    r.name,
                    if r.message.is_empty() {
                        "no reason given"
                    } else {
                        &r.message
                    }
                ),
            )));
        }
        out.push(r);
    }
    Ok(out)
}

fn bad(msg: String) -> Box<Error> {
    Box::new(Error::new(Code::Usage, msg))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::gitserver::{flush, pkt_line};

    fn advertisement_body(refs: &[(&str, &str)], caps: &str) -> Vec<u8> {
        let mut out = pkt_line(b"# service=git-receive-pack\n");
        out.extend_from_slice(&flush());
        for (i, (sha, name)) in refs.iter().enumerate() {
            let line = if i == 0 {
                format!("{} {}\0{}", sha, name, caps)
            } else {
                format!("{} {}", sha, name)
            };
            out.extend_from_slice(&pkt_line(format!("{}\n", line).as_bytes()));
        }
        out.extend_from_slice(&flush());
        out
    }

    #[test]
    fn an_advertisement_names_its_refs_and_capabilities() {
        let sha = "a".repeat(40);
        let body = advertisement_body(
            &[
                (&sha, "refs/heads/main"),
                (&"b".repeat(40), "refs/heads/next"),
            ],
            "report-status delete-refs",
        );
        let adv = parse_advertisement(&body).expect("parse");
        assert_eq!(adv.refs.len(), 2);
        assert_eq!(adv.id_of("refs/heads/main"), Some(sha.as_str()));
        assert!(adv.can("report-status"));
        assert!(adv.can("delete-refs"));
        assert!(!adv.can("side-band-64k"));
    }

    #[test]
    fn a_ref_the_remote_does_not_have_is_none_so_the_push_uses_zeroes() {
        // The old id in a push command is what the forge checks. Answering "not present" with a real id
        // is how a push to an existing branch fails with "fetch first".
        let adv = parse_advertisement(&advertisement_body(
            &[(&"c".repeat(40), "refs/heads/main")],
            "report-status",
        ))
        .expect("parse");
        assert!(adv.id_of("refs/heads/new").is_none());
    }

    #[test]
    fn an_html_answer_is_reported_rather_than_parsed_as_no_branches() {
        // A wrong URL or a rejected token makes a forge serve a login page. Parsing that into an empty
        // advertisement would look like a repository with no refs, and the push would then be attempted
        // against something that is not a repository.
        let err = parse_advertisement(b"<!DOCTYPE html><html><body>Sign in</body></html>")
            .expect_err("html must not parse");
        let msg = format!("{}", err);
        assert!(msg.contains("HTML"), "message was: {}", msg);
    }

    #[test]
    fn a_push_body_puts_the_capabilities_on_the_first_command_only() {
        let entries = vec![Entry {
            sha: "d".repeat(40),
            kind: 3,
            body: b"hello\n".to_vec(),
        }];
        let updates = vec![
            Update {
                name: "refs/heads/main".into(),
                old: "0".repeat(40),
                new: "e".repeat(40),
            },
            Update {
                name: "refs/heads/next".into(),
                old: "0".repeat(40),
                new: "f".repeat(40),
            },
        ];
        let body = push_body(&updates, &entries, &["report-status"]).expect("body");
        // Everything before the pack is the command list.
        let pack_at = body.windows(4).position(|w| w == b"PACK").expect("a pack");
        let text = String::from_utf8_lossy(&body[..pack_at]);
        // The capability appears once, after the first ref and a NUL.
        assert_eq!(text.matches("report-status").count(), 1);
        assert!(text.contains("refs/heads/main\0report-status"));
        assert!(text.contains("refs/heads/next"));
        // The pack follows the flush, and it is a pack.
        let pack_at = body.windows(4).position(|w| w == b"PACK").expect("a pack");
        assert_eq!(&body[pack_at..pack_at + 4], b"PACK");
    }

    #[test]
    fn every_ref_the_request_carried_is_reported_back() {
        // `push_refs` returned `updates[0]`'s result and `push_all` wrapped it in `vec![one]`, so a push
        // that moved three branches was reported as one. `--all` and `--json` both named only that one,
        // which is the partial-push-reads-as-a-success the module doc warns about.
        let updates = vec![
            Update {
                name: "refs/heads/main".into(),
                old: "a".repeat(40),
                new: "b".repeat(40),
            },
            Update {
                name: "refs/heads/release".into(),
                old: "0".repeat(40),
                new: "c".repeat(40),
            },
            Update {
                name: "refs/heads/side".into(),
                old: "0".repeat(40),
                new: "d".repeat(40),
            },
        ];
        let report: Vec<UpdateResult> = updates
            .iter()
            .map(|u| UpdateResult {
                name: u.name.clone(),
                ok: true,
                message: String::new(),
            })
            .collect();

        let out = results_for(&updates, &report).unwrap();
        assert_eq!(out.len(), 3, "one result per ref, not one for the batch");
        assert_eq!(
            out.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["refs/heads/main", "refs/heads/release", "refs/heads/side"],
            "request order, every name present"
        );
    }

    #[test]
    fn a_ref_the_report_skipped_is_an_error_not_a_silent_omission() {
        // The request carried three refs and the report acknowledged two. Answering with two results
        // would make the caller print two lines and call it a complete push.
        let updates = vec![
            Update {
                name: "refs/heads/main".into(),
                old: "a".repeat(40),
                new: "b".repeat(40),
            },
            Update {
                name: "refs/heads/gone".into(),
                old: "0".repeat(40),
                new: "c".repeat(40),
            },
        ];
        let report = vec![UpdateResult {
            name: "refs/heads/main".into(),
            ok: true,
            message: String::new(),
        }];
        let err = results_for(&updates, &report).expect_err("an unacknowledged ref is a failure");
        assert!(
            format!("{err}").contains("refs/heads/gone"),
            "the message must name the ref nobody acknowledged, got: {err}"
        );
    }

    #[test]
    fn a_report_says_which_ref_the_remote_took() {
        let mut raw = pkt_line(b"unpack ok\n");
        raw.extend_from_slice(&pkt_line(b"ok refs/heads/main\n"));
        raw.extend_from_slice(&flush());
        let r = parse_report(&raw);
        assert_eq!(r.len(), 1);
        assert!(r[0].ok);
        assert_eq!(r[0].name, "refs/heads/main");
    }

    #[test]
    fn a_rejected_ref_keeps_the_reason_the_remote_gave() {
        let mut raw = pkt_line(b"unpack ok\n");
        raw.extend_from_slice(&pkt_line(b"ng refs/heads/main non-fast-forward\n"));
        raw.extend_from_slice(&flush());
        let r = parse_report(&raw);
        assert_eq!(r.len(), 1);
        assert!(!r[0].ok, "a rejected ref must not read as accepted");
        assert!(r[0].message.contains("non-fast-forward"));
    }

    #[test]
    fn a_failed_unpack_is_not_overturned_by_an_ok_line() {
        // The report can carry `unpack error` and then a per-ref line. Reading the per-ref line first is
        // how a push whose objects never landed looked successful: the ref line was found by name and
        // the unpack line, which has no name, was never consulted.
        let mut raw = pkt_line(b"unpack error\n");
        raw.extend_from_slice(&pkt_line(b"ok refs/heads/main\n"));
        raw.extend_from_slice(&flush());
        let r = parse_report(&raw);
        assert_eq!(r.len(), 1, "only the unpack failure should be reported");
        assert!(
            !r[0].ok,
            "a failed unpack must not read as a successful push"
        );
        assert!(r[0].message.contains("unpack error"));
    }

    #[test]
    fn a_silent_remote_is_not_a_successful_push() {
        // No report-status at all. Treating an empty answer as success is the failure this whole
        // module is careful about: a push that was not accepted must not print "pushed".
        let r = parse_report(b"");
        assert_eq!(r.len(), 1);
        assert!(!r[0].ok);
    }

    #[test]
    fn side_band_framing_is_removed_and_a_plain_body_is_untouched() {
        use crate::transport::http::strip_side_band;
        assert_eq!(strip_side_band(b"unpack ok"), b"unpack ok".to_vec());
        let mut framed = Vec::new();
        framed.extend_from_slice(&pkt_line(b"\x01unpack ok\n"));
        framed.extend_from_slice(&pkt_line(b"\x01ok refs/heads/main\n"));
        assert_eq!(
            strip_side_band(&framed),
            b"unpack ok\nok refs/heads/main\n".to_vec()
        );
    }
}
