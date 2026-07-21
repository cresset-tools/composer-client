//! The download seam: fetch one dist URL to a verified cache file.
//!
//! [`Fetcher`] abstracts the single HTTP GET that populates the dist cache —
//! the one point where the installer touches the network. The downloader owns
//! everything around it that needs no HTTP client (the mirror-fallback loop,
//! the GitHub-zipball URL rewrite, local `type: artifact` copies, the cache
//! key), and calls [`Fetcher::fetch`] once per candidate URL. Extraction is
//! separate again ([`crate::archive`]) — a fetcher only downloads.
//!
//! [`ReqwestFetcher`] is the batteries-included default: a `reqwest::blocking`
//! GET with a Composer-compatible User-Agent, sha1 verification, an atomic
//! temp-write-then-rename, and a small retry budget. Supply your own `Fetcher`
//! to reuse a client, inject a proxy, or serve from a pre-seeded store.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use sha1::{Digest, Sha1};

/// One dist to fetch: a single URL plus its verification and auth metadata.
/// The mirror-fallback loop lives in the downloader, which builds a fresh
/// `FetchSpec` per candidate URL.
#[derive(Debug, Clone)]
pub struct FetchSpec<'a> {
    /// The URL to GET. Always `http`/`https` — the downloader handles
    /// non-HTTP (`type: artifact` local file) dists itself.
    pub url: &'a str,
    /// Expected sha1 hex of the archive (lower-case), or the empty string
    /// to skip verification. Composer publishes an empty `shasum` for
    /// VCS-driver zipballs (GitHub/GitLab/Bitbucket), the common case on
    /// public Packagist; those are fetched unverified, exactly as Composer
    /// does (`FileDownloader.php:212`).
    pub sha1: &'a str,
    /// Where the verified bytes are placed (atomically). Typically the
    /// content-addressed dist cache path.
    pub dest: &'a Path,
    /// Directory for the in-progress temp file. Should ideally sit on the
    /// same filesystem as `dest` so the final rename is intra-filesystem;
    /// the default impl also stages a `.incoming` sibling of `dest` to
    /// guarantee that even when it isn't.
    pub partial_dir: &'a Path,
    /// Pre-rendered `Authorization` header *value* (e.g. `Basic <b64>` or
    /// `Bearer <tok>`), or `None` for public dists.
    pub auth_header: Option<&'a str>,
    /// Header *name* for the credential. Defaults to `authorization` when
    /// `None`; set to `private-token` for GitLab private-token auth.
    pub auth_header_name: Option<&'a str>,
}

/// Fetch a single dist URL and place the verified bytes at a cache path.
///
/// Implementations must be `Sync` — the downloader calls `fetch` from a rayon
/// parallel iterator. A no-op when `spec.dest` already exists is expected (the
/// downloader also checks first, but a fetcher should be idempotent).
pub trait Fetcher: Sync {
    /// GET `spec.url`, verify its sha1 unless `spec.sha1` is empty, and place
    /// the bytes atomically at `spec.dest`. Returns `Ok(())` on success
    /// (including when `dest` already exists).
    fn fetch(&self, spec: &FetchSpec<'_>) -> Result<()>;
}

/// The `User-Agent` the default fetcher advertises. The `Composer/2` prefix is
/// load-bearing: some Composer-protocol servers (notably `repo.magento.com`
/// and similar Private Packagist tenants) gate dist downloads on a
/// `Composer/…` UA and return `403` to an anonymous one. We still identify as
/// `composer-install` after the prefix so operators can attribute traffic.
const USER_AGENT: &str = concat!(
    "Composer/2 composer-install/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/cresset-tools/composer-client)",
);

/// Idle/read-gap timeout: max time a transfer may go without receiving bytes
/// before it's treated as stalled and retried. For the blocking client this
/// is a per-read deadline (reqwest recomputes it per `read()`), so a
/// progressing download of any size is unaffected — each chunk just has to
/// arrive within the window of the previous one.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times a failed fetch is retried before giving up.
const RETRY_BUDGET: u32 = 3;

/// Base unit for the exponential backoff between retries. Attempt `n`
/// (1-indexed) sleeps `BACKOFF_BASE * 2^(n-1)` plus up to one base of jitter,
/// de-synchronizing a parallel fan-out that all hit the same flaky mirror.
const BACKOFF_BASE: Duration = Duration::from_millis(250);

/// The default [`Fetcher`]: a `reqwest::blocking` client with a Composer
/// User-Agent, sha1 verification, atomic placement, and bounded retries.
#[derive(Debug, Clone)]
pub struct ReqwestFetcher {
    client: reqwest::blocking::Client,
}

impl ReqwestFetcher {
    /// Build a fetcher with the crate's default client (Composer UA, 10s
    /// connect timeout, 30s idle-read timeout).
    ///
    /// # Errors
    ///
    /// Fails if the underlying `reqwest` client can't be constructed (e.g. no
    /// usable TLS backend).
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(STALL_TIMEOUT)
            .build()
            .wrap_err("building HTTP client")?;
        Ok(Self { client })
    }

    /// Build a fetcher over a caller-provided client — reuse a connection
    /// pool, attach a proxy, or set custom timeouts. The caller owns the
    /// User-Agent policy in this case.
    pub fn with_client(client: reqwest::blocking::Client) -> Self {
        Self { client }
    }
}

