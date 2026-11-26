//! Minimal HTTP/1.1 transport so a repository can be served and
//! reached over a network.
//!
//! Written on `std::net` on purpose: no TLS stack, no async runtime, no
//! dependency that could pull in an unexpected C library. The protocol is small
//! and fixed, so the parsing surface is small and testable.
//!
//! Endpoints (all under `/v1`):
//!   GET  /v1/health                     -> `ok`
//!   GET  /v1/branches                   -> one `name<TAB>head` per line
//!   GET  /v1/bundle?branch=X&have=a,b   -> zlib bundle, octet-stream
//!   POST /v1/bundle?branch=X            -> body is a zlib bundle; 409 if it
//!                                          would move the branch backwards

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::store::{ancestors, branch_head, init_schema, open_db};
use crate::transport::auth::{is_loopback, Auth, Role};
use crate::transport::remote::{self, Bundle};
use crate::{bad, ZResult};

/// Refuse bodies larger than this rather than reading until memory runs out.
const MAX_BODY: usize = 512 * 1024 * 1024;

/// The most a whole response may occupy, headers included. The body cap plus room for a head that
/// names the body, so a server cannot exhaust the client with a head alone.
const MAX_RESPONSE: usize = MAX_BODY + 64 * 1024;

/// Read to the end, but never past `limit`, and refuse a response that goes over it.
///
/// `read_to_end` grows until the peer stops, which lets a server that never ends its body exhaust
/// the client's memory (#154). Reading one chunk past the limit is deliberate: it is how "bigger
/// than the cap" is told apart from "exactly the cap".
fn read_capped(stream: &mut dyn Read, raw: &mut Vec<u8>, limit: usize) -> std::io::Result<()> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the response exceeds the limit of {} bytes", limit),
            ));
        }
    }
}

/// A parsed request line plus headers and body.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Raw `Authorization` header value, if the client sent one.
    pub authorization: Option<String>,
    /// Raw `Git-Protocol` header value. A v2 client sends `version=2` on every request,
    /// and that header is the only way to tell a v2 body from a v0 one before parsing it.
    pub git_protocol: Option<String>,
}

impl Request {
    /// The body as UTF-8 text, or `None` when it is not valid UTF-8.
    pub fn body_text(&self) -> Option<String> {
        String::from_utf8(self.body.clone()).ok()
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok_text(s: impl Into<String>) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "text/plain; charset=utf-8",
            body: s.into().into_bytes(),
        }
    }

    pub fn ok_json(v: crate::support::json::Json) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "application/json; charset=utf-8",
            body: v.render().into_bytes(),
        }
    }

    /// An error whose body is JSON, which is what the git-lfs calls expect: a client parses the body
    /// of a failed lock to learn who holds it, and a plain text body loses that.
    pub fn error_with_type(status: u16, reason: &'static str, body: Vec<u8>) -> Response {
        Response {
            status,
            reason,
            content_type: "application/vnd.git-lfs+json",
            body,
        }
    }

    /// A binary answer with the content type its consumer requires. A git client refuses a
    /// response whose type is not the one it asked for, and reports it as a protocol error.
    pub fn ok_with_type(b: Vec<u8>, content_type: &'static str) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type,
            body: b,
        }
    }

    pub fn ok_binary(b: Vec<u8>) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "application/octet-stream",
            body: b,
        }
    }

    pub fn error(status: u16, reason: &'static str, msg: impl Into<String>) -> Response {
        Response {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: msg.into().into_bytes(),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        // A 401 without this header tells a browser or git to give up rather than to try. Git sends
        // no credentials on the first request by design: it waits to be asked.
        if self.status == 401 {
            head.push_str("WWW-Authenticate: Basic realm=\"gesta\"\r\n");
        }
        head.push_str("Cache-Control: no-store\r\n\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Parsed request line: method, path, query, headers, and bytes consumed.
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub consumed: usize,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse the request line and headers from a raw buffer.
///
/// Returns the parsed request (body not yet read) and the number of bytes
/// consumed by the header block.
pub fn parse_head(buf: &[u8]) -> ZResult<RequestHead> {
    let end = find_subslice(buf, b"\r\n\r\n")
        .ok_or_else(|| bad("malformed request: no end of headers"))?;
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| bad("headers are not UTF-8"))?;

    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or_else(|| bad("empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| bad("missing method"))?
        .to_string();
    let target = parts.next().ok_or_else(|| bad("missing path"))?.to_string();

    if method.len() > 16 || target.len() > 8192 {
        return Err(bad("request line too long"));
    }

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target, Vec::new()),
    };

    Ok(RequestHead {
        method,
        path,
        query,
        headers,
        consumed: end + 4,
    })
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((url_decode(k), url_decode(v)));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// A remote address: `http://host:port` parsed into its parts.
#[derive(Debug)]
pub struct HttpUrl {
    pub host: String,
    pub port: u16,
    pub base_path: String,
    /// True for `https://`. The scheme decides both the transport and the
    /// default port, so it is carried in the parsed form rather than re-tested.
    pub tls: bool,
}

pub fn parse_http_url(url: &str) -> ZResult<HttpUrl> {
    let (rest, tls, default_port) = if let Some(r) = url.strip_prefix("https://") {
        (r, true, 443u16)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, false, 80u16)
    } else {
        return Err(bad(format!(
            "not an http or https url: {} (expected a scheme)",
            url
        )));
    };
    let (authority, base_path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| bad(format!("bad port in url: {}", p)))?,
        ),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(bad("empty host in url"));
    }
    Ok(HttpUrl {
        host,
        port,
        base_path,
        tls,
    })
}

pub fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Same as `request_with_token`, plus a certificate pin for a private server.
///
/// `pin` is the SHA-256 fingerprint a server printed when it started. Without
/// one, the certificate is verified against the public roots, which is the only
/// safe default for a host reached over the internet.
pub fn request_with_pin(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    token: Option<&str>,
    #[allow(unused_variables)] pin: Option<&str>,
) -> ZResult<(u16, Vec<u8>)> {
    let u = parse_http_url(url)?;
    let timeout = Duration::from_secs(120);
    let head = build_request_head(method, &u, body, token);

    // Both transports run the same request bytes and the same response parser;
    // only the socket differs.
    let mut raw: Vec<u8> = Vec::new();
    if u.tls {
        #[cfg(not(feature = "tls"))]
        return Err(bad(
            "this build has no TLS support: rebuild with --features tls, or use http:// on a trusted network",
        ));
        #[cfg(feature = "tls")]
        {
            let mut stream = crate::transport::tls::connect(&u.host, u.port, pin, timeout)?;
            stream.write_all(head.as_bytes())?;
            if let Some(b) = body {
                stream.write_all(b)?;
            }
            stream.flush()?;
            if let Err(e) = read_capped(&mut stream, &mut raw, MAX_RESPONSE) {
                // A server that closes without close_notify leaves a complete
                // response that rustls refuses to hand over quietly. If bytes
                // arrived and the document is whole, say so instead of failing.
                let complete = find_subslice(&raw, b"\r\n\r\n").is_some();
                if !(complete && is_eof_error(&e)) {
                    return Err(bad(format!("reading the response failed: {}", e)));
                }
            }
        }
    } else {
        let mut stream = TcpStream::connect((u.host.as_str(), u.port))
            .map_err(|e| bad(format!("cannot connect to {}:{}: {}", u.host, u.port, e)))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(head.as_bytes())?;
        if let Some(b) = body {
            stream.write_all(b)?;
        }
        stream.flush()?;
        read_capped(&mut stream, &mut raw, MAX_RESPONSE)?;
    }

    let split =
        find_subslice(&raw, b"\r\n\r\n").ok_or_else(|| bad("response has no end of headers"))?;
    let head_text =
        std::str::from_utf8(&raw[..split]).map_err(|_| bad("response head not UTF-8"))?;
    let status = head_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| bad("response has no status code"))?;

    let payload = raw[split + 4..].to_vec();
    let payload = complete_body(head_text, payload)?;
    Ok((status, payload))
}

/// Assemble the request header block for either transport.
/// Is this the "peer closed without close_notify" case?
#[cfg(feature = "tls")]
fn is_eof_error(e: &std::io::Error) -> bool {
    let text = e.to_string();
    text.contains("close_notify") || text.contains("UnexpectedEof")
}

