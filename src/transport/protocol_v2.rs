//! Protocol v2 for `git-upload-pack`.
//!
//! v2 replaces the ref advertisement with a capability list and then one command per request. The
//! gain is not speed on a small repository; it is that a client asks for what it wants instead of
//! being told everything, so a fetch on a repository with many refs stops paying for all of them.
//!
//! A v2 client sends `Git-Protocol: version=2` on every request, including the `info/refs` that used
//! to carry the advertisement. What answers that request is the capability list, not refs:
//!
//! ```text
//! version 2
//! agent=gesta/0.x
//! ls-refs=unborn
//! fetch=
//! object-format=sha1
//! 0000
//! ```
//!
//! Then `POST /git-upload-pack` carries a command:
//!
//! ```text
//! command=ls-refs
//! agent=git/2.x
//! object-format=sha1
//! 0000
//! symrefs
//! peel
//! 0000
//! ```
//!
//! The command's own arguments follow a second flush. Getting that structure wrong produces a client
//! that hangs waiting for a section that never ends, so each piece is separated explicitly here.
//!
//! What is not implemented: `shallow`, `filter` (partial clone) and `wait-for-done`. They are not
//! advertised either. A capability a server declares and cannot honour is worse than one it omits,
//! because the client has no way to fall back.

use std::collections::HashSet;

use crate::transport::gitserver::{pkt_line, read_pkt_lines};
use crate::ZResult;

