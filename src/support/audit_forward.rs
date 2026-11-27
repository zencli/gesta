// Audit forwarding.
//
// The record is a local file, which answers "what happened on this machine" and cannot answer "what
// happened across the fleet" without somebody reading every file. A forwarder posts each event to a
// URL as it is written, so a collector sees a refusal while it is still worth acting on.
//
// Two rules shape everything here, and both come from what happens when a log pipeline breaks:
//
// - A slow or dead collector must never slow a request. Delivery is queued and a background thread
//   drains it, so the request path pays for a channel send and nothing else.
// - A dropped event must be counted and readable, not silently lost. The queue is bounded because an
//   unbounded one is a memory leak with a security-shaped name; when it fills, the count of dropped
//   events goes into the next line that is delivered, so the gap is in the record rather than in
//   somebody's assumptions.

use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// How many events may be waiting to be delivered.
///
/// A collector that is down for a minute at a few requests per second is well inside this, and a
/// collector that is down for an hour is a reason to forget events rather than to grow without limit.
/// How long a collector gets to accept a connection before the event is counted as dropped.
///
/// Two minutes is the kernel's own SYN retry schedule, which is the behaviour this replaces.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

const QUEUE_DEPTH: usize = 4096;

/// What the forwarder has dropped since it started, so the loss is countable.
#[derive(Default)]
struct Counters {
    /// Drops that have not yet been reported to the collector. The gap note carries this and then
    /// clears it, and a failed post puts it back.
    dropped: u64,
    /// Every drop since the forwarder started. Never cleared.
    ///
    /// It exists because one field was answering two different questions. `stats()` is what an
    /// operator reads and what the test below asserts on - "how many events have I lost" - while the
    /// gap note needs "how many have I lost since the last line that went out". The consumer clears
    /// the lending counter before posting, so `stats()` returned 0 for events that were gone: a loss
    /// counter that reports itself healthy after doing its job. Drops were exactly the events that
    /// never reached the collector, and the number meant to make that visible said none were lost.
    dropped_total: u64,
    delivered: u64,
    failed: u64,
}

/// A handle a request path uses to hand an event to the sender.
///
/// Cloneable and cheap: `record` takes one of these and a format function, and the thread doing the
/// work never waits for the network.
#[derive(Clone)]
pub struct Forwarder {
    tx: SyncSender<String>,
    counters: Arc<Mutex<Counters>>,
}

impl Forwarder {
    /// Start a forwarder posting to `url`.
    ///
    /// `token`, when given, is sent as a bearer token. It is not put in the URL: a URL is logged by
    /// every proxy on the way, and a credential in a log is a credential that has to be rotated.
    pub fn start(url: String, token: Option<String>) -> Forwarder {
        let (tx, rx) = sync_channel::<String>(QUEUE_DEPTH);
        let counters = Arc::new(Mutex::new(Counters::default()));
        let c = Arc::clone(&counters);

        std::thread::spawn(move || {
            // Events dropped since the last line that reached the collector. The count lived in a
            // counter nothing read (#73): the header promised the gap would appear in the record and
            // no gap ever did, so an outage left a history that looked complete and was not.
            let mut carried: u64 = 0;
            while let Ok(body) = rx.recv() {
                // Take the count at delivery time. Anything dropped since the last attempt belongs
                // to this line, whether or not this line gets through.
                if let Ok(mut n) = c.lock() {
                    carried += n.dropped;
                    n.dropped = 0;
                }
                let with_note = if carried > 0 {
                    format!(
                        "{body}\n<<gesta audit gap: {carried} event(s) were dropped while the collector was unreachable>>"
                    )
                } else {
                    body
                };
                match post(&url, token.as_deref(), &with_note) {
                    Ok(()) => {
                        if let Ok(mut n) = c.lock() {
                            n.delivered += 1;
                        }
                        // The gap reached the collector inside this line.
                        carried = 0;
                    }
                    Err(e) => {
                        // The error goes to stderr once per failure rather than per retry: a collector
                        // that refuses everything would otherwise fill the operator's terminal and
                        // hide the requests the server is still serving correctly.
                        eprintln!("audit forward failed: {}", e);
                        if let Ok(mut n) = c.lock() {
                            n.failed += 1;
                            // This line never arrived, so neither did its note. Back in the counter,
                            // to be added to the next line - otherwise the gap disappears with the
                            // failure that created it.
                            n.dropped += carried;
                        }
                        carried = 0;
                    }
                }
            }
        });

        Forwarder { tx, counters }
    }

