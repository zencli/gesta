#![allow(dead_code)]
//! The record of what the server decided, not only what it did.
//!
//! `access.log` records the requests that succeeded. A request that was refused left no trace, so a
//! token being guessed, a path outside a scope being tried, or a branch outside a token's range being
//! pushed all looked like nothing happened. That is the wrong way round: the refusals are the events
//! worth reading, and a log that only holds successes answers "was it used" but never "was it tried".
//!
//! The format is one JSON document per line: a person can read it, `jq` can filter it, and a reader
//! that does not understand a field can ignore it.
//!
//! A line is written with a single `write_all` to a descriptor opened `O_APPEND`, and the line is
//! far below `PIPE_BUF`, so the kernel appends it whole: two writers do not interleave a line and a
//! reader never sees half of one. That is a property of `O_APPEND` on POSIX, not of a lock, and
//! there is no lock here. An earlier version of this comment claimed one, and gave it as the reason
//! the code was safe - a guarantee that is described but not implemented is worse than a smaller one
//! that is, because the next person reads the comment instead of the code.
//!
//! What this does not give: atomicity across several lines. A record is one line and one `write`,
//! and nothing here writes two.

use crate::ZResult;
use std::io::Write;

/// What the server decided about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Decision {
    Allowed,
    /// No credential, or one that matches nothing.
    NoToken,
    /// A token that exists and has expired.
    Expired,
    /// A real token whose role is below what the request needed.
    WrongRole,
    /// A real token that may not write this path.
    PathRefused,
    /// A real token that may not write this branch.
    BranchRefused,
    /// The request was understood and the arguments were wrong.
    BadRequest,
    /// The work was attempted and failed.
    Failed,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allowed => "allowed",
            Decision::NoToken => "no-token",
            Decision::Expired => "expired",
            Decision::WrongRole => "wrong-role",
            Decision::PathRefused => "path-refused",
            Decision::BranchRefused => "branch-refused",
            Decision::BadRequest => "bad-request",
            Decision::Failed => "failed",
        }
    }

    /// Whether the request changed anything. A reader filtering for events that matter usually wants
    /// the refusals, so this is stated rather than inferred from the string.
    pub fn allowed(self) -> bool {
        self == Decision::Allowed
    }
}

/// One line of the record.
#[allow(dead_code)]
pub struct Event<'a> {
    pub who: &'a str,
    pub decision: Decision,
    pub action: &'a str,
    /// The path, branch or ref the request named.
    pub target: &'a str,
    /// Why it was refused, or what it did. Short, and never a credential.
    pub reason: &'a str,
    pub remote: Option<&'a str>,
}

/// Append one event.
///
/// Never returns an error the caller is expected to act on: a request that was served must not fail
/// because the log could not be written, and a request that was refused is already being refused. The
/// failure is printed once to stderr so an operator who looks sees it.
#[allow(dead_code)]
pub fn record(root: &std::path::Path, event: &Event) {
    record_with(root, event, None)
}