/// The command a v2 request names, if it names one.
///
/// Read from the pkt-line sections rather than by searching the raw bytes: a `want` argument could
/// contain the text of a command name, and a body is framed so a substring search is a guess at where
/// the line begins.
pub fn command_of(body: &[u8]) -> Option<String> {
    for line in read_pkt_lines(body) {
        if let Some(rest) = line.trim().strip_prefix("command=") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Does this request ask for protocol v2?
pub fn wants_v2(protocol_header: Option<&str>) -> bool {
    match protocol_header {
        Some(v) => v.split(':').any(|part| part.trim() == "version=2"),
        None => false,
    }
}

/// The capability advertisement that answers a v2 `info/refs` request.
///
/// No refs appear here. A client that reads this and expects the v0 ref list would find nothing, so
/// the response is only correct for a request that said `version=2`.
pub fn capability_advertisement(version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&pkt_line(b"version 2\n"));
    out.extend_from_slice(&pkt_line(format!("agent=gesta/{}\n", version).as_bytes()));
    // `unborn` lets a clone of an empty repository work: the client can say which branch it wants
    // before any commit exists. Without it git clones an empty repository and then complains about
    // the missing branch rather than creating it.
    out.extend_from_slice(&pkt_line(b"ls-refs=unborn\n"));
    // `ref-in-want` is deliberately absent: it would need a way to send objects for a ref name
    // rather than a sha, and the v0 path already covers that case.
    // `shallow` is deliberately absent (#175). The module docs already said shallow fetch is not
    // implemented and "not advertised either", and the code advertised it anyway. A v2 client that
    // sees `fetch=shallow` sends `deepen` arguments and believes it received a shallow history; this
    // server answers with the whole pack and no error, so the client's own record of what it holds
    // is wrong from then on. A capability a server declares and cannot honour is worse than one it
    // omits, because the client has no way to fall back - which is what the module docs say and what
    // this line now follows.
    //
    // The bundle path does support depth (`build_bundle_with_depth`, used by the native `clone
    // --depth`), so a native shallow clone still works; it is only the git protocol surface that
    // cannot honour the request.
    out.extend_from_slice(&pkt_line(b"fetch=\n"));
    out.extend_from_slice(&pkt_line(b"object-format=sha1\n"));
    out.extend_from_slice(&crate::transport::gitserver::flush());
    out
}

/// One ref as `ls-refs` reports it.
fn ref_line(name: &str, sha: &str, symref_target: Option<&str>) -> Vec<u8> {
    let mut line = format!("{} {}", sha, name);
    if let Some(target) = symref_target {
        line.push_str(&format!(" symref-target:{}", target));
    }
    line.push('\n');
    pkt_line(line.as_bytes())
}

/// Answer `command=ls-refs`.
///
/// The request body is the command section, then a flush, then the argument section. Arguments that
/// a client may send and this server supports:
///
/// - `peel`: send a peeled line for annotated tags. There are no annotated tags in this store, so the
///   flag changes nothing, and it is accepted rather than refused for that reason.
/// - `symrefs`: report what `HEAD` points at, as `symref-target:`.
/// - `ref-prefix <prefix>`: only refs under these prefixes. A client that clones sends one prefix.
/// - `unborn`: report the branch a clone asked for even when it has no commit yet.
pub fn ls_refs(
    body: &[u8],
    refs: &[(String, String)],
    head_branch: Option<&str>,
) -> ZResult<Vec<u8>> {
    let lines = read_pkt_lines(body);
    let mut want_symrefs = false;
    let mut want_unborn = false;
    let mut prefixes: Vec<String> = Vec::new();

    // Everything after the first flush is the argument section. `read_pkt_lines` already walks past a
    // flush, so the command lines are recognized by their `=` and skipped here.
    for line in &lines {
        let line = line.trim();
        if line.is_empty() || line.contains('=') && !line.starts_with("ref-prefix") {
            continue;
        }
        if line == "symrefs" {
            want_symrefs = true;
        } else if line == "unborn" {
            want_unborn = true;
        } else if line == "peel" {
            // Accepted for compatibility; this store holds no annotated tags to peel.
        } else if let Some(rest) = line.strip_prefix("ref-prefix ") {
            prefixes.push(rest.trim().to_string());
        }
    }

    let mut out = Vec::new();
    // `HEAD` first, as v2 expects, so a clone picks its branch from the first answer it can use.
    if let Some(branch) = head_branch {
        if let Some((_, sha)) = refs.iter().find(|(n, _)| n == branch) {
            // The target has to outlive the call, because the slice is borrowed into the line.
            let target = format!("refs/heads/{}", branch);
            out.extend_from_slice(&ref_line(
                "HEAD",
                sha,
                if want_symrefs {
                    Some(target.as_str())
                } else {
                    None
                },
            ));
        }
    }

    let matches = |name: &str| -> bool {
        if prefixes.is_empty() {
            return true;
        }
        prefixes.iter().any(|p| name.starts_with(p.as_str()))
    };

    let mut emitted: HashSet<&str> = HashSet::new();
    for (name, sha) in refs {
        let full = format!("refs/heads/{}", name);
        if !matches(&full) {
            continue;
        }
        if emitted.insert(name.as_str()) {
            out.extend_from_slice(&ref_line(&full, sha, None));
        }
    }

    // An empty repository with `unborn`: name the branch with the zero id so the client knows which
    // one to create. Without this a clone of an empty repository leaves the client with no branch.
    if out.is_empty() && want_unborn && prefixes.iter().any(|p| p.starts_with("refs/heads/")) {
        let branch = prefixes
            .iter()
            .filter_map(|p| p.strip_prefix("refs/heads/"))
            .next()
            .unwrap_or("main");
        if !branch.is_empty() && !branch.contains('*') {
            out.extend_from_slice(&ref_line(
                &format!("refs/heads/{}", branch),
                &"0".repeat(40),
                None,
            ));
        }
    }

    out.extend_from_slice(&crate::transport::gitserver::flush());
    Ok(out)
}

/// Read the `want` and `have` lines out of a v2 `fetch` request.
///
/// The command section comes first, then a flush, then the arguments, then a flush, then the
/// `have`/`done` section for a negotiation. Only the arguments matter here: the shas to send and the
/// ones the client already has.
pub fn parse_fetch(body: &[u8]) -> (Vec<String>, Vec<String>, bool) {
    let lines = read_pkt_lines(body);
    let mut wants = Vec::new();
    let mut haves = Vec::new();
    let mut done = false;
    for line in &lines {
        let line = line.trim();
        // The command section's own lines are `key=value`; the arguments are bare words.
        if line.contains('=') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("want ") {
            if let Some(sha) = rest.split_whitespace().next() {
                if sha.len() == 40 {
                    wants.push(sha.to_string());
                }
            }
        } else if let Some(rest) = line.strip_prefix("have ") {
            if let Some(sha) = rest.split_whitespace().next() {
                if sha.len() == 40 {
                    haves.push(sha.to_string());
                }
            }
        } else if line == "done" {
            done = true;
        }
    }
    (wants, haves, done)
}

/// Build the v0 request body that the existing reader understands.
///
/// The two protocols ask the same question and spell it differently: v0 sends bare `want <sha>` and
/// `have <sha>` lines, while v2 puts the same words in an argument section. Rebuilding the v0 form is
/// how one implementation stays the implementation, and it is three lines against a second copy of
/// the reachability walk.
pub fn v0_fetch_body(wants: &[String], haves: &[String], done: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for w in wants {
        out.extend_from_slice(&pkt_line(format!("want {}\n", w).as_bytes()));
    }
    out.extend_from_slice(&crate::transport::gitserver::flush());
    for h in haves {
        out.extend_from_slice(&pkt_line(format!("have {}\n", h).as_bytes()));
    }
    if done {
        out.extend_from_slice(&pkt_line(b"done\n"));
    }
    out
}

/// Wrap a pack in the side-band framing a v2 fetch expects.
///
/// The section header is `packfile` on its own line, then the side-band stream, then a flush. A client
/// reads the header to know what the following bytes are, and without it the pack bytes are read as
/// the next section's header.
pub fn fetch_response(pack: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&pkt_line(b"packfile\n"));
    out.extend_from_slice(&pack_framed(pack));
    out
}

