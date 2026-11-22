//! The download engine and HTTP egress policy (threats T4, T5).
//!
//! Every byte the daemon fetches from the internet goes through [`Downloader`]:
//! * **HTTPS only**, rustls with the web PKI roots;
//! * the initial host **and every redirect target** must be on the caller's [`HostPolicy`];
//! * IP-literal URLs are refused;
//! * a descriptive User-Agent (Paper, Modrinth and mcsrvstat require one) with no user data;
//! * hashes are computed while streaming; the file is only published after the expected hash
//!   matches; SHA-256 is always computed and used as the content-addressed cache key;
//! * downloads resume with `Range` when the server supports it, retry with backoff on network
//!   errors and 5xx, honour `Retry-After` on 429, enforce a size cap, and can be cancelled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{StatusCode, header};
use sha1::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

/// User-Agent sent on every request. TODO(Q-04): final product name and repository URL.
pub const USER_AGENT: &str = concat!(
    "MCServerCenter/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/SinghZoren/mc-server-center)"
);

const MAX_REDIRECTS: usize = 5;
const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Hosts a particular adapter may contact. Matching is exact on the lowercase host name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPolicy {
    hosts: Vec<String>,
    #[cfg(test)]
    allow_http: bool,
}

impl HostPolicy {
    /// A policy allowing exactly these hosts.
    #[must_use]
    pub fn new(hosts: &[&str]) -> Self {
        Self {
            hosts: hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
            #[cfg(test)]
            allow_http: false,
        }
    }

    #[cfg(test)]
    fn for_tests(hosts: &[&str]) -> Self {
        Self {
            allow_http: true,
            ..Self::new(hosts)
        }
    }

    /// Checks a URL against the policy.
    ///
    /// # Errors
    /// [`DownloadError::HostNotAllowed`] with the reason.
    pub fn check(&self, url: &Url) -> Result<(), DownloadError> {
        let deny = |why: &str| Err(DownloadError::HostNotAllowed(format!("{url}: {why}")));
        #[cfg(test)]
        let http_ok = self.allow_http && url.scheme() == "http";
        #[cfg(not(test))]
        let http_ok = false;
        if url.scheme() != "https" && !http_ok {
            return deny("only https is allowed");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return deny("credentials in URLs are not allowed");
        }
        match url.host() {
            Some(Host::Domain(d)) if self.hosts.iter().any(|h| h == &d.to_ascii_lowercase()) => {
                Ok(())
            }
            #[cfg(test)]
            Some(Host::Ipv4(ip)) if http_ok && ip.is_loopback() => Ok(()),
            Some(Host::Domain(_)) => deny("host is not on the allowlist"),
            Some(Host::Ipv4(_) | Host::Ipv6(_)) => deny("IP-address URLs are not allowed"),
            None => deny("URL has no host"),
        }
    }
}

/// Expected digest of a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// SHA-1 hex (Mojang, Modrinth secondary). Weak, but what some upstreams publish.
    Sha1(String),
    /// SHA-256 hex.
    Sha256(String),
    /// SHA-512 hex (Modrinth primary).
    Sha512(String),
    /// Upstream publishes no hash: trust on first use; the caller records the SHA-256.
    None,
}

/// How strongly a download's integrity was established, for provenance records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assurance {
    /// Matched an upstream SHA-256 or SHA-512.
    UpstreamStrong,
    /// Matched an upstream SHA-1 only.
    UpstreamSha1,
    /// No upstream hash; TLS + allowlisted host only.
    TlsOnly,
}

/// What to download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSpec {
    /// Source URL.
    pub url: Url,
    /// Expected digest.
    pub expected: Expected,
    /// Hard cap on the body size.
    pub max_bytes: u64,
    /// Allowed hosts (initial and redirects).
    pub policy: HostPolicy,
}