/// `record`, and also hand the line to a forwarder when one is configured.
///
/// The local file is written first and unconditionally. A collector being down must not cost the
/// record on the machine that produced it, and the file is what an operator reads when the collector
/// is the thing that broke.
pub fn record_with(
    root: &std::path::Path,
    event: &Event,
    forward: Option<&crate::support::audit_forward::Forwarder>,
) {
    let path = crate::repo_dir(root).join("audit.log");
    let mut fields: Vec<(&str, String)> = vec![
        (
            "at",
            crate::support::clock::format_rfc3339(crate::support::clock::unix_now()),
        ),
        ("who", event.who.to_string()),
        ("decision", event.decision.as_str().to_string()),
        ("action", event.action.to_string()),
        ("target", event.target.to_string()),
        ("reason", event.reason.to_string()),
    ];
    if let Some(remote) = event.remote {
        fields.push(("remote", remote.to_string()));
    }

    // Rendered through the project's own writer so a quote or a newline in a reason cannot produce a
    // line that is not a document.
    let doc = crate::support::json::Json::obj(
        fields
            .into_iter()
            .map(|(k, v)| (k, crate::support::json::Json::str(v)))
            .collect(),
    );
    let line = format!("{}\n", doc.render());

    // Both failures are reported. `if let Ok(..)` with no `else` was the shape of the bug: a log
    // that could not be opened - the directory gone, permissions changed, the disk full - dropped
    // every event with nothing printed and nothing returned, so an operator reading the file saw an
    // intact history that silently stopped at some point. A missing write is the one failure in an
    // audit log that must not be quiet, because the whole value of the log is that it is complete.
    // Rotate before writing, once, when the file is already over the cap (#78). There is no rotation
    // anywhere in the tree: on a busy server `audit.log` grows forever, and `read` had to load all of
    // it. One generation is kept, which is what a log needs to survive a look at yesterday without
    // becoming a disk-filling problem - the file is a record of recent refusals, not an archive.
    //
    // The check is a `metadata` on the path the write is about to open, so it costs one stat on a
    // path already being opened, and it is done here rather than on a timer because a repository that
    // is quiet should not pay for a clock it does not need.
    rotate_if_full(&path);

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("audit log is not writable: {}: {}", path.display(), e);
            }
        }
        Err(e) => {
            eprintln!("audit log could not be opened: {}: {}", path.display(), e);
        }
    }

    // After the file, never instead of it. The send does not block, so a slow collector cannot slow a
    // request, and a full queue is counted by the forwarder rather than raised here.
    if let Some(f) = forward {
        let _ = f.send(&line);
    }
}

/// Actions this tool performs that are worth a line even though no request carried them.
///
/// The HTTP record answers "what did the server decide". A commit, a push out, a sign-off and a
/// rotation are the same kind of fact and were invisible: an operator reading the log saw the
/// requests that arrived and nothing about what the machine did with what it held.
pub mod local {
    use super::{record_with, Decision, Event};

    /// A commit landed on a branch.
    pub fn commit(
        root: &std::path::Path,
        branch: &str,
        changeset: &str,
        author: &str,
        files: usize,
    ) {
        record_with(
            root,
            &Event {
                who: author,
                decision: Decision::Allowed,
                action: "commit",
                target: branch,
                reason: &format!("{} {} file(s)", changeset, files),
                remote: None,
            },
            forwarder(root).as_ref(),
        );
    }

    /// A branch moved because a push arrived.
    pub fn push(root: &std::path::Path, branch: &str, who: &str, detail: &str) {
        record_with(
            root,
            &Event {
                who,
                decision: Decision::Allowed,
                action: "push",
                target: branch,
                reason: detail,
                remote: None,
            },
            forwarder(root).as_ref(),
        );
    }

    /// A push was refused, and why.
    pub fn push_refused(root: &std::path::Path, branch: &str, who: &str, reason: &str) {
        record_with(
            root,
            &Event {
                who,
                decision: Decision::Failed,
                action: "push",
                target: branch,
                reason,
                remote: None,
            },
            forwarder(root).as_ref(),
        );
    }

    /// Somebody signed off a change.
    pub fn approve(root: &std::path::Path, changeset: &str, by: &str, branch: &str) {
        record_with(
            root,
            &Event {
                who: by,
                decision: Decision::Allowed,
                action: "approve",
                target: branch,
                reason: changeset,
                remote: None,
            },
            forwarder(root).as_ref(),
        );
    }

    /// A signing key was replaced.
    pub fn key_rotated(
        root: &std::path::Path,
        who: &str,
        new_public: &str,
        archived: Option<&str>,
    ) {
        let reason = match archived {
            Some(a) => format!("archived {}", a),
            None => "no previous key".to_string(),
        };
        record_with(
            root,
            &Event {
                who,
                decision: Decision::Allowed,
                action: "key-rotate",
                // `head_chars`, not `&new_public[..16]`: the name is a `&str` of arbitrary text and
                // byte 16 can sit inside a multi-byte character (#290). Real keys are ASCII, but the
                // function takes any string and panicked on the one it was given.
                target: crate::support::paths::head_chars(new_public, 16),
                reason: &reason,
                remote: None,
            },
            forwarder(root).as_ref(),
        );
    }