    /// Build a forwarder from what this repository is configured with, if anything.
    ///
    /// Two files rather than flags, so a collector that moves is a file edit: `.gesta/audit-url`
    /// holds the URL and `.gesta/audit-token` the bearer token. They are separate because a URL is
    /// logged by every proxy on the way and a credential in a log has to be rotated.
    ///
    /// Returns `Ok(None)` when no URL is configured, which is the ordinary case for a local
    /// repository and is not an error.
    pub fn from_repo(root: &std::path::Path) -> crate::ZResult<Option<Forwarder>> {
        let dir = crate::repo_dir(root);
        let url = match std::fs::read_to_string(dir.join("audit-url")) {
            Ok(u) if !u.trim().is_empty() => u.trim().to_string(),
            _ => return Ok(None),
        };
        let token = std::fs::read_to_string(dir.join("audit-token"))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Ok(Some(Forwarder::start(url, token)))
    }

    /// Hand one line to the forwarder.
    ///
    /// Returns false when the queue was full, so the caller can count it. Nothing blocks: a request
    /// that is being served must not wait for a collector.
    pub fn send(&self, line: &str) -> bool {
        match self.tx.try_send(line.to_string()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                if let Ok(mut n) = self.counters.lock() {
                    n.dropped += 1;
                    n.dropped_total += 1;
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                if let Ok(mut n) = self.counters.lock() {
                    n.dropped += 1;
                    n.dropped_total += 1;
                }
                false
            }
        }
    }

    /// What has been delivered, refused and dropped.
    ///
    /// Read by the tests and by an operator with `--json`: a queue that dropped events and never says
    /// so is a record with a hole in it that looks complete.
    #[allow(dead_code)]
    pub fn stats(&self) -> (u64, u64, u64) {
        match self.counters.lock() {
            Ok(n) => (n.delivered, n.failed, n.dropped_total),
            Err(_) => (0, 0, 0),
        }
    }
}

