//! Storing a bundle in S3-compatible object storage.
//!
//! This is a remote that needs no server: push uploads one object, pull downloads
//! it. Any S3-compatible endpoint works (IDrive e2, Backblaze B2, MinIO, R2), so
//! a single machine can keep an off-site copy without running anything.
//!
//! Requests are signed with AWS Signature Version 4, implemented here in about a
//! hundred lines rather than pulled in as a dependency. The algorithm is fixed and
//! published; the risk of getting it wrong is covered by testing against the
//! published test vectors, which is cheaper than auditing a new crate tree.
//!
//! Two properties matter for correctness:
//!
//! 1. The payload hash is part of the signature, so a body that changes in transit
//!    is rejected by the server rather than silently stored.
//! 2. A push is a single PUT, so an interrupted push leaves the previous object
//!    intact instead of a half-written one.

use crate::transport::sigv4::{self, Request};
use crate::{bad, ZResult};

/// Credentials and endpoint for one bucket.
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

impl S3Config {
    /// Parse an `s3://` remote URL, taking credentials from the environment.
    ///
    /// The URL carries no secrets: `s3://bucket/prefix` plus `GESTA_S3_ENDPOINT`,
    /// `GESTA_S3_REGION`, `GESTA_S3_ACCESS_KEY` and `GESTA_S3_SECRET_KEY`. Putting
    /// a secret in a URL means it ends up in shell history and in the repository
    /// config, which is a copy nobody rotates.
    /// Whether a plain http endpoint is allowed, given the endpoint text.
    ///
    /// Split out of `from_url` so it can be tested without writing the process environment. A test
    /// that sets `GESTA_S3_ENDPOINT` shares that variable with every other test in the binary, and
    /// the two that did exactly that failed on a runner whose thread schedule differed and nowhere
    /// else.
    pub fn plain_http_is_allowed(endpoint: &str) -> bool {
        match endpoint.strip_prefix("http://") {
            None => true,
            Some(rest) => {
                // A bracketed IPv6 literal holds colons, so splitting on the first colon would cut
                // `[::1]:9000` down to `[`. Take the bracket first, then the port.
                let host = if let Some(after) = rest.strip_prefix('[') {
                    after.split(']').next().unwrap_or("")
                } else {
                    rest.split(['/', ':']).next().unwrap_or("")
                };
                host == "127.0.0.1" || host == "localhost" || host == "::1"
            }
        }
    }

    pub fn from_url(url: &str) -> ZResult<(S3Config, String)> {
        let rest = url
            .strip_prefix("s3://")
            .ok_or_else(|| bad(format!("not an s3 url: {}", url)))?;
        let mut parts = rest.splitn(2, '/');
        let bucket = parts.next().unwrap_or("").to_string();
        let prefix = parts.next().unwrap_or("").trim_matches('/').to_string();
        if bucket.is_empty() {
            return Err(bad("s3 url needs a bucket: s3://bucket/prefix".to_string()));
        }
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let endpoint = env("GESTA_S3_ENDPOINT");
        if endpoint.is_empty() {
            return Err(bad(
                "set GESTA_S3_ENDPOINT to the bucket endpoint, for example https://x.region.idrivee2.com"
                    .to_string(),
            ));
        }
        // A plain endpoint sends the signature and the object bodies in the clear. The signature
        // is not a password, but it identifies the key and it covers the payload, so anyone on
        // the path reads the repository and can replay within the signing window. Loopback is
        // allowed because a local test server cannot be read by anyone else.
        if !S3Config::plain_http_is_allowed(&endpoint) {
            return Err(bad(format!(
                "refusing a plain http endpoint ({}): the signature and the content would travel in the clear. Use https, or 127.0.0.1 for a local test server",
                endpoint
            )));
        }
        // Credentials come from the environment first, then from the encrypted
        // store. The order matters: an environment variable is what a test or a
        // one-off job sets, and the store is what a person sets once and forgets.
        let mut access_key = env("GESTA_S3_ACCESS_KEY");
        let mut secret_key = env("GESTA_S3_SECRET_KEY");
        if access_key.is_empty() || secret_key.is_empty() {
            if let Ok(store) = crate::support::secrets::SecretStore::from_env() {
                if let Ok(entries) = store.load() {
                    if access_key.is_empty() {
                        access_key = entries.get("s3.access_key").cloned().unwrap_or_default();
                    }
                    if secret_key.is_empty() {
                        secret_key = entries.get("s3.secret_key").cloned().unwrap_or_default();
                    }
                }
            }
        }
        if access_key.is_empty() || secret_key.is_empty() {
            return Err(bad(
                "no credentials: run `gesta secret set s3.access_key` and `gesta secret set s3.secret_key`, or set GESTA_S3_ACCESS_KEY and GESTA_S3_SECRET_KEY"
                    .to_string(),
            ));
        }
        let region = {
            let r = env("GESTA_S3_REGION");
            if r.is_empty() {
                "us-east-1".to_string()
            } else {
                r
            }
        };
        Ok((
            S3Config {
                endpoint,
                region,
                bucket,
                access_key,
                secret_key,
            },
            prefix,
        ))
    }