    /// The forwarder this repository is configured with, rebuilt per call.
    ///
    /// Rebuilding reads two small files, and holding one in a static would mean a thread that outlives
    /// the process it was configured for. A local event happens a few times a minute at most.
    fn forwarder(root: &std::path::Path) -> Option<crate::support::audit_forward::Forwarder> {
        crate::support::audit_forward::Forwarder::from_repo(root)
            .ok()
            .flatten()
    }
}

/// A parsed line, for `gesta remote audit`.
#[allow(dead_code)]
pub struct Entry {
    pub at: String,
    pub who: String,
    pub decision: String,
    pub action: String,
    pub target: String,
    pub reason: String,
    pub raw: String,
}

/// Read the record, newest last.
///
/// `only_refused` answers the question a person actually asks after an incident: what was tried and
/// turned away.
#[allow(dead_code)]
/// How much of the tail to read per line asked for.
///
/// An audit line is a small JSON document - the reason text is the only thing that varies, and it is
/// written by the program, not by the client. 1 KiB per line leaves room for a long reason and still
/// reads 200 lines out of a 200 KiB window.
const TAIL_BYTES_PER_LINE: u64 = 1024;

/// The size at which `audit.log` is rotated.
///
/// 32 MiB is roughly 300k refusals - far more history than an operator reads, and small enough that
/// the file never becomes the reason a disk fills. The cap exists so the log has a bound; what
/// matters is that the bound is declared, not that it is large.
pub const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;

/// Move the log aside when it is over the cap, keeping one generation.
///
/// A failure to rotate is reported and then ignored: the write below still appends, and losing the
/// rotation is a smaller problem than losing the event. That is the opposite of the choice made for a
/// failed write a few lines down, and deliberately so - a missing event is a hole in the record, a
/// missed rotation is a large file.
fn rotate_if_full(path: &std::path::Path) {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if size <= MAX_LOG_BYTES {
        return;
    }
    let old = path.with_extension("log.1");
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(path, &old) {
        eprintln!(
            "audit log is over the {} MiB cap but could not be rotated: {}: {}",
            MAX_LOG_BYTES / (1024 * 1024),
            path.display(),
            e
        );
    }
}

/// The last `limit` lines of a file, or the whole file when there is no limit.
///
/// `Ok(None)` when the file is not there, which is not an error: a repository that has never refused
/// anything has no audit log, and the answer is an empty history.
fn tail_text(path: &std::path::Path, limit: Option<usize>) -> std::io::Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();

    let want = match limit {
        None => {
            let mut s = String::new();
            f.read_to_string(&mut s)?;
            return Ok(Some(s));
        }
        // A limit of zero asks for nothing, and reading nothing is the honest answer.
        Some(0) => return Ok(Some(String::new())),
        Some(n) => n as u64,
    };

    // The window is a guess at how much text `want` lines occupy, and it grows until the window holds
    // enough lines - it does not fall back to reading the whole file (#289).
    //
    // The fallback was `read_to_string` on the entire file, so `--limit 20` on a log whose lines are
    // longer than the guess held the whole log in memory: the bug #78 fixed, surviving in the branch
    // that was supposed to be the careful one. A log that really is mostly long lines is exactly the
    // file whose tail somebody wants.
    //
    // `saturating_mul` because `want * TAIL_BYTES_PER_LINE` overflows `u64` at a `--limit` around
    // 2^54: the window wrapped small, the line count came up short, and that fed the whole-file read.
    let mut window = want.saturating_mul(TAIL_BYTES_PER_LINE).min(len);
    loop {
        let start = len - window;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;

        let text = if start == 0 {
            buf
        } else {
            // The window began mid-line, so the first fragment is not a line.
            match buf.find('\n') {
                Some(i) => buf[i + 1..].to_string(),
                None => String::new(),
            }
        };
        let enough =
            start == 0 || text.lines().filter(|l| !l.trim().is_empty()).count() >= want as usize;
        if enough {
            return Ok(Some(text));
        }
        // Grow geometrically so a file of very long lines costs a handful of reads, not one per line,
        // and stop at the whole file: at that point there is nothing more to read, and the answer is
        // what is there.
        if window >= len {
            return Ok(Some(text));
        }
        window = window.saturating_mul(4).min(len).max(window + 1);
    }
}