/// A verified file in the content-addressed cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// Path inside the cache (read-only by convention).
    pub path: PathBuf,
    /// Integrity assurance level.
    pub assurance: Assurance,
    /// Final URL after redirects.
    pub final_url: Url,
}

/// Download failure.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// URL or redirect not permitted.
    #[error("download host not allowed: {0}")]
    HostNotAllowed(String),
    /// Content did not match the expected digest.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Expected hex digest.
        expected: String,
        /// Computed hex digest.
        actual: String,
    },
    /// Body exceeded the size cap.
    #[error("download exceeds the {0}-byte limit")]
    TooLarge(u64),
    /// Upstream returned an unexpected status.
    #[error("upstream returned HTTP {0}")]
    Status(u16),
    /// Upstream is rate limiting.
    #[error("rate limited by upstream")]
    RateLimited,
    /// Upstream returned data we could not parse or that failed an integrity check.
    #[error("malformed upstream data: {0}")]
    Malformed(String),
    /// Network-level failure after retries.
    #[error("network error: {0}")]
    Network(String),
    /// Local file error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Cancelled by the user.
    #[error("cancelled")]
    Cancelled,
}

impl From<DownloadError> for msm_core::MsmError {
    fn from(e: DownloadError) -> Self {
        use msm_core::ErrorCode as C;
        let code = match &e {
            DownloadError::HostNotAllowed(_) => C::DownloadHostNotAllowed,
            DownloadError::HashMismatch { .. } => C::DownloadHashMismatch,
            DownloadError::RateLimited => C::RateLimited,
            DownloadError::Cancelled => C::Cancelled,
            DownloadError::TooLarge(_)
            | DownloadError::Status(_)
            | DownloadError::Network(_)
            | DownloadError::Malformed(_) => C::UpstreamError,
            DownloadError::Io(_) => C::Internal,
        };
        msm_core::MsmError::new(code, e.to_string())
    }
}

/// Progress callback: `(bytes_so_far, total_if_known)`.
pub type Progress<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

/// Downloads files into a content-addressed cache.
#[derive(Debug, Clone)]
pub struct Downloader {
    cache_dir: PathBuf,
    partial_dir: PathBuf,
}