    /// Host and port from the endpoint, and whether the connection is plain.
    fn host_port(&self) -> ZResult<(String, u16, bool)> {
        let mut rest = self.endpoint.trim();
        let tls = if let Some(r) = rest.strip_prefix("https://") {
            rest = r;
            true
        } else if let Some(r) = rest.strip_prefix("http://") {
            rest = r;
            false
        } else {
            return Err(bad(
                "endpoint must start with https:// or http://".to_string()
            ));
        };
        let rest = rest.trim_end_matches('/');
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
                h.to_string(),
                p.parse::<u16>().map_err(|e| bad(e.to_string()))?,
            ),
            _ => (rest.to_string(), if tls { 443 } else { 80 }),
        };
        Ok((host, port, tls))
    }
}

/// The object key for a branch, inside the configured prefix.
pub fn key_for(prefix: &str, branch: &str) -> String {
    if prefix.is_empty() {
        format!("{}.bundle", branch)
    } else {
        format!("{}/{}.bundle", prefix, branch)
    }
}

impl S3Config {
    /// The URL and Host header value for one object.
    fn object_target(&self, key: &str) -> ZResult<(String, String, String)> {
        let (host, port, tls) = self.host_port()?;
        let scheme = if tls { "https" } else { "http" };
        // The Host header is signed, so it has to carry the port whenever the
        // request uses one the scheme does not imply.
        let authority = if (tls && port == 443) || (!tls && port == 80) {
            host.clone()
        } else {
            format!("{}:{}", host, port)
        };
        let path = format!("/{}/{}", self.bucket, key);
        Ok((
            format!("{}://{}{}", scheme, authority, path),
            authority,
            path,
        ))
    }

    /// Build and sign a request for one object.
    ///
    /// The signature covers lowercased header names and their trimmed values. The
    /// same values are then sent, with the original spelling, because a header the
    /// signature covers but the request omits is rejected.
    fn signed(
        &self,
        method: &str,
        key: &str,
        payload: &[u8],
        amz_date: &str,
    ) -> ZResult<(String, Vec<(String, String)>)> {
        let (url, host, path) = self.object_target(key)?;
        let payload_hash = sigv4::sha256_hex(payload);

        let mut canon = vec![
            ("host".to_string(), host.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.to_string()),
        ];
        if method == "PUT" {
            canon.push(("content-length".to_string(), payload.len().to_string()));
        }
        let req = Request {
            method: method.to_string(),
            path,
            query: Vec::new(),
            headers: canon.clone(),
            payload_hash: payload_hash.clone(),
        };
        let (_sig, auth) = req.sign(
            &self.access_key,
            &self.secret_key,
            &self.region,
            "s3",
            amz_date,
        )?;

        let mut out: Vec<(String, String)> = canon
            .into_iter()
            .map(|(k, v)| {
                let name = match k.as_str() {
                    "host" => "Host".to_string(),
                    "content-length" => "Content-Length".to_string(),
                    other => other.to_string(),
                };
                (name, v)
            })
            .collect();
        out.push(("Authorization".to_string(), auth));
        Ok((url, out))
    }

    /// Upload one object.
    pub fn put(&self, key: &str, body: &[u8]) -> ZResult<()> {
        let now = crate::support::clock::unix_now();
        let amz_date = crate::support::clock::format_amz_date(now);
        let (url, headers) = self.signed("PUT", key, body, &amz_date)?;
        let (status, resp) =
            crate::transport::http::request_with_headers("PUT", &url, &headers, Some(body))?;
        if !(200..300).contains(&status) {
            return Err(bad(format!(
                "upload failed with status {}: {}",
                status,
                String::from_utf8_lossy(&resp)
                    .chars()
                    .take(300)
                    .collect::<String>()
            )));
        }
        Ok(())
    }