/// A minimal HTTP POST. Deliberately not the project's client: the audit path must not depend on the
/// TLS stack, and a collector on a private network is reached over plain HTTP behind that network.
fn post(url: &str, token: Option<&str>, body: &str) -> Result<(), String> {
    use std::io::{Read, Write};

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("audit url must start with http://: {}", url))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|e| format!("bad port: {}", e))?),
        None => (host_port, 80),
    };

    // A timeout on the connect, not only on the reads and writes (#75). `TcpStream::connect` blocks
    // for the kernel's SYN retry schedule - around two minutes on Linux - so a collector that
    // black-holes the SYN stalls the drain for that long per event, while the read and write
    // timeouts set below never come into play because there is no stream yet. The audit path is on
    // the critical path of every recorded decision, so an unreachable collector must be given up on
    // in seconds and counted as dropped, which is what the gap note exists to report.
    let addr = format!("{}:{}", host, port);
    let sock = std::net::ToSocketAddrs::to_socket_addrs(&addr)
        .map_err(|e| format!("resolve {}: {}", addr, e))?
        .next()
        .ok_or_else(|| format!("resolve {}: no address", addr))?;
    let mut stream = std::net::TcpStream::connect_timeout(&sock, CONNECT_TIMEOUT)
        .map_err(|e| format!("connect {}: {}", addr, e))?;
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));

    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n",
        path, host_port, body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    req.push_str("\r\n");
    req.push_str(body);

    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {}", e))?;
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    // Only the status line matters. A collector that answered 200 has the line; anything else is a
    // delivery failure the operator needs to see.
    if reply.starts_with("HTTP/1.1 2") || reply.starts_with("HTTP/1.0 2") {
        Ok(())
    } else {
        Err(format!(
            "collector answered: {}",
            reply.lines().next().unwrap_or("nothing")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A collector that accepts one request and answers 200.
    fn one_shot_collector() -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            text
        });
        (format!("http://127.0.0.1:{}/audit", port), handle)
    }

    use std::io::Write;

    #[test]
    fn an_event_reaches_the_collector() {
        let (url, handle) = one_shot_collector();
        let f = Forwarder::start(url, Some("t".to_string()));
        assert!(
            f.send("{\"decision\":\"no-token\"}\n"),
            "the queue should accept it"
        );
        // The collector thread returns what it read, so the assertion is about the bytes on the wire
        // rather than about our own return value.
        let seen = handle.join().unwrap();
        assert!(seen.contains("POST /audit"), "request line: {}", seen);
        assert!(
            seen.contains("Authorization: Bearer t"),
            "auth header: {}",
            seen
        );
        assert!(seen.contains("no-token"), "body: {}", seen);
    }

    #[test]
    fn a_collector_that_is_down_does_not_block_the_caller() {
        // A port nobody is listening on. The send must return immediately and the queue must accept
        // the event; the failure is counted, not raised.
        let f = Forwarder::start("http://127.0.0.1:1/audit".to_string(), None);
        let started = std::time::Instant::now();
        for _ in 0..50 {
            let _ = f.send("{\"a\":1}\n");
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "50 sends took {:?}, which means the request path waited on the network",
            started.elapsed()
        );
    }

    #[test]
    fn a_full_queue_is_counted_rather_than_grown() {
        // A collector that never answers, and more events than the queue holds.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let f = Forwarder::start(format!("http://127.0.0.1:{}/audit", port), None);
        let mut accepted = 0;
        for _ in 0..(QUEUE_DEPTH + 200) {
            if f.send("{\"a\":1}\n") {
                accepted += 1;
            }
        }
        let (_, _, dropped) = f.stats();
        assert!(
            accepted <= QUEUE_DEPTH + 1,
            "accepted {} which is past the depth",
            accepted
        );
        assert!(dropped > 0, "a full queue must be counted, not hidden");
        drop(listener);
    }
    /// A collector that fails the first `fail_first` requests, then answers 200 and keeps serving
    /// until it has seen `want_gap`, so the test does not depend on which request happens to be the
    /// one that carries the note. Every request it reads is returned, so a test can read what
    /// actually went over the wire.
    fn collector_until_gap(
        fail_first: usize,
        max_requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen: Vec<String> = Vec::new();
            for i in 0..max_requests {
                let (mut s, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let mut raw = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match s.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            raw.extend_from_slice(&chunk[..n]);
                            if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
                                let want: Option<usize> = head
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse().ok());
                                if let Some(want) = want {
                                    if raw.len() - (split + 4) >= want {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                if i < fail_first {
                    let _ = s.write_all(b"HTTP/1.1 500 Nope\r\nContent-Length: 0\r\n\r\n");
                } else {
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                }
                let carries_gap = text.contains("gesta audit gap");
                seen.push(text);
                // Stop once the gap has been delivered and acknowledged, so the test can join.
                if carries_gap && i >= fail_first {
                    break;
                }
            }
            seen
        });
        (format!("http://127.0.0.1:{}/audit", port), handle)
    }

    #[test]
    fn a_dropped_event_is_reported_in_the_next_line_that_is_delivered() {
        // #73: `Counters.dropped` was incremented and nothing read it, so a collector outage lost
        // audit events that were counted in a process nobody could ask. The header promised the
        // count "goes into the next line that is delivered" and no line ever carried it.
        //
        // The collector must be given a real chance to see the note: it fails the first request, and
        // the queue holds thousands of events, so which request carries the note depends on timing.
        // The first version of this test assumed the second request was the one and failed 2 runs in
        // 3; asserting on `seen.last()` was asserting on a race.
        let fail_first = 1;
        let (url, handle) = collector_until_gap(fail_first, 64);
        let f = Forwarder::start(url, None);

        let mut accepted = 0usize;
        let mut dropped = 0usize;
        for _ in 0..(QUEUE_DEPTH + 8) {
            if f.send("{\"decision\":\"no-token\"}") {
                accepted += 1;
            } else {
                dropped += 1;
            }
        }
        assert!(
            dropped > 0,
            "the queue must overflow, or this tests nothing"
        );
        // Wait for the consumer to have taken at least one line, then read the counter.
        //
        // Without this the assertion is a race and it reads as a proof. The consumer clears the
        // lending counter as it takes each line, so with the two jobs sharing one field the answer
        // depended on whether the consumer happened to have run yet: 20 out of 20 runs passed on the
        // buggy code in a plain build, and the coverage build (slower, instrumented) failed every
        // time. A test that only fails on one build is not measuring the code, it is measuring the
        // schedule. Once a line has been taken, the lending counter is zero either way, so `stats()`
        // has to be reading something that was never lent out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while f.stats().0 + f.stats().1 == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            f.stats().0 + f.stats().1 > 0,
            "the consumer never took a line, so this test cannot tell the two counters apart"
        );
        assert_eq!(
            f.stats().2,
            dropped as u64,
            "the counter must agree: {dropped} send(s) were refused, and a loss counter that \
             reports zero after the loss has been delivered makes the record look complete"
        );
        assert_eq!(
            accepted + dropped,
            QUEUE_DEPTH + 8,
            "every send is either accepted or counted as dropped, never both and never neither"
        );

        let seen = handle.join().unwrap();
        // The note that counts is one the collector ACCEPTED, and this asserted on whichever request
        // carried a note first - including the one answered with 500.
        //
        // The forwarder's rule is that a note whose line fails to arrive goes back into the counter
        // for the next line, so an unaccepted note is not a delivered one. On the Windows runner the
        // first request carried a note, `find` returned it, and its count (1, all that had been
        // dropped at that moment) was compared against `dropped` read after every send (6). The
        // forwarder was right and the test was wrong.
        //
        // Even an accepted note cannot be compared for equality against `dropped`: the collector
        // stops serving once a note is delivered, so the note carries what had been lost by the time
        // that line went out, not the total after every send. Equality would assert that the consumer
        // had drained before the note left, which nothing guarantees. The promise in the header is
        // that the gap is reported with a count bounded by what was actually lost.
        let with_gap = seen
            .iter()
            .enumerate()
            .filter(|(i, _)| *i >= fail_first)
            .map(|(_, r)| r)
            .find(|r| r.contains("gesta audit gap"))
            .unwrap_or_else(|| {
                panic!(
                    "no accepted request among {} carried the gap note; first was: {}",
                    seen.len(),
                    seen.first().map(|s| s.as_str()).unwrap_or("<none>")
                )
            });
        let announced: u64 = with_gap
            .split("gesta audit gap:")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("the note must announce a count, got: {with_gap}"));
        assert!(
            announced >= 1,
            "a note reporting zero is not reporting the gap: {with_gap}"
        );
        assert!(
            announced <= dropped as u64,
            "the note claims {announced} were dropped, but only {dropped} ever were: {with_gap}"
        );
    }
    #[test]
    fn a_collector_that_never_answers_is_given_up_on_in_seconds() {
        // #75: `TcpStream::connect` has no timeout of its own and blocks for the kernel's SYN retry
        // schedule - around two minutes on Linux - so a collector that black-holes the SYN stalls the
        // drain for that long per event. The read and write timeouts set afterwards never come into
        // play because there is no stream yet.
        //
        // 203.0.113.0/24 is TEST-NET-3: reserved for documentation, never routed, so a connect there
        // hangs rather than being refused, which is the shape this needs. A refused connection would
        // return immediately and prove nothing.
        let started = std::time::Instant::now();
        let err = post("http://203.0.113.1:9/audit", None, "{}\n").unwrap_err();
        let took = started.elapsed();
        assert!(
            err.contains("connect"),
            "the failure must be the connect, got: {err}"
        );
        assert!(
            took < std::time::Duration::from_secs(10),
            "giving up took {:.1}s; without CONNECT_TIMEOUT the kernel retries for about two \
             minutes, which stalls every recorded decision behind an unreachable collector",
            took.as_secs_f32()
        );
    }
}