fn build_request_head(
    method: &str,
    u: &HttpUrl,
    body: Option<&[u8]>,
    token: Option<&str>,
) -> String {
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: */*\r\n",
        method,
        if u.base_path.is_empty() {
            "/".to_string()
        } else {
            u.base_path.clone()
        },
        u.host
    );
    if let Some(tok) = token {
        head.push_str("Authorization: Bearer ");
        head.push_str(tok);
        head.push_str("\r\n");
    }
    if let Some(b) = body {
        head.push_str(&format!(
            "Content-Type: application/octet-stream\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("\r\n");
    head
}

/// How many connections are handled at once.
///
/// A thread per connection with no ceiling is an outage waiting for a caller: a few hundred
/// sockets that open and never speak cost a few hundred threads, and the server that was meant to
/// answer a team stops answering anyone.
const MAX_CONNECTIONS: usize = 128;

/// Run `work` on a thread, at most `MAX_CONNECTIONS` at a time.
///
/// The slot is taken before the spawn and given back when the work returns, so a worker that is
/// slow holds its slot for as long as it runs. A caller over the ceiling waits here rather than
/// being dropped: the socket is already accepted, and refusing it would turn a busy moment into a
/// client error.
fn spawn_bounded<F>(slots: Arc<std::sync::Mutex<usize>>, work: F)
where
    F: FnOnce() + Send + 'static,
{
    loop {
        {
            let mut n = slots.lock().unwrap_or_else(|e| e.into_inner());
            if *n < MAX_CONNECTIONS {
                *n += 1;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    std::thread::spawn(move || {
        work();
        let mut n = slots.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
    });
}

/// Serve `root` over HTTP until the process is killed.
/// Serve `root` over TLS until the process is killed.
///
/// The request and response handling is shared with the plaintext server: only
/// the socket differs. A repository served with a certificate is meant to leave
/// loopback, so a bind without tokens is still refused.
#[cfg(feature = "tls")]
pub fn serve_tls(
    root: PathBuf,
    bind: String,
    port: u16,
    auth: Auth,
    cert: &Path,
    key: &Path,
) -> ZResult<()> {
    // #171: refuse before anything observable happens. The bind used to come first, so a refused
    // start printed "gesta listening on https://..." for an address it never served and held the
    // port for as long as the certificate work took. Nothing the caller can see should exist when
    // the refusal is the answer.
    if !auth.required {
        return Err(bad(format!(
            "refusing to serve TLS on {} without tokens: add entries to .gesta/auth",
            bind
        )));
    }

    let cfg = crate::transport::tls::server_config(cert, key)?;
    let listener = TcpListener::bind((bind.as_str(), port))
        .map_err(|e| bad(format!("cannot bind {}:{}: {}", bind, port, e)))?;
    let local = listener.local_addr()?;
    println!("gesta listening on https://{}", local);
    println!("certificate: {}", cert.display());
    println!(
        "fingerprint: {}",
        crate::transport::tls::fingerprint_of(cert)?
    );
    println!("repository: {}", root.display());
    println!("access: {} token(s) configured", auth.token_count());
    println!("press Ctrl-C to stop");

    let auth_path = Arc::new(crate::repo_dir(&root).join("auth"));
    let root = Arc::new(root);
    let auth = Arc::new(std::sync::Mutex::new(auth));
    let slots = Arc::new(std::sync::Mutex::new(0usize));
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        let root = Arc::clone(&root);
        let auth = Arc::clone(&auth);
        let auth_path = Arc::clone(&auth_path);
        let cfg = Arc::clone(&cfg);
        let slots = Arc::clone(&slots);
        spawn_bounded(slots, move || {
            // A TLS client that finishes the handshake and then goes silent held a worker for as
            // long as the socket lived: the plaintext path set a read timeout and this one did not,
            // so the encrypted listener was the easier one to exhaust.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(120)));
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(120)));
            match crate::transport::tls::accept(stream, cfg) {
                Ok(tls) => {
                    // TLS serving does not forward audit lines yet: the collector is reached over plain HTTP
                    // on a private network, and a TLS listener is a different deployment.
                    let _ = handle_tls_connection(tls, &root, &auth, &auth_path, None);
                }
                Err(e) => eprintln!("tls handshake failed: {}", e),
            }
        });
    }
    Ok(())
}

#[cfg(feature = "tls")]
fn handle_tls_connection(
    mut stream: crate::transport::tls::TlsStream,
    root: &Path,
    auth: &std::sync::Mutex<Auth>,
    auth_path: &Path,
    forward: Option<&crate::support::audit_forward::Forwarder>,
) -> ZResult<()> {
    let resp = match read_request_generic(&mut stream) {
        Ok(req) => {
            // Reloaded in place, under the lock: a copy that reloads itself and is then dropped
            // forgets what it learned, and the next request starts from the table read at startup.
            let snapshot = match auth.lock() {
                Ok(mut g) => {
                    g.reload_if_changed(auth_path);
                    g.clone()
                }
                Err(poisoned) => {
                    let mut g = poisoned.into_inner();
                    g.reload_if_changed(auth_path);
                    g.clone()
                }
            };
            route(root, &req, &snapshot, forward)
        }
        Err(e) => Response::error(400, "Bad Request", e.to_string()),
    };
    stream.write_all(&resp.to_bytes())?;
    stream.flush()?;
    // A dropped TLS stream looks like a truncated response to the peer, so the
    // session is closed explicitly before the socket goes away.
    stream.close()?;
    Ok(())
}

pub fn serve(
    root: PathBuf,
    bind: String,
    port: u16,
    auth: Auth,
    forward: Option<crate::support::audit_forward::Forwarder>,
) -> ZResult<()> {
    // The auth file, so a token removed while the server runs stops working without a restart.
    // Built here rather than taken as an argument: the caller already read this file to build the
    // Auth, and passing the path as well would let the two disagree.
    let auth_path = crate::repo_dir(&root).join("auth");
    // The policy is decided before the socket exists. Binding first printed "listening on
    // http://0.0.0.0:8899" and then refused, so the one line a person reads said the server was up
    // when it was about to exit, and the port was open for the length of that message.
    if !auth.required && !is_loopback(&bind) {
        return Err(bad(format!(
            "refusing to serve {} without tokens: add entries to .gesta/auth, or bind 127.0.0.1",
            bind
        )));
    }

    let listener = TcpListener::bind((bind.as_str(), port))
        .map_err(|e| bad(format!("cannot bind {}:{}: {}", bind, port, e)))?;
    let local = listener.local_addr()?;
    println!("gesta listening on http://{}", local);
    println!("repository: {}", root.display());
    if auth.required {
        println!("access: {} token(s) configured", auth.token_count());
    } else {
        println!("access: open, loopback only");
    }
    if forward.is_some() {
        println!("audit: forwarding to the configured collector");
    }
    println!("press Ctrl-C to stop");

    // The repository is opened per request: SQLite in WAL mode handles
    // concurrent readers, and a long-lived Connection would need a mutex.
    let auth_path = Arc::new(auth_path);
    let root = Arc::new(root);
    let auth = Arc::new(std::sync::Mutex::new(auth));
    // Shared rather than moved: every connection thread hands its events to the same queue, and the
    // forwarder is Clone for exactly this shape.
    let forward = Arc::new(forward);

    // The socket keeps its blocking accept, on a thread of its own, and the main thread only waits
    // for a stop. Two other shapes were tried and both were wrong:
    //
    //  - A non-blocking socket polled with a 100 ms sleep, checking the flag first. Every request
    //    then waited for the remainder of the interval: measured 101.2 ms median, 101.5 p90, which
    //    is the interval itself. A server that adds a tenth of a second to every request is worse
    //    than one that cannot be stopped.
    //  - The same, checking the flag only after an accept attempt. Idle it still slept, so a
    //    connection arriving during the sleep waited for it.
    //
    // A thread costs nothing here because the process ends when the main thread returns, which
    // ends the accept thread with it. The accept thread never needs to be told to stop.
    crate::transport::signal::install();
    let accept_root = Arc::clone(&root);
    let accept_auth = Arc::clone(&auth);
    let accept_auth_path = Arc::clone(&auth_path);
    let accept_forward = Arc::clone(&forward);
    let accept_slots = Arc::new(std::sync::Mutex::new(0usize));
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                // A connection that died between the kernel and here must not end the server.
                Err(_) => continue,
            };
            let root = Arc::clone(&accept_root);
            let auth = Arc::clone(&accept_auth);
            let auth_path = Arc::clone(&accept_auth_path);
            let forward = Arc::clone(&accept_forward);
            let slots = Arc::clone(&accept_slots);
            spawn_bounded(slots, move || {
                let _ =
                    handle_connection(stream, &root, &auth, &auth_path, forward.as_ref().as_ref());
            });
        }
    });

    // Wait until a stop signal arrives, not for a fixed period and then return. The earlier version
    // passed `from_secs(86_400)`, so a server up for a day fell out of the wait, printed nothing and
    // returned `Ok(())`: a supervisor reading only the exit status sees a clean shutdown, does not
    // restart, and the port goes dark. The wait is factored out so the decision is testable without
    // waiting a day or a minute.
    crate::transport::signal::wait_until_stopped(crate::transport::signal::wait_for_stop);
    println!("stopping: a stop signal was received");
    Ok(())
}

/// The part of a path before `/info/lfs/`, so the batch href resolves to the same prefix the client
/// called. A client that asked `/x/repo.git/info/lfs/objects/batch` needs `/x/repo.git/info/lfs/...`,
/// and building the link from a fixed string loses whatever the repository is mounted under.
fn lfs_prefix(path: &str) -> String {
    match path.find("/info/lfs/") {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// The user name in an `Authorization` header, without verifying anything.
///
/// Used for attribution only, on routes that do not gate on a credential. A caller must never treat
/// this as an identity; `auth::authenticate_now` is the one that decides access.
///
/// The name is the part **before** the colon. `decode_basic_token` returns the part after it, because
/// every other caller wants the token; using it here made a lock held by `alice:x` report its owner as
/// `x`, and two different users who happened to send the same password looked like one person.
fn presented_name(header: &str) -> Option<String> {
    use crate::transport::remote::decode_base64_str;
    let rest = header.strip_prefix("Basic ")?;
    let raw = decode_base64_str(rest)?;
    let text = String::from_utf8(raw).ok()?;
    let (user, _token) = text.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some(user.to_string())
}

fn handle_connection(
    mut stream: TcpStream,
    root: &Path,
    auth: &std::sync::Mutex<Auth>,
    auth_path: &Path,
    forward: Option<&crate::support::audit_forward::Forwarder>,
) -> ZResult<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let resp = match read_request(&mut stream) {
        Ok(req) => {
            // The lock is held only to take a copy. Reloading under the lock and routing under it
            // would serialise every request behind a file read for no reason.
            let snapshot = match auth.lock() {
                Ok(mut g) => {
                    g.reload_if_changed(auth_path);
                    g.clone()
                }
                Err(poisoned) => {
                    let mut g = poisoned.into_inner();
                    g.reload_if_changed(auth_path);
                    g.clone()
                }
            };
            route(root, &req, &snapshot, forward)
        }
        Err(e) => Response::error(400, "Bad Request", e.to_string()),
    };
    stream.write_all(&resp.to_bytes())?;
    stream.flush()?;
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> ZResult<Request> {
    read_request_generic(stream)
}

/// The request reader, generic over the socket so TLS and plaintext share it.
fn read_request_generic<S: Read>(stream: &mut S) -> ZResult<Request> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    // Read until the header block is complete.
    let head_end = loop {
        if let Some(i) = find_subslice(&buf, b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 64 * 1024 {
            return Err(bad("headers too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(bad("connection closed before headers completed"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = parse_head(&buf[..head_end])?;
    debug_assert_eq!(head.consumed, head_end, "header parser and reader disagree");
    let authorization = head.header("authorization").map(|s| s.to_string());
    // A v2 client says which protocol it speaks on every request, including the advertisement.
    // Read here, beside the credential, and before `head` is destructured.
    let git_protocol = head.header("Git-Protocol").map(|v| v.to_string());
    let method = head.method;
    let path = head.path;
    let query = head.query;

    let head_text =
        std::str::from_utf8(&buf[..head_end]).map_err(|_| bad("headers are not UTF-8"))?;
    let want: usize = head_text
        .split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if want > MAX_BODY {
        return Err(bad(format!("body of {} bytes exceeds the limit", want)));
    }

    let mut body = buf[head_end..].to_vec();
    while body.len() < want {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(bad("connection closed mid-body"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(want);

    Ok(Request {
        method,
        path,
        query,
        body,
        authorization,
        git_protocol,
    })
}

/// Dispatch one request. Kept separate from socket code so routes and access
/// control can be tested without a network.
/// Which role a route needs, or `None` when it needs no credential at all.
///
/// Pulled out of `route` so the answer can be asserted without a socket. The test that guarded this
/// before asked only for a status of 400 or more, which a 404 for a path that is not routed also
/// satisfies, so it passed with the LFS arm deleted: a check that cannot fail on the wrong code is
/// not worth having.
fn required_role(method: &str, path: &str) -> Option<Role> {
    match (method, path) {
        ("GET", "/v1/health") => None,
        (_, p) if p.starts_with("/v1/bundle") && method == "POST" => Some(Role::Write),
        (_, "/v1/changeset") if method == "POST" => Some(Role::Write),
        (_, "/v1/changeset") if method == "GET" || method == "HEAD" => Some(Role::Read),
        // A git client sends a path it was given, not one of ours, and it authenticates with Basic
        // while our tokens use Bearer. Both are read, because a clone that cannot authenticate is a
        // clone nobody finishes.
        // A push writes, so the advertisement for receive-pack and the POST that follows it both
        // need Write. Serving the advertisement to a read-only token would let a client start a
        // push it cannot finish.
        (_, p) if p.contains("/info/refs") => {
            let service = p.split("service=").nth(1).unwrap_or("");
            if service.starts_with("git-receive-pack") {
                Some(Role::Write)
            } else {
                Some(Role::Read)
            }
        }
        (_, p) if p.ends_with("/git-receive-pack") => Some(Role::Write),
        (_, p) if p.ends_with("/git-upload-pack") => Some(Role::Read),
        // LFS downloads objects and LFS locks stop two clients writing the same asset, so a write
        // token is what the whole surface needs. It used to fall through to `_ => None`, which the
        // matcher reads as "no credential required": a server with tokens configured served every
        // stored asset and answered every lock request to an anonymous caller, while the same
        // repository over the JSON routes refused it.
        (_, p) if p.contains("/info/lfs/") => Some(Role::Write),
        (_, p) if p.starts_with("/v1/") => Some(Role::Read),
        _ => None,
    }
}

pub fn route(
    root: &Path,
    req: &Request,
    auth: &Auth,
    forward: Option<&crate::support::audit_forward::Forwarder>,
) -> Response {
    let path = req.path.trim_end_matches('/');

    // Health is open so a monitor can tell the process is alive without a
    // credential. Every other route needs one when the server has tokens.
    let needed = required_role(&req.method, path);

    let mut who = "anonymous".to_string();
    // A name for attribution, whether or not this route requires a credential. Locks are the reason:
    // the answer to "who is holding the 200 MB asset" is the whole point of a lock, and a server with
    // auth turned off still has to say something better than `anonymous` when a credential was sent.
    // The value is taken from the same header the auth path reads and is never used to grant access.
    if let Some(name) = req.authorization.as_deref().and_then(presented_name) {
        who = name;
    }
    let mut grant: Option<crate::transport::auth::Grant> = None;
    // Recorded once the decision is made. A request with no credential requirement is not an event
    // worth a line, but one that was authenticated is.
    let mut record_allowed = false;
    if let Some(needed) = needed {
        if auth.required {
            // Held in a local so the decoded token lives as long as this scope. The previous shape
            // ended the chain with `Box::leak(...)` to get a `&str` out of an owned `String`, which
            // leaked the credential into the process heap on every authenticated request: a
            // long-running server grew by one token per request, and the plaintext stayed readable
            // for the life of the process. A `String` binding has the same lifetime and no leak.
            let basic = req
                .authorization
                .as_deref()
                .and_then(|v| v.strip_prefix("Basic "))
                .and_then(decode_basic_token);
            let presented: Option<&str> = match req.authorization.as_deref() {
                Some(v) if v.starts_with("Bearer ") => v.strip_prefix("Bearer "),
                _ => basic.as_deref(),
            };
            let now = crate::support::clock::unix_now();
            match presented.map(|p| auth.authenticate_now(p, now)) {
                Some(crate::transport::auth::AuthOutcome::Granted(g))
                    if Auth::allows(g.role, needed) =>
                {
                    who = g.name.clone();
                    grant = Some(g);
                    record_allowed = true;
                }
                Some(crate::transport::auth::AuthOutcome::Expired(name)) => {
                    crate::support::audit::record_with(
                        root,
                        &crate::support::audit::Event {
                            who: &name,
                            decision: crate::support::audit::Decision::Expired,
                            action: req.method.as_str(),
                            target: path,
                            reason: "token expired",
                            remote: None,
                        },
                        forward,
                    );
                    return Response::error(
                        401,
                        "Unauthorized",
                        format!("token for {} expired", name),
                    );
                }
                Some(crate::transport::auth::AuthOutcome::Granted(g)) => {
                    // A real token asking for more than it has. The name is recorded because this is
                    // a misconfiguration worth finding, not an attack worth hiding.
                    crate::support::audit::record_with(
                        root,
                        &crate::support::audit::Event {
                            who: &g.name,
                            decision: crate::support::audit::Decision::WrongRole,
                            action: req.method.as_str(),
                            target: path,
                            reason: &format!("needs {:?}, token is {:?}", needed, g.role),
                            remote: None,
                        },
                        forward,
                    );
                    return Response::error(
                        403,
                        "Forbidden",
                        format!("token does not grant {:?} access", needed),
                    );
                }
                None | Some(crate::transport::auth::AuthOutcome::Unknown) => {
                    // A short sleep blunts online guessing without needing state
                    // per client. It costs an attacker 200 ms per attempt.
                    // Recorded before the sleep, so a flood of guesses leaves one line each rather
                    // than a file that grows only after the attacker gives up.
                    crate::support::audit::record_with(
                        root,
                        &crate::support::audit::Event {
                            who: "anonymous",
                            decision: crate::support::audit::Decision::NoToken,
                            action: req.method.as_str(),
                            target: path,
                            reason: "missing or invalid token",
                            remote: None,
                        },
                        forward,
                    );
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    return Response::error(
                        401,
                        "Unauthorized",
                        "missing or invalid token: send Authorization: Bearer <token>",
                    );
                }
            }
        }
    }

    // Written after the auth decision and before the work, so a request that is about to run appears
    // in the record even if it then fails part way.
    if record_allowed {
        crate::support::audit::record_with(
            root,
            &crate::support::audit::Event {
                who: &who,
                decision: crate::support::audit::Decision::Allowed,
                action: req.method.as_str(),
                target: path,
                reason: "",
                remote: None,
            },
            forward,
        );
    }

    match (req.method.as_str(), path) {
        ("GET", "/v1/health") => Response::ok_text("ok"),
        ("GET", "/v1/branches") => match list_branches(root) {
            Ok(s) => Response::ok_text(s),
            Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
        },
        ("GET", "/v1/bundle") => {
            let branch = req.param("branch").unwrap_or("main");
            let have: HashSet<String> = req
                .param("have")
                .map(|h| {
                    h.split(',')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            // `depth=0` means the head alone, and a missing parameter means everything. Both have to
            // be told apart from `depth=0` being falsy.
            let depth = req
                .param("depth")
                .and_then(|d| d.trim().parse::<usize>().ok());
            match fetch_bundle_with_depth(root, branch, &have, depth) {
                Ok(Some(bytes)) => {
                    log_access(
                        root,
                        &who,
                        "bundle",
                        branch,
                        &format!("{} bytes", bytes.len()),
                    );
                    Response::ok_binary(bytes)
                }
                Ok(None) => Response::error(404, "Not Found", format!("no branch {}", branch)),
                Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
            }
        }
        // ---- git-lfs ----
        //
        // The batch call is how a client learns where to fetch a large object, and the object route is
        // what it fetches. Both sit under the same prefix so one auth decision covers them: a client
        // that may read the repository may read the objects its trees point at.
        //
        // The `href` is a path, not an absolute URL. `Request` carries no host and no scheme, and
        // inventing one from the path would produce a link that is wrong behind any proxy. A client
        // resolves a relative href against the endpoint it called, which is the correct answer by
        // construction.
        // ---- git-lfs locks ----
        //
        // The three calls git-lfs makes to hold a path: list, claim, release. They sit under the same
        // prefix as the objects so one auth decision covers the repository, and each one writes an
        // access row, because "who took the lock on the 200 MB asset" is the question these exist to
        // answer afterwards.
        ("GET", p) if p.ends_with("/info/lfs/locks") => {
            let conn = match open_db(root) {
                Ok(c) => c,
                Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
            };
            // `path` and `id` are the two filters the spec defines for this call, and they are
            // different columns. Passing the path filter as an owner is how this returned every lock
            // while reporting success; the filters stay separate here.
            //
            // `req.query` is the parsed query string, and it is the only place these filters exist:
            // `p` is the path with the query already trimmed, so reading `?path=` off it yields `None`
            // and the list comes back unfiltered while the response says 200. That is the third time
            // this endpoint produced a filter that silently did nothing.
            let param = |k: &str| -> Option<String> {
                req.query
                    .iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.clone())
            };
            let want_path = param("path");
            let want_id = param("id");
            let locks =
                crate::transport::lfs::list(&conn, None, want_path.as_deref(), want_id.as_deref());
            log_access(
                root,
                &who,
                "git-lfs-locks",
                p,
                &format!("{} listed", locks.len()),
            );
            let body = crate::transport::lfs::locks_json(&locks);
            Response::ok_with_type(body.into_bytes(), "application/vnd.git-lfs+json")
        }
        ("POST", p) if p.ends_with("/info/lfs/locks") => {
            let conn = match open_db(root) {
                Ok(c) => c,
                Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
            };
            let (path, refname) = match crate::transport::lfs::parse_lock_request(&req.body) {
                Some(v) => v,
                None => {
                    return Response::error(
                        422,
                        "Unprocessable Entity",
                        "a lock body must name a path".to_string(),
                    )
                }
            };
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            match crate::transport::lfs::lock(&conn, &path, &who, refname.as_deref(), at) {
                Ok(l) => {
                    log_access(root, &who, "git-lfs-lock", &path, "taken");
                    let body = format!("{{\"lock\":{}}}", crate::transport::lfs::one_json(&l));
                    Response::ok_with_type(body.into_bytes(), "application/vnd.git-lfs+json")
                }
                Err(msg) => {
                    // 409 with the holder named, which is what git-lfs reads to show the user who to
                    // ask. A plain 403 would lose that.
                    let holder = crate::transport::lfs::list(&conn, None, Some(&path), None)
                        .into_iter()
                        .find(|l| l.path == path);
                    let body = match holder {
                        Some(l) => format!(
                            "{{\"lock\":{},\"message\":{}}}",
                            crate::transport::lfs::one_json(&l),
                            crate::transport::lfs::quote(&msg)
                        ),
                        None => format!("{{\"message\":{}}}", crate::transport::lfs::quote(&msg)),
                    };
                    log_access(root, &who, "git-lfs-lock", &path, "refused");
                    Response::error_with_type(409, "Conflict", body.into_bytes())
                }
            }
        }
        ("POST", p) if p.ends_with("/info/lfs/locks/verify") => {
            let conn = match open_db(root) {
                Ok(c) => c,
                Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
            };
            // `verify` answers "which of these paths can I lock", and git-lfs calls it before a push
            // so it can refuse early. Our answer is the locks that exist, split the same way the spec
            // splits them: ours, and theirs.
            let locks = crate::transport::lfs::list(&conn, None, None, None);
            let (mine, theirs): (Vec<_>, Vec<_>) = locks.into_iter().partition(|l| l.owner == who);
            let body = crate::transport::lfs::verify_json(&mine, &theirs);
            Response::ok_with_type(body.into_bytes(), "application/vnd.git-lfs+json")
        }
        ("POST", p) if p.ends_with("/info/lfs/locks/unlock") || p.contains("/locks/unlock") => {
            let conn = match open_db(root) {
                Ok(c) => c,
                Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
            };
            let (id, force) = match crate::transport::lfs::parse_unlock(&req.body) {
                Some(v) => v,
                None => {
                    return Response::error(
                        422,
                        "Unprocessable Entity",
                        "an unlock body must name a lock id".to_string(),
                    )
                }
            };
            // `force` releases a lock held by somebody else, so it is a privilege and not a
            // preference: the caller may only assert it when the server actually knows who they are
            // (#153). Before this, `force` came straight out of the request body, so any caller could
            // release any lock - and on a server with auth off the holder name is self-asserted too,
            // which made every lock releasable by anyone.
            //
            // When `grant` is `None` the request was not authenticated at all: either the route does
            // not require it (auth off) or the token was absent, and `who` is then whatever the
            // client typed. Neither is an identity. Keep the ordinary path - an owner releasing their
            // own lock - and refuse the privileged one.
            // `force` releases a lock somebody else holds, so the question is who is asking (#153).
            //
            // Two cases, and the first version of this fix got them backwards. On a server with
            // tokens configured, `force` needs a credential that the server verified: without one,
            // `who` is a name the client typed and the audit line would name the victim. On a server
            // with auth off there is nothing to verify against, and `force` is how the single-user
            // workflow releases a stale lock - refusing it there breaks the feature to protect
            // nobody. `bench/lfs_locks.py` covers that second case and went red when I got this
            // wrong, which is the value of having the bench.
            let force = if auth.required {
                match grant.as_ref() {
                    None => {
                        return Response::error(
                            403,
                            "Forbidden",
                            "this server requires a token to release another client's lock"
                                .to_string(),
                        )
                    }
                    Some(g) if g.role == crate::transport::auth::Role::Read => {
                        return Response::error(
                            403,
                            "Forbidden",
                            "releasing another client's lock needs a write token".to_string(),
                        )
                    }
                    Some(_) => force,
                }
            } else {
                force
            };
            match crate::transport::lfs::unlock(&conn, &id, &who, force) {
                Ok(l) => {
                    log_access(root, &who, "git-lfs-unlock", &l.path, "released");
                    let body = format!("{{\"lock\":{}}}", crate::transport::lfs::one_json(&l));
                    Response::ok_with_type(body.into_bytes(), "application/vnd.git-lfs+json")
                }
                Err(msg) => Response::error(403, "Forbidden", msg),
            }
        }
        ("POST", p) if p.ends_with("/info/lfs/objects/batch") => {
            let (op, wanted) = match crate::transport::lfs::parse_batch(&req.body) {
                Some(v) => v,
                None => {
                    return Response::error(
                        422,
                        "Unprocessable Entity",
                        "a batch body must name an operation and its objects".to_string(),
                    )
                }
            };
            if op != "download" && op != "upload" {
                return Response::error(
                    422,
                    "Unprocessable Entity",
                    format!("this server does not serve the {op} operation"),
                );
            }
            // The answer comes from the store, never from what the request claimed. An oid the store
            // does not hold is reported as missing inside the batch, so a client learns the object is
            // gone instead of retrying a link that never worked.
            let mut results = Vec::new();
            let mut present = 0usize;
            let mut held = 0usize;
            for obj in &wanted {
                // An upload asks "do you need these bytes?" and the answer for an object the store
                // already holds is "no", with no actions block. That is a different answer from
                // Missing: a client that reads Missing for an upload retries forever, while
                // AlreadyThere tells it the transfer is done.
                let found = match crate::store::sidecar::read(root, &obj.oid) {
                    Some(bytes) => {
                        present += 1;
                        if op == "upload" {
                            held += 1;
                            crate::transport::lfs::Found::AlreadyThere
                        } else {
                            crate::transport::lfs::Found::Present {
                                size: bytes.len() as u64,
                            }
                        }
                    }
                    None if op == "upload" => crate::transport::lfs::Found::Missing,
                    None => crate::transport::lfs::Found::Missing,
                };
                results.push((obj.clone(), found));
            }
            let prefix = lfs_prefix(p);
            let body = crate::transport::lfs::batch_response(
                &results,
                |oid| format!("{prefix}/info/lfs/objects/{oid}"),
                "2099-01-01T00:00:00Z",
            );
            log_access(
                root,
                &who,
                "git-lfs-batch",
                p,
                &format!(
                    "{} asked, {present} present, {held} already held",
                    results.len()
                ),
            );
            Response::ok_with_type(body.into_bytes(), "application/vnd.git-lfs+json")
        }
        ("GET", p) if p.contains("/info/lfs/objects/") => {
            let oid = p.rsplit('/').next().unwrap_or("");
            if oid.len() != 64 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
                return Response::error(
                    400,
                    "Bad Request",
                    "an oid is 64 hex characters".to_string(),
                );
            }
            match crate::store::sidecar::read(root, oid) {
                Some(bytes) => {
                    log_access(
                        root,
                        &who,
                        "git-lfs-object",
                        p,
                        &format!("{} bytes", bytes.len()),
                    );
                    Response::ok_with_type(bytes, "application/octet-stream")
                }
                None => Response::error(404, "Not Found", format!("no object {oid}")),
            }
        }
        // ---- git smart HTTP ----
        //
        // `git clone http://host/git/repo` asks for `info/refs?service=git-upload-pack` first, and
        // then POSTs to `git-upload-pack`. Two requests, and a repository that answers the first and
        // not the second makes the client fail after it has already printed progress.
        // One block for both services. Two blocks where the first matched every `/info/refs` made the
        // second unreachable, and a push then read an upload-pack advertisement and failed with a
        // message about a service the server never got a chance to serve.
        ("GET", p) if p.contains("/info/refs") => {
            let service = req.param("service").unwrap_or("");
            match service {
                "git-upload-pack" => {
                    // A v2 client says so on every request, including this one. Answering with the v0
                    // ref list would leave it parsing refs as capabilities and failing with a protocol
                    // error that names nothing useful.
                    if crate::transport::protocol_v2::wants_v2(req.git_protocol.as_deref()) {
                        // `object-format=sha1` is what this server can actually serve, whatever the
                        // store's objects were imported as. `ls-refs` and the pack stream carry this
                        // tool's own changeset names (`cs` plus 12 hex characters), which are not git
                        // oids at all; claiming sha256 made a real client answer `invalid ls-refs
                        // response` because it parsed those names as 64-character object ids.
                        let bytes = crate::transport::protocol_v2::capability_advertisement(env!(
                            "CARGO_PKG_VERSION"
                        ));
                        log_access(
                            root,
                            &who,
                            "git-upload-pack-advert-v2",
                            p,
                            &format!("{} bytes", bytes.len()),
                        );
                        return Response::ok_with_type(
                            bytes,
                            "application/x-git-upload-pack-advertisement",
                        );
                    }
                    match crate::transport::gitserver::advertise(root) {
                        Ok(body) => {
                            log_access(
                                root,
                                &who,
                                "git-info-refs",
                                p,
                                &format!("{} bytes", body.len()),
                            );
                            Response::ok_with_type(
                                body,
                                "application/x-git-upload-pack-advertisement",
                            )
                        }
                        Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
                    }
                }
                "git-receive-pack" => match crate::transport::gitserver::advertise_receive(root) {
                    Ok(bytes) => {
                        log_access(
                            root,
                            &who,
                            "git-receive-pack-advert",
                            p,
                            &format!("{} bytes", bytes.len()),
                        );
                        Response::ok_with_type(
                            bytes,
                            "application/x-git-receive-pack-advertisement",
                        )
                    }
                    Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
                },
                other => Response::error(
                    403,
                    "Forbidden",
                    format!(
                        "service {} is not served here (git-upload-pack and git-receive-pack are)",
                        other
                    ),
                ),
            }
        }
        ("POST", p) if p.ends_with("/git-receive-pack") => {
            // The grant when the server has one. A server with no tokens has already decided the
            // request is allowed; a server with tokens refused an unknown one above.
            let check = grant.as_ref().map(|g| {
                let g = g.clone();
                move |branch: &str| g.permits_branch(branch)
            });
            let allowed: Option<&dyn Fn(&str) -> bool> =
                check.as_ref().map(|f| f as &dyn Fn(&str) -> bool);
            // The role and the branch scope are the two checks this route used to make. The path
            // scope is the third, passed through because only `receive_pack` knows which files the
            // pushed commits touch, and it can only answer that after the import.
            match crate::transport::gitserver::receive_pack_scoped(
                root,
                &req.body,
                allowed,
                grant.as_ref(),
                &who,
            ) {
                Ok(bytes) => {
                    log_access(
                        root,
                        &who,
                        "git-receive-pack",
                        p,
                        &format!("{} bytes", bytes.len()),
                    );
                    Response::ok_with_type(bytes, "application/x-git-receive-pack-result")
                }
                // A pack that could not be unpacked is the client's bytes, not our failure. The
                // message also goes to stderr: git prints only the status code, so a reason that
                // lives only in the body is a reason nobody reads.
                Err(e) => {
                    eprintln!("receive-pack refused: {}", e);
                    Response::error(400, "Bad Request", e.to_string())
                }
            }
        }
        ("POST", p) if p.ends_with("/git-upload-pack") => {
            // A v2 request names its command in the first section. The v0 path reads `want` lines
            // directly, and handing it a v2 body finds no wants and answers an empty pack, which
            // looks like a repository with no objects rather than a protocol mismatch.
            if crate::transport::protocol_v2::wants_v2(req.git_protocol.as_deref()) {
                let command = crate::transport::protocol_v2::command_of(&req.body);
                if command.as_deref() == Some("ls-refs") {
                    let served = match crate::transport::http::open_repo_for_serve(root) {
                        Ok(s) => s,
                        Err(e) => {
                            return Response::error(500, "Internal Server Error", e.to_string())
                        }
                    };
                    let head = if served.refs.iter().any(|(n, _)| n == "main") {
                        Some("main")
                    } else {
                        served.refs.first().map(|(n, _)| n.as_str())
                    };
                    match crate::transport::protocol_v2::ls_refs(&req.body, &served.refs, head) {
                        Ok(bytes) => {
                            log_access(
                                root,
                                &who,
                                "git-v2-ls-refs",
                                p,
                                &format!("{} bytes", bytes.len()),
                            );
                            Response::ok_with_type(bytes, "application/x-git-upload-pack-result")
                        }
                        Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
                    }
                } else if command.as_deref() == Some("fetch") {
                    // The v2 arguments are read here and handed to the same code that answers v0. A
                    // second implementation of reachability would be a second thing to keep in step,
                    // and the difference between the two requests is the framing, not the answer.
                    let (wants, haves, done) =
                        crate::transport::protocol_v2::parse_fetch(&req.body);

                    // A fetch that named haves and did not say `done` is a negotiation round: the
                    // client asks whether anything is shared and expects `acknowledgments`, not a
                    // pack. Answering with `packfile` makes it report `expected 'acknowledgments,
                    // received 'packfile'`, which names neither side's mistake.
                    if !haves.is_empty() && !done {
                        let known: Vec<String> =
                            match crate::transport::http::open_repo_for_serve(root) {
                                Ok(served) => {
                                    let server_shas: Vec<String> =
                                        served.refs.iter().map(|(_, sha)| sha.clone()).collect();
                                    // A have counts as shared when the server holds a commit with that
                                    // id; the export names every object, but only refs are checked here
                                    // because a full reachability walk per round is what v2 exists to
                                    // avoid repeating.
                                    haves
                                        .iter()
                                        .filter(|h| server_shas.iter().any(|s| s == *h))
                                        .cloned()
                                        .collect()
                                }
                                Err(_) => Vec::new(),
                            };
                        let bytes = crate::transport::protocol_v2::acknowledgments(&haves, &known);
                        log_access(
                            root,
                            &who,
                            "git-v2-ack",
                            p,
                            &format!("{} have(s), {} shared", haves.len(), known.len()),
                        );
                        return Response::ok_with_type(
                            bytes,
                            "application/x-git-upload-pack-result",
                        );
                    }

                    let v0_body =
                        crate::transport::protocol_v2::v0_fetch_body(&wants, &haves, done);
                    match crate::transport::gitserver::upload_pack(root, &v0_body) {
                        Ok(raw) => {
                            // `upload_pack` answers in v0 framing. The bytes it names are the answer;
                            // the header around them is the protocol.
                            let pack = strip_v0_framing(&raw);
                            let bytes = crate::transport::protocol_v2::fetch_response(&pack);
                            log_access(
                                root,
                                &who,
                                "git-v2-fetch",
                                p,
                                &format!("{} bytes", bytes.len()),
                            );
                            Response::ok_with_type(bytes, "application/x-git-upload-pack-result")
                        }
                        Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
                    }
                } else {
                    Response::error(
                        400,
                        "Bad Request",
                        "a v2 request must name command=ls-refs or command=fetch",
                    )
                }
            } else {
                match crate::transport::gitserver::upload_pack(root, &req.body) {
                    Ok(body) => {
                        log_access(
                            root,
                            &who,
                            "git-upload-pack",
                            p,
                            &format!("{} bytes", body.len()),
                        );
                        Response::ok_with_type(body, "application/x-git-upload-pack-result")
                    }
                    Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
                }
            }
        }
        ("GET", "/v1/file") => {
            let path = match req.param("path") {
                Some(p) => p.to_string(),
                None => return Response::error(400, "Bad Request", "path is required"),
            };
            read_file_at(root, &path, req.param("rev"), &who, grant.as_ref())
        }
        ("POST", "/v1/changeset") => {
            let body = match req.body_text() {
                Some(b) => b,
                None => return Response::error(400, "Bad Request", "body must be UTF-8 JSON"),
            };
            let payload = match crate::support::json::parse(&body) {
                Ok(v) => v,
                Err(e) => return Response::error(400, "Bad Request", format!("bad JSON: {}", e)),
            };
            match create_changeset_from_json(root, &payload, &who, grant.as_ref()) {
                Ok(v) => {
                    // `who` is the authenticated token name, which is the only
                    // identity worth auditing. The author the client claims is
                    // recorded in the detail, next to it, and never instead.
                    let claimed = crate::support::json::field(&payload, "author")
                        .and_then(crate::support::json::as_str)
                        .unwrap_or("-");
                    let detail = format!("claimed_author={} {}", claimed, v.render());
                    log_access(root, &who, "changeset", "-", &detail);
                    Response::ok_json(v)
                }
                Err(ChangeErr::Bad(msg)) => Response::error(400, "Bad Request", msg),
                Err(ChangeErr::Denied(msg)) => Response::error(403, "Forbidden", msg),
                Err(ChangeErr::Internal(msg)) => Response::error(500, "Internal Server Error", msg),
            }
        }
        ("POST", "/v1/bundle") => {
            let branch = req.param("branch").unwrap_or("main");
            match receive_bundle(root, branch, &req.body, &who, grant.as_ref()) {
                Ok(msg) => Response::ok_text(msg),
                Err(ApplyError::Rejected(msg)) => Response::error(409, "Conflict", msg),
                Err(ApplyError::Other(e)) => Response::error(400, "Bad Request", e.to_string()),
            }
        }
        (_, p) if !p.starts_with("/v1/") => {
            Response::error(404, "Not Found", format!("unknown path {}", p))
        }
        (m, p) => Response::error(405, "Method Not Allowed", format!("{} {}", m, p)),
    }
}

/// Decode `Basic base64(user:token)` and return the token part.
fn decode_basic_token(value: &str) -> Option<String> {
    use crate::transport::remote::decode_base64_str;
    let raw = decode_base64_str(value)?;
    let text = String::from_utf8(raw).ok()?;
    let (_user, token) = text.split_once(':')?;
    Some(token.to_string())
}

/// Record a scope refusal.
///
/// `Decision::PathRefused` and `Decision::BranchRefused` existed with a `Display` arm and a unit
/// test but no production caller, so every `permits`/`permits_branch` denial answered 403 and wrote
/// nothing. `docs/credentials.md` promises a line for every refusal, and the refusal a scope exists
/// to produce - a token probing for what it can reach - was the one that left no trace.
fn audit_scope_refusal(
    root: &Path,
    who: &str,
    branch_refused: bool,
    action: &str,
    target: &str,
    reason: &str,
) {
    crate::support::audit::record_with(
        root,
        &crate::support::audit::Event {
            who,
            decision: if branch_refused {
                crate::support::audit::Decision::BranchRefused
            } else {
                crate::support::audit::Decision::PathRefused
            },
            action,
            target,
            reason,
            remote: None,
        },
        None,
    );
}

/// `GET /v1/file`: the object at `path` as `rev` names it.
///
/// Extracted from the route so the scope check can be tested by calling it. The route used to be one
/// arm of a match, and the only test of its scope would have been a test of `permits` - which passes
/// whether or not the handler calls it (#159).
fn read_file_at(
    root: &Path,
    path: &str,
    rev: Option<&str>,
    who: &str,
    grant: Option<&crate::transport::auth::Grant>,
) -> Response {
    let path = crate::normalise(path);
    // The path scope covers reads too (#159).
    //
    // `docs/credentials.md` says a read token "is never limited by branch" because "reading is
    // already covered by the path scope". No GET handler called `permits`, so the second half of
    // that sentence was false: a token scoped to `docs/**` could read any file. Checked before the
    // store is opened, so a refused read does not report whether the repository exists.
    if let Some(g) = grant {
        if !g.permits(&path) {
            audit_scope_refusal(root, &g.name, false, "GET", &path, "path outside the scope");
            return Response::error(
                403,
                "Forbidden",
                format!("token {} may not read {}", g.name, path),
            );
        }
    }
    let conn = match crate::store::open_db(root) {
        Ok(c) => c,
        Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
    };
    let rev = match rev {
        Some(r) => r.to_string(),
        None => {
            // No rev means "what the branch looks like right now", which includes changesets still
            // queued. Reading only the head would make a file written by an API call invisible until
            // somebody committed it.
            let branch = match crate::store::current_branch(&conn) {
                Ok(b) => b,
                Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
            };
            let pending: Option<String> = conn
                .query_row(
                    "SELECT id FROM changesets
                     WHERE branch = ?1 AND status = 'pending'
                     ORDER BY created_at DESC, rowid DESC LIMIT 1",
                    rusqlite::params![branch],
                    |r| r.get(0),
                )
                .ok();
            pending.unwrap_or(branch)
        }
    };
    // A caller may name a branch, a tag or a changeset.
    let resolved = match crate::store::resolve_rev(&conn, &rev) {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Response::error(
                404,
                "Not Found",
                format!("no such branch, tag or changeset: {}", rev),
            )
        }
        Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
    };
    match crate::store::hash_at_point(&conn, &path, &resolved) {
        Ok(Some(hash)) => match crate::store::get_object(&conn, &hash) {
            Ok(content) => {
                log_access(
                    root,
                    who,
                    "file",
                    &path,
                    &format!("{} bytes", content.len()),
                );
                Response::ok_text(content)
            }
            Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
        },
        Ok(None) => Response::error(
            404,
            "Not Found",
            format!("{} does not exist at {}", path, rev),
        ),
        Err(e) => Response::error(500, "Internal Server Error", e.to_string()),
    }
}

/// Why a changeset request was refused.
enum ChangeErr {
    Bad(String),
    Denied(String),
    Internal(String),
}

/// Build a changeset straight from a JSON body, with no working copy.
///
/// This is the server side of "commit in memory": an agent running in a sandbox
/// sends `{branch, message, author, files: [{path, content}], delete: [path]}` and
/// the server writes blobs and a changeset without touching a checkout. Paths are
/// checked against the token's scope, so a token limited to `src/**` cannot write
/// outside it.
fn create_changeset_from_json(
    root: &Path,
    payload: &crate::support::json::Json,
    who: &str,
    grant: Option<&crate::transport::auth::Grant>,
) -> Result<crate::support::json::Json, ChangeErr> {
    use crate::support::json::{self, field, Json};

    let conn = crate::store::open_db(root).map_err(|e| ChangeErr::Internal(e.to_string()))?;
    crate::store::init_schema(&conn).map_err(|e| ChangeErr::Internal(e.to_string()))?;

    let message = field(payload, "message")
        .and_then(json::as_str)
        .ok_or_else(|| ChangeErr::Bad("message is required".into()))?;
    let author = field(payload, "author")
        .and_then(json::as_str)
        .unwrap_or(who);
    let branch = field(payload, "branch")
        .and_then(json::as_str)
        .unwrap_or("main");
    // The branch scope, before anything is written (#155).
    //
    // This handler checked `permits` for every path and never checked the branch, so a token scoped
    // to `main` could create - and with `commit: true`, commit - a changeset on `release`. The push
    // route had the mirror-image gap (it checked the branch and not the paths), and the comment
    // there claimed this route "has always checked" the branch, which was the one thing it did not
    // do. The branch comes straight from the request body, so it is caller-controlled input.
    if let Some(g) = grant {
        if !g.permits_branch(branch) {
            audit_scope_refusal(
                root,
                &g.name,
                true,
                "POST",
                branch,
                "branch outside the scope",
            );
            return Err(ChangeErr::Denied(format!(
                "this token may not write branch {:?}; its scope is {}",
                branch,
                if g.branches.is_empty() {
                    "unrestricted".to_string()
                } else {
                    g.branches.join(", ")
                }
            )));
        }
    }
    let commit = field(payload, "commit")
        .map(|v| matches!(v, Json::Bool(true)))
        .unwrap_or(false);

    let mut files: Vec<(String, String)> = Vec::new();
    if let Some(Json::Arr(items)) = field(payload, "files") {
        for item in items {
            let path = field(item, "path")
                .and_then(json::as_str)
                .ok_or_else(|| ChangeErr::Bad("every file needs a path".into()))?;
            let content = field(item, "content")
                .and_then(json::as_str)
                .ok_or_else(|| ChangeErr::Bad(format!("{} needs content", path)))?;
            let path = crate::normalise(path);
            if path.is_empty() || path.starts_with('/') || path.split('/').any(|s| s == "..") {
                return Err(ChangeErr::Bad(format!("unsafe path {:?}", path)));
            }
            if let Some(g) = grant {
                if !g.permits(&path) {
                    audit_scope_refusal(
                        root,
                        &g.name,
                        false,
                        "POST",
                        &path,
                        "path outside the scope",
                    );
                    return Err(ChangeErr::Denied(format!(
                        "token {} may not write {}",
                        g.name, path
                    )));
                }
            }
            files.push((path, content.to_string()));
        }
    }

    let mut delete: Vec<String> = Vec::new();
    if let Some(Json::Arr(items)) = field(payload, "delete") {
        for item in items {
            let path = json::as_str(item)
                .ok_or_else(|| ChangeErr::Bad("delete entries must be strings".into()))?;
            let path = crate::normalise(path);
            if let Some(g) = grant {
                if !g.permits(&path) {
                    audit_scope_refusal(
                        root,
                        &g.name,
                        false,
                        "POST",
                        &path,
                        "path outside the scope",
                    );
                    return Err(ChangeErr::Denied(format!(
                        "token {} may not delete {}",
                        g.name, path
                    )));
                }
            }
            delete.push(path);
        }
    }

    if files.is_empty() && delete.is_empty() {
        return Err(ChangeErr::Bad("nothing to do: send files or delete".into()));
    }

    let result = crate::store::with_retry(|| {
        crate::store::content::changeset_from_content(
            crate::store::content::Repo { conn: &conn, root },
            message,
            author,
            branch,
            &files,
            &delete,
            None,
        )
    })
    .map_err(|e| ChangeErr::Internal(e.to_string()))?;

    let mut out = vec![
        ("id", Json::str(result.id.clone())),
        ("branch", Json::str(branch.to_string())),
        ("files", Json::int(result.paths.len() as i64)),
        (
            "paths",
            Json::arr(result.paths.iter().cloned().map(Json::str).collect()),
        ),
        (
            "skipped",
            Json::arr(result.skipped.iter().cloned().map(Json::str).collect()),
        ),
    ];

    if commit {
        let id = result.id.clone();
        let n = crate::store::with_retry(|| crate::repo::commit_one(&conn, &id))
            .map_err(|e| ChangeErr::Internal(e.to_string()))?;
        out.push(("committed", Json::Bool(n > 0)));
    } else {
        out.push(("committed", Json::Bool(false)));
    }

    Ok(Json::obj(out))
}

fn list_branches(root: &Path) -> ZResult<String> {
    let conn = open_db(root)?;
    init_schema(&conn)?;
    let mut stmt = conn.prepare("SELECT name, head FROM branches ORDER BY name")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = String::new();
    for r in rows {
        let (name, head) = r?;
        out.push_str(&format!(
            "{}\t{}\n",
            name,
            head.unwrap_or_else(|| "-".into())
        ));
    }
    Ok(out)
}

/// The same bundle, cut to a depth the caller asked for.
///
/// `--depth` was accepted by `clone` and then dropped on this path: the client sent a plain bundle URL
/// and the server always built the whole history, so a shallow clone was as large as a full one and
/// the flag did nothing. It is honoured here.
fn fetch_bundle_with_depth(
    root: &Path,
    branch: &str,
    have: &HashSet<String>,
    depth: Option<usize>,
) -> ZResult<Option<Vec<u8>>> {
    let conn = open_db(root)?;
    init_schema(&conn)?;
    if branch_head(&conn, branch).is_err() {
        return Ok(None);
    }
    let bundle = remote::build_bundle_with_depth(&conn, branch, have, depth)?;
    Ok(Some(bundle.encode()?))
}

pub enum ApplyError {
    /// The push would lose changes: the remote head is not an ancestor.
    Rejected(String),
    Other(Box<dyn std::error::Error>),
}

/// Without this the message a `Rejected` carries cannot be read at all, so the refusal a caller sees
/// is whatever the outer layer prints about the type. That is how a scope refusal reached the client
/// as an unhelpful generic error while the sentence explaining it sat in the variant.
impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Rejected(m) => write!(f, "{m}"),
            ApplyError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<Box<dyn std::error::Error>> for ApplyError {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        ApplyError::Other(e)
    }
}

impl From<rusqlite::Error> for ApplyError {
    fn from(e: rusqlite::Error) -> Self {
        ApplyError::Other(Box::new(e))
    }
}

fn receive_bundle(
    root: &Path,
    branch: &str,
    body: &[u8],
    who: &str,
    grant: Option<&crate::transport::auth::Grant>,
) -> Result<String, ApplyError> {
    // The grant's branch scope, asked before the bundle is decoded or applied. The JSON changeset
    // route has always checked it; this route checked only the role, so a token scoped to one branch
    // could push to any other, and the branch is the part a mistake costs the most to undo. Checked
    // first so a refused push writes nothing, rather than being undone afterwards.
    if let Some(g) = grant {
        if !g.permits_branch(branch) {
            audit_scope_refusal(root, who, true, "POST", branch, "branch outside the scope");
            return Err(ApplyError::Rejected(format!(
                "this token may not write branch {:?}; its scope is {}",
                branch,
                if g.branches.is_empty() {
                    "unrestricted".to_string()
                } else {
                    g.branches.join(", ")
                }
            )));
        }
    }
    let conn = open_db(root)?;
    init_schema(&conn)?;

    let bundle = Bundle::decode(body)?;

    // The path scope, before anything is written (#149).
    //
    // This route checked the role and the branch scope and never asked about paths, so a token
    // issued for `docs/**` pushed any file it liked through the most capable push endpoint there is,
    // while the JSON changeset route and git-receive-pack both refused it. The scope a token was
    // issued with was silently void here, and the caller had no way to know.
    //
    // Every path in the bundle, not just the ones that differ from what the server holds: the cost
    // is a glob per version row and the alternative is deciding which rows "count", which is the
    // question this is here to answer. Checked before `apply_bundle` so a refused push writes
    // nothing rather than being rolled back - the branch roll-back below exists for a fast-forward
    // violation, which cannot be known any earlier.
    if let Some(g) = grant {
        let mut offending: Option<String> = None;
        for (path, ..) in &bundle.versions {
            if !g.permits(path) {
                offending = Some(path.clone());
                break;
            }
        }
        if offending.is_none() {
            for (path, ..) in &bundle.files {
                if !g.permits(path) {
                    offending = Some(path.clone());
                    break;
                }
            }
        }
        if let Some(path) = offending {
            audit_scope_refusal(root, who, false, "POST", &path, "path outside the scope");
            return Err(ApplyError::Rejected(format!(
                "this token may not write {:?}; its scope is {}",
                path,
                if g.scope.is_empty() {
                    "unrestricted".to_string()
                } else {
                    g.scope.join(", ")
                }
            )));
        }
    }

    let previous = branch_head(&conn, branch)?;

    // Apply first, check afterwards. A bundle normally carries only the
    // changesets the server lacks, so the incoming head's ancestry cannot be
    // resolved until those rows exist.
    let applied = remote::apply_bundle(&conn, &bundle)?;

    // A push must never move a branch to a head that does not contain where the
    // branch already was: that silently deletes other people's work.
    if let (Some(incoming), Some(prev)) = (&bundle.head, &previous) {
        let reachable = ancestors(&conn, incoming)?;
        if incoming != prev && !reachable.iter().any(|a| a == prev) {
            // Roll the ref back; the objects stay but the branch is untouched.
            crate::store::set_branch_head(&conn, branch, prev)?;
            return Err(ApplyError::Rejected(format!(
                "remote branch {} is at {} which the push does not contain; pull first",
                branch, prev
            )));
        }
    }

    // The served repository must show the pushed files on disk, otherwise
    // `status` inside the origin directory reports them as deleted.
    crate::repo::materialise(&conn, root, branch)?;

    // A push carries changesets and objects, never this store's derived rows. The server is a
    // repository like any other, so it has to derive its own: without this, blame on the receiving
    // side answered "never committed" for every file the push just delivered. Isolated per push so
    // one bad blob cannot fail the request after the branch already moved.
    if let Err(e) = crate::repo::rebuild_all_blame(&conn) {
        eprintln!("blame rebuild after push failed: {}", e);
    }

    let head = branch_head(&conn, branch)?.unwrap_or_else(|| "-".into());
    log_access(
        root,
        who,
        "push",
        branch,
        &format!("{} changesets, head {}", applied, head),
    );
    Ok(format!("applied {}\nhead {}\n", applied, head))
}

/// Append one line to the access log in the repository directory, recording who
/// did what.
///
/// The server is the only place that knows the authenticated identity, so the
/// audit trail has to be written here rather than on the client.
pub fn log_access(root: &Path, who: &str, action: &str, branch: &str, detail: &str) {
    use std::io::Write;
    let path = crate::repo_dir(root).join("access.log");
    let line = format!(
        "{}\t{}\t{}\t{}\t{}\n",
        crate::support::clock::format_rfc3339(crate::support::clock::unix_now()),
        who,
        action,
        branch,
        detail
    );
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// URL a client should hit for a given repository base and branch.
pub fn bundle_url(base: &str, branch: &str, have: &[String]) -> String {
    bundle_url_depth(base, branch, have, None)
}

/// The bundle URL, optionally asking the server to cut the history to `depth` changesets.
pub fn bundle_url_depth(base: &str, branch: &str, have: &[String], depth: Option<usize>) -> String {
    let mut url = format!(
        "{}/v1/bundle?branch={}",
        base.trim_end_matches('/'),
        url_encode(branch)
    );
    if !have.is_empty() {
        url.push_str("&have=");
        url.push_str(&have.join("%2C"));
    }
    if let Some(d) = depth {
        url.push_str(&format!("&depth={}", d));
    }
    url
}

/// Perform one request with caller supplied headers, over TLS or plain.
///
/// S3 needs its own Authorization and x-amz-* headers, which the bearer token form
/// cannot express. The request line, framing and response parsing are the same, so
/// this shares them instead of growing a second HTTP implementation.
pub fn request_with_headers(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> ZResult<(u16, Vec<u8>)> {
    let u = parse_http_url(url)?;
    let timeout = Duration::from_secs(300);
    let mut head = format!(
        "{} {} HTTP/1.1\r\nConnection: close\r\n",
        method,
        if u.base_path.is_empty() {
            "/".to_string()
        } else {
            u.base_path.clone()
        }
    );
    // HTTP/1.1 requires Host, and a forge behind nginx answers 400 without one. The existing callers
    // that sign a request (S3) pass their own Host, so this only adds it when the caller did not, and a
    // signature stays correct.
    let caller_sets_host = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host"));
    // A caller that signs Content-Length has to send the same value, so it is
    // passed in and not added again here. Two of them make nginx answer 400 with
    // no hint that framing was the problem.
    let caller_sets_length = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
    if !caller_sets_host {
        let host = if (u.tls && u.port == 443) || (!u.tls && u.port == 80) {
            u.host.clone()
        } else {
            format!("{}:{}", u.host, u.port)
        };
        head.push_str(&format!("Host: {}\r\n", host));
    }
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    if let Some(b) = body {
        if !caller_sets_length {
            head.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
    }
    head.push_str("\r\n");

    let mut raw: Vec<u8> = Vec::new();
    if u.tls {
        #[cfg(not(feature = "tls"))]
        return Err(bad(
            "this build has no TLS support: rebuild with --features tls".to_string(),
        ));
        #[cfg(feature = "tls")]
        {
            let mut stream = crate::transport::tls::connect(&u.host, u.port, None, timeout)?;
            stream.write_all(head.as_bytes())?;
            if let Some(b) = body {
                stream.write_all(b)?;
            }
            stream.flush()?;
            if let Err(e) = read_capped(&mut stream, &mut raw, MAX_RESPONSE) {
                let complete = find_subslice(&raw, b"\r\n\r\n").is_some();
                if !(complete && is_eof_error(&e)) {
                    return Err(bad(format!("reading the response failed: {}", e)));
                }
            }
        }
    } else {
        let mut stream = TcpStream::connect((u.host.as_str(), u.port))
            .map_err(|e| bad(format!("cannot connect to {}:{}: {}", u.host, u.port, e)))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(head.as_bytes())?;
        if let Some(b) = body {
            stream.write_all(b)?;
        }
        stream.flush()?;
        read_capped(&mut stream, &mut raw, MAX_RESPONSE)
            .map_err(|e| bad(format!("reading the response failed: {}", e)))?;
    }
    let (status, body) = parse_response(&raw)?;
    Ok((status, body))
}

/// A request with HTTP Basic credentials and an explicit content type, which is the shape a forge
/// expects: git's smart HTTP protocols answer with their own media types and reject a request that
/// asks for `*/*` with a plain-text error that looks like a repository problem.
///
/// `user:password` is base64 encoded here rather than by the caller, because every forge documents the
/// header differently and every one of them means the same wire format.
pub fn request_with_auth(
    method: &str,
    url: &str,
    body: Option<&[u8]>,
    basic: Option<&str>,
    accept: Option<&str>,
) -> ZResult<(u16, Vec<u8>)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(a) = accept {
        headers.push(("Accept".to_string(), a.to_string()));
        if body.is_some() {
            headers.push((
                "Content-Type".to_string(),
                a.replace("advertisement", "request"),
            ));
        }
    }
    if let Some(userpass) = basic {
        headers.push((
            "Authorization".to_string(),
            format!("Basic {}", base64_encode(userpass.as_bytes())),
        ));
    }
    request_with_headers(method, url, &headers, body)
}

/// Standard base64, which is what a Basic header carries.
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Remove side-band framing from a response body if one arrived.
///
/// v0 git wraps part of an answer in `\x01`-prefixed pkt-lines. This client does not request side-band,
/// so a body without any is returned untouched; a body that has it is unwrapped rather than parsed as
/// if the frames were the content.
/// Undo HTTP/1.1 chunked transfer encoding.
///
/// GitHub answers `git-receive-pack` with `Transfer-Encoding: chunked`, and the report-status lines
/// are the payload of that encoding. A reader that takes the body as-is sees the hex length prefixes
/// as if they were pkt-line lengths: `51\r\n002b\u{1}...` parses as a 0x51 byte pkt instead of the
/// `002b` pkt inside it, so every report is unreadable and a successful push reports as unconfirmed.
///
/// A body that does not begin with a valid chunk length is returned unchanged, because not every
/// server chunks and the caller should not have to know which one it is talking to.
pub fn dechunk(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut chunks = 0usize;
    while i < raw.len() {
        let Some(nl) = raw[i..].windows(2).position(|w| w == b"\r\n") else {
            return if chunks > 0 { out } else { raw.to_vec() };
        };
        let header = match std::str::from_utf8(&raw[i..i + nl]) {
            Ok(h) => h,
            Err(_) => return if chunks > 0 { out } else { raw.to_vec() },
        };
        // The size may carry `;ext=...`; only the hex prefix matters.
        let size_hex = header.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            return if chunks > 0 { out } else { raw.to_vec() };
        };
        i += nl + 2;
        if size == 0 {
            return out;
        }
        if i + size > raw.len() {
            return if chunks > 0 { out } else { raw.to_vec() };
        }
        out.extend_from_slice(&raw[i..i + size]);
        chunks += 1;
        i += size;
        // Each chunk ends with CRLF, which is not part of the payload.
        if raw[i..].starts_with(b"\r\n") {
            i += 2;
        }
    }
    if chunks > 0 {
        out
    } else {
        raw.to_vec()
    }
}

pub fn strip_side_band(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut saw_frame = false;
    while i + 5 <= raw.len() {
        let Ok(hex) = std::str::from_utf8(&raw[i..i + 4]) else {
            return raw.to_vec();
        };
        let Ok(len) = usize::from_str_radix(hex, 16) else {
            return raw.to_vec();
        };
        if len < 5 || i + len > raw.len() {
            return raw.to_vec();
        }
        let band = raw[i + 4];
        // Band 1 is data, 2 is progress, 3 is an error. Anything else means this was not framing.
        if !(1..=3).contains(&band) {
            return raw.to_vec();
        }
        saw_frame = true;
        if band == 1 {
            out.extend_from_slice(&raw[i + 5..i + len]);
        }
        i += len;
    }
    if saw_frame {
        out
    } else {
        raw.to_vec()
    }
}

/// Split an HTTP response into its status code and body.
fn parse_response(raw: &[u8]) -> ZResult<(u16, Vec<u8>)> {
    let split = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| bad("response has no header terminator".to_string()))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| bad(format!("cannot read status from {}", head)))?;
    // The same completeness gate as `request_with_pin`. Two client paths with two different notions
    // of a complete response is how one of them ends up trusting a truncated body (#154).
    let body = complete_body(&head, raw[split + 4..].to_vec())?;
    Ok((status, body))
}

/// The `Content-Length` a response head declares, if it declares one.
fn declared_length(head_text: &str) -> Option<usize> {
    head_text.split("\r\n").find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.eq_ignore_ascii_case("content-length") {
            v.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

/// Take a response body that respects both the cap and the length the server claimed.
///
/// The cap is checked against the declared length *before* the bytes are read, so a server that
/// claims a terabyte is refused without the client having to absorb it. A body that arrives shorter
/// than its declared length is an error too: a truncated pack handed to a decoder as if whole is the
/// failure this exists to prevent, and the bytes alone cannot show the difference between a short
/// body and a complete small one (#154).
fn complete_body(head_text: &str, payload: Vec<u8>) -> ZResult<Vec<u8>> {
    let Some(want) = declared_length(head_text) else {
        // No claim, nothing to verify against. A `Connection: close` response without a length is
        // the end of the stream by definition.
        return Ok(payload);
    };
    if want > MAX_BODY {
        return Err(bad(format!(
            "body of {} bytes exceeds the limit of {}",
            want, MAX_BODY
        )));
    }
    if payload.len() < want {
        return Err(bad(format!(
            "the response declared {} bytes but only {} arrived",
            want,
            payload.len()
        )));
    }
    Ok(payload[..want].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that answers one request with the given head and body, then closes.
    fn one_shot_reply(head: &'static str, body: &'static [u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body);
            let _ = s.flush();
        });
        format!("http://127.0.0.1:{}/x", port)
    }

    #[test]
    fn a_response_shorter_than_its_declared_length_is_refused() {
        // #154: `request_with_pin` read `Content-Length` into `declared` and then only used it to
        // truncate. A body shorter than the declared length came back as a success, so a truncated
        // pack was handed to the decoder as if it were whole. A short body is a broken transfer, not
        // a smaller one, and the caller cannot tell the difference from the bytes alone.
        let url = one_shot_reply(
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n",
            b"0123456789",
        );
        let err = request_with_pin("GET", &url, None, None, None)
            .expect_err("a body shorter than its declared length must not be a success")
            .to_string();
        assert!(
            err.contains("100") && err.contains("10"),
            "the error should name the declared and the received length: {:?}",
            err
        );
    }

    #[test]
    fn a_complete_response_is_still_accepted() {
        // The guard must not break the ordinary case: an exact body still comes through whole.
        let url = one_shot_reply("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n", b"abc");
        let (status, body) = request_with_pin("GET", &url, None, None, None).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"abc");
    }

    #[test]
    fn an_unbounded_body_is_cut_off_rather_than_read_until_memory_runs_out() {
        // #154: both client read paths used `read_to_end` with no cap, so a malicious or compromised
        // server could exhaust the client's memory by never ending the body. The cap has to trip on
        // the declared length, before the bytes are read, so the client does not have to survive the
        // stream to refuse it.
        let url = one_shot_reply("HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n", b"");
        let err = request_with_pin("GET", &url, None, None, None)
            .expect_err("a body over the cap must be refused")
            .to_string();
        assert!(
            err.contains("exceeds"),
            "the refusal should say the body exceeds the limit: {:?}",
            err
        );
    }

    #[cfg(feature = "tls")]
    #[test]
    fn a_tokenless_tls_server_is_refused_before_it_takes_the_port() {
        // #171: `serve_tls` bound the socket, printed "gesta listening on ...", and only then
        // refused to run without tokens. A refused start therefore announced a listening address it
        // never served, and the port was held for the length of the certificate work. The refusal
        // must come first: nothing is bound, nothing is printed as listening, and the address is
        // still free when the call returns.
        let dir = std::env::temp_dir().join(format!("gesta-tls-tokenless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let (cert, key, _fingerprint) =
            crate::transport::tls::generate_self_signed("localhost", &dir).unwrap();

        // Hold the port for the whole test. If `serve_tls` binds before it checks the tokens, the
        // error is about the bind; if it checks first, the error is about the tokens. That makes the
        // two orders distinguishable rather than both failing with the same message.
        let held = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let port = held.local_addr().unwrap().port();

        let auth = Auth::from_entries(&[]);
        assert!(!auth.required, "the fixture was supposed to have no tokens");

        let err = serve_tls(dir.clone(), "0.0.0.0".to_string(), port, auth, &cert, &key)
            .expect_err("a TLS server with no tokens must be refused")
            .to_string();

        assert!(
            err.contains("refusing to serve TLS"),
            "the refusal did not happen before the bind: {:?}",
            err
        );
        assert!(
            !err.contains("cannot bind"),
            "the socket was bound before the tokens were checked: {:?}",
            err
        );
        // The port is still only ours: nothing else took it on the way in or out.
        assert!(
            std::net::TcpListener::bind(("0.0.0.0", port)).is_err(),
            "serve_tls released a port it never owned"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_refusal_leaves_an_audit_line() {
        // #165: `PathRefused` had a `Display` arm and a unit test but no production caller, so a
        // token probing for what it can reach got a 403 and left nothing behind. The refusal a scope
        // exists to produce was the one missing from the log.
        let dir = std::env::temp_dir().join(format!("gesta-audit-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();

        let g = grant_to_paths(&["docs/**"]);
        let payload = crate::support::json::parse(
            r#"{"message":"m","branch":"main","files":[{"path":"src/main.rs","content":"x"}]}"#,
        )
        .unwrap();
        let err = create_changeset_from_json(&dir, &payload, "t", Some(&g));
        assert!(err.is_err(), "a path outside the scope must be refused");

        let entries = crate::support::audit::read(&dir, false, None).unwrap();
        let line = entries
            .iter()
            .find(|e| e.decision == "path-refused")
            .unwrap_or_else(|| {
                panic!(
                    "no path-refused line; decisions were {:?}",
                    entries
                        .iter()
                        .map(|e| e.decision.as_str())
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(
            line.target, "src/main.rs",
            "the line must name the refused path"
        );
        assert_eq!(line.who, "t", "the line must name the token");
        assert!(
            line.reason.contains("scope"),
            "the reason must say why: {}",
            line.reason
        );
    }

    #[test]
    fn a_branch_refusal_leaves_an_audit_line() {
        // The mirror of the test above, and the one #155 left behind: a token scoped to `main` being
        // used on `release` is exactly the mistake the branch scope exists to catch.
        let dir = std::env::temp_dir().join(format!("gesta-audit-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();

        let g = grant_to(&["main"]);
        let payload = crate::support::json::parse(
            r#"{"message":"m","branch":"release","files":[{"path":"a.txt","content":"x"}]}"#,
        )
        .unwrap();
        assert!(create_changeset_from_json(&dir, &payload, "t", Some(&g)).is_err());

        let entries = crate::support::audit::read(&dir, false, None).unwrap();
        let line = entries
            .iter()
            .find(|e| e.decision == "branch-refused")
            .unwrap_or_else(|| {
                panic!(
                    "no branch-refused line; decisions were {:?}",
                    entries
                        .iter()
                        .map(|e| e.decision.as_str())
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(line.target, "release");
        assert_eq!(line.who, "t");
    }

    #[test]
    fn a_permitted_request_writes_no_refusal_line() {
        // The negative half: a log that records a refusal on every request would be noise, and noise
        // is how a real refusal goes unread.
        let dir = std::env::temp_dir().join(format!("gesta-audit-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();

        let g = grant_to_paths(&["docs/**"]);
        let payload = crate::support::json::parse(
            r#"{"message":"m","branch":"main","files":[{"path":"docs/a.md","content":"x"}]}"#,
        )
        .unwrap();
        let _ = create_changeset_from_json(&dir, &payload, "t", Some(&g));

        let entries = crate::support::audit::read(&dir, false, None).unwrap();
        assert!(
            !entries.iter().any(|e| e.decision == "path-refused"),
            "a permitted path produced a path-refused line"
        );
    }

    #[test]
    fn the_connection_ceiling_holds_and_releases_its_slots() {
        // One thread per connection with no ceiling is the shape that turns a busy moment into an
        // outage: a caller that opens sockets and never speaks costs a thread each, and the server
        // that was meant to answer a team answers nobody. This checks the ceiling is real and that
        // a finished worker gives its slot back.
        //
        // The workers sleep rather than wait on a latch: `spawn_bounded` blocks the caller once the
        // ceiling is reached, so a latch the caller holds would deadlock the spawn loop against its
        // own workers, and an earlier version of this test did exactly that.
        let slots = Arc::new(std::sync::Mutex::new(0usize));
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..(MAX_CONNECTIONS + 40) {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            spawn_bounded(Arc::clone(&slots), move || {
                let now = running.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(30));
                running.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while running.load(std::sync::atomic::Ordering::SeqCst) > 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let peak = peak.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            peak <= MAX_CONNECTIONS,
            "peak concurrency {peak} exceeded the ceiling of {MAX_CONNECTIONS}"
        );
        assert!(
            peak > MAX_CONNECTIONS / 2,
            "only {peak} workers ran at once, so the ceiling was not exercised"
        );
        let left = *slots.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            left, 0,
            "workers finished but {left} slots were never released"
        );
    }

    fn grant_to(branches: &[&str]) -> crate::transport::auth::Grant {
        crate::transport::auth::Grant {
            name: "t".to_string(),
            role: crate::transport::auth::Role::Write,
            scope: Vec::new(),
            expires_at: None,
            branches: branches.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn grant_to_paths(paths: &[&str]) -> crate::transport::auth::Grant {
        let mut g = grant_to(&[]);
        g.scope = paths.iter().map(|s| s.to_string()).collect();
        g
    }

    #[test]
    fn a_changeset_on_an_unlisted_branch_is_refused() {
        // #155: this handler checked `permits` for every path and never checked the branch, so a
        // token scoped to `main` could create - and with `commit: true`, commit - a changeset on
        // `release`. The branch is read straight from the request body, so it is caller-controlled.
        let dir = std::env::temp_dir().join(format!("gesta-cs-branch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // `.gesta` exists so a refusal that happened *after* opening the store would get a different
        // error and be visible as such. With the check first, the store is never opened.
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();

        let payload = crate::support::json::parse(
            r#"{"message":"m","branch":"release","files":[{"path":"a.txt","content":"one"}]}"#,
        )
        .unwrap();
        let g = grant_to(&["main"]);
        let msg = match create_changeset_from_json(&dir, &payload, "t", Some(&g)) {
            Ok(_) => panic!("a token scoped to main created a changeset on release"),
            Err(ChangeErr::Denied(m)) => m,
            Err(ChangeErr::Bad(m)) | Err(ChangeErr::Internal(m)) => {
                panic!("the branch scope must refuse with Denied, got {m}")
            }
        };
        assert!(
            msg.contains("may not write branch") && msg.contains("release"),
            "the refusal must name the branch and the scope it has: {msg}"
        );

        // The same body on a permitted branch gets past the scope check, which is what tells the two
        // paths apart rather than a message that would also appear for a malformed payload.
        let allowed = grant_to(&["release"]);
        match create_changeset_from_json(&dir, &payload, "t", Some(&allowed)) {
            Ok(_) => {}
            Err(ChangeErr::Denied(m)) => panic!("a permitted branch was refused: {m}"),
            Err(_) => {}
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_outside_the_path_scope_is_refused() {
        // #159: no GET handler called `permits`, so a token scoped to `docs/**` could read any file
        // through `/v1/file`. The check has to be made by the handler, not by `permits` on its own -
        // a test that asserts `!g.permits(...)` passes whether or not `read_file_at` ever calls it,
        // and did pass with the fix removed. So this calls the handler.
        let dir = std::env::temp_dir().join(format!("gesta-read-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        {
            let conn = crate::store::open_db(&dir).unwrap();
            crate::store::init_schema(&conn).unwrap();
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
                [],
            )
            .unwrap();
            std::fs::create_dir_all(dir.join("docs")).unwrap();
            std::fs::write(dir.join("docs/readme.md"), "public\n").unwrap();
            std::fs::write(dir.join("secret.rs"), "key = 1\n").unwrap();
            let cs = crate::repo::create_changeset(&conn, &dir, "seed", "t")
                .unwrap()
                .expect("two files is a changeset");
            crate::repo::commit_one(&conn, &cs.id).unwrap();
        }

        let g = grant_to_paths(&["docs/**"]);
        let denied = read_file_at(&dir, "secret.rs", Some("main"), "t", Some(&g));
        assert_eq!(
            denied.status, 403,
            "a docs-scoped token read a file outside its scope; the handler returned {}",
            denied.status
        );
        // And the file inside the scope still reads, or the check would just be a wall.
        let allowed = read_file_at(&dir, "docs/readme.md", Some("main"), "t", Some(&g));
        assert_eq!(
            allowed.status, 200,
            "a file inside the scope must still be readable, got {}",
            allowed.status
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bundle_push_to_an_unlisted_branch_is_refused_before_anything_is_written() {
        // #149: this route checked the role and nothing else, so a token scoped to one branch could
        // push to any other. The refusal has to come before `Bundle::decode`, or a refused push has
        // already written rows.
        let dir = std::env::temp_dir().join(format!("gesta-bundle-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let g = grant_to(&["release/*"]);
        let err = receive_bundle(&dir, "main", b"not a bundle", "t", Some(&g)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("may not write branch") && msg.contains("release/*"),
            "the refusal must name the branch and the scope it has: {msg}"
        );

        // The empty directory proves nothing was written: a decode of the same bytes with a
        // permitted branch fails later, which is what tells the two paths apart.
        let allowed = grant_to(&["release/*"]);
        let err2 =
            receive_bundle(&dir, "release/1.0", b"not a bundle", "t", Some(&allowed)).unwrap_err();
        assert!(
            !err2.to_string().contains("may not write branch"),
            "the permitted branch must get past the scope check: {err2}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unscoped_grant_may_push_anywhere() {
        let dir = std::env::temp_dir().join(format!("gesta-bundle-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = grant_to(&[]);
        let err = receive_bundle(&dir, "anything", b"not a bundle", "t", Some(&g)).unwrap_err();
        assert!(!err.to_string().contains("may not write branch"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lfs_routes_need_a_credential_when_the_server_has_tokens() {
        // #153: the LFS surface fell through to `_ => None`, which the matcher reads as "no
        // credential required", so a server with tokens served every stored asset and answered every
        // lock request to an anonymous caller. Asked of the decision itself: a route that needs no
        // role is open, and a 404 for an unrouted path also answers 400-or-more, which is why the
        // earlier version of this test passed with the fix removed.
        for path in [
            "/repo.git/info/lfs/objects/batch",
            "/repo.git/info/lfs/locks/verify",
            "/repo.git/info/lfs/objects/abc123",
            "/repo.git/info/lfs/locks/42/unlock",
        ] {
            assert_eq!(
                required_role("POST", path),
                Some(Role::Write),
                "{path} must require a write credential, not be open"
            );
        }
        // The routes that are meant to be open stay open, so the check above is not just
        // "everything needs a credential".
        assert_eq!(required_role("GET", "/v1/health"), None);
        // And git over HTTP still maps the way it did.
        assert_eq!(
            required_role("POST", "/r.git/git-receive-pack"),
            Some(Role::Write)
        );
        assert_eq!(
            required_role("POST", "/r.git/git-upload-pack"),
            Some(Role::Read)
        );
    }

    #[test]
    fn dechunk_undoes_chunked_transfer_encoding() {
        // The exact body GitHub returned for a push, captured from the wire: one chunk of `0x51`
        // bytes, then the terminator. Read as pkt-line, `51` becomes the length of an 81-byte packet
        // and every byte inside it is misaligned, which is how a correct push was reported as
        // unconfirmed against a real forge.
        let raw = b"51\r\n002b\x010026unpack no voting servers succeeded001eng refs/heads/main failed\n00000000\r\n0\r\n\r\n";
        let plain = dechunk(raw);
        assert_eq!(
            plain,
            b"002b\x010026unpack no voting servers succeeded001eng refs/heads/main failed\n00000000"
        );
        assert_eq!(plain.len(), 0x51);
    }

    #[test]
    fn dechunk_leaves_an_unchunked_body_alone() {
        // Not every server chunks. A body that does not begin with a valid chunk length has to come
        // back untouched, or every other caller of this path is corrupted by the fix for one server.
        let plain = b"0023unpack ok\n000eok refs/heads/main\n0000";
        assert_eq!(dechunk(plain), plain.to_vec());
    }

    #[test]
    fn dechunk_handles_several_chunks() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), b"hello world".to_vec());
    }

    #[test]
    fn parses_a_request_line_with_query() {
        let raw = b"GET /v1/bundle?branch=dev&have=cs1,cs2 HTTP/1.1\r\nHost: x\r\n\r\n";
        let head = parse_head(raw).unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.path, "/v1/bundle");
        assert_eq!(head.query.len(), 2);
        assert_eq!(head.query[0], ("branch".to_string(), "dev".to_string()));
        assert_eq!(head.query[1], ("have".to_string(), "cs1,cs2".to_string()));
        assert_eq!(head.consumed, raw.len());
    }

    #[test]
    fn rejects_a_head_without_terminator() {
        assert!(parse_head(b"GET / HTTP/1.1\r\n").is_err());
    }

    #[test]
    fn url_encode_decode_roundtrip() {
        // Includes a non-ASCII string on purpose: branch names may contain any
        // UTF-8, and percent-encoding must round-trip it byte for byte.
        for s in ["main", "feature/x", "a b", "t\u{e9}st", "cs1,cs2"] {
            assert_eq!(url_decode(&url_encode(s)), s);
        }
    }

    #[test]
    fn parses_http_url_with_and_without_port() {
        let u = parse_http_url("http://example.com:8787/repo").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.base_path.as_str()),
            ("example.com", 8787, "/repo")
        );

        let u = parse_http_url("http://127.0.0.1").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("127.0.0.1", 80));
    }

    #[test]
    fn https_parses_with_tls_and_the_default_443() {
        let u = parse_http_url("https://example.com/path").unwrap();
        assert!(u.tls);
        assert_eq!(u.port, 443);
        assert_eq!(u.host, "example.com");
        assert_eq!(u.base_path, "/path");
    }

    #[test]
    fn http_still_parses_as_plaintext_on_port_80() {
        let u = parse_http_url("http://example.com").unwrap();
        assert!(!u.tls);
        assert_eq!(u.port, 80);
    }

    #[test]
    fn an_explicit_https_port_overrides_the_default() {
        let u = parse_http_url("https://example.com:9443").unwrap();
        assert!(u.tls);
        assert_eq!(u.port, 9443);
    }

    #[test]
    fn a_url_without_a_scheme_is_refused() {
        let err = parse_http_url("example.com").unwrap_err().to_string();
        assert!(err.contains("not an http or https url"), "got: {}", err);
    }

    #[test]
    fn unknown_route_is_404() {
        let req = Request {
            method: "GET".into(),
            path: "/v1/nope".into(),
            query: vec![],
            body: vec![],
            authorization: None,
            git_protocol: None,
        };
        let resp = route(
            Path::new("/tmp/does-not-matter"),
            &req,
            &Auth::from_entries(&[]),
            None,
        );
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn wrong_method_on_a_known_path_is_405() {
        let req = Request {
            method: "DELETE".into(),
            path: "/v1/bundle".into(),
            query: vec![],
            body: vec![],
            authorization: None,
            git_protocol: None,
        };
        let resp = route(
            Path::new("/tmp/does-not-matter"),
            &req,
            &Auth::from_entries(&[]),
            None,
        );
        assert_eq!(resp.status, 405);
    }

    #[test]
    fn health_needs_no_repository() {
        let req = Request {
            method: "GET".into(),
            path: "/v1/health".into(),
            query: vec![],
            body: vec![],
            authorization: None,
            git_protocol: None,
        };
        let resp = route(
            Path::new("/tmp/does-not-matter"),
            &req,
            &Auth::from_entries(&[]),
            None,
        );
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok".to_vec());
    }

    #[test]
    fn bundle_url_encodes_branch_and_have_list() {
        let u = bundle_url(
            "http://localhost:8787/",
            "feature/x",
            &["cs1".into(), "cs2".into()],
        );
        assert_eq!(
            u,
            "http://localhost:8787/v1/bundle?branch=feature%2Fx&have=cs1%2Ccs2"
        );
    }

    #[test]
    fn response_has_content_length_and_connection_close() {
        let r = Response::ok_text("hello");
        let bytes = r.to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Content-Length: 5"));
        assert!(text.contains("Connection: close"));
        assert!(text.ends_with("hello"));
    }
    #[test]
    fn force_releasing_another_clients_lfs_lock_needs_a_real_credential() {
        // #153. Two separate holes on the lock surface, and the route check was already fixed, so
        // what is left is the one the issue's second half names: `force` came from the request body,
        // and `who` comes from a name the client typed into an `Authorization` header that, on a
        // server with auth off, is never verified. Together that meant anyone could release anyone's
        // lock - and could do it while claiming to be them.
        //
        // This calls `route`, not `required_role`: the role table answers "does this route need a
        // credential", and it says yes, and the request below carries one. The defect was inside the
        // handler, which is why a routing test cannot see it (A36).
        let dir = std::env::temp_dir().join(format!("gesta-lfs-force-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let lock_id;
        {
            let conn = crate::store::open_db(&dir).unwrap();
            crate::store::init_schema(&conn).unwrap();
            let l = crate::transport::lfs::lock(&conn, "big.bin", "victim", None, 1_700_000_000)
                .unwrap();
            lock_id = l.id;
        }

        // No Authorization header at all: `who` is `anonymous`, `force` is true, and the lock belongs
        // to `victim`. Before the fix this deleted it.
        let req = Request {
            method: "POST".to_string(),
            path: "/repo.git/info/lfs/locks/unlock".to_string(),
            query: Vec::new(),
            body: format!("{{\"lock\":{{\"id\":\"{}\"}},\"force\":true}}", lock_id).into_bytes(),
            authorization: None,
            git_protocol: None,
        };
        // A server WITH tokens configured, because that is the case the refusal is about: there,
        // `who` is a name the client typed into a header nothing verified, and `force` would let a
        // stranger release the lock while the access log named its holder. On a server without
        // tokens there is nothing to verify against and force is the normal single-user workflow
        // (`bench/lfs_locks.py` asserts that, and my first version broke it).
        let mut auth =
            crate::transport::auth::Auth::from_entries(&["writer-token:write:alice".to_string()]);
        auth.required = true;
        let resp = route(&dir, &req, &auth, None);
        assert_ne!(
            resp.status, 200,
            "an unauthenticated caller force-released a lock held by someone else"
        );

        // The lock is still there, which is the half a status code alone does not prove: a 403 after
        // the delete would read as passing.
        let conn = crate::store::open_db(&dir).unwrap();
        let still = crate::transport::lfs::list(&conn, None, None, None);
        assert_eq!(
            still.len(),
            1,
            "the lock was released even though the request was refused"
        );
        assert_eq!(still[0].owner, "victim");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_owner_can_still_release_their_own_lock_without_force() {
        // The check above must not turn into "nobody may ever unlock". A caller releasing the lock
        // they hold needs no privilege: the owner matches, `force` is irrelevant, and this has to
        // keep working on an auth-off server or the fix has broken the feature.
        let dir = std::env::temp_dir().join(format!("gesta-lfs-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let lock_id;
        {
            let conn = crate::store::open_db(&dir).unwrap();
            crate::store::init_schema(&conn).unwrap();
            let l = crate::transport::lfs::lock(&conn, "mine.bin", "owner", None, 1_700_000_000)
                .unwrap();
            lock_id = l.id;
        }
        let req = Request {
            method: "POST".to_string(),
            path: "/repo.git/info/lfs/locks/unlock".to_string(),
            query: Vec::new(),
            body: format!("{{\"lock\":{{\"id\":\"{}\"}}}}", lock_id).into_bytes(),
            // The name the client claims. With auth off there is nothing to verify it against, and
            // the ordinary path is allowed to trust it for exactly that reason: it only releases a
            // lock that already says the same name.
            // base64("owner:whatever"), written out rather than computed: the crate has a decoder
            // and no encoder, and a test that used the decoder to build its own input would not
            // notice the two disagreeing.
            authorization: Some("Basic b3duZXI6d2hhdGV2ZXI=".to_string()),
            git_protocol: None,
        };
        let auth = crate::transport::auth::Auth::from_entries(&[]);
        let resp = route(&dir, &req, &auth, None);
        assert_eq!(
            resp.status,
            200,
            "the holder could not release their own lock: {}",
            String::from_utf8_lossy(&resp.body)
        );
        let conn = crate::store::open_db(&dir).unwrap();
        assert_eq!(
            crate::transport::lfs::list(&conn, None, None, None).len(),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bundle_push_outside_the_path_scope_writes_nothing() {
        // #149. `receive_bundle` checked the branch scope and never checked paths, so a token issued
        // for `docs/**` pushed `src/main.rs` through the primary push endpoint - while the JSON
        // changeset route and git-receive-pack both refused the same file. The scope was void on the
        // most capable route, and the caller was never told.
        //
        // The check runs before `apply_bundle`, so this asserts on the store as well as the answer:
        // a rejection after the write would satisfy a status assertion and still have written.
        let src = std::env::temp_dir().join(format!("gesta-bundle-src-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("gesta-bundle-dst-{}", std::process::id()));
        for d in [&src, &dst] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d.join(".gesta")).unwrap();
        }
        // The destination has a `main`, because a server that accepts a push has one: a bundle
        // arrives against an existing branch, and without it the failure below would be "no such
        // branch" rather than the scope question this test is about.
        {
            let conn = crate::store::open_db(&dst).unwrap();
            crate::store::init_schema(&conn).unwrap();
            crate::store::migrate(&conn).unwrap();
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
                [],
            )
            .unwrap();
        }

        // The source repository: one changeset with a file the token may not touch.
        let body = {
            let conn = crate::store::open_db(&src).unwrap();
            crate::store::init_schema(&conn).unwrap();
            crate::store::migrate(&conn).unwrap();
            conn.execute(
                "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
                [],
            )
            .unwrap();
            std::fs::create_dir_all(src.join("src")).unwrap();
            std::fs::write(src.join("src/main.rs"), "fn main() {}\n").unwrap();
            let cs = crate::repo::create_changeset(&conn, &src, "add main", "t")
                .unwrap()
                .expect("one file is a changeset");
            crate::repo::commit_one(&conn, &cs.id).unwrap();
            crate::transport::remote::build_bundle(&conn, "main", &std::collections::HashSet::new())
                .unwrap()
                .encode()
                .unwrap()
        };

        let g = grant_to_paths(&["docs/**"]);
        let res = receive_bundle(&dst, "main", &body, "t", Some(&g));
        match res {
            Err(ApplyError::Rejected(msg)) => assert!(
                msg.contains("src/main.rs"),
                "the refusal does not name the file it refused: {msg}"
            ),
            Ok(_) => panic!("a docs-scoped token pushed src/main.rs"),
            // `ApplyError` has no `Debug`, and printing a variant by hand is what the other tests
            // here do rather than deriving it for a test's convenience.
            Err(ApplyError::Other(e)) => panic!("wrong failure: {e}"),
        }

        // And the destination is untouched - no changeset, no branch head.
        let conn = crate::store::open_db(&dst).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM changesets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "the refused push wrote {count} changeset(s) anyway"
        );

        for d in [&src, &dst] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn force_still_works_on_a_server_without_tokens() {
        // The other side of #153, and the half my first fix broke. `bench/lfs_locks.py` drives
        // `force` with no credential at all and expects a 200: on a server with no tokens there is
        // nobody to verify against, so the alternative to trusting the caller is deleting the
        // feature. Guarding a check-with-nothing-behind-it is not security, it is a regression.
        let dir = std::env::temp_dir().join(format!("gesta-lfs-forceoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let lock_id;
        {
            let conn = crate::store::open_db(&dir).unwrap();
            crate::store::init_schema(&conn).unwrap();
            let l = crate::transport::lfs::lock(&conn, "shared.bin", "bob", None, 1_700_000_000)
                .unwrap();
            lock_id = l.id;
        }
        let req = Request {
            method: "POST".to_string(),
            path: "/repo.git/info/lfs/locks/unlock".to_string(),
            query: Vec::new(),
            body: format!("{{\"lock\":{{\"id\":\"{}\"}},\"force\":true}}", lock_id).into_bytes(),
            authorization: Some("Basic YWxpY2U6eA==".to_string()), // alice:x
            git_protocol: None,
        };
        // `required` false: no tokens in the file, which is what `Auth::from_entries(&[])` builds.
        let auth = crate::transport::auth::Auth::from_entries(&[]);
        let resp = route(&dir, &req, &auth, None);
        assert_eq!(
            resp.status, 200,
            "force was refused on a server with no tokens, which has nothing to check it against: {}",
            String::from_utf8_lossy(&resp.body)
        );
        let conn = crate::store::open_db(&dir).unwrap();
        assert_eq!(
            crate::transport::lfs::list(&conn, None, None, None).len(),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Open a repository at `root` for a server request.
///
/// The server is handed the directory it was started in, and the branch it serves is `main` unless
/// the repository says otherwise. Returning all three keeps the caller from guessing which of them
/// it needs.
/// What a server request needs from a repository, read in one pass.
///
/// A tuple of four would have been shorter and unreadable at the call site, and clippy is right that
/// `(Connection, String, Vec<(String, String)>, Vec<Entry>)` is not a type anybody can follow.
pub struct ServedRepository {
    /// Branch name to the git commit id it points at.
    pub refs: Vec<(String, String)>,
    /// Every object those refs reach.
    pub objects: Vec<crate::transport::gitserver::Entry>,
}

/// Pull the pack out of a v0 upload-pack response.
///
/// v0 sends `NAK` then side-band chunks. A v2 fetch needs the same bytes under a different header, so
/// the framing is removed and the pack returned whole. Doing it this way keeps one implementation of
/// what objects to send, which is the part with the reasoning in it.
fn strip_v0_framing(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= raw.len() {
        let Ok(hex) = std::str::from_utf8(&raw[i..i + 4]) else {
            break;
        };
        let len = match usize::from_str_radix(hex, 16) {
            Ok(0) => {
                i += 4;
                continue;
            }
            Ok(n) if n >= 4 => n,
            _ => break,
        };
        if i + len > raw.len() {
            break;
        }
        let payload = &raw[i + 4..i + len];
        // Band 1 carries the pack. A bare `NAK` has no band byte and is skipped.
        if payload.first() == Some(&1u8) {
            out.extend_from_slice(&payload[1..]);
        }
        i += len;
    }
    out
}

pub fn open_repo_for_serve(root: &Path) -> ZResult<ServedRepository> {
    let conn = crate::store::open_db(root)?;
    let names: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT name FROM branches WHERE head IS NOT NULL ORDER BY name")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if names.is_empty() {
        return Ok(ServedRepository {
            refs: Vec::new(),
            objects: Vec::new(),
        });
    }

    // One export, read twice: the refs a client asks for, and the objects it receives. Two exports
    // to the same temporary path in the same second collided, and the second one read a directory
    // the first had already removed, which is why the pack had a count and no bodies. The name now
    // comes from `test_scratch`, which counts instead of asking the clock, so the collision cannot
    // come back on a host whose clock is coarser than a nanosecond.
    let dir = crate::support::scratch::scratch("gesta-serve");
    std::fs::create_dir_all(&dir)?;
    let mut refs = Vec::new();
    let mut objects = Vec::new();
    if crate::cli::git_bridge::export(&conn, &dir, None).is_ok() {
        if let Ok(repo) = crate::store::gitobj::Repo::open(&dir) {
            for name in names {
                if let Ok(Some(sha)) = repo.read_head(&name) {
                    refs.push((name, sha));
                }
            }
            if let Ok(all) = repo.all_objects() {
                for (sha, kind, body) in all {
                    objects.push(crate::transport::gitserver::Entry { sha, kind, body });
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(ServedRepository { refs, objects })
}

#[cfg(test)]
mod serve_tests {
    #[test]
    fn an_export_can_be_read_back_as_objects() {
        // The server sends a pack built from an export, so the export has to come back with objects.
        // A pack with a count and no bodies is what this reads like when it does not, and git
        // reports it as a clone that succeeded and checked out nothing.
        let dir = std::env::temp_dir().join(format!("gesta-serve-t-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".gesta")).unwrap();
        let conn = crate::store::open_db(&dir).unwrap();
        crate::store::init_schema(&conn).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("d").join("b.txt"), "deep\n").unwrap();
        // A branch has to exist before a changeset can be committed onto it, which is what `init`
        // does when a person runs it. Building the store by hand skipped that and the export then
        // had nowhere to write a ref.
        conn.execute(
            "INSERT INTO branches(name, head, created_at, from_branch) VALUES('main', NULL, 0, NULL)",
            [],
        )
        .unwrap();
        crate::repo::create_changeset(&conn, &dir, "first", "t").unwrap();
        let pending: String = conn
            .query_row("SELECT id FROM changesets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        crate::repo::commit_one(&conn, &pending).unwrap();

        let exported = dir.join("out");
        crate::cli::git_bridge::export(&conn, &exported, None).unwrap();
        let repo = crate::store::gitobj::Repo::open(&exported);
        assert!(
            repo.is_ok(),
            "the export is not a git repository: {:?}",
            repo.err()
        );
        let repo = repo.unwrap();
        let objects = repo.all_objects().unwrap();
        assert!(!objects.is_empty(), "the export holds no objects");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
