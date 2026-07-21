//! Parallel Composer dist downloader.
//!
//! Two-phase: download every dist into a persistent cache at
//! `<cache_root>/<key>.zip`, then extract each into its
//! `vendor/<vendor>/<package>/` destination. Splitting the phases means a
//! partial download failure aborts before any extraction starts — `vendor/` is
//! either fully populated by this call (modulo what was already there) or
//! untouched.
//!
//! `<key>` is the sha1 hex Composer publishes as `dist.shasum` when present,
//! falling back to the dist's git `reference` when the shasum is empty (the
//! normal case for GitHub/GitLab zipball dists, whose `GitHubDriver::getDist()`
//! emits `'shasum' => ''`). With a real shasum the cache is genuinely
//! content-addressed; with a reference fallback it's content-*coordinate*-
//! addressed — the git ref locks the upstream tree.
//!
//! The single HTTP GET is delegated to a [`Fetcher`]; this module owns
//! everything around it (the mirror-fallback loop, the GitHub-zipball rewrite,
//! local `type: artifact` copies, the cache key) and the extraction.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use rayon::prelude::*;
use sha1::Digest as _;

use crate::archive::{detect_zip_top_level, extract_zip};
use crate::fetch::{FetchSpec, Fetcher};
use crate::progress::Progress;

/// One package to materialize into `vendor/`. Built by the orchestrator from a
/// `composer.lock` `packages[]` entry; tests construct these by hand against
/// fixture HTTP servers. Composer dists are always zip, so there is no archive
/// selector — a non-zip dist is rejected in preflight before this is built.
#[derive(Debug, Clone, Copy)]
pub struct DistRequest<'a> {
    /// Canonical Composer package name (`vendor/package`). Used for the
    /// progress label and error messages — the cache key is the hash, not
    /// the name.
    pub package_name: &'a str,
    /// `dist.url` straight from the lockfile.
    pub url: &'a str,
    /// `dist.shasum` straight from the lockfile (sha1 hex, lower-case). Empty
    /// when the registry didn't publish one (GitHub/GitLab/Bitbucket zipballs,
    /// most of public Packagist) — the downloader skips verification and keys
    /// the cache off [`reference`](Self::reference) instead.
    pub sha1: &'a str,
    /// `dist.reference` from the lockfile — the upstream git ref. Used as the
    /// cache key when `sha1` is empty.
    pub reference: &'a str,
    /// Top-level directory inside the archive to strip — e.g.
    /// `monolog-monolog-1234567`. `None` (the default for `composer.lock`-
    /// driven callers) means auto-detect from the cached archive's central
    /// directory via [`detect_zip_top_level`]; pass `Some` only when the
    /// caller already knows the wrapper name.
    pub strip_prefix: Option<&'a str>,
    /// Where the extracted tree should live — typically
    /// `<project>/vendor/<vendor>/<package>/`. Any existing contents are
    /// replaced by the extractor.
    pub vendor_dest: &'a Path,
    /// Pre-rendered `Authorization` header value for the primary URL, or
    /// `None` for public dists. Set by the orchestrator when the dist host
    /// matches an auth entry.
    pub auth_header: Option<&'a str>,
    /// Header name for [`auth_header`](Self::auth_header) (defaults to
    /// `authorization`).
    pub auth_header_name: Option<&'a str>,
    /// Project root, used to resolve non-http dist URLs (Composer
    /// `type: artifact` repositories serialize the artifact zip's path
    /// straight into `dist.url` as a project-relative string).
    pub project_root: &'a Path,
    /// Fallback download locations, tried in order when the GET against
    /// [`url`](Self::url) fails — Composer's dist-mirror semantics. The
    /// orchestrator builds the full candidate list (preferred mirror first),
    /// puts the first in `url` and the rest here. Empty for the vast majority
    /// of dists.
    pub fallbacks: &'a [DistCandidate],
}

/// One alternative download location for a dist: a fully substituted mirror
/// URL plus its pre-rendered per-host auth header. Owned strings (unlike the
/// borrowed [`DistRequest`] fields) because the URLs are produced by
/// placeholder substitution at request-build time.
#[derive(Debug, Clone)]
pub struct DistCandidate {
    pub url: String,
    pub auth_header: Option<String>,
    pub auth_header_name: Option<&'static str>,
}

/// Per-dist outcome so the caller can distinguish cache hits from fresh
/// downloads. `Downloaded` carries the archive's on-disk size (a telemetry
/// counter — not a transfer-accurate byte count, since compression and resume
/// both make that a different number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistOutcome {
    CacheHit,
    Downloaded { bytes: u64 },
}

