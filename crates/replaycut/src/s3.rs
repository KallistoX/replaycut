//! S3-compatible storage (since 2.5): AWS S3, Cloudflare R2, Backblaze B2,
//! MinIO, Wasabi. Requests are signed with Signature Version 4 (header
//! signature, unsigned payload); objects go to `<prefix>/<month>/<file>`,
//! the link is either `<publicBase>/<key>` or a presigned GET.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::integrations::Published;

pub struct S3 {
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    public_base: String,
    presign_days: u32,
    access_key: String,
    secret_key: String,
    client: reqwest::Client,
}

/// A request to sign: method, URL path (already percent-encoded), sorted
/// query pairs (encoded), and the headers that take part.
struct Canonical<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a [(String, String)],
    host: &'a str,
    amz_date: &'a str,
    payload_hash: &'a str,
    content_type: Option<&'a str>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(msg)
        .finalize();
    let outer = Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    out
}

/// AWS URI encoding: everything but unreserved characters, `/` kept in paths.
fn aws_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Signing key, canonical request hash and the `Authorization` header value
/// (or, for presigning, the signature) per the SigV4 recipe.
fn sign(
    secret_key: &str,
    access_key: &str,
    region: &str,
    c: &Canonical,
    presign: bool,
) -> (String, String) {
    let date = &c.amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let mut headers: Vec<(String, String)> = vec![("host".into(), c.host.to_string())];
    if !presign {
        headers.push(("x-amz-content-sha256".into(), c.payload_hash.to_string()));
        headers.push(("x-amz-date".into(), c.amz_date.to_string()));
        if let Some(ct) = c.content_type {
            headers.push(("content-type".into(), ct.to_string()));
        }
    }
    headers.sort();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let canonical_query = c
        .query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        c.method, c.path, canonical_query, canonical_headers, signed_headers, c.payload_hash
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        c.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );
    (authorization, signature)
}

fn now_amz() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

