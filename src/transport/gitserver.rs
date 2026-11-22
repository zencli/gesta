//! The git smart HTTP protocol, enough of it that `git clone` works against a gesta server.
//!
//! Three layers, and each is separate on purpose:
//!
//! - `pkt-line`, the framing git uses for every message it sends.
//! - `ref_advertisement`, what the server says it has, which is the first thing `git clone` reads.
//! - `pack`, the object transfer. Written here rather than delegated, because the whole point of a
//!   server is that a client with no gesta binary can read from it.
//!
//! `receive-pack` is here too, so a plain `git push` works. It hands the pack to git to unpack and
//! then imports the result through the same path `git import` uses; a second pack reader for objects
//! that arrive over the wire would be a second implementation to keep in step.
//!
//! A push sends a thin pack: the objects the server is known to hold are left out and named as
//! bases. Unpacking into an empty repository therefore fails on an object the client never sent, so
//! the server's own history is exported into the scratch repository first. That one detail is the
//! difference between a push that works and a push that reports a missing object.
//!
//! What is served: protocol v2 (`ls-refs`, `fetch` with `acknowledgments`), `multi_ack_detailed`
//! negotiation, `ofs-delta` in the packs, and ref deletion. What is deliberately absent: multipack
//! indexes, shallow beyond announcing it, partial-clone filters, and `wait-for-done`. A repository
//! that advertises a capability it does not implement is worse than one that advertises less: the
//! client takes the offer and then hangs.
//!
//! Packs carry deltas. `store::delta_pack::encode` tries each object against a candidate base and
//! keeps the copy only when it measures smaller; a delta that does not verify against its hash is
//! dropped rather than shipped, because a wrong delta produces a pack git refuses with a message
//! about a bad object number, and that failure looks like a protocol bug rather than an encoder bug.
//! This paragraph used to say the opposite, and said it long after the encoder landed.

use crate::ZResult;

/// A request that this server cannot serve, with the part of it that made no sense.
fn bad(msg: String) -> Box<crate::support::error::Error> {
    Box::new(crate::support::error::Error::new(
        crate::support::error::Code::Usage,
        msg,
    ))
}

/// One git object, as the pack writer needs it.
pub struct Entry {
    /// The object's name. A pack does not carry it, but the server needs it to answer "does the
    /// client already hold this", which is the whole of a fetch negotiation.
    pub sha: String,
    /// 1 commit, 2 tree, 3 blob, 4 tag.
    pub kind: u8,
    /// The object body, uncompressed.
    pub body: Vec<u8>,
}

/// Frame one pkt-line: a four byte hex length that counts itself, then the payload.
///
/// The length includes the header, which is the detail that makes a hand-built pkt-line fail with
/// the client reporting a protocol error and no clue which message was wrong.
pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
    out
}

/// The flush packet, which ends a section of the conversation.
pub fn flush() -> Vec<u8> {
    b"0000".to_vec()
}

/// What the server offers a client that has not asked for anything yet.
///
/// Whether a string could be an object name in either format this repository knowledge allows.
///
/// The two lengths are 40 (SHA-1) and 64 (SHA-256). This is deliberately a shape check and not a
/// per-repository one: these functions receive names from a client before any repository has been
/// consulted, and rejecting a 64 character name outright turns a format mismatch into "the server
/// holds nothing", which is the failure that took longest to diagnose elsewhere.
fn is_oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `git-upload-pack` wants the first ref annotated with the capabilities, and every line terminated
/// by NUL only on that first one. A client that receives capabilities on the wrong line ignores them
/// and then asks for a side-band it was never offered.
pub fn ref_advertisement(refs: &[(String, String)], capabilities: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    let caps = capabilities.join(" ");
    for (i, (name, sha)) in refs.iter().enumerate() {
        let line = if i == 0 {
            format!("{} {}\0{}\n", sha, name, caps)
        } else {
            format!("{} {}\n", sha, name)
        };
        out.extend_from_slice(&pkt_line(line.as_bytes()));
    }
    out.extend_from_slice(&flush());
    out
}