/// Download every dist in `dists` in parallel into the cache, then extract
/// each into its `vendor_dest`. Returns once every extraction completes; on
/// any download failure the remaining downloads still finish (rayon aborts the
/// *result*, not the work in flight) but extraction does not start.
///
/// `progress` is notified once per dist as its download resolves, then once
/// per dist as its extraction completes. Outcomes are returned in `dists`
/// order.
#[tracing::instrument(skip_all, fields(dists = dists.len()))]
pub fn fetch_and_extract_dists(
    fetcher: &dyn Fetcher,
    cache_root: &Path,
    dists: &[DistRequest<'_>],
    progress: &dyn Progress,
) -> Result<Vec<DistOutcome>> {
    std::fs::create_dir_all(cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;

    let outcomes: Vec<DistOutcome> = dists
        .par_iter()
        .map(|d| {
            let outcome = download_to_cache(fetcher, cache_root, d)?;
            progress.on_download(d.package_name, outcome);
            Ok(outcome)
        })
        .collect::<Result<Vec<_>>>()?;
    dists.par_iter().try_for_each(|d| {
        extract_from_cache(cache_root, d)?;
        progress.on_extract(d.package_name);
        Ok::<_, eyre::Report>(())
    })?;
    Ok(outcomes)
}

/// Download one dist into the content-addressed cache. No-op when the cache
/// already has a copy.
#[tracing::instrument(skip_all, fields(package = dist.package_name))]
fn download_to_cache(
    fetcher: &dyn Fetcher,
    cache_root: &Path,
    dist: &DistRequest<'_>,
) -> Result<DistOutcome> {
    let cache_path = cache_path_for(cache_root, dist);
    if cache_path.exists() {
        return Ok(DistOutcome::CacheHit);
    }
    if !is_http_url(dist.url) {
        return copy_local_dist(dist, &cache_path);
    }
    // Composer's FileDownloader loop: try each candidate URL in order,
    // warn-and-continue on failure, surface the last error when every
    // candidate is exhausted. `fallbacks` is empty for repos without dist
    // mirrors, so the common path is a single attempt.
    let candidates = std::iter::once((dist.url, dist.auth_header, dist.auth_header_name)).chain(
        dist.fallbacks
            .iter()
            .map(|c| (c.url.as_str(), c.auth_header.as_deref(), c.auth_header_name)),
    );
    let total = 1 + dist.fallbacks.len();
    let mut last_err: Option<eyre::Report> = None;
    for (idx, (url, auth_header, auth_header_name)) in candidates.enumerate() {
        let url = rewrite_github_dist_url(url);
        let spec = FetchSpec {
            url: &url,
            sha1: dist.sha1,
            dest: &cache_path,
            partial_dir: cache_root,
            auth_header,
            auth_header_name,
        };
        match fetcher.fetch(&spec) {
            Ok(()) => {
                return Ok(DistOutcome::Downloaded {
                    bytes: file_size(&cache_path),
                });
            }
            Err(err) => {
                if idx + 1 < total {
                    tracing::warn!(
                        package = dist.package_name,
                        url = %url,
                        error = %err,
                        "dist download failed; trying the next mirror",
                    );
                }
                last_err = Some(err.wrap_err(format!(
                    "downloading dist for {} from {:?}",
                    dist.package_name, url,
                )));
            }
        }
    }
    // The candidate iterator always yields at least the primary URL, so
    // reaching this point means the loop ran and stored an error.
    let err = last_err.expect("download loop ran at least once");
    if total > 1 {
        Err(err.wrap_err(format!(
            "downloading dist for {}: all {} candidate URLs failed",
            dist.package_name, total,
        )))
    } else {
        Err(err)
    }
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Materialize a Composer `type: artifact` dist into the cache by copying the
/// local zip. Composer serializes the artifact's path as `dist.url` relative
/// to the project root (e.g. `artifacts/vendor-pkg-1.2.3.zip`) — resolve it
/// against `project_root`, verify the sha1 if the lockfile carries one, then
/// copy into the content-addressed cache so extraction stays
/// transport-agnostic. Absolute paths and `file://` URLs are accepted as they
/// appear; relative paths are joined onto `project_root`.
fn copy_local_dist(dist: &DistRequest<'_>, cache_path: &Path) -> Result<DistOutcome> {
    let raw = dist.url.strip_prefix("file://").unwrap_or(dist.url);
    let candidate = Path::new(raw);
    let src = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        dist.project_root.join(candidate)
    };
    if !src.exists() {
        return Err(eyre::eyre!(
            "dist for {} points to local file {} which does not exist \
             (Composer `type: artifact` repository file missing?)",
            dist.package_name,
            src.display(),
        ));
    }
    if !dist.sha1.is_empty() {
        verify_local_sha1(&src, dist.sha1)
            .wrap_err_with(|| format!("verifying local dist for {}", dist.package_name))?;
    }
    std::fs::copy(&src, cache_path).wrap_err_with(|| {
        format!(
            "copying local dist for {} ({} → {})",
            dist.package_name,
            src.display(),
            cache_path.display(),
        )
    })?;
    Ok(DistOutcome::Downloaded {
        bytes: file_size(cache_path),
    })
}

/// Best-effort on-disk size for the telemetry byte counter.
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn verify_local_sha1(path: &Path, expected_hex: &str) -> Result<()> {
    let mut file =
        std::fs::File::open(path).wrap_err_with(|| format!("opening {}", path.display()))?;
    let mut hasher = sha1::Sha1::new();
    std::io::copy(&mut file, &mut hasher)
        .wrap_err_with(|| format!("hashing {}", path.display()))?;
    let actual = hasher.finalize();
    let mut actual_hex = String::with_capacity(40);
    for b in actual {
        use std::fmt::Write as _;
        let _ = write!(actual_hex, "{b:02x}");
    }
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        return Err(eyre::eyre!(
            "sha1 mismatch on {}: expected {}, got {}",
            path.display(),
            expected_hex,
            actual_hex,
        ));
    }
    Ok(())
}

/// Rewrite `api.github.com` zipball URLs to `codeload.github.com` direct
/// downloads. The API endpoint 302-redirects to codeload anyway, but the
/// redirect consumes a GitHub REST API rate-limit point; going directly to
/// codeload skips it. The `legacy.zip` codeload path produces byte-identical
/// archives (same wrapper directory). Non-GitHub URLs pass through unchanged.
fn rewrite_github_dist_url(url: &str) -> Cow<'_, str> {
    const PREFIX: &str = "https://api.github.com/repos/";
    const ZIPBALL: &str = "/zipball/";

    let Some(rest) = url.strip_prefix(PREFIX) else {
        return Cow::Borrowed(url);
    };
    let Some(idx) = rest.find(ZIPBALL) else {
        return Cow::Borrowed(url);
    };
    let owner_repo = &rest[..idx];
    let reference = &rest[idx + ZIPBALL.len()..];
    if owner_repo.is_empty() || reference.is_empty() {
        return Cow::Borrowed(url);
    }
    Cow::Owned(format!(
        "https://codeload.github.com/{owner_repo}/legacy.zip/{reference}"
    ))
}

