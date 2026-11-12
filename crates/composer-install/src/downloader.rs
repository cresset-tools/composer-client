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

use crate::LinkMode;
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
    link_mode: LinkMode,
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
        match link_mode {
            LinkMode::Extract => extract_from_cache(cache_root, d)?,
            LinkMode::Hardlink => install_from_store(cache_root, d)?,
        }
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

/// The flat, traversal-safe cache token for a dist — the sha1 shasum when the
/// upstream published one, else `ref-<hash>` of the git reference, else
/// `url-<hash>` of the package name + URL (a Composer `type: package` entry with
/// neither). Both the zip cache (`<key>.zip`) and the extracted store
/// (`extracted/<key>/`) key off this identical token.
fn cache_key(dist: &DistRequest<'_>) -> String {
    if !dist.sha1.is_empty() {
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
    }
}

/// `<cache_root>/<key>.zip`. The extension is for human-readable cache listings
/// only; lookup is keyed on [`cache_key`].
fn cache_path_for(cache_root: &Path, dist: &DistRequest<'_>) -> PathBuf {
    cache_root.join(format!("{}.zip", cache_key(dist)))
}

/// [`LinkMode::Hardlink`] materialization: decompress the cached zip ONCE into a
/// persistent extracted store, then hard-link the store tree into `vendor_dest`.
///
/// The store lives beside the zip cache at `<cache_root>/extracted/<key>/`, with
/// a sibling `<key>.complete` marker (kept OUT of the store dir so it is never
/// linked into `vendor/`). The marker is written only after a full extraction,
/// so a crashed run leaves a marker-less (untrusted) store dir that the next run
/// re-extracts from scratch. When the store is already complete, no
/// decompression happens — the whole cost is the hard links.
///
/// Patch safety: mutating a linked `vendor/` file is safe only because the
/// patcher writes atomically (temp file + rename), which drops the link and
/// gives that one file a fresh inode; the store and every other link are
/// untouched. See [`LinkMode::Hardlink`].
#[tracing::instrument(skip_all, fields(package = dist.package_name))]
fn install_from_store(cache_root: &Path, dist: &DistRequest<'_>) -> Result<()> {
    let cache_path = cache_path_for(cache_root, dist);
    let key = cache_key(dist);
    let extracted_root = cache_root.join("extracted");
    let store_dir = extracted_root.join(&key);
    let marker = extracted_root.join(format!("{key}.complete"));

    if !marker.exists() {
        // A prior half-extraction (crash between mkdir and marker) must not be
        // trusted — wipe and re-extract into a fresh store.
        let _ = std::fs::remove_dir_all(&store_dir);
        std::fs::create_dir_all(&store_dir)
            .wrap_err_with(|| format!("creating store {}", store_dir.display()))?;
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
        extract_zip(&cache_path, &store_dir, strip).wrap_err_with(|| {
            format!(
                "extracting dist for {} into store ({} → {})",
                dist.package_name,
                cache_path.display(),
                store_dir.display(),
            )
        })?;
        std::fs::write(&marker, [])
            .wrap_err_with(|| format!("writing store marker {}", marker.display()))?;
    }

    if let Some(parent) = dist.vendor_dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_dir_all(dist.vendor_dest);
    std::fs::create_dir_all(dist.vendor_dest)
        .wrap_err_with(|| format!("creating {}", dist.vendor_dest.display()))?;
    link_tree(&store_dir, dist.vendor_dest).wrap_err_with(|| {
        format!(
            "hard-linking store into vendor for {} ({} → {})",
            dist.package_name,
            store_dir.display(),
            dist.vendor_dest.display(),
        )
    })?;
    Ok(())
}

/// Mirror the tree at `src` into `dst`, hard-linking regular files (falling back
/// to a copy on any link error, e.g. a cross-device `EXDEV`). `dst` is assumed
/// to already exist and be empty. Directories are created, symlinks recreated,
/// regular files hard-linked-or-copied.
///
/// The link-vs-copy decision is made once up front: if `src` and `dst` sit on
/// different filesystems (comparing device ids on unix) a hard link can never
/// work, so we copy directly and skip 63k doomed `hard_link` syscalls.
fn link_tree(src: &Path, dst: &Path) -> Result<()> {
    let can_link = same_filesystem(src, dst);
    link_tree_inner(src, dst, can_link)
}

/// Whether `a` and `b` live on the same filesystem (a hard link is possible).
/// Unix compares `st_dev`; elsewhere we optimistically say yes and let the
/// per-file copy fallback handle any failure.
fn same_filesystem(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(am), Ok(bm)) => am.dev() == bm.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        true
    }
}

fn link_tree_inner(src: &Path, dst: &Path, can_link: bool) -> Result<()> {
    for entry in std::fs::read_dir(src).wrap_err_with(|| format!("reading {}", src.display()))? {
        let entry = entry.wrap_err_with(|| format!("reading entry in {}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry
            .file_type()
            .wrap_err_with(|| format!("stat {}", from.display()))?;
        if ft.is_dir() {
            std::fs::create_dir_all(&to).wrap_err_with(|| format!("creating {}", to.display()))?;
            link_tree_inner(&from, &to, can_link)?;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&from)
                .wrap_err_with(|| format!("readlink {}", from.display()))?;
            symlink(&target, &to)
                .wrap_err_with(|| format!("symlinking {} -> {}", to.display(), target.display()))?;
        } else {
            // Regular file: hard-link when possible, copy on any failure so a
            // cross-device store or a filesystem without link support still
            // produces a correct (if un-shared) tree.
            let linked = can_link && std::fs::hard_link(&from, &to).is_ok();
            if !linked {
                std::fs::copy(&from, &to)
                    .wrap_err_with(|| format!("copying {} -> {}", from.display(), to.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    // Windows: package symlinks are rare and need elevation; recreate as a copy
    // of the resolved target so the tree stays complete. FLAGGED: this diverges
    // from a real symlink on Windows.
    let resolved = link
        .parent()
        .map(|p| p.join(target))
        .unwrap_or_else(|| target.to_path_buf());
    if resolved.is_dir() {
        copy_dir_all(&resolved, link)
    } else {
        std::fs::copy(&resolved, link).map(|_| ())
    }
}

#[cfg(not(unix))]
fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let f = entry.path();
        let t = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&f, &t)?;
        } else {
            std::fs::copy(&f, &t)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