/// Read the pkt-lines in a request body, skipping the ones a client sends as progress.
pub fn read_pkt_lines(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let hex = match std::str::from_utf8(&body[i..i + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let len = match usize::from_str_radix(hex, 16) {
            Ok(0) => {
                // A flush ends a section, not the request. A fetch sends `want`s, a flush, then its
                // `have`s and `done`. Stopping at the first flush dropped every `have`, which made a
                // negotiation that reads the list behave exactly like one that ignores it: 100% of a
                // full pack, with nothing to say why.
                i += 4;
                continue;
            }
            // 0001 is a delimiter and 0002 the end of the request, and neither is a payload length.
            // Protocol v2 uses both: the command section ends with 0001, the arguments follow, and a
            // reader that treats 0001 as a length reads one byte of the next header as data and then
            // finds no command at all.
            Ok(1) | Ok(2) => {
                i += 4;
                continue;
            }
            Ok(n) => n,
            Err(_) => break,
        };
        if len < 4 || i + len > body.len() {
            break;
        }
        let payload = &body[i + 4..i + len];
        out.push(String::from_utf8_lossy(payload).trim_end().to_string());
        i += len;
    }
    out
}

/// Every object the named commits reach, walking parents, trees and their entries.
///
/// The export holds objects without history, so reachability is computed here from the commit and
/// tree bodies rather than asked of the store. A `have` that names something absent is skipped: the
/// client is allowed to name a commit this server does not have.
pub(crate) fn reachable_from(
    entries: &[Entry],
    have: &[String],
) -> std::collections::HashSet<String> {
    use std::collections::{HashMap, HashSet};

    let mut by_sha: HashMap<&str, &Entry> = HashMap::with_capacity(entries.len());
    for e in entries {
        by_sha.insert(e.sha.as_str(), e);
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = have.to_vec();
    while let Some(sha) = stack.pop() {
        if !seen.insert(sha.clone()) {
            continue;
        }
        let Some(e) = by_sha.get(sha.as_str()) else {
            continue;
        };
        match e.kind {
            // A commit names its tree and its parents.
            1 => {
                if let Ok(c) = crate::store::gitobj::parse_commit(&e.body) {
                    if !c.tree.is_empty() {
                        stack.push(c.tree);
                    }
                    for p in c.parents {
                        stack.push(p);
                    }
                }
            }
            // A tree names blobs and subtrees.
            2 => {
                if let Ok(parsed) = crate::store::gitobj::parse_tree(&e.body) {
                    for ent in parsed {
                        stack.push(ent.hash);
                    }
                }
            }
            _ => {}
        }
    }
    seen
}

/// Write a pack file from a list of objects.
///
/// The header is the magic, a version, and the object count; then each object's type and size as a
/// varint; then the zlib stream of its body; then a SHA-1 of everything before it. A pack whose
/// trailing checksum is wrong is rejected by git with "index-pack failed", and the client does not
/// say which object.
pub fn write_pack(entries: &[Entry]) -> ZResult<Vec<u8>> {
    // Blobs are grouped by the size they started from and deltas are tried within a group. Two blobs
    // of very different sizes are rarely worth comparing, and the cost of trying is a scan of the
    // base per candidate.
    let bases = pick_delta_bases(entries);

    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    // Where each entry's header starts, so an OFS_DELTA can name the distance back to its base.
    let mut offsets: Vec<u64> = Vec::with_capacity(entries.len());

    for (i, e) in entries.iter().enumerate() {
        let start = out.len() as u64;
        offsets.push(start);

        // A delta replaces the body and changes the type to OFS_DELTA, with the distance to the base
        // written between the header and the compressed bytes.
        let (kind, body, base_distance) = match bases.get(&i) {
            Some((base_index, delta)) => {
                (6u8, delta.as_slice(), Some(start - offsets[*base_index]))
            }
            None => (e.kind, e.body.as_slice(), None),
        };

        // The type and the low four bits of the size share a byte, and a size above 15 continues in
        // the following bytes, seven bits at a time, with the high bit as a continuation marker.
        let mut size = body.len();
        let mut first = ((kind & 0x7) << 4) | ((size & 0xf) as u8);
        size >>= 4;
        while size > 0 {
            first |= 0x80;
            out.push(first);
            first = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(first);

        if let Some(distance) = base_distance {
            // The base distance uses a different continuation rule from the size: the value is
            // shifted with a bias of one per continuation byte, so a writer that used the size rule
            // produces a pack whose deltas point at the wrong object.
            let mut value = distance;
            let mut buf = vec![(value & 0x7f) as u8];
            value >>= 7;
            while value > 0 {
                value -= 1;
                buf.push(0x80 | (value & 0x7f) as u8);
                value >>= 7;
            }
            buf.reverse();
            out.extend_from_slice(&buf);
        }

        let packed = crate::store::compress_bytes(body)?;
        out.extend_from_slice(&packed);
    }

    let digest = sha1(&out);
    out.extend_from_slice(&digest);
    Ok(out)
}

/// Which entries are sent as a delta against an earlier entry, and against which.
///
/// Only blobs are considered, and only against a blob that already travelled: a delta names its base
/// by a backward distance, so the base has to come first in the pack. Tree and commit objects are
/// small and their text is mostly hashes, which delta poorly.
fn pick_delta_bases(entries: &[Entry]) -> std::collections::HashMap<usize, (usize, Vec<u8>)> {
    // How deep a delta chain may go. Git's own pack of this history reaches 34 levels, so a low cap
    // is not a quality improvement: at 8 the pack grew from 3703 to 4147 KiB and held 36 deltas
    // instead of 688. A cap exists to bound how many steps a client walks for one object, and git's
    // own answer to that is the number to match rather than a smaller one chosen by feel.
    const MAX_DEPTH: usize = 50;

    let mut out = std::collections::HashMap::new();
    // The most recent blob at each size band, with how deep its own delta chain is. A file edited
    // between two commits has a similar size, and the size band is the cheapest way to bring the two
    // versions together without indexing paths, which the pack does not carry.
    let mut by_band: std::collections::HashMap<usize, Vec<(usize, usize)>> =
        std::collections::HashMap::new();

    for (i, e) in entries.iter().enumerate() {
        if e.kind != 3 || e.body.len() < 64 {
            // Small blobs and non-blobs are sent whole. A tree of a large directory is the one
            // exception worth having and it is not worth the code.
            continue;
        }
        let band = (e.body.len().ilog2() as usize).max(1);
        let mut chosen: Option<(usize, Vec<u8>)> = None;
        if let Some(candidates) = by_band.get(&band) {
            // Every earlier blob in this band, newest first. The cap is on how many are *tried*,
            // not on how many are considered: a file with 68 versions has its other versions in
            // this list, and a window of the last eight only finds them when the history happens to
            // be in order.
            let window: Vec<(usize, usize)> = candidates.iter().rev().take(64).copied().collect();
            for &(cand, depth) in window.iter() {
                if depth >= MAX_DEPTH {
                    continue;
                }
                if let Some(delta) = crate::store::delta_pack::encode(&entries[cand].body, &e.body)
                {
                    let better = match &chosen {
                        None => true,
                        Some((_, best)) => delta.len() < best.len(),
                    };
                    if better {
                        chosen = Some((cand, delta));
                    }
                }
            }
        }
        let depth = match &chosen {
            Some((base, _)) => by_band
                .get(&band)
                .and_then(|c| c.iter().find(|(idx, _)| idx == base).map(|(_, d)| d + 1))
                .unwrap_or(1),
            None => 0,
        };
        if let Some((base, delta)) = chosen {
            out.insert(i, (base, delta));
        }
        by_band.entry(band).or_default().push((i, depth));
    }
    out
}

/// SHA-1, which git names objects by. This is the only place a pack needs it.
fn sha1(data: &[u8]) -> [u8; 20] {
    crate::store::gitobj::sha1_raw(data)
}

/// What the server has, for a client that is about to ask for it.
///
/// The first line needs the service banner: a client reads it to decide the protocol version, and
/// without it `git clone` reports "not a git repository" on a URL that answered 200.
pub fn advertise(root: &std::path::Path) -> ZResult<Vec<u8>> {
    let served = crate::transport::http::open_repo_for_serve(root)?;
    let refs = served.refs;

    let mut out = pkt_line(b"# service=git-upload-pack\n");
    out.extend_from_slice(&flush());

    // One ref per branch, named the way git names them. A branch whose head is missing is skipped
    // rather than advertised with a zero id, which a client would try to fetch.
    let mut lines: Vec<(String, String)> = Vec::new();
    for (name, sha) in refs {
        lines.push((format!("refs/heads/{}", name), sha));
    }
    if lines.is_empty() {
        // A client needs at least one ref to ask for. An empty repository advertises the
        // capabilities line with no refs, which git reads as "nothing to clone".
        let mut empty =
            pkt_line(b"0000000000000000000000000000000000000000 capabilities^{}\0agent=gesta\n");
        empty.extend_from_slice(&flush());
        out.extend_from_slice(&empty);
        return Ok(out);
    }
    // `symref=HEAD:refs/heads/main` is what tells a clone which branch to check out. Without it git
    // warns "remote HEAD refers to nonexistent ref" and leaves an empty working tree, and the
    // message points at the server's HEAD rather than at the missing advertisement.
    let head_branch = if lines.iter().any(|(name, _)| name == "refs/heads/main") {
        "main"
    } else {
        lines[0].0.trim_start_matches("refs/heads/")
    };
    let symref = format!("symref=HEAD:refs/heads/{}", head_branch);
    // `HEAD` is advertised as a ref of its own as well as named by symref. A clone uses the symref
    // to decide what to check out; without a `HEAD` line it still files everything under
    // refs/remotes/origin and leaves the working tree empty with a warning about a missing ref.
    let head_sha = lines
        .iter()
        .find(|(name, _)| name.as_str() == format!("refs/heads/{}", head_branch).as_str())
        .map(|(_, sha)| sha.clone());
    let mut advert = Vec::new();
    if let Some(sha) = head_sha {
        advert.push(("HEAD".to_string(), sha));
    }
    advert.extend(lines);
    // `multi_ack_detailed` is advertised because the negotiation below now answers it. Advertising a
    // capability the code did not honour is worse than not advertising it: a client would wait for
    // ACK lines that never come and stall until its own timeout.
    out.extend_from_slice(&ref_advertisement(
        &advert,
        &["ofs-delta", "side-band-64k", "multi_ack_detailed", &symref],
    ));
    Ok(out)
}

/// Answer `git-upload-pack`: read what the client wants, then send the objects and a status.
///
/// The client sends `want <sha>` lines, then a flush, then `done`. Everything after `done` is our
/// side. Writing the pack straight into the response without the side-band wrapper makes a client
/// that negotiated `side-band-64k` read the pack bytes as channel headers and fail.
pub fn upload_pack(root: &std::path::Path, body: &[u8]) -> ZResult<Vec<u8>> {
    let lines = read_pkt_lines(body);
    let wanted: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            line.strip_prefix("want ")
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        })
        .filter(|s| is_oid(s))
        .collect();

    // What the client already holds. A fetch after the first one names the commits it has, and every
    // object those commits reach is an object it does not need sent again. Ignoring this list is what
    // made a second fetch cost as much as the first.
    let have: Vec<String> = lines
        .iter()
        .filter_map(|line| {
            line.strip_prefix("have ")
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        })
        .filter(|s| is_oid(s))
        .collect();

    // One export gives both the refs and every object they reach, so nothing is walked twice.
    let served = crate::transport::http::open_repo_for_serve(root)?;
    let entries = served.objects;

    if wanted.is_empty() || entries.is_empty() {
        return Ok(flush());
    }

    // Subtracting what the client has is the difference between a fetch that sends the tree and one
    // that sends the change. A `have` the server does not recognise is not an error: git sends them
    // optimistically, and a client can name a commit this server has never seen.
    //
    // Under `multi_ack_detailed` the client expects an ACK for each `have` it sent and for the last
    // common one, in three forms: ACK <sha> common, ACK <sha> ready, and a final ACK <sha>. Only the
    // final ACK belongs before the pack. The others are what stop the client sending `have`s the
    // server already has, so they are written here and discarded by a client that did not ask.
    let mut negotiation_acks: Vec<u8> = Vec::new();
    if !have.is_empty() {
        let held = reachable_from(&entries, &have);
        for h in &have {
            if held.contains(h) {
                // The last one in the list is the one the server is ready to serve from.
                let ready = h == have.last().unwrap_or(h);
                let line = if ready {
                    format!("ACK {} ready\n", h)
                } else {
                    format!("ACK {} common\n", h)
                };
                negotiation_acks.extend_from_slice(&pkt_line(line.as_bytes()));
            }
        }
    }
    let entries = if have.is_empty() {
        entries
    } else {
        let held = reachable_from(&entries, &have);
        let before = entries.len();
        let kept: Vec<Entry> = entries
            .into_iter()
            .filter(|e| !held.contains(&e.sha))
            .collect();
        // Nothing new to send is a valid answer: the client is up to date. It still has to be
        // framed the way a client expects a fetch result, or it reports a protocol error instead of
        // "Already up to date."
        if kept.is_empty() {
            let mut out = Vec::new();
            if !negotiation_acks.is_empty() {
                out.extend_from_slice(&negotiation_acks);
                if let Some(last) = have.last() {
                    out.extend_from_slice(&pkt_line(format!("ACK {}\n", last).as_bytes()));
                }
            } else {
                out.extend_from_slice(&pkt_line(b"NAK\n"));
            }
            out.extend_from_slice(&pkt_line(&[1u8]));
            out.extend_from_slice(&flush());
            return Ok(out);
        }
        let _ = before;
        kept
    };

    let pack = write_pack(&entries)?;

    // Side-band 1 carries the pack. Each chunk is a pkt-line whose first byte is the band number,
    // and a payload above 65515 bytes overflows the four hex digits that hold the length.
    //
    // `NAK` is the only line before the pack in a stateless-rpc v0 exchange. The symref a clone
    // needs to pick a branch travels in the advertisement, and this response does not repeat it:
    // repeating the refs here made git store refs/remotes/origin/main and leave HEAD on master,
    // which is the "remote HEAD refers to nonexistent ref" warning and an empty working tree.
    let mut out = Vec::new();
    // The negotiation lines first, then the answer. A client that sent `have`s and negotiated
    // multi_ack_detailed reads these to decide what is still missing; a client that did not asked for
    // none of it and was sent none.
    if !negotiation_acks.is_empty() {
        out.extend_from_slice(&negotiation_acks);
        if let Some(last) = have.last() {
            out.extend_from_slice(&pkt_line(format!("ACK {}\n", last).as_bytes()));
        }
    } else {
        out.extend_from_slice(&pkt_line(b"NAK\n"));
    }
    for chunk in pack.chunks(65500) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(1u8);
        payload.extend_from_slice(chunk);
        out.extend_from_slice(&pkt_line(&payload));
    }
    // Under side-band the stream ends with a band-1 packet carrying nothing, not a bare flush. A
    // client reads `0000` as the end of the pkt-line section and stops listening, so the pack it
    // received is never acknowledged and the clone reports that the remote has no commits.
    out.extend_from_slice(&pkt_line(&[1u8]));
    out.extend_from_slice(&flush());
    Ok(out)
}