    /// Download one object.
    pub fn get(&self, key: &str) -> ZResult<Option<Vec<u8>>> {
        let now = crate::support::clock::unix_now();
        let amz_date = crate::support::clock::format_amz_date(now);
        let (url, headers) = self.signed("GET", key, b"", &amz_date)?;
        let (status, resp) =
            crate::transport::http::request_with_headers("GET", &url, &headers, None)?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(bad(format!(
                "download failed with status {}: {}",
                status,
                String::from_utf8_lossy(&resp)
                    .chars()
                    .take(300)
                    .collect::<String>()
            )));
        }
        Ok(Some(resp))
    }

    /// List the branch names stored under a prefix.
    ///
    /// One request with `list-type=2` and a prefix, which every S3 implementation
    /// supports. A bucket holds `<prefix>/<branch>.bundle`, so stripping the known
    /// prefix and suffix leaves the branch.
    pub fn list_branches(&self, prefix: &str) -> ZResult<Vec<String>> {
        let now = crate::support::clock::unix_now();
        let amz_date = crate::support::clock::format_amz_date(now);
        let (host, port, tls) = self.host_port()?;
        let scheme = if tls { "https" } else { "http" };
        let authority = if (tls && port == 443) || (!tls && port == 80) {
            host.clone()
        } else {
            format!("{}:{}", host, port)
        };
        let path = format!("/{}", self.bucket);
        let query = vec![
            ("list-type".to_string(), "2".to_string()),
            (
                "prefix".to_string(),
                if prefix.is_empty() {
                    String::new()
                } else {
                    format!("{}/", prefix)
                },
            ),
            ("max-keys".to_string(), "1000".to_string()),
        ];
        let payload_hash = sigv4::sha256_hex(b"");
        let canon = vec![
            ("host".to_string(), authority.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.to_string()),
        ];
        let req = Request {
            method: "GET".to_string(),
            path: path.clone(),
            query: query.clone(),
            headers: canon.clone(),
            payload_hash,
        };
        let (_sig, auth) = req.sign(
            &self.access_key,
            &self.secret_key,
            &self.region,
            "s3",
            &amz_date,
        )?;
        let mut out: Vec<(String, String)> = canon
            .into_iter()
            .map(|(k, v)| {
                let name = if k == "host" { "Host".to_string() } else { k };
                (name, v)
            })
            .collect();
        out.push(("Authorization".to_string(), auth));

        let qs = query
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    sigv4::uri_encode(k, false),
                    sigv4::uri_encode(v, false)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{}://{}{}?{}", scheme, authority, path, qs);

        let (status, body) = crate::transport::http::request_with_headers("GET", &url, &out, None)?;
        if !(200..300).contains(&status) {
            return Err(bad(format!(
                "listing failed with status {}: {}",
                status,
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        let text = String::from_utf8_lossy(&body);
        let mut names = Vec::new();
        let open = "<Key>";
        let close = "</Key>";
        let mut rest = text.as_ref();
        while let Some(s) = rest.find(open) {
            let after = &rest[s + open.len()..];
            let Some(e) = after.find(close) else { break };
            let key = &after[..e];
            if let Some(branch) = key
                .strip_prefix(&if prefix.is_empty() {
                    String::new()
                } else {
                    format!("{}/", prefix)
                })
                .and_then(|k| k.strip_suffix(".bundle"))
            {
                names.push(branch.to_string());
            }
            rest = &after[e + close.len()..];
        }
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> S3Config {
        S3Config {
            endpoint: "https://x.us-east-1.idrivee2.com".to_string(),
            region: "us-east-1".to_string(),
            bucket: "b".to_string(),
            // Published example key from the AWS documentation, used as a test
            // vector only. Not a credential.
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        }
    }

    #[test]
    fn the_endpoint_splits_into_host_port_and_scheme() {
        let (h, p, tls) = cfg().host_port().unwrap();
        assert_eq!(h, "x.us-east-1.idrivee2.com");
        assert_eq!(p, 443);
        assert!(tls);

        let mut c = cfg();
        c.endpoint = "http://localhost:9000".to_string();
        let (h, p, tls) = c.host_port().unwrap();
        assert_eq!((h.as_str(), p, tls), ("localhost", 9000, false));
    }

    #[test]
    fn a_key_uses_the_prefix_when_there_is_one() {
        assert_eq!(key_for("", "main"), "main.bundle");
        assert_eq!(key_for("repos/gesta", "main"), "repos/gesta/main.bundle");
    }

    #[test]
    fn a_url_without_a_bucket_is_refused() {
        assert!(S3Config::from_url("s3:///x").is_err());
        assert!(S3Config::from_url("https://example.com").is_err());
    }
    #[test]
    fn a_plain_http_endpoint_is_refused_because_the_signature_would_be_readable() {
        // No environment is written: the decision is a pure function, so this cannot race the tests
        // that read the environment. The earlier version wrote GESTA_S3_ENDPOINT and failed on a
        // runner whose thread schedule differed, which looked like a Windows bug and was not one.
        assert!(!S3Config::plain_http_is_allowed("http://s3.example.com"));
        assert!(!S3Config::plain_http_is_allowed("http://10.0.0.1:9000"));
    }

    #[test]
    fn a_loopback_endpoint_is_allowed_for_a_local_test_server() {
        assert!(S3Config::plain_http_is_allowed("http://127.0.0.1:9303"));
        assert!(S3Config::plain_http_is_allowed("http://localhost:9000/x"));
        assert!(S3Config::plain_http_is_allowed("http://[::1]:9000"));
        assert!(S3Config::plain_http_is_allowed("https://s3.example.com"));
    }

    #[test]
    fn an_https_endpoint_is_accepted() {
        // The scheme check only, because the rest of `from_url` reads credentials from the process
        // environment or the secret store, and both are process-wide.
        assert!(S3Config::plain_http_is_allowed(
            "https://s3.ap-southeast-2.example.com"
        ));
        assert!(S3Config::plain_http_is_allowed("https://s3.amazonaws.com"));
    }
}
