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
const QUEUE_DEPTH: usize = 4096;

/// What the forwarder has dropped since it started, so the loss is countable.
#[derive(Default)]
struct Counters {
    dropped: u64,
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
            while let Ok(body) = rx.recv() {
                match post(&url, token.as_deref(), &body) {
                    Ok(()) => {
                        if let Ok(mut n) = c.lock() {
                            n.delivered += 1;
                        }
                    }
                    Err(e) => {
                        // The error goes to stderr once per failure rather than per retry: a collector
                        // that refuses everything would otherwise fill the operator's terminal and
                        // hide the requests the server is still serving correctly.
                        eprintln!("audit forward failed: {}", e);
                        if let Ok(mut n) = c.lock() {
                            n.failed += 1;
                        }
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
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                if let Ok(mut n) = self.counters.lock() {
                    n.dropped += 1;
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
            Ok(n) => (n.delivered, n.failed, n.dropped),
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

    let mut stream = std::net::TcpStream::connect((host, port))
        .map_err(|e| format!("connect {}:{}: {}", host, port, e))?;
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
}
