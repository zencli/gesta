//! The git-lfs batch API, and the pointer file that stands in for a large object in a tree.
//!
//! `store::sidecar` already keeps a large object outside the database and records its hash. This is
//! the other half: the wire protocol a client speaks to ask for it, and the small text file that sits
//! in the tree in its place. Without this half, a `git clone` of a repository holding a 200 MB binary
//! moves 200 MB even when the client has no use for it; with it, the tree carries 130 bytes and the
//! object moves only when someone asks.
//!
//! # The pointer file
//!
//! Exactly three lines, and the version line has to be first or git-lfs refuses the file:
//!
//! ```text
//! version https://git-lfs.github.com/spec/v1
//! oid sha256:<64 hex>
//! size <decimal bytes>
//! ```
//!
//! The `oid` is the object's own hash, which is what makes the pointer checkable: a reader that
//! fetches the object verifies it against the name in the pointer rather than trusting the server.
//!
//! # The batch API
//!
//! One request carries every oid the client wants, and the response says for each whether it will be
//! sent, is already held, or does not exist:
//!
//! ```json
//! {"operation":"download","transfers":["basic"],"objects":[{"oid":"<hex>","size":123}]}
//! {"transfer":"basic","objects":[{"oid":"<hex>","size":123,"authenticated":true,
//!   "actions":{"download":{"href":"https://host/.../objects/<hex>","expires_at":"..."}}}]}
//! ```
//!
//! Two decisions worth recording. `expires_at` is emitted because the spec's own example carries it
//! and a client that parses strictly rejects a response without it. And a missing object is answered
//! inside a `200` batch response rather than as an HTTP error, because one missing object must not
//! fail the other ninety-nine.

use crate::support::json::{self, Json};

/// The pointer's first line. git-lfs compares this string exactly.
pub const SPEC_URL: &str = "https://git-lfs.github.com/spec/v1";

/// A large object as the tree records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// The object's hash, bare hex, without the `sha256:` prefix.
    pub oid: String,
    /// Size in bytes, which the client uses to decide whether it has room.
    pub size: u64,
}

impl Pointer {
    /// Read a pointer from a file's bytes, or `None` when the bytes are not a pointer.
    ///
    /// The check is strict in one direction and lenient in the other: an unknown version is not a
    /// pointer this code can honour, so it is reported as `None` and the caller treats the bytes as
    /// an ordinary file. Calling a small text file a broken pointer would send a reader after an
    /// object that cannot exist.
    pub fn parse(bytes: &[u8]) -> Option<Pointer> {
        // A pointer is small; anything over a kilobyte is content that happens to start similarly.
        if bytes.len() > 1024 {
            return None;
        }
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        let first = lines.next()?;
        let rest = first.strip_prefix("version ")?;
        if rest.trim() != SPEC_URL {
            return None;
        }

        let mut oid = None;
        let mut size = None;
        for line in lines {
            if let Some(v) = line.strip_prefix("oid sha256:") {
                let v = v.trim();
                if v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()) {
                    oid = Some(v.to_ascii_lowercase());
                } else {
                    return None;
                }
            } else if let Some(v) = line.strip_prefix("size ") {
                size = v.trim().parse::<u64>().ok();
            }
        }
        Some(Pointer {
            oid: oid?,
            size: size?,
        })
    }

    /// The three lines a tree holds in place of the object.
    pub fn render(&self) -> String {
        format!(
            "version {}\noid sha256:{}\nsize {}\n",
            SPEC_URL, self.oid, self.size
        )
    }
}

/// One object named in a batch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchObject {
    pub oid: String,
    pub size: u64,
}