/// What a push says it wants to do, one line per ref.
pub struct PushCommand {
    pub old: String,
    pub new: String,
    pub name: String,
}

/// Split a receive-pack body into the commands and the pack that follows them.
///
/// The body is pkt-lines up to a flush, then raw pack bytes. The pack has no length prefix and no
/// framing of its own, so everything after the flush belongs to it whether or not it starts with
/// `PACK`, and a body that ends at the flush carried no objects.
pub fn split_push(body: &[u8]) -> ZResult<(Vec<PushCommand>, Vec<u8>)> {
    let mut commands = Vec::new();
    let mut i = 0usize;
    loop {
        if i + 4 > body.len() {
            return Err(bad("a push ended before its flush".to_string()));
        }
        let len = std::str::from_utf8(&body[i..i + 4])
            .ok()
            .and_then(|h| usize::from_str_radix(h, 16).ok())
            .ok_or_else(|| bad("a push carried a length that is not hexadecimal".to_string()))?;
        if len == 0 {
            i += 4;
            break;
        }
        if i + len > body.len() {
            return Err(bad("a push carried a line longer than its body".to_string()));
        }
        let line = String::from_utf8_lossy(&body[i + 4..i + len]).to_string();
        i += len;
        // The first line carries the capabilities after a NUL, and they belong to the exchange
        // rather than to the ref.
        let line = line.split('\0').next().unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(' ');
        let old = parts.next().unwrap_or("").to_string();
        let new = parts.next().unwrap_or("").to_string();
        let name = parts.next().unwrap_or("").to_string();
        if !is_oid(&old) || !is_oid(&new) || name.is_empty() {
            return Err(bad(format!(
                "a push named a ref in a shape that is not one: {}",
                line
            )));
        }
        commands.push(PushCommand { old, new, name });
    }
    Ok((commands, body[i..].to_vec()))
}