fn build_client(policy: HostPolicy) -> reqwest::Client {
    let redirect = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        if policy.check(attempt.url()).is_err() {
            // Stop and surface the 3xx so the caller reports a policy violation.
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .https_only(!cfg!(test))
        .redirect(redirect)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

impl Downloader {
    /// Creates a downloader caching into `cache_dir` (e.g. `cache/blobs/sha256`) and keeping
    /// partial downloads in `partial_dir` (e.g. `tmp/downloads`). Both must be on one volume.
    #[must_use]
    pub fn new(cache_dir: impl Into<PathBuf>, partial_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
            partial_dir: partial_dir.into(),
        }
    }

    /// A small JSON GET for metadata APIs, under the same egress policy (no caching).
    ///
    /// # Errors
    /// Policy, status, size or network errors; malformed JSON is reported as `Malformed`.
    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &Url,
        policy: &HostPolicy,
        max_bytes: u64,
    ) -> Result<T, DownloadError> {
        let bytes = self.get_bytes(url, policy, max_bytes).await?;
        serde_json::from_slice(&bytes).map_err(|e| DownloadError::Malformed(e.to_string()))
    }

    /// A small GET returning the raw body (e.g. to verify a hash before parsing).
    ///
    /// # Errors
    /// Policy, status, size or network errors.
    pub async fn get_bytes(
        &self,
        url: &Url,
        policy: &HostPolicy,
        max_bytes: u64,
    ) -> Result<Vec<u8>, DownloadError> {
        policy.check(url)?;
        let client = build_client(policy.clone());
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = client
                .get(url.clone())
                .header(header::ACCEPT, "application/json")
                .send()
                .await;
            match result {
                Ok(resp) => {
                    policy.check(resp.url())?;
                    match resp.status() {
                        s if s.is_success() => {
                            return read_capped(resp, max_bytes).await;
                        }
                        StatusCode::TOO_MANY_REQUESTS if attempt < MAX_ATTEMPTS => {
                            tokio::time::sleep(retry_after(&resp)).await;
                        }
                        StatusCode::TOO_MANY_REQUESTS => return Err(DownloadError::RateLimited),
                        s if s.is_redirection() => {
                            return Err(DownloadError::HostNotAllowed(format!(
                                "{url}: redirect to a host that is not allowed"
                            )));
                        }
                        s if s.is_server_error() && attempt < MAX_ATTEMPTS => {
                            tokio::time::sleep(backoff(attempt)).await;
                        }
                        s => return Err(DownloadError::Status(s.as_u16())),
                    }
                }
                Err(e) if attempt < MAX_ATTEMPTS && !e.is_redirect() => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) if e.is_redirect() => {
                    return Err(DownloadError::HostNotAllowed(format!(
                        "redirect refused: {e}"
                    )));
                }
                Err(e) => return Err(DownloadError::Network(e.to_string())),
            }
        }
    }

    /// Fetches `spec` into the cache, verifying integrity. Returns immediately if a blob with
    /// the expected SHA-256 is already cached.
    ///
    /// # Errors
    /// See [`DownloadError`]. On any error nothing is published to the cache.
    pub async fn fetch(
        &self,
        spec: &DownloadSpec,
        progress: Progress<'_>,
        cancel: &CancellationToken,
    ) -> Result<Blob, DownloadError> {
        spec.policy.check(&spec.url)?;
        if let Expected::Sha256(hex) = &spec.expected {
            let cached = self.cache_dir.join(hex.to_ascii_lowercase());
            if let Ok(meta) = tokio::fs::metadata(&cached).await {
                return Ok(Blob {
                    sha256: hex.to_ascii_lowercase(),
                    size: meta.len(),
                    path: cached,
                    assurance: Assurance::UpstreamStrong,
                    final_url: spec.url.clone(),
                });
            }
        }
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        tokio::fs::create_dir_all(&self.partial_dir).await?;
        let partial = self.partial_dir.join(partial_name(&spec.url));
        let client = build_client(spec.policy.clone());

        let mut attempt = 0;
        let final_url = loop {
            attempt += 1;
            match self
                .try_fetch(&client, spec, &partial, progress, cancel)
                .await
            {
                Ok(url) => break url,
                Err(Retry::Wait(d)) if attempt < MAX_ATTEMPTS => {
                    tokio::select! {
                        () = tokio::time::sleep(d) => {}
                        () = cancel.cancelled() => return Err(DownloadError::Cancelled),
                    }
                }
                Err(Retry::Throttled(d)) if attempt < MAX_ATTEMPTS => {
                    tokio::select! {
                        () = tokio::time::sleep(d) => {}
                        () = cancel.cancelled() => return Err(DownloadError::Cancelled),
                    }
                }
                Err(Retry::Wait(_)) => {
                    return Err(DownloadError::Network("giving up after retries".into()));
                }
                Err(Retry::Throttled(_)) => return Err(DownloadError::RateLimited),
                Err(Retry::Fatal(e)) => {
                    if matches!(
                        e,
                        DownloadError::HashMismatch { .. } | DownloadError::TooLarge(_)
                    ) {
                        let _ = tokio::fs::remove_file(&partial).await;
                    }
                    return Err(e);
                }
            }
        };

        let (digest, sha256, size) = hash_file(&partial, &spec.expected).await?;
        let assurance = match &spec.expected {
            Expected::Sha1(want) | Expected::Sha256(want) | Expected::Sha512(want) => {
                if !digest.eq_ignore_ascii_case(want) {
                    let _ = tokio::fs::remove_file(&partial).await;
                    return Err(DownloadError::HashMismatch {
                        expected: want.clone(),
                        actual: digest,
                    });
                }
                if matches!(spec.expected, Expected::Sha1(_)) {
                    Assurance::UpstreamSha1
                } else {
                    Assurance::UpstreamStrong
                }
            }
            Expected::None => Assurance::TlsOnly,
        };
        let dest = self.cache_dir.join(&sha256);
        if tokio::fs::metadata(&dest).await.is_ok() {
            let _ = tokio::fs::remove_file(&partial).await;
        } else {
            msm_fsguard::rename_with_retry(&partial, &dest)?;
        }
        Ok(Blob {
            sha256,
            size,
            path: dest,
            assurance,
            final_url,
        })
    }

    async fn try_fetch(
        &self,
        client: &reqwest::Client,
        spec: &DownloadSpec,
        partial: &Path,
        progress: Progress<'_>,
        cancel: &CancellationToken,
    ) -> Result<Url, Retry> {
        let have = tokio::fs::metadata(partial)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let mut req = client.get(spec.url.clone());
        if have > 0 {
            req = req.header(header::RANGE, format!("bytes={have}-"));
        }
        let resp = tokio::select! {
            r = req.send() => r,
            () = cancel.cancelled() => return Err(Retry::Fatal(DownloadError::Cancelled)),
        };
        let resp = match resp {
            Ok(r) => r,
            Err(e) if e.is_redirect() => {
                return Err(Retry::Fatal(DownloadError::HostNotAllowed(format!(
                    "redirect refused: {e}"
                ))));
            }
            Err(_) => return Err(Retry::Wait(Duration::from_secs(1))),
        };
        spec.policy.check(resp.url()).map_err(Retry::Fatal)?;
        let final_url = resp.url().clone();

        let (append, start) = match resp.status() {
            StatusCode::PARTIAL_CONTENT if have > 0 => (true, have),
            StatusCode::OK => (false, 0),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                let _ = tokio::fs::remove_file(partial).await;
                return Err(Retry::Wait(Duration::ZERO));
            }
            StatusCode::TOO_MANY_REQUESTS => return Err(Retry::Throttled(retry_after(&resp))),
            s if s.is_redirection() => {
                return Err(Retry::Fatal(DownloadError::HostNotAllowed(format!(
                    "{}: redirect to a host that is not allowed",
                    spec.url
                ))));
            }
            s if s.is_server_error() => return Err(Retry::Wait(Duration::from_secs(2))),
            s => return Err(Retry::Fatal(DownloadError::Status(s.as_u16()))),
        };
        let total = resp.content_length().map(|l| l + start);
        if total.is_some_and(|t| t > spec.max_bytes) {
            return Err(Retry::Fatal(DownloadError::TooLarge(spec.max_bytes)));
        }

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(partial)
            .await
            .map_err(|e| Retry::Fatal(e.into()))?;
        let mut written = start;
        let mut stream = resp.bytes_stream();
        loop {
            let chunk = tokio::select! {
                c = stream.next() => c,
                () = cancel.cancelled() => return Err(Retry::Fatal(DownloadError::Cancelled)),
            };
            match chunk {
                None => break,
                Some(Ok(bytes)) => {
                    written += bytes.len() as u64;
                    if written > spec.max_bytes {
                        return Err(Retry::Fatal(DownloadError::TooLarge(spec.max_bytes)));
                    }
                    file.write_all(&bytes)
                        .await
                        .map_err(|e| Retry::Fatal(e.into()))?;
                    progress(written, total);
                }
                // Keep what we have; the next attempt resumes with Range.
                Some(Err(_)) => return Err(Retry::Wait(Duration::from_secs(1))),
            }
        }
        file.flush().await.map_err(|e| Retry::Fatal(e.into()))?;
        file.sync_all().await.map_err(|e| Retry::Fatal(e.into()))?;
        Ok(final_url)
    }
}