impl Fetcher for ReqwestFetcher {
    fn fetch(&self, spec: &FetchSpec<'_>) -> Result<()> {
        if spec.dest.exists() {
            return Ok(());
        }
        fs::create_dir_all(spec.partial_dir)
            .wrap_err_with(|| format!("creating {}", spec.partial_dir.display()))?;
        if let Some(parent) = spec.dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }

        let mut attempts = 0;
        loop {
            match self.try_once(spec) {
                Ok(()) => return Ok(()),
                Err(e) if attempts < RETRY_BUDGET => {
                    attempts += 1;
                    tracing::warn!(error = %e, attempt = attempts, "dist fetch failed; retrying");
                    backoff_sleep(attempts);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl ReqwestFetcher {
    /// One attempt: stream to a verified partial, then stage + rename into
    /// place so `dest` only ever appears fully written and verified.
    fn try_once(&self, spec: &FetchSpec<'_>) -> Result<()> {
        let tmp = self.fetch_to_partial(spec)?;
        // Stage the verified bytes as a sibling of `dest` so the rename is
        // always intra-filesystem, even when `partial_dir` is on a different
        // filesystem from `dest` (cache vs data dir).
        let incoming = sibling_with_suffix(spec.dest, ".incoming");
        let _ = fs::remove_file(&incoming);
        fs::copy(&tmp, &incoming)
            .wrap_err_with(|| format!("staging {} → {}", tmp.display(), incoming.display()))?;
        fs::rename(&incoming, spec.dest)
            .wrap_err_with(|| format!("rename {} → {}", incoming.display(), spec.dest.display()))?;
        let _ = fs::remove_file(&tmp);
        Ok(())
    }

    /// Stream the response into `<partial_dir>/<token>.partial`, hashing as we
    /// go. Verifies the sha1 (unless skip-verify) and returns the partial's
    /// path; deletes it and errors on mismatch.
    fn fetch_to_partial(&self, spec: &FetchSpec<'_>) -> Result<PathBuf> {
        // Skip-verify mode (empty sha1): name the partial after a hash of the
        // URL so concurrent fetches in the same dir don't collide and a retry
        // resumes the same path.
        let partial_token: String;
        let partial_name = if spec.sha1.is_empty() {
            partial_token = format_hex(&Sha1::digest(spec.url.as_bytes()));
            partial_token.as_str()
        } else {
            spec.sha1
        };
        let tmp = spec.partial_dir.join(format!("{partial_name}.partial"));

        let mut req = self.client.get(spec.url);
        if let Some(value) = spec.auth_header {
            let name = spec.auth_header_name.unwrap_or("authorization");
            req = req.header(name, value);
        }
        let mut resp = req
            .send()
            .wrap_err_with(|| format!("fetching dist from url {:?}", spec.url))?;
        if !resp.status().is_success() {
            return Err(eyre!(
                "GET {:?}: server returned HTTP {}",
                spec.url,
                resp.status()
            ));
        }

        let mut file =
            File::create(&tmp).wrap_err_with(|| format!("creating {}", tmp.display()))?;
        let actual = stream_into_file(&mut resp, &mut file, spec.url, &tmp)?;
        file.flush().wrap_err("flushing partial dist")?;

        if !spec.sha1.is_empty() && !actual.eq_ignore_ascii_case(spec.sha1) {
            let _ = fs::remove_file(&tmp);
            return Err(eyre!(
                "sha1 mismatch on {:?}: expected sha1:{}, got sha1:{}",
                spec.url,
                spec.sha1,
                actual
            ));
        }
        Ok(tmp)
    }
}

/// Stream `resp` into `file` while feeding bytes to a sha1 hasher. Returns the
/// computed hex digest.
fn stream_into_file(
    resp: &mut reqwest::blocking::Response,
    file: &mut File,
    url: &str,
    tmp: &Path,
) -> Result<String> {
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = resp
            .read(&mut buf)
            .wrap_err_with(|| format!("reading dist body from {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .wrap_err_with(|| format!("writing {}", tmp.display()))?;
    }
    Ok(format_hex(&hasher.finalize()))
}

/// Sleep for attempt `attempt` (1-indexed) with exponential backoff plus
/// additive jitter. Pure `std` — jitter comes from the wall clock's
/// sub-millisecond bits rather than a `rand` dependency.
fn backoff_sleep(attempt: u32) {
    let factor = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(8);
    let base = BACKOFF_BASE.saturating_mul(factor);
    let clock_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    let window = u64::from(BACKOFF_BASE.subsec_nanos()).max(1);
    let jitter = Duration::from_nanos(clock_nanos % window);
    std::thread::sleep(base + jitter);
}

/// Lower-case hex encoding of a digest.
fn format_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `<parent-of-p>/<name-of-p><suffix>` — a sibling path with an appended
/// suffix, used to stage the atomic `.incoming` rename.
fn sibling_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let parent = p.parent().unwrap_or_else(|| Path::new(""));
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("blob");
    parent.join(format!("{name}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_hex_lowercase() {
        assert_eq!(format_hex(&[0xab, 0xcd]), "abcd");
        assert_eq!(format_hex(&[0]), "00");
    }

    #[test]
    fn sibling_with_suffix_appends() {
        let p = Path::new("/a/b/c");
        assert_eq!(
            sibling_with_suffix(p, ".incoming"),
            Path::new("/a/b/c.incoming")
        );
    }
}
