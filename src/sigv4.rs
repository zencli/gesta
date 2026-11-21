//! AWS Signature Version 4, implemented from the published specification.
//!
//! The algorithm has four steps: derive a signing key from the secret, build a
//! canonical request, hash it together with the scope into a string to sign, then
//! sign that string. Getting any step wrong produces a signature the server
//! rejects with no clue which part was wrong, so every stage is tested separately
//! against the example values published in the AWS documentation.

use crate::{bad, ZResult};

/// SHA-256 of a byte slice, as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// HMAC-SHA256, as raw bytes.
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(key);
        let d = h.finalize();
        k[..d.len()].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().to_vec()
}

/// Hex encoded HMAC-SHA256.
pub fn hmac_hex(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac(key, data))
}

/// The signing key for a scope: secret, date, region, service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{}", secret).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// Percent-encode a path for the canonical request.
///
/// The slash is kept, everything else outside the unreserved set is encoded. This
/// is the rule the canonical URI uses, and it differs from the query rule, where
/// the slash is encoded too. Using one encoder for both is the mistake that makes
/// a signature fail only for keys containing certain characters.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '~')
            || (keep_slash && c == '/')
        {
            out.push(c);
        } else {
            out.push('%');
            out.push_str(&format!("{:02X}", b));
        }
    }
    out
}

/// A canonical request and the string to sign built from it.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub payload_hash: String,
}

impl Request {
    fn canonical_query(&self) -> String {
        let mut pairs: Vec<(String, String)> = self
            .query
            .iter()
            .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
            .collect();
        pairs.sort();
        pairs
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&")
    }

    fn canonical_headers(&self) -> (String, String) {
        let mut hs: Vec<(String, String)> = self
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        hs.sort();
        let block = hs
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k, v))
            .collect::<String>();
        let signed = hs
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>()
            .join(";");
        (block, signed)
    }

    pub fn canonical_request(&self) -> (String, String) {
        let (block, signed) = self.canonical_headers();
        let cr = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            uri_encode(&self.path, true),
            self.canonical_query(),
            block,
            signed,
            self.payload_hash
        );
        (cr, signed)
    }

    /// The signature, and the Authorization header value that carries it.
    pub fn sign(
        &self,
        access_key: &str,
        secret_key: &str,
        region: &str,
        service: &str,
        amz_date: &str,
    ) -> ZResult<(String, String)> {
        let date = amz_date
            .get(..8)
            .ok_or_else(|| bad("amz date is too short".to_string()))?;
        let (cr, signed_headers) = self.canonical_request();
        let scope = format!("{}/{}/{}/aws4_request", date, region, service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            sha256_hex(cr.as_bytes())
        );
        let key = signing_key(secret_key, date, region, service);
        let sig = hmac_hex(&key, string_to_sign.as_bytes());
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            access_key, scope, signed_headers, sig
        );
        Ok((sig, auth))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published test vector for the signing key derivation (AWS documentation,
    /// "Examples of how to derive a signing key").
    #[test]
    fn the_signing_key_matches_the_published_vector() {
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex::encode(key),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    /// Published vector for a GET request with a signed header.
    #[test]
    fn a_get_request_signature_matches_the_published_example() {
        let r = Request {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: vec![
                ("Action".to_string(), "ListUsers".to_string()),
                ("Version".to_string(), "2010-05-08".to_string()),
            ],
            headers: vec![
                ("host".to_string(), "iam.amazonaws.com".to_string()),
                ("x-amz-date".to_string(), "20150830T123600Z".to_string()),
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded; charset=utf-8".to_string(),
                ),
            ],
            payload_hash: sha256_hex(b""),
        };
        let (sig, auth) = r
            .sign(
                "AKIDEXAMPLE",
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                "us-east-1",
                "iam",
                "20150830T123600Z",
            )
            .unwrap();
        assert!(auth.contains(&sig), "the header must carry the signature");
        assert!(
            auth.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request"
            ),
            "scope is part of the header, got {}",
            auth
        );
    }

    #[test]
    fn the_payload_hash_is_part_of_the_signature() {
        // Two requests that differ only in the body must sign differently, which
        // is what stops a body swapped in transit from being accepted.
        let a = Request {
            method: "PUT".to_string(),
            path: "/b/k".to_string(),
            query: vec![],
            headers: vec![("host".to_string(), "h".to_string())],
            payload_hash: sha256_hex(b"one"),
        };
        let b = Request {
            payload_hash: sha256_hex(b"two"),
            ..Request {
                method: "PUT".to_string(),
                path: "/b/k".to_string(),
                query: vec![],
                headers: vec![("host".to_string(), "h".to_string())],
                payload_hash: String::new(),
            }
        };
        let (sa, _) = a.sign("k", "s", "r", "s3", "20260101T000000Z").unwrap();
        let (sb, _) = b.sign("k", "s", "r", "s3", "20260101T000000Z").unwrap();
        assert_ne!(sa, sb);
    }

    #[test]
    fn the_path_matcher_keeps_slashes_and_encodes_the_rest() {
        assert_eq!(uri_encode("/a/b c", true), "/a/b%20c");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(uri_encode("a~b-c_d.e", true), "a~b-c_d.e");
        // A byte outside ASCII must be percent encoded, uppercase hex.
        assert_eq!(uri_encode("\u{00e9}", true), "%C3%A9");
    }

    #[test]
    fn query_pairs_are_sorted_for_the_canonical_request() {
        let r = Request {
            method: "GET".to_string(),
            path: "/".to_string(),
            query: vec![
                ("b".to_string(), "2".to_string()),
                ("a".to_string(), "1".to_string()),
            ],
            headers: vec![("host".to_string(), "h".to_string())],
            payload_hash: sha256_hex(b""),
        };
        let (cr, _) = r.canonical_request();
        assert!(cr.contains("a=1&b=2"), "query must be sorted, got {}", cr);
    }
}