enum Retry {
    /// Transient failure; retry after the delay.
    Wait(Duration),
    /// HTTP 429; retry after the server-requested delay, report `RateLimited` when exhausted.
    Throttled(Duration),
    /// Do not retry.
    Fatal(DownloadError),
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(6)))
}

fn retry_after(resp: &reqwest::Response) -> Duration {
    resp.headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(Duration::from_secs(5), Duration::from_secs)
        .min(MAX_RETRY_AFTER)
}

async fn read_capped(resp: reqwest::Response, max: u64) -> Result<Vec<u8>, DownloadError> {
    if resp.content_length().is_some_and(|l| l > max) {
        return Err(DownloadError::TooLarge(max));
    }
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DownloadError::Network(e.to_string()))?;
        if (out.len() + chunk.len()) as u64 > max {
            return Err(DownloadError::TooLarge(max));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Stable partial-file name per URL so retries and restarts resume the same file.
fn partial_name(url: &Url) -> String {
    let digest = sha2::Sha256::digest(url.as_str().as_bytes());
    format!("{}.part", hex::encode(&digest[..16]))
}

/// Hashes a file, returning `(expected-algorithm digest, sha256, size)`.
async fn hash_file(
    path: &Path,
    expected: &Expected,
) -> Result<(String, String, u64), DownloadError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut sha256 = sha2::Sha256::new();
    let mut sha1 = sha1::Sha1::new();
    let mut sha512 = sha2::Sha512::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        size += n as u64;
        sha256.update(&buf[..n]);
        match expected {
            Expected::Sha1(_) => sha1.update(&buf[..n]),
            Expected::Sha512(_) => sha512.update(&buf[..n]),
            _ => {}
        }
    }
    let sha256 = hex::encode(sha256.finalize());
    let digest = match expected {
        Expected::Sha1(_) => hex::encode(sha1.finalize()),
        Expected::Sha512(_) => hex::encode(sha512.finalize()),
        Expected::Sha256(_) | Expected::None => sha256.clone(),
    };
    Ok((digest, sha256, size))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const BODY: &[u8] = b"hello minecraft server jar";

    fn sha256_hex(b: &[u8]) -> String {
        hex::encode(sha2::Sha256::digest(b))
    }
    fn sha1_hex(b: &[u8]) -> String {
        hex::encode(sha1::Sha1::digest(b))
    }

    fn setup() -> (tempfile::TempDir, Downloader) {
        let dir = tempfile::tempdir().unwrap();
        let d = Downloader::new(dir.path().join("cache"), dir.path().join("partial"));
        (dir, d)
    }

    fn spec(server: &MockServer, p: &str, expected: Expected) -> DownloadSpec {
        DownloadSpec {
            url: Url::parse(&format!("{}{p}", server.uri())).unwrap(),
            expected,
            max_bytes: 1024,
            policy: HostPolicy::for_tests(&[]),
        }
    }

    fn no_progress() -> impl Fn(u64, Option<u64>) + Send + Sync {
        |_, _| {}
    }

    #[test]
    fn policy_rules() {
        let p = HostPolicy::new(&["piston-data.mojang.com"]);
        assert!(
            p.check(&Url::parse("https://piston-data.mojang.com/x").unwrap())
                .is_ok()
        );
        assert!(
            p.check(&Url::parse("https://PISTON-DATA.mojang.com/x").unwrap())
                .is_ok()
        );
        for bad in [
            "http://piston-data.mojang.com/x",
            "https://evil.example/x",
            "https://piston-data.mojang.com.evil.example/x",
            "https://user:pw@piston-data.mojang.com/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "file:///etc/passwd",
        ] {
            assert!(p.check(&Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn downloads_verifies_and_caches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/server.jar"))
            .and(header_exists("user-agent"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
            .mount(&server)
            .await;
        let (_d, dl) = setup();
        let seen = Arc::new(AtomicU64::new(0));
        let seen2 = seen.clone();
        let progress = move |n: u64, _| seen2.store(n, Ordering::SeqCst);
        let blob = dl
            .fetch(
                &spec(&server, "/server.jar", Expected::Sha1(sha1_hex(BODY))),
                &progress,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(blob.sha256, sha256_hex(BODY));
        assert_eq!(blob.assurance, Assurance::UpstreamSha1);
        assert_eq!(std::fs::read(&blob.path).unwrap(), BODY);
        assert_eq!(seen.load(Ordering::SeqCst), BODY.len() as u64);
    }

    #[tokio::test]
    async fn hash_mismatch_publishes_nothing() {
        let server = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
            .mount(&server)
            .await;
        let (dir, dl) = setup();
        let err = dl
            .fetch(
                &spec(&server, "/x", Expected::Sha256("00".repeat(32))),
                &no_progress(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::HashMismatch { .. }));
        assert_eq!(
            std::fs::read_dir(dir.path().join("cache")).unwrap().count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("partial"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn size_cap_enforced() {
        let server = MockServer::start().await;
        Mock::given(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 4096]))
            .mount(&server)
            .await;
        let (_d, dl) = setup();
        let err = dl
            .fetch(
                &spec(&server, "/big", Expected::None),
                &no_progress(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::TooLarge(1024)));
    }

    #[tokio::test]
    async fn redirect_to_disallowed_host_is_refused() {
        let server = MockServer::start().await;
        Mock::given(path("/r"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", "https://evil.example/payload"),
            )
            .mount(&server)
            .await;
        let (_d, dl) = setup();
        let err = dl
            .fetch(
                &spec(&server, "/r", Expected::None),
                &no_progress(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::HostNotAllowed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn client_errors_are_not_retried_and_server_errors_are() {
        let server = MockServer::start().await;
        Mock::given(path("/404"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
            .mount(&server)
            .await;
        let (_d, dl) = setup();
        let err = dl
            .fetch(
                &spec(&server, "/404", Expected::None),
                &no_progress(),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::Status(404)));
        let blob = dl
            .fetch(
                &spec(&server, "/flaky", Expected::None),
                &no_progress(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(blob.assurance, Assurance::TlsOnly);
    }

    #[tokio::test]
    async fn resumes_with_range() {
        let server = MockServer::start().await;
        Mock::given(path("/resume"))
            .and(wiremock::matchers::header("range", "bytes=5-"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(&BODY[5..]))
            .mount(&server)
            .await;
        let (dir, dl) = setup();
        let s = spec(&server, "/resume", Expected::Sha256(sha256_hex(BODY)));
        std::fs::create_dir_all(dir.path().join("partial")).unwrap();
        std::fs::write(
            dir.path().join("partial").join(partial_name(&s.url)),
            &BODY[..5],
        )
        .unwrap();
        let blob = dl
            .fetch(&s, &no_progress(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(std::fs::read(blob.path).unwrap(), BODY);
    }

    #[tokio::test]
    async fn cancellation_stops_the_download() {
        let server = MockServer::start().await;
        Mock::given(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(BODY)
                    .set_delay(Duration::from_secs(10)),
            )
            .mount(&server)
            .await;
        let (_d, dl) = setup();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        let err = dl
            .fetch(
                &spec(&server, "/slow", Expected::None),
                &no_progress(),
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::Cancelled));
    }

    #[tokio::test]
    async fn get_json_parses_and_caps() {
        let server = MockServer::start().await;
        Mock::given(path("/meta.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"a":1}"#))
            .mount(&server)
            .await;
        let (_d, dl) = setup();
        let url = Url::parse(&format!("{}/meta.json", server.uri())).unwrap();
        let v: serde_json::Value = dl
            .get_json(&url, &HostPolicy::for_tests(&[]), 1024)
            .await
            .unwrap();
        assert_eq!(v["a"], 1);
        let err = dl
            .get_json::<serde_json::Value>(&url, &HostPolicy::for_tests(&[]), 3)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::TooLarge(3)));
    }
}