/// Answer `git-receive-pack`: take the objects, make them changesets, move the refs.
///
/// The pack is unpacked by git into a scratch bare repository and then read through the same import
/// path a `git import` uses. Writing a second reader for a pack that arrives over the wire would be
/// a second implementation to keep in step with the first, and the first difference between them
/// shows up as a push that reports success and a branch that did not move.
/// Answer `git-receive-pack`.
///
/// `allowed`, when given, is which branches this caller may write. Returning false for a ref rejects
/// that ref and leaves the others alone, which is what a push naming three refs expects when one of
/// them is not its business. The pack is still unpacked: refusing the whole transfer would let one
/// forbidden branch hide the objects the permitted ones need.
pub fn receive_pack_allowing(
    root: &std::path::Path,
    body: &[u8],
    allowed: Option<&dyn Fn(&str) -> bool>,
    pusher: &str,
) -> ZResult<Vec<u8>> {
    let (commands, pack) = split_push(body)?;

    // Refused refs, remembered so the import's own branch creation can be undone. The import walks
    // the scratch repository's refs and creates a branch for each, so a forbidden branch was created
    // and left behind pointing at a commit the caller may not write to.
    let refused: Vec<String> = commands
        .iter()
        .filter(|c| match allowed {
            Some(permitted) => {
                let branch = c.name.strip_prefix("refs/heads/").unwrap_or(&c.name);
                !permitted(branch)
            }
            None => false,
        })
        .map(|c| {
            c.name
                .strip_prefix("refs/heads/")
                .unwrap_or(&c.name)
                .to_string()
        })
        .collect();
    if commands.is_empty() {
        return Ok(flush());
    }
    if pack.is_empty() {
        // A push with no objects is a ref deletion, or a move to a commit the server already holds.
        // Nothing to unpack, and git's own reader would reject an empty file.
        //
        // This path used to answer `ok` for every command without doing anything, which is the worst
        // answer a server can give: git printed `- [deleted] branch` and the branch was still there.
        // A deletion is now performed here, and only refs the caller may write are touched.
        let served = crate::transport::http::open_repo_for_serve(root)?;
        let before = served.refs;
        let mut out = pkt_line(b"unpack ok\n");
        for c in &commands {
            let branch = c.name.strip_prefix("refs/heads/").unwrap_or(&c.name);
            if refused.iter().any(|r| r == branch) {
                out.extend_from_slice(
                    pkt_line(
                        format!("ng {} this token may not write that branch\n", c.name).as_bytes(),
                    )
                    .as_slice(),
                );
                continue;
            }
            let zero = "0".repeat(40);
            if c.new == zero {
                // The client believes the branch holds `old`. Deleting something that moved on
                // would discard whatever arrived in between, so a mismatch is refused.
                let was = before
                    .iter()
                    .find(|(b, _)| b == branch)
                    .map(|(_, sha)| sha.clone());
                match was {
                    None => {
                        out.extend_from_slice(
                            pkt_line(
                                format!("ng {} no such branch on the server\n", c.name).as_bytes(),
                            )
                            .as_slice(),
                        );
                    }
                    Some(was) if c.old != zero && was != c.old => {
                        out.extend_from_slice(
                            pkt_line(
                                format!(
                                    "ng {} the branch moved on the server since the client last fetched\n",
                                    c.name
                                )
                                .as_bytes(),
                            )
                            .as_slice(),
                        );
                    }
                    Some(_) => {
                        let conn = crate::store::open_db(root)?;
                        // A repository with no branches holds history nothing points at. `status`
                        // reports "there are no branches yet" and every command that needs a branch
                        // refuses, which is a repository nobody can work in. Deleting the last
                        // branch is refused here, the way deleting the checked-out branch is refused
                        // locally.
                        let count: i64 =
                            conn.query_row("SELECT COUNT(*) FROM branches", [], |r| r.get(0))?;
                        if count <= 1 {
                            out.extend_from_slice(
                                pkt_line(
                                    format!(
                                        "ng {} this is the last branch; deleting it would leave a repository with no branch to work on\n",
                                        c.name
                                    )
                                    .as_bytes(),
                                )
                                .as_slice(),
                            );
                            continue;
                        }
                        conn.execute(
                            "DELETE FROM branches WHERE name = ?1",
                            rusqlite::params![branch],
                        )?;
                        conn.execute(
                            "DELETE FROM branch_tree WHERE branch = ?1",
                            rusqlite::params![branch],
                        )?;
                        crate::support::ops::record(&conn, "push delete", Some(&c.name), None)?;
                        out.extend_from_slice(&pkt_line(format!("ok {}\n", c.name).as_bytes()));
                    }
                }
                continue;
            }
            // Not a deletion and nothing to unpack: the new id is a commit the server already holds,
            // which happens when a client re-pushes a branch that is already there.
            out.extend_from_slice(&pkt_line(format!("ok {}\n", c.name).as_bytes()));
        }
        out.extend_from_slice(&flush());
        return Ok(out);
    }

    let scratch = std::env::temp_dir().join(format!(
        "gesta-push-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = (|| -> ZResult<Vec<u8>> {
        std::fs::create_dir_all(&scratch)?;
        let init = std::process::Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("-q")
            .arg(&scratch)
            .output()
            .map_err(|e| bad(format!("a push needs git to unpack it: {}", e)))?;
        if !init.status.success() {
            return Err(bad(format!(
                "git could not prepare a scratch repository: {}",
                String::from_utf8_lossy(&init.stderr).trim()
            )));
        }

        // `index-pack` reads a pack from a file and writes the objects beside it. `--stdin` would
        // read from the process, and the pack is already in memory.
        let pack_file = scratch.join("objects").join("pack").join("incoming.pack");
        let pack_dir = scratch.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        std::fs::write(&pack_file, &pack)?;
        // `index-pack` would leave the objects inside a pack, and the import reads loose objects. So
        // the pack is unpacked loose here. One extra write of the same bytes, and the alternative is
        // teaching the importer to walk a pack it did not write.
        let idx = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&scratch)
            .arg("unpack-objects")
            .arg("-q")
            .stdin(std::process::Stdio::from(
                std::fs::File::open(&pack_file)
                    .map_err(|e| bad(format!("the pack a push sent could not be read: {}", e)))?,
            ))
            .output()
            .map_err(|e| bad(format!("a push needs git to unpack it: {}", e)))?;
        if !idx.status.success() {
            return Err(bad(format!(
                "the pack a push sent could not be read: {}",
                String::from_utf8_lossy(&idx.stderr).trim()
            )));
        }
        // The pack itself is not a loose object and the importer does not look at it.
        let _ = std::fs::remove_file(&pack_file);

        // A push sends a thin pack: the objects the server is already known to hold are left out,
        // and the pack names them as bases. Unpacking into an empty repository therefore fails with
        // a missing object that the client never sent. The existing history is exported into the
        // scratch first, so the bases are there when the new objects arrive.
        let conn = crate::store::open_db(root)?;
        if let Err(e) = crate::cli::git_bridge::export(&conn, &scratch, None) {
            return Err(bad(format!(
                "the server's own history could not be staged for a push: {}",
                e
            )));
        }

        // The refs have to be in the scratch repository before the import walks them, because the
        // import starts from branch heads rather than from a list of objects.
        for c in &commands {
            let branch_dir = scratch.join("refs").join("heads");
            std::fs::create_dir_all(&branch_dir)?;
            let ref_file = branch_dir.join(c.name.strip_prefix("refs/heads/").unwrap_or(&c.name));
            std::fs::write(&ref_file, format!("{}\n", c.new))?;
        }

        // The client's `old` value is checked against the branch as it was before this push. Reading
        // it after the import would compare the client's expectation with the result of applying the
        // push, which always differs and rejects every push that is correct.
        // The branches the push names and the ones that already existed are different questions. A
        // refused push must not delete history, and the map of names in the request answers the
        // wrong one: it lists every ref the client offered, whether the server ever had it or not.
        let existed: Vec<String> = commands
            .iter()
            .map(|c| {
                c.name
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&c.name)
                    .to_string()
            })
            .filter(|b| {
                conn.query_row(
                    "SELECT COUNT(*) FROM branches WHERE name = ?1",
                    rusqlite::params![b],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                    > 0
            })
            .collect();

        // The git sha and the changeset id the branch pointed at. The id is what a refusal needs: the
        // import moves the branch as it creates changesets, so a gate that only refuses the `ok` line
        // leaves the branch where the import put it, and git reports a rejected push that landed.
        let before: Vec<(String, Option<String>, Option<String>)> = commands
            .iter()
            .map(|c| {
                let branch = c
                    .name
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&c.name)
                    .to_string();
                let head = crate::store::branch_head(&conn, &branch).ok().flatten();
                let sha = head.as_ref().and_then(|id| {
                    conn.query_row(
                        "SELECT git_commit FROM changesets WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                });
                (branch, sha, head)
            })
            .collect();

        crate::cli::git_bridge::import(&conn, &scratch, None)?;

        // A branch the import created for a refused ref did not exist before this push, so removing
        // it puts the repository back. Checking first whether the push created it is what keeps a
        // refused push to an existing branch from deleting that branch's history.
        for name in &refused {
            // Only a branch that did not exist before this push is removed. One that already existed
            // belongs to the repository, and a refused push to it must leave it alone.
            let existed_before = existed.iter().any(|b| b == name);
            if !existed_before {
                let _ = conn.execute(
                    "DELETE FROM branches WHERE name = ?1",
                    rusqlite::params![name],
                );
            }
        }

        // After the import every pushed commit has a changeset, so the branch can point at it.
        let mut out = pkt_line(b"unpack ok\n");
        for c in &commands {
            // Filled when a refusal could not be undone. A refusal is only honest if the branch really
            // did not move, so a failed rollback has to reach the client rather than a log.
            let mut rollback_errors: Vec<String> = Vec::new();
            let branch = c
                .name
                .strip_prefix("refs/heads/")
                .unwrap_or(&c.name)
                .to_string();

            // A token scoped to a path could still rewrite every branch. The branch is the part a
            // mistake costs the most to undo, so it is refused by name and the other refs in the
            // same push still apply.
            if let Some(permitted) = allowed {
                if !permitted(&branch) {
                    out.extend_from_slice(
                        pkt_line(
                            format!("ng {} this token may not write that branch\n", c.name)
                                .as_bytes(),
                        )
                        .as_slice(),
                    );
                    continue;
                }
            }
            let changeset: Option<String> = conn
                .query_row(
                    "SELECT id FROM changesets WHERE git_commit = ?1",
                    rusqlite::params![c.new],
                    |r| r.get(0),
                )
                .ok();

            // A protected branch needs a sign-off from a credential other than the one pushing.
            // Checked here, after the import, because the approval names a changeset and the
            // changeset only exists once the objects have landed. A refusal leaves the branch where
            // it was: the row is reported `ng` and the history is not moved.
            if let Some(cs) = &changeset {
                if let Err(e) = crate::support::approval::check_push(&conn, &branch, cs, pusher) {
                    // The import already moved the branch. Putting it back is what makes the refusal
                    // true: git prints `! [remote rejected]` and the branch is where it was.
                    let had = before
                        .iter()
                        .find(|(b, _, _)| b == &branch)
                        .and_then(|(_, _, id)| id.clone());
                    match had {
                        Some(prev) => {
                            // A rollback that fails silently leaves the branch moved while the client is
                            // told `! [remote rejected]`: the refusal is then a lie, and the store is in
                            // a state nobody knows about. The failures are collected rather than
                            // discarded so the report can say the branch may have moved.
                            if let Err(e) = crate::store::set_branch_head(&conn, &branch, &prev) {
                                rollback_errors.push(format!("restore head: {}", e));
                            }
                            if let Err(e) = crate::store::refresh_branch_tree(&conn, &branch) {
                                rollback_errors.push(format!("restore tree: {}", e));
                            }
                        }
                        // A push that created the branch: undo the creation.
                        None => {
                            if let Err(e) = conn.execute(
                                "DELETE FROM branches WHERE name = ?1",
                                rusqlite::params![branch],
                            ) {
                                rollback_errors.push(format!("drop branch: {}", e));
                            }
                            if let Err(e) = conn.execute(
                                "DELETE FROM branch_tree WHERE branch = ?1",
                                rusqlite::params![branch],
                            ) {
                                rollback_errors.push(format!("drop tree: {}", e));
                            }
                        }
                    }
                    let reason = if rollback_errors.is_empty() {
                        format!("{}", e)
                    } else {
                        format!(
                            "{} (and the branch could not be put back: {})",
                            e,
                            rollback_errors.join("; ")
                        )
                    };
                    out.extend_from_slice(
                        pkt_line(format!("ng {} {}\n", c.name, reason).as_bytes()).as_slice(),
                    );
                    crate::support::audit::local::push_refused(
                        root,
                        &branch,
                        pusher,
                        &reason,
                    );
                    continue;
                }
            }
            // The `old` value is what the client believed the branch held. If the server moved on
            // since, applying the push would silently discard whatever arrived in between, which is
            // the one outcome a version control server must never produce. A new branch sends zeros.
            let zero = "0".repeat(40);

            // A new id of all zeroes is a deletion. The `old` check below still runs first, so a
            // client deleting a branch it has not fetched is refused rather than deleting whatever
            // arrived in between.
            if c.new == zero {
                if c.old != zero {
                    let was = before
                        .iter()
                        .find(|(b, _, _)| b == &branch)
                        .and_then(|(_, sha, _)| sha.clone());
                    if let Some(was) = was {
                        if was != c.old {
                            out.extend_from_slice(
                                pkt_line(
                                    format!(
                                        "ng {} the branch moved on the server since the client last fetched\n",
                                        c.name
                                    )
                                    .as_bytes(),
                                )
                                .as_slice(),
                            );
                            continue;
                        }
                    }
                }
                let known: Option<String> = conn
                    .query_row(
                        "SELECT name FROM branches WHERE name = ?1",
                        rusqlite::params![branch],
                        |r| r.get(0),
                    )
                    .ok();
                if known.is_none() {
                    out.extend_from_slice(
                        pkt_line(
                            format!("ng {} no such branch on the server\n", c.name).as_bytes(),
                        )
                        .as_slice(),
                    );
                    continue;
                }
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM branches", [], |r| r.get(0))?;
                if count <= 1 {
                    out.extend_from_slice(
                        pkt_line(
                            format!(
                                "ng {} this is the last branch; deleting it would leave a repository with no branch to work on\n",
                                c.name
                            )
                            .as_bytes(),
                        )
                        .as_slice(),
                    );
                    continue;
                }
                conn.execute(
                    "DELETE FROM branches WHERE name = ?1",
                    rusqlite::params![branch],
                )?;
                // The derived tree goes with the branch: leaving it would make a branch created
                // later under the same name start from a tree nobody wrote.
                conn.execute(
                    "DELETE FROM branch_tree WHERE branch = ?1",
                    rusqlite::params![branch],
                )?;
                crate::support::ops::record(&conn, "push delete", Some(&c.name), None)?;
                out.extend_from_slice(&pkt_line(format!("ok {}\n", c.name).as_bytes()));
                continue;
            }
            if c.old != zero {
                let was = before
                    .iter()
                    .find(|(b, _, _)| b == &branch)
                    .and_then(|(_, sha, _)| sha.clone());
                if let Some(was) = was {
                    if was != c.old {
                        out.extend_from_slice(
                            pkt_line(
                                format!(
                                    "ng {} the branch moved on the server since the client last fetched\n",
                                    c.name
                                )
                                .as_bytes(),
                            )
                            .as_slice(),
                        );
                        continue;
                    }
                }
            }
            match changeset {
                Some(id) => {
                    // A branch that does not exist yet is created by the push, which is what a
                    // client expects when it pushes a branch the server has never seen.
                    let known: Option<String> = conn
                        .query_row(
                            "SELECT name FROM branches WHERE name = ?1",
                            rusqlite::params![branch],
                            |r| r.get(0),
                        )
                        .ok();
                    if known.is_none() {
                        conn.execute(
                            "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, NULL, ?2, NULL)",
                            rusqlite::params![branch, crate::support::clock::unix_now()],
                        )?;
                    }
                    crate::store::set_branch_head(&conn, &branch, &id)?;
                    crate::store::refresh_branch_tree(&conn, &branch)?;
                    // A push is where a branch moves for somebody else's work, which is the event
                    // an operator most wants in one place.
                    crate::support::audit::local::push(
                        root,
                        &branch,
                        pusher,
                        &format!("{} -> {}", c.old, c.new),
                    );
                    out.extend_from_slice(&pkt_line(format!("ok {}\n", c.name).as_bytes()));
                }
                None => {
                    out.extend_from_slice(
                        pkt_line(
                            format!("ng {} the commit was unpacked but not imported\n", c.name)
                                .as_bytes(),
                        )
                        .as_slice(),
                    );
                }
            }
        }
        out.extend_from_slice(&flush());
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// What the server accepts, for a client that is about to push.
pub fn advertise_receive(root: &std::path::Path) -> ZResult<Vec<u8>> {
    let served = crate::transport::http::open_repo_for_serve(root)?;
    let mut out = pkt_line(b"# service=git-receive-pack\n");
    out.extend_from_slice(&flush());

    let mut lines: Vec<(String, String)> = Vec::new();
    for (name, sha) in served.refs {
        lines.push((format!("refs/heads/{}", name), sha));
    }
    if lines.is_empty() {
        // A push to a repository with no branches still needs an advertisement, and git writes the
        // capabilities on a zero-id line when there is nothing to advertise.
        let zero = "0".repeat(40);
        lines.push((
            zero.clone(),
            format!("{} capabilities^{{}}\0report-status", zero),
        ));
    }
    // `delete-refs` is advertised because a push whose new id is all zeroes is a deletion and this
    // server now performs it. Advertising a capability the code does not honour is worse than not
    // advertising it: a client would send a deletion and be told the push succeeded.
    out.extend_from_slice(&ref_advertisement(
        &lines,
        &["report-status", "delete-refs"],
    ));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_pack_still_names_every_object_it_wrote() {
        // The pack writer now replaces a body with a delta against an earlier object. A delta that
        // rebuilds to the wrong bytes makes git reject the whole pack with a message about a bad
        // object, so the count and the offsets have to agree with what was written.
        let entries = vec![
            Entry {
                sha: "test-1".into(),
                kind: 3,
                body: b"the quick brown fox jumps over the lazy dog\n".repeat(40),
            },
            Entry {
                sha: "test-1".into(),
                kind: 3,
                body: {
                    let mut v = b"the quick brown fox jumps over the lazy dog\n".repeat(40);
                    v.extend_from_slice(b"one more line\n");
                    v
                },
            },
        ];
        let pack = write_pack(&entries).unwrap();
        assert_eq!(&pack[..4], b"PACK");
        let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]);
        assert_eq!(count, 2, "the header must name both objects");

        // The second object should have been written as a delta: a copy of the first plus one line
        // costs far less than the body itself.
        assert!(
            pack.len() < entries[1].body.len(),
            "a two-line change should not cost a whole body: pack {} bytes, body {}",
            pack.len(),
            entries[1].body.len()
        );
    }

    #[test]
    fn a_pkt_line_counts_its_own_header() {
        let got = pkt_line(b"hello\n");
        assert_eq!(
            &got[..4],
            b"000a",
            "6 bytes of payload plus a 4 byte header"
        );
        assert_eq!(&got[4..], b"hello\n");
    }

    #[test]
    fn a_flush_is_four_zeroes() {
        assert_eq!(flush(), b"0000".to_vec());
    }

    #[test]
    fn the_first_ref_line_carries_capabilities_after_a_nul() {
        let refs = vec![
            ("refs/heads/main".to_string(), "a".repeat(40)),
            ("refs/heads/dev".to_string(), "b".repeat(40)),
        ];
        let out = ref_advertisement(&refs, &["ofs-delta", "side-band-64k"]);
        let text = String::from_utf8_lossy(&out);
        let first = text.split('\n').next().unwrap();
        assert!(
            first.contains('\0'),
            "the first line needs the capabilities"
        );
        assert!(first.contains("ofs-delta"));
        // Only the first. A client reads capabilities once.
        let second = text.split('\n').nth(1).unwrap();
        assert!(
            !second.contains('\0'),
            "capabilities belong to one line only"
        );
        assert!(out.ends_with(b"0000"));
    }

    #[test]
    fn pkt_lines_round_trip() {
        let body = [pkt_line(b"want abc\n"), pkt_line(b"done\n"), flush()].concat();
        let got = read_pkt_lines(&body);
        assert_eq!(got, vec!["want abc".to_string(), "done".to_string()]);
    }

    #[test]
    fn a_truncated_pkt_line_is_not_panicked_over() {
        // A body that claims more bytes than it holds is a client bug or a truncation, and reading
        // it must stop rather than slice past the end.
        let got = read_pkt_lines(b"0064short");
        assert!(got.is_empty());
    }

    #[test]
    fn a_pack_starts_with_the_magic_version_and_count() {
        let entries = vec![Entry {
            sha: "test-hello".into(),
            kind: 3,
            body: b"hello\n".to_vec(),
        }];
        let pack = write_pack(&entries).unwrap();
        assert_eq!(&pack[..4], b"PACK");
        assert_eq!(u32::from_be_bytes([pack[4], pack[5], pack[6], pack[7]]), 2);
        assert_eq!(
            u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]),
            1
        );
        // Four header bytes, four version, four count, then the object, then a 20 byte checksum.
        assert!(pack.len() > 12 + 20);
    }

    #[test]
    fn an_object_larger_than_fifteen_bytes_declares_its_size_across_bytes() {
        let body = vec![b'x'; 300];
        let entries = vec![Entry {
            sha: "test-large".into(),
            kind: 3,
            body: body.clone(),
        }];
        let pack = write_pack(&entries).unwrap();
        // Read the varint back the way git does. The type lives in the FIRST byte, and the loop
        // moves past it: reading the type after the loop reads the last size byte, which is how this
        // test failed while the encoder was correct.
        let mut i = 12;
        let kind = (pack[i] >> 4) & 0x7;
        let mut size = (pack[i] & 0x0f) as usize;
        let mut shift = 4;
        while pack[i] & 0x80 != 0 {
            i += 1;
            size |= ((pack[i] & 0x7f) as usize) << shift;
            shift += 7;
        }
        assert_eq!(size, 300, "the size varint did not survive");
        assert_eq!(kind, 3, "the type is a blob");
    }
    #[test]
    fn a_push_that_deletes_a_ref_is_a_deletion_not_a_success() {
        // A push with no pack used to answer `ok` for every command without touching anything, so git
        // printed `- [deleted] branch` and the branch was still on the server. This checks the empty
        // pack path against a real repository rather than against the bytes.
        let d = std::env::temp_dir().join(format!("gesta-delref-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();
        // A branch has to point at a commit to appear in the served refs, which is the same shape a
        // client sees after fetching. A branch with a NULL head is not advertised, and a commit needs
        // the branch to exist first.
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        let first = crate::repo::create_changeset(&conn, &d, "first", "t")
            .unwrap()
            .unwrap();
        crate::repo::commit_one(&conn, &first.id).unwrap();
        let head: String = conn
            .query_row("SELECT head FROM branches WHERE name = 'main'", [], |r| {
                r.get(0)
            })
            .unwrap();
        for name in ["keep", "gone"] {
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES(?1, ?2, 0, NULL)",
                rusqlite::params![name, head],
            )
            .unwrap();
        }
        drop(conn);

        // The client believes `gone` is at a commit it never fetched, so pass zeros for `old`, which is
        // what a client sends when it has not seen the ref.
        let zero = "0".repeat(40);
        let cmd = format!(
            "{} {} refs/heads/gone\0 report-status delete-refs",
            zero, zero
        );
        let mut body = Vec::new();
        let payload = cmd.as_bytes();
        body.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
        body.extend_from_slice(payload);
        body.extend_from_slice(b"0000");

        let reply =
            receive_pack_allowing(&d, &body, None, "tester").expect("a deletion needs no pack");
        let text = String::from_utf8_lossy(&reply);
        assert!(text.contains("ok refs/heads/gone"), "reply: {}", text);

        let conn = crate::store::open_db(&d).unwrap();
        let left: Vec<String> = conn
            .prepare("SELECT name FROM branches ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            left,
            vec!["keep".to_string(), "main".to_string()],
            "gone must be deleted"
        );

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_last_branch_cannot_be_deleted() {
        // A repository with no branch reports "there are no branches yet" for every command that
        // needs one, which is a repository nobody can work in.
        let d = std::env::temp_dir().join(format!("gesta-lastbr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&d).unwrap();
        crate::store::init_schema(&conn).unwrap();
        // A commit needs a branch to land on, and `init_schema` does not create one: that is what
        // `gesta init` does. The branch is renamed afterwards so this test owns the only one.
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        let first = crate::repo::create_changeset(&conn, &d, "first", "t")
            .unwrap()
            .unwrap();
        crate::repo::commit_one(&conn, &first.id).unwrap();
        conn.execute("UPDATE branches SET name = 'only' WHERE name = 'main'", [])
            .unwrap();
        drop(conn);

        let zero = "0".repeat(40);
        let cmd = format!(
            "{} {} refs/heads/only\0 report-status delete-refs",
            zero, zero
        );
        let mut body = Vec::new();
        let payload = cmd.as_bytes();
        body.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
        body.extend_from_slice(payload);
        body.extend_from_slice(b"0000");

        let reply = receive_pack_allowing(&d, &body, None, "tester").unwrap();
        let text = String::from_utf8_lossy(&reply);
        assert!(text.contains("ng refs/heads/only"), "reply: {}", text);
        assert!(text.contains("last branch"), "reply: {}", text);

        let conn = crate::store::open_db(&d).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM branches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "the last branch must still be there");

        let _ = std::fs::remove_dir_all(&d);
    }
}