pub fn read(
    root: &std::path::Path,
    only_refused: bool,
    limit: Option<usize>,
) -> ZResult<Vec<Entry>> {
    let path = crate::repo_dir(root).join("audit.log");
    // With a limit, read only the tail (#78). `fs::read_to_string` loaded the whole file and the
    // limit then trimmed the PARSED output - so `gesta remote audit --limit 20` on a 2 GiB log read
    // 2 GiB to print 20 lines, which is the opposite of what a limit is for. The window is generous
    // (limit lines, at `TAIL_BYTES_PER_LINE` each) and widened once if it turns out to hold fewer
    // complete lines than asked for; a line longer than the window is still reported as unreadable
    // by the parser, which is what the caller already does with a damaged line.
    let text = match tail_text(&path, limit) {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(Vec::new()),
        Err(e) => {
            return Err(Box::new(crate::support::error::Error::new(
                crate::support::error::Code::NotFound,
                format!("cannot read {}: {}", path.display(), e),
            )))
        }
    };

    let mut out: Vec<Entry> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let doc = match crate::support::json::parse(line) {
            Ok(d) => d,
            // A line that does not parse is a line written by an older build or cut short by a full
            // disk. Skipping it silently would hide the damage; keeping the raw text shows it.
            Err(_) => {
                out.push(Entry {
                    at: String::new(),
                    who: String::new(),
                    decision: "unreadable".to_string(),
                    action: String::new(),
                    target: String::new(),
                    reason: "this line is not a document".to_string(),
                    raw: line.to_string(),
                });
                continue;
            }
        };
        let get = |k: &str| -> String {
            crate::support::json::field(&doc, k)
                .and_then(crate::support::json::as_str)
                .unwrap_or("")
                .to_string()
        };
        let entry = Entry {
            at: get("at"),
            who: get("who"),
            decision: get("decision"),
            action: get("action"),
            target: get("target"),
            reason: get("reason"),
            raw: line.to_string(),
        };
        if only_refused && entry.decision == "allowed" {
            continue;
        }
        out.push(entry);
    }

    if let Some(n) = limit {
        if out.len() > n {
            out = out.split_off(out.len() - n);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = super::super::scratch::scratch("gesta-audit");
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        d
    }

    #[test]
    fn an_event_that_cannot_be_written_says_so() {
        // #76: `if let Ok(f) = ..open(..)` with no `else` meant an unopenable log dropped every event
        // with nothing printed. An operator reading the file saw a history that silently stopped.
        //
        // stderr is captured by re-running this test binary as a child with an env var that makes it
        // take this branch, because a test cannot read its own stderr.
        if std::env::var("GESTA_AUDIT_FAIL_PROBE").is_ok() {
            let d = std::env::temp_dir().join(format!("gesta-audit-probe-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            // `.gesta` is a FILE, so `.gesta/audit.log` cannot be created.
            std::fs::write(d.join(".gesta"), b"not a directory").unwrap();
            record(
                &d,
                &Event {
                    who: "anonymous",
                    decision: Decision::NoToken,
                    action: "file",
                    target: "a.txt",
                    reason: "probe",
                    remote: None,
                },
            );
            let _ = std::fs::remove_dir_all(&d);
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            // `--nocapture`, or the harness swallows the very stderr this test exists to read.
            .args([
                "support::audit::tests::an_event_that_cannot_be_written_says_so",
                "--exact",
                "--nocapture",
            ])
            .env("GESTA_AUDIT_FAIL_PROBE", "1")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("audit log could not be opened"),
            "a dropped audit event must be reported, stderr was: {err}"
        );
    }

    #[test]
    fn a_refusal_is_recorded_and_is_a_document() {
        let d = dir();
        record(
            &d,
            &Event {
                who: "anonymous",
                decision: Decision::NoToken,
                action: "file",
                target: "a.txt",
                reason: "missing or invalid token",
                remote: Some("10.0.0.7"),
            },
        );
        let rows = read(&d, false, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decision, "no-token");
        assert_eq!(rows[0].who, "anonymous");
        assert_eq!(rows[0].target, "a.txt");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reason_with_a_quote_stays_one_line_and_one_document() {
        // The reason is where a path or a branch name lands, and both can hold a quote.
        let d = dir();
        record(
            &d,
            &Event {
                who: "agent",
                decision: Decision::BranchRefused,
                action: "push",
                target: "refs/heads/a\"b\nc",
                reason: "may not write that branch",
                remote: None,
            },
        );
        let rows = read(&d, false, None).unwrap();
        assert_eq!(rows.len(), 1, "a newline in the target made two lines");
        assert_eq!(rows[0].decision, "branch-refused");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn filtering_for_refusals_drops_the_successes() {
        let d = dir();
        for (decision, who) in [
            (Decision::Allowed, "agent"),
            (Decision::NoToken, "anonymous"),
            (Decision::Allowed, "agent"),
        ] {
            record(
                &d,
                &Event {
                    who,
                    decision,
                    action: "file",
                    target: "x",
                    reason: "",
                    remote: None,
                },
            );
        }
        let all = read(&d, false, None).unwrap();
        assert_eq!(all.len(), 3);
        let refused = read(&d, true, None).unwrap();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].decision, "no-token");
        // The newest N, because the question is always about what just happened.
        let recent = read(&d, false, Some(2)).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].who, "agent");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_line_that_does_not_parse_is_shown_rather_than_skipped() {
        let d = dir();
        // Two lines: an invalid JSON line and a plain text line. Both are kept as evidence,
        // because a log that silently drops the line that does not parse hides the damage.
        std::fs::write(
            d.join(".gesta").join("audit.log"),
            "{\"at\":\"x\"\nnot json\n",
        )
        .unwrap();
        let rows = read(&d, false, None).unwrap();
        // Both lines are unreadable; both are kept as evidence.
        assert_eq!(rows.len(), 2);
        // First line is the truncated JSON, second is the plain text.
        assert_eq!(rows[0].decision, "unreadable");
        assert_eq!(rows[0].raw, "{\"at\":\"x\"");
        assert_eq!(rows[1].decision, "unreadable");
        assert_eq!(rows[1].raw, "not json");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn the_module_does_not_claim_a_lock_it_does_not_take() {
        // #70: the header said lines were appended "under a lock" and gave that as the reason the
        // log was safe. There is no lock - and none is needed, because one `write_all` to an
        // `O_APPEND` descriptor is one `write` and the kernel does not interleave it - but a stated
        // guarantee that does not exist is read instead of the code and trusted.
        let src = include_str!("audit.rs");
        let header = &src[..src.find("use crate::ZResult;").unwrap()];
        assert!(
            !header.contains("under a lock"),
            "the header must not claim a lock that is not taken"
        );
        assert!(
            header.contains("O_APPEND"),
            "the header must name the actual reason a line is not interleaved"
        );
        // And there really is no lock, which is what makes the claim wrong. The scan stops at the
        // test module: this test's own text contains the words it looks for.
        let start = src.find("pub fn record").unwrap();
        let end = src.find("#[cfg(test)]").unwrap();
        let body = &src[start..end];
        assert!(!body.contains("flock"), "no lock is taken");
        assert!(!body.contains("lock()"), "no mutex guards the file");
    }
    fn lines_in(path: &std::path::Path) -> usize {
        match std::fs::read_to_string(path) {
            Ok(t) => t.lines().filter(|l| !l.trim().is_empty()).count(),
            Err(_) => 0,
        }
    }

    #[test]
    fn a_limited_read_does_not_load_the_whole_file() {
        // #78: `read_to_string` loaded the entire log and the `limit` then trimmed the PARSED output,
        // so `--limit 20` on a 2 GiB log read 2 GiB to print 20 lines.
        //
        // Measured on `tail_text`, which is the decision, not on `read`, which is the caller. The
        // first version of this test went through `read` and asserted on the returned entries - and
        // stayed green with the whole-file read restored, because the limit trims the output either
        // way. The bytes that were read are the thing under test, and `tail_text` is where they are
        // countable: its result must not contain a line from the start of the file.
        let d = dir();
        let path = crate::repo_dir(&d).join("audit.log");
        let mut body = String::from("FIRST-LINE-IS-HERE\n");
        for i in 0..4000 {
            body.push_str(&format!("{{\"at\":\"t{}\",\"decision\":\"refused\"}}\n", i));
        }
        std::fs::write(&path, body).unwrap();

        let tail = tail_text(&path, Some(3)).unwrap().unwrap();
        assert!(
            !tail.contains("FIRST-LINE-IS-HERE"),
            "the tail read pulled in the start of the file; that is the whole-file read again, and \
             a 2 GiB log would be held in memory to print 3 lines"
        );
        assert!(
            tail.contains("t3999"),
            "and the tail must reach the end of the file"
        );
        assert!(
            tail.lines().filter(|l| !l.trim().is_empty()).count() >= 3,
            "the window must hold at least the lines asked for"
        );

        // `read` still answers the same thing, which is the contract the callers see.
        let got = read(&d, false, Some(3)).unwrap();
        assert_eq!(got.len(), 3, "a limit of 3 must return 3 entries");
        assert_eq!(got[2].at, "t3999", "the tail must be the END of the log");

        // And no limit still reads everything.
        let all = read(&d, false, None).unwrap();
        assert_eq!(
            all.len(),
            4001,
            "no limit must read the whole log, including the marker line"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_limit_larger_than_the_file_returns_every_line() {
        // The window is a guess; when it is too small to hold the lines asked for, the read must widen
        // rather than report a short log. Otherwise "limit 100" on a 5-line file would look truncated.
        let d = dir();
        let path = crate::repo_dir(&d).join("audit.log");
        let mut body = String::new();
        for i in 0..5 {
            body.push_str(&format!("{{\"at\":\"t{}\"}}\n", i));
        }
        std::fs::write(&path, body).unwrap();
        let got = read(&d, false, Some(100)).unwrap();
        assert_eq!(
            got.len(),
            5,
            "all five lines must come back, got {}",
            got.len()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_oversized_log_is_rotated_and_still_written() {
        // #78: nothing rotated the log, so it grew without bound. One generation is kept, and the new
        // event must still land - a rotation that loses the event it was triggered by turns a disk
        // problem into a hole in the record.
        let d = dir();
        let path = crate::repo_dir(&d).join("audit.log");
        {
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(MAX_LOG_BYTES + 1).unwrap();
        }
        record(
            &d,
            &Event {
                who: "zen",
                decision: Decision::NoToken,
                action: "push",
                target: "main",
                reason: "a reason",
                remote: None,
            },
        );
        let old = path.with_extension("log.1");
        assert!(
            old.exists(),
            "a log over the cap must be moved aside, or it grows forever"
        );
        assert_eq!(
            lines_in(&path),
            1,
            "the event that triggered the rotation must still be in the new log"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn a_tail_of_long_lines_does_not_read_the_whole_file() {
        // #289: when the window held fewer lines than asked for, the fallback read the ENTIRE file -
        // the bug #78 fixed, surviving in the branch that was supposed to be the careful one. A log of
        // long lines (a JSON entry with a big diff in it, say) is exactly where somebody wants the
        // tail, so this is the case that matters.
        let dir = std::env::temp_dir().join(format!("gesta-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");

        // Twenty lines, each far longer than the bytes-per-line guess, so one window is not enough.
        let mut body = String::new();
        for i in 0..20 {
            body.push_str(&format!(
                "{}\n",
                "x".repeat(200_000 - 8) + &format!("{i:08}")
            ));
        }
        std::fs::write(&path, &body).unwrap();

        let got = tail_text(&path, Some(3)).unwrap().unwrap();
        let lines: Vec<&str> = got.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            3,
            "three lines were asked for and three lines exist, so three must come back"
        );
        assert!(
            lines[2].ends_with("00000019"),
            "and they must be the LAST three, not the first: got {:?}",
            &lines[2][lines[2].len() - 8..]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
