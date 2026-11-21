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
            if let Err(e) = stream.read_to_end(&mut raw) {
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
        stream.read_to_end(&mut raw)?;
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

    let declared = head_text.split("\r\n").find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.eq_ignore_ascii_case("content-length") {
            v.trim().parse::<usize>().ok()
        } else {
            None
        }
    });

    let payload = raw[split + 4..].to_vec();
    let payload = match declared {
        Some(n) if n <= payload.len() => payload[..n].to_vec(),
        _ => payload,
    };
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
    if auth.required {
        println!("access: {} token(s) configured", auth.token_count());
    } else {
        return Err(bad(format!(
            "refusing to serve TLS on {} without tokens: add entries to .gesta/auth",
            bind
        )));
    }
    println!("press Ctrl-C to stop");

    let auth_path = Arc::new(crate::repo_dir(&root).join("auth"));
    let root = Arc::new(root);
    let auth = Arc::new(std::sync::Mutex::new(auth));
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        let root = Arc::clone(&root);
        let auth = Arc::clone(&auth);
        let auth_path = Arc::clone(&auth_path);
        let cfg = Arc::clone(&cfg);
        std::thread::spawn(move || {
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
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        let root = Arc::clone(&root);
        let auth = Arc::clone(&auth);
        let auth_path = Arc::clone(&auth_path);
        let forward = Arc::clone(&forward);
        std::thread::spawn(move || {
            let _ = handle_connection(stream, &root, &auth, &auth_path, forward.as_ref().as_ref());
        });
    }
    Ok(())
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
pub fn route(
    root: &Path,
    req: &Request,
    auth: &Auth,
    forward: Option<&crate::support::audit_forward::Forwarder>,
) -> Response {
    let path = req.path.trim_end_matches('/');

    // Health is open so a monitor can tell the process is alive without a
    // credential. Every other route needs one when the server has tokens.
    let needed = match (req.method.as_str(), path) {
        ("GET", "/v1/health") => None,
        (_, p) if p.starts_with("/v1/bundle") && req.method == "POST" => Some(Role::Write),
        (_, "/v1/changeset") if req.method == "POST" => Some(Role::Write),
        (_, "/v1/changeset") if req.method == "GET" || req.method == "HEAD" => Some(Role::Read),
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
        (_, p) if p.starts_with("/v1/") => Some(Role::Read),
        _ => None,
    };

    let mut who = "anonymous".to_string();
    let mut grant: Option<crate::transport::auth::Grant> = None;
    // Recorded once the decision is made. A request with no credential requirement is not an event
    // worth a line, but one that was authenticated is.
    let mut record_allowed = false;
    if let Some(needed) = needed {
        if auth.required {
            let presented = req
                .authorization
                .as_deref()
                .and_then(|v| v.strip_prefix("Bearer "))
                .or_else(|| {
                    req.authorization
                        .as_deref()
                        .and_then(|v| v.strip_prefix("Basic "))
                        .and_then(decode_basic_token)
                        .map(|s| Box::leak(s.into_boxed_str()) as &str)
                });
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
            match fetch_bundle(root, branch, &have) {
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
            match crate::transport::gitserver::receive_pack_allowing(root, &req.body, allowed, &who)
            {
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
                Some(p) => crate::normalise(p),
                None => return Response::error(400, "Bad Request", "path is required"),
            };
            let conn = match crate::store::open_db(root) {
                Ok(c) => c,
                Err(e) => return Response::error(500, "Internal Server Error", e.to_string()),
            };
            let rev = match req.param("rev") {
                Some(r) => r.to_string(),
                None => {
                    // No rev means "what the branch looks like right now", which
                    // includes changesets still queued. Reading only the head
                    // would make a file written by an API call invisible until
                    // somebody committed it.
                    let branch = match crate::store::current_branch(&conn) {
                        Ok(b) => b,
                        Err(e) => {
                            return Response::error(500, "Internal Server Error", e.to_string())
                        }
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
                            &who,
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
            match receive_bundle(root, branch, &req.body, &who) {
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
            &conn, message, author, branch, &files, &delete,
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

fn fetch_bundle(root: &Path, branch: &str, have: &HashSet<String>) -> ZResult<Option<Vec<u8>>> {
    let conn = open_db(root)?;
    init_schema(&conn)?;
    if branch_head(&conn, branch).is_err() {
        return Ok(None);
    }
    let bundle = remote::build_bundle(&conn, branch, have)?;
    Ok(Some(bundle.encode()?))
}

pub enum ApplyError {
    /// The push would lose changes: the remote head is not an ancestor.
    Rejected(String),
    Other(Box<dyn std::error::Error>),
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

fn receive_bundle(root: &Path, branch: &str, body: &[u8], who: &str) -> Result<String, ApplyError> {
    let conn = open_db(root)?;
    init_schema(&conn)?;

    let bundle = Bundle::decode(body)?;
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
    let mut url = format!(
        "{}/v1/bundle?branch={}",
        base.trim_end_matches('/'),
        url_encode(branch)
    );
    if !have.is_empty() {
        url.push_str("&have=");
        url.push_str(&have.join("%2C"));
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
    // A caller that signs Content-Length has to send the same value, so it is
    // passed in and not added again here. Two of them make nginx answer 400 with
    // no hint that framing was the problem.
    let caller_sets_length = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
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
            if let Err(e) = stream.read_to_end(&mut raw) {
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
        stream
            .read_to_end(&mut raw)
            .map_err(|e| bad(format!("reading the response failed: {}", e)))?;
    }
    let (status, body) = parse_response(&raw)?;
    Ok((status, body))
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
    Ok((status, raw[split + 4..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    // the first had already removed, which is why the pack had a count and no bodies.
    let dir = std::env::temp_dir().join(format!(
        "gesta-serve-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
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