/// The acknowledgments section, which a v2 fetch sends before the pack when the client offered
/// `have`s and did not say `done`.
///
/// A client that sent haves and no `done` is asking "do we share anything"; answering with `packfile`
/// makes it report `expected 'acknowledgments', received 'packfile'`. The section holds one line per
/// sha the server recognizes, and `ready` when it has everything it needs.
pub fn acknowledgments(haves: &[String], recognized: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&pkt_line(b"acknowledgments\n"));

    // `ACK` for each have the server has, and then at most ONE `NAK` (#322).
    //
    // The loop used to write a `NAK` per unrecognised have, so a client with fifty unknown haves was
    // answered with fifty `NAK` lines. The v2 shape is `ACK`s, or a single `NAK` when nothing is
    // common, then `ready` - a client reading the section expects one verdict, not one per line it
    // sent.
    let mut sent_nak = false;
    for h in haves {
        if recognized.iter().any(|r| r == h) {
            out.extend_from_slice(&pkt_line(format!("ACK {}\n", h).as_bytes()));
        } else if !sent_nak {
            out.extend_from_slice(&pkt_line(b"NAK\n"));
            sent_nak = true;
        }
    }
    if !recognized.is_empty() {
        out.extend_from_slice(&pkt_line(b"ready\n"));
    }
    out.extend_from_slice(&crate::transport::gitserver::flush());
    out
}