impl S3 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        prefix: &str,
        public_base: &str,
        presign_days: u32,
        access_key: String,
        secret_key: String,
    ) -> Result<Self> {
        anyhow::ensure!(
            endpoint.starts_with("http://") || endpoint.starts_with("https://"),
            "the S3 endpoint must start with http:// or https://"
        );
        anyhow::ensure!(!bucket.trim().is_empty(), "the S3 bucket is empty");
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            region: if region.trim().is_empty() {
                "auto".to_string()
            } else {
                region.trim().to_string()
            },
            bucket: bucket.trim().trim_matches('/').to_string(),
            prefix: prefix.trim().trim_matches('/').to_string(),
            public_base: public_base.trim().trim_end_matches('/').to_string(),
            presign_days: presign_days.min(7),
            access_key,
            secret_key,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    /// Object key for a file: `<prefix>/<month>/<file>` (no leading slash).
    pub fn key(&self, month: &str, file_name: &str) -> String {
        if self.prefix.is_empty() {
            format!("{month}/{file_name}")
        } else {
            format!("{}/{month}/{file_name}", self.prefix)
        }
    }

    fn host(&self) -> String {
        self.endpoint
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    }

    /// Path-style: `/<bucket>/<key>`, each segment encoded the AWS way.
    fn object_path(&self, key: &str) -> String {
        format!(
            "/{}/{}",
            aws_encode(&self.bucket, false),
            aws_encode(key, true)
        )
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<reqwest::Body>,
        content_type: Option<&str>,
        content_length: Option<u64>,
    ) -> Result<reqwest::Response> {
        let amz_date = now_amz();
        let payload_hash = "UNSIGNED-PAYLOAD";
        let host = self.host();
        let (authorization, _) = sign(
            &self.secret_key,
            &self.access_key,
            &self.region,
            &Canonical {
                method: method.as_str(),
                path,
                query: &[],
                host: &host,
                amz_date: &amz_date,
                payload_hash,
                content_type,
            },
            false,
        );
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.endpoint))
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header(reqwest::header::AUTHORIZATION, authorization);
        if let Some(ct) = content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, ct);
        }
        if let Some(len) = content_length {
            req = req.header(reqwest::header::CONTENT_LENGTH, len);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        Ok(req.send().await?)
    }

    /// The link for a key: the public base when configured, else a presigned GET.
    pub fn link(&self, key: &str) -> String {
        if !self.public_base.is_empty() {
            return format!("{}/{}", self.public_base, aws_encode(key, true));
        }
        let amz_date = now_amz();
        let expires = (self.presign_days.max(1) as u64) * 86_400;
        let scope = format!("{}/{}/s3/aws4_request", &amz_date[..8], self.region);
        let mut query: Vec<(String, String)> = vec![
            ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
            (
                "X-Amz-Credential".into(),
                aws_encode(&format!("{}/{scope}", self.access_key), false),
            ),
            ("X-Amz-Date".into(), amz_date.clone()),
            ("X-Amz-Expires".into(), expires.to_string()),
            ("X-Amz-SignedHeaders".into(), "host".into()),
        ];
        query.sort();
        let path = self.object_path(key);
        let host = self.host();
        let (_, signature) = sign(
            &self.secret_key,
            &self.access_key,
            &self.region,
            &Canonical {
                method: "GET",
                path: &path,
                query: &query,
                host: &host,
                amz_date: &amz_date,
                payload_hash: "UNSIGNED-PAYLOAD",
                content_type: None,
            },
            true,
        );
        let qs = query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{}{path}?{qs}&X-Amz-Signature={signature}", self.endpoint)
    }

    pub async fn publish(&self, file: &Path, month: &str) -> Result<Published> {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let key = self.key(month, &name);
        let f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let len = f.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let res = self
            .request(
                reqwest::Method::PUT,
                &self.object_path(&key),
                Some(body),
                Some("video/mp4"),
                Some(len),
            )
            .await
            .context("S3 upload")?;
        if !res.status().is_success() {
            bail!(
                "S3 upload: HTTP {} {}",
                res.status(),
                first_line(&res.text().await.unwrap_or_default())
            );
        }
        let link = self.link(&key);
        Ok(Published {
            page: link.clone(),
            direct: link,
            path: key,
        })
    }

    /// Delete objects by key; missing ones do not count as errors.
    pub async fn delete(&self, keys: &[String]) -> Result<usize> {
        let mut n = 0;
        for key in keys {
            let res = self
                .request(
                    reqwest::Method::DELETE,
                    &self.object_path(key),
                    None,
                    None,
                    None,
                )
                .await?;
            match res.status().as_u16() {
                200 | 204 => n += 1,
                404 => {}
                s => bail!("S3 delete {key}: HTTP {s}"),
            }
        }
        Ok(n)
    }

    /// HEAD the bucket: reachable and the keys are accepted (the diagnostics row).
    pub async fn head_bucket(&self) -> Result<()> {
        let res = self
            .request(
                reqwest::Method::HEAD,
                &format!("/{}", aws_encode(&self.bucket, false)),
                None,
                None,
                None,
            )
            .await
            .context("S3 endpoint")?;
        if !res.status().is_success() {
            bail!(
                "bucket {}: HTTP {} - check endpoint, bucket and keys",
                self.bucket,
                res.status()
            );
        }
        Ok(())
    }

    /// The connection test: HEAD the bucket, then PUT and DELETE a probe object.
    pub async fn probe(&self) -> Result<()> {
        self.head_bucket().await?;
        let key = self.key("probe", ".replaycut-probe");
        let res = self
            .request(
                reqwest::Method::PUT,
                &self.object_path(&key),
                Some(reqwest::Body::from("replaycut")),
                Some("text/plain"),
                Some(9),
            )
            .await?;
        if !res.status().is_success() {
            bail!(
                "probe upload: HTTP {} {}",
                res.status(),
                first_line(&res.text().await.unwrap_or_default())
            );
        }
        self.delete(std::slice::from_ref(&key)).await?;
        Ok(())
    }

    pub fn describe_link_mode(&self) -> String {
        if self.public_base.is_empty() {
            format!("presigned links, {} day(s)", self.presign_days.max(1))
        } else {
            format!("public links under {}", self.public_base)
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `get-vanilla` vector from the AWS SigV4 test suite (service
    /// `service`, region us-east-1): signing key and canonical request are
    /// the same recipe as for S3, so the signature must match.
    #[test]
    fn sigv4_matches_the_aws_test_vector() {
        // the vector uses service "service"; we hard-wire "s3", so redo the last
        // steps here with the vector's own values to check the primitives
        let date = "20150830";
        let k_date = hmac_sha256(
            b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, b"us-east-1");
        let k_service = hmac_sha256(&k_region, b"service");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let canonical = "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
        assert_eq!(
            signature,
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn hmac_and_encoding_primitives() {
        // RFC 4231 test case 2
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            aws_encode("2026-09/a b+c.mp4", true),
            "2026-09/a%20b%2Bc.mp4"
        );
        assert_eq!(
            aws_encode("AKID/20150830/us-east-1", false),
            "AKID%2F20150830%2Fus-east-1"
        );
    }

    #[test]
    fn keys_paths_and_links() {
        let s3 = S3::new(
            "https://acct.r2.cloudflarestorage.com/",
            "auto",
            "clips",
            "replaycut/",
            "https://clips.example.com",
            0,
            "AK".into(),
            "SK".into(),
        )
        .unwrap();
        assert_eq!(s3.key("2026-09", "a b.mp4"), "replaycut/2026-09/a b.mp4");
        assert_eq!(
            s3.object_path("replaycut/2026-09/a b.mp4"),
            "/clips/replaycut/2026-09/a%20b.mp4"
        );
        assert_eq!(
            s3.link("replaycut/2026-09/a b.mp4"),
            "https://clips.example.com/replaycut/2026-09/a%20b.mp4"
        );
        let presigned = S3::new(
            "http://127.0.0.1:9000",
            "us-east-1",
            "b",
            "",
            "",
            3,
            "AK".into(),
            "SK".into(),
        )
        .unwrap();
        let link = presigned.link("x/y.mp4");
        assert!(
            link.starts_with("http://127.0.0.1:9000/b/x/y.mp4?X-Amz-Algorithm=AWS4-HMAC-SHA256&"),
            "{link}"
        );
        assert!(
            link.contains("X-Amz-Expires=259200") && link.contains("&X-Amz-Signature="),
            "{link}"
        );
        assert!(S3::new("ftp://x", "", "b", "", "", 0, "a".into(), "b".into()).is_err());
    }
}