/// Extract one cached dist archive into its `vendor_dest`. The destination is
/// wiped beforehand so the call is idempotent — a previous half-done install
/// does not poison the new tree.
#[tracing::instrument(skip_all, fields(package = dist.package_name))]
fn extract_from_cache(cache_root: &Path, dist: &DistRequest<'_>) -> Result<()> {
    let cache_path = cache_path_for(cache_root, dist);
    if let Some(parent) = dist.vendor_dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_dir_all(dist.vendor_dest);
    std::fs::create_dir_all(dist.vendor_dest)
        .wrap_err_with(|| format!("creating {}", dist.vendor_dest.display()))?;
    let detected: String;
    let strip = if let Some(s) = dist.strip_prefix {
        s
    } else {
        detected = detect_zip_top_level(&cache_path).wrap_err_with(|| {
            format!(
                "detecting top-level dir in dist for {} ({})",
                dist.package_name,
                cache_path.display(),
            )
        })?;
        detected.as_str()
    };
    extract_zip(&cache_path, dist.vendor_dest, strip).wrap_err_with(|| {
        format!(
            "extracting dist for {} ({} → {})",
            dist.package_name,
            cache_path.display(),
            dist.vendor_dest.display(),
        )
    })?;
    Ok(())
}

/// `<cache_root>/<key>.zip`. The extension is for human-readable cache listings
/// only; lookup is keyed on the hash (or the git reference when the upstream
/// didn't publish a hash).
fn cache_path_for(cache_root: &Path, dist: &DistRequest<'_>) -> PathBuf {
    let key = if !dist.sha1.is_empty() {
        // A sha1 shasum is already a safe hex string and content-addresses the
        // archive, so use it verbatim.
        dist.sha1.to_string()
    } else if !dist.reference.is_empty() {
        // The git reference can contain `/` (branch names) or even `..`, which
        // would land in an uncreated subdir (ENOENT) or escape the cache root.
        // Hash it into a flat, traversal-safe token.
        let digest = sha1::Sha1::digest(dist.reference.as_bytes());
        format!("ref-{digest:x}")
    } else {
        // Neither a content hash nor an upstream reference — the shape a
        // Composer `type: package` repository entry takes. Hashing the empty
        // reference would collapse every such dist onto one cache file, so
        // fold in the package name + URL (mirroring Composer's `getCacheKey`)
        // so distinct packages never collide.
        let mut hasher = sha1::Sha1::new();
        hasher.update(dist.package_name.as_bytes());
        hasher.update([0]);
        hasher.update(dist.url.as_bytes());
        let digest = hasher.finalize();
        format!("url-{digest:x}")
    };
    cache_root.join(format!("{key}.zip"))
}

#[cfg(test)]
mod tests;