/// Frame a pack on its own, for the case where a section header was already written.
fn pack_framed(pack: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in pack.chunks(65500) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(1u8);
        payload.extend_from_slice(chunk);
        out.extend_from_slice(&pkt_line(&payload));
    }
    // A band-1 packet carrying nothing ends the stream; a bare flush is read as the end of the
    // section and the client never acknowledges the pack it received.
    out.extend_from_slice(&pkt_line(&[1u8]));
    out.extend_from_slice(&crate::transport::gitserver::flush());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_v2_header_is_a_v2_request() {
        assert!(wants_v2(Some("version=2")));
        assert!(wants_v2(Some("version=2:something")));
        assert!(!wants_v2(Some("version=1")));
        assert!(!wants_v2(None));
    }

    #[test]
    fn the_advertisement_names_no_refs() {
        let out = capability_advertisement("0.9.0");
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("version 2"));
        assert!(text.contains("ls-refs"));
        assert!(text.contains("fetch"));
        // A ref line would hold a 40 character sha; none may appear.
        assert!(
            !text.contains("refs/heads/"),
            "the capability advertisement must not carry refs"
        );
    }

    #[test]
    fn the_advertisement_does_not_claim_shallow() {
        // #175. This assertion was missing: the old test asked only whether the text contained
        // "fetch", which stays true whether or not the value is honest, so `fetch=shallow` shipped
        // for as long as the module docs said shallow was not implemented - and those docs also
        // said it was "not advertised either". A client that negotiates shallow then receives a full
        // pack and no error, so it believes it holds a shallow history and is wrong.
        //
        // A capability the server cannot honour must not be listed, which is the rule the module
        // docs already state. Checked as a whole value rather than a substring, so a future
        // `shallow-since` or `shallow-exclude` also has to be a deliberate decision.
        // Checked through the pkt-line framing, not `lines()`: each line arrives with a four-byte
        // hex length prefix, so a raw text search for a line that starts with `fetch=` finds nothing
        // and a naive assertion would fail for the wrong reason - which is what the first version of
        // this test did.
        let caps: Vec<String> = read_pkt_lines(&capability_advertisement("0.9.0"))
            .into_iter()
            .map(|l| l.trim().to_string())
            .collect();
        let fetch_line = caps
            .iter()
            .find(|l| l.starts_with("fetch="))
            .unwrap_or_else(|| {
                panic!(
                    "a v2 advertisement without a fetch line is not a v2 advertisement: {caps:?}"
                )
            });
        assert_eq!(
            fetch_line, "fetch=",
            "shallow fetch is not implemented, so it must not be advertised"
        );
        let joined = caps.join("\n");
        // `filter` and `wait-for-done` are in the same position, and the module docs list them
        // together with shallow as not implemented.
        for absent in ["shallow", "filter", "wait-for-done", "ref-in-want"] {
            assert!(
                !joined.contains(absent),
                "unimplemented capability {absent:?} must stay off the list: {joined}"
            );
        }
    }

    #[test]
    fn ls_refs_reports_head_and_branches() {
        let refs = vec![
            ("main".to_string(), "a".repeat(40)),
            ("dev".to_string(), "b".repeat(40)),
        ];
        let body = [
            pkt_line(b"command=ls-refs\n"),
            pkt_line(b"agent=git/2.40\n"),
            crate::transport::gitserver::flush(),
            pkt_line(b"symrefs\n"),
            crate::transport::gitserver::flush(),
        ]
        .concat();
        let out = ls_refs(&body, &refs, Some("main")).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("refs/heads/main"), "{}", text);
        assert!(text.contains("refs/heads/dev"), "{}", text);
        assert!(text.contains("symref-target:refs/heads/main"), "{}", text);
        // HEAD comes first.
        let head_at = text.find("HEAD").expect("HEAD must be advertised");
        let main_at = text.find("refs/heads/main").unwrap();
        assert!(head_at < main_at, "HEAD is not first: {}", text);
    }

    #[test]
    fn a_ref_prefix_narrows_the_answer() {
        let refs = vec![
            ("main".to_string(), "a".repeat(40)),
            ("feature/x".to_string(), "b".repeat(40)),
        ];
        let body = [
            pkt_line(b"command=ls-refs\n"),
            crate::transport::gitserver::flush(),
            pkt_line(b"ref-prefix refs/heads/feature/\n"),
            crate::transport::gitserver::flush(),
        ]
        .concat();
        let out = ls_refs(&body, &refs, Some("main")).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("feature/x"), "{}", text);
        assert!(
            !text.contains("refs/heads/main "),
            "prefix ignored: {}",
            text
        );
    }

    #[test]
    fn an_unborn_branch_is_named_with_zeroes() {
        let body = [
            pkt_line(b"command=ls-refs\n"),
            crate::transport::gitserver::flush(),
            pkt_line(b"unborn\n"),
            pkt_line(b"ref-prefix refs/heads/main\n"),
            crate::transport::gitserver::flush(),
        ]
        .concat();
        let out = ls_refs(&body, &[], None).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("refs/heads/main"), "{}", text);
        assert!(text.contains(&"0".repeat(40)), "{}", text);
    }

    #[test]
    fn fetch_arguments_are_separated_from_the_command_section() {
        let body = [
            pkt_line(b"command=fetch\n"),
            pkt_line(b"agent=git/2.40\n"),
            crate::transport::gitserver::flush(),
            pkt_line(format!("want {}\n", "a".repeat(40)).as_bytes()),
            pkt_line(format!("have {}\n", "b".repeat(40)).as_bytes()),
            pkt_line(b"done\n"),
            crate::transport::gitserver::flush(),
        ]
        .concat();
        let (wants, haves, done) = parse_fetch(&body);
        assert_eq!(wants, vec!["a".repeat(40)]);
        assert_eq!(haves, vec!["b".repeat(40)]);
        assert!(done);
    }

    #[test]
    fn a_fetch_answer_starts_with_the_packfile_header() {
        let out = fetch_response(b"PACKDATA");
        let text = String::from_utf8_lossy(&out[..12]).to_string();
        assert!(text.contains("packfile"), "{}", text);
    }
    #[test]
    fn the_acknowledgments_section_carries_one_nak_not_one_per_unknown_have() {
        // #322: the loop wrote a `NAK` for every have the server did not recognize, so a client with
        // fifty unknown haves got fifty `NAK` lines. The v2 section is ACKs, or a single NAK, then
        // `ready`; a client reading it expects one verdict.
        let haves: Vec<String> = (0..50).map(|i| format!("{:040x}", i)).collect();
        let out = acknowledgments(&haves, &[]);
        let text = String::from_utf8_lossy(&out);
        let naks = text.matches("NAK").count();
        assert_eq!(
            naks, 1,
            "one NAK for the whole section, not one per have; got {naks} in {text:?}"
        );
    }
}