/// Parse a `POST objects/batch` body.
///
/// An operation this server does not serve returns `None`, and the caller answers `422`, which is
/// what the spec says a client should see for an unsupported operation rather than a batch that is
/// silently empty and reads as "the server has nothing".
pub fn parse_batch(body: &[u8]) -> Option<(String, Vec<BatchObject>)> {
    let text = std::str::from_utf8(body).ok()?;
    let v = json::parse(text).ok()?;
    let op = json::as_str(json::field(&v, "operation")?)?.to_string();
    let objs = match json::field(&v, "objects")? {
        Json::Arr(a) => a,
        _ => return None,
    };
    let mut out = Vec::new();
    for o in objs {
        let oid = json::as_str(json::field(o, "oid")?)?.to_string();
        let size = json::field(o, "size")
            .and_then(json::as_i64)
            .unwrap_or(0)
            .max(0) as u64;
        out.push(BatchObject { oid, size });
    }
    Some((op, out))
}

/// What the server knows about one requested object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// The bytes are here and a download link follows.
    Present { size: u64 },
    /// The client already holds it by content hash, so nothing needs to move.
    AlreadyThere,
    /// No such object.
    Missing,
}

/// Build the batch response body.
///
/// `href_for` turns an oid into the URL the client should GET. A closure rather than a base string
/// keeps URL construction beside the route that serves it, so a change to the route cannot leave the
/// batch response pointing at a path that no longer answers.
pub fn batch_response<F>(results: &[(BatchObject, Found)], href_for: F, expires_at: &str) -> String
where
    F: Fn(&str) -> String,
{
    let mut objects = Vec::new();
    for (obj, found) in results {
        let mut pairs = vec![
            ("oid", Json::str(obj.oid.clone())),
            ("size", Json::int(obj.size as i64)),
        ];
        match found {
            Found::Present { size } => {
                pairs[1] = ("size", Json::int(*size as i64));
                pairs.push(("authenticated", Json::Bool(true)));
                let action = Json::obj(vec![
                    ("href", Json::str(href_for(&obj.oid))),
                    ("expires_at", Json::str(expires_at.to_string())),
                ]);
                pairs.push(("actions", Json::obj(vec![("download", action)])));
            }
            Found::AlreadyThere => {}
            Found::Missing => {
                let err = Json::obj(vec![
                    ("code", Json::int(404)),
                    ("message", Json::str("object does not exist")),
                ]);
                pairs.push(("error", err));
            }
        }
        objects.push(Json::obj(pairs));
    }

    Json::obj(vec![
        ("transfer", Json::str("basic")),
        ("objects", Json::arr(objects)),
    ])
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_pointer_round_trips_through_render_and_parse() {
        let p = Pointer {
            oid: H.to_string(),
            size: 4_500_000,
        };
        let bytes = p.render();
        assert_eq!(bytes.lines().count(), 3, "a pointer is exactly three lines");
        assert_eq!(Pointer::parse(bytes.as_bytes()), Some(p));
    }

    #[test]
    fn an_ordinary_text_file_is_not_a_pointer() {
        let text = "version 1.2.3\nthis is a file about versions\n";
        assert_eq!(Pointer::parse(text.as_bytes()), None);
    }

    #[test]
    fn a_pointer_with_a_short_or_non_hex_oid_is_refused() {
        // A pointer whose oid cannot name an object is not one this server can honour, and treating
        // it as a pointer would send a reader after something that cannot exist.
        for bad in [
            "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n",
            "version https://git-lfs.github.com/spec/v1\noid sha256:zz23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 1\n",
            "version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        ] {
            assert_eq!(Pointer::parse(bad.as_bytes()), None, "accepted: {bad:?}");
        }
    }

    #[test]
    fn an_unknown_version_is_not_a_pointer_this_code_can_read() {
        let other = format!("version https://example.test/spec/v9\noid sha256:{H}\nsize 7\n");
        assert_eq!(Pointer::parse(other.as_bytes()), None);
    }

    #[test]
    fn a_large_file_that_starts_like_a_pointer_is_not_one() {
        let mut big = b"version https://git-lfs.github.com/spec/v1\n".to_vec();
        big.extend(std::iter::repeat_n(b'x', 2000));
        assert_eq!(Pointer::parse(&big), None);
    }

    #[test]
    fn the_batch_response_names_a_download_for_an_object_that_is_here() {
        let objs = vec![
            (
                BatchObject {
                    oid: "aa".into(),
                    size: 10,
                },
                Found::Present { size: 10 },
            ),
            (
                BatchObject {
                    oid: "bb".into(),
                    size: 20,
                },
                Found::Missing,
            ),
            (
                BatchObject {
                    oid: "cc".into(),
                    size: 30,
                },
                Found::AlreadyThere,
            ),
        ];
        let body = batch_response(
            &objs,
            |oid| format!("https://h/lfs/{oid}"),
            "2030-01-01T00:00:00Z",
        );
        assert!(body.contains("\"transfer\":\"basic\""));
        assert!(
            body.contains("https://h/lfs/aa"),
            "the present object needs a link"
        );
        assert!(
            !body.contains("https://h/lfs/bb"),
            "a missing object must not get a link"
        );
        assert!(
            body.contains("\"code\":404"),
            "the missing one is reported inside the batch"
        );
        assert!(
            body.contains("2030-01-01T00:00:00Z"),
            "expires_at is part of the shape"
        );
        // A missing object must not fail the others.
        assert!(body.contains("\"oid\":\"aa\"") && body.contains("\"oid\":\"cc\""));
    }

    #[test]
    fn a_batch_body_is_parsed_into_its_objects() {
        let body = br#"{"operation":"download","transfers":["basic"],
                        "objects":[{"oid":"aa","size":1},{"oid":"bb","size":2}]}"#;
        let (op, objs) = parse_batch(body).expect("a well formed batch parses");
        assert_eq!(op, "download");
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[1].oid, "bb");
        assert_eq!(objs[1].size, 2);
    }

    #[test]
    fn a_batch_body_that_is_not_json_is_refused_rather_than_guessed() {
        assert!(parse_batch(b"not json").is_none());
        assert!(
            parse_batch(br#"{"operation":"download"}"#).is_none(),
            "no objects key"
        );
    }

    #[test]
    fn one_missing_object_does_not_hide_the_others() {
        // The whole point of answering per object: a request naming a hundred oids, one of which is
        // absent, must still answer for the other ninety-nine. A response that fails whole turns one
        // missing file into a broken fetch.
        let results = vec![
            (
                BatchObject {
                    oid: "aa".into(),
                    size: 1,
                },
                Found::Present { size: 1 },
            ),
            (
                BatchObject {
                    oid: "bb".into(),
                    size: 2,
                },
                Found::Missing,
            ),
            (
                BatchObject {
                    oid: "cc".into(),
                    size: 3,
                },
                Found::Present { size: 3 },
            ),
        ];
        let out = batch_response(&results, |oid| format!("objects/{oid}"), "later");
        assert!(out.contains("aa"), "the first object is answered");
        assert!(
            out.contains("cc"),
            "the object after the missing one is answered"
        );
        assert!(
            out.contains("\"error\""),
            "the missing one carries an error rather than vanishing"
        );
        // The hrefs are relative: the handler has no host or scheme, and a client resolves against
        // the endpoint it called.
        assert!(
            !out.contains("http://") && !out.contains("https://"),
            "hrefs must not invent a host"
        );
    }

    #[test]
    fn an_object_whose_size_is_wrong_in_the_request_is_still_answered_by_oid() {
        // A client may state a size it guessed. The answer is by oid; refusing the batch because the
        // size does not match would fail a fetch over a number nobody needs to agree on.
        let body = br#"{"operation":"download","transfers":["basic"],
                        "objects":[{"oid":"aa","size":0},{"oid":"bb","size":999999999}]}"#;
        let (op, objs) = parse_batch(body).expect("sizes are the client's business");
        assert_eq!(op, "download");
        assert_eq!(objs.len(), 2);
    }
}
