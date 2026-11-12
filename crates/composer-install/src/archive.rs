//! Zip extraction — the dependency-free half of the downloader.
//!
//! The [`Fetcher`](crate::Fetcher) trait only *downloads* (its default impl
//! wraps `reqwest`); extraction is pure `zip`-crate logic with no HTTP or
//! app coupling, so it lives here and runs the same regardless of who fetched
//! the bytes. Composer dist archives are always zip; the wrapping directory
//! (`<vendor>-<package>-<short_sha>/`) is detected and stripped.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

/// Extract a `.zip` archive into `into`, stripping `strip_prefix` as a
/// leading path component from every entry. Composer dist zips wrap their
/// contents in `<vendor>-<package>-<short_sha>/`; pass that as `strip_prefix`
/// (or `""` to extract verbatim) so the tree lands at the destination root.
///
/// The walk is rolled by hand rather than via `ZipArchive::extract` so the
/// strip-prefix rewrite (which `extract` doesn't support) stays trivial.
/// Symlink entries (only seen in unix-built zips) are unpacked as plain
/// files, which is fine for Composer dists.
#[tracing::instrument(skip_all, fields(into = %into.display()))]
pub fn extract_zip(zip_path: &Path, into: &Path, strip_prefix: &str) -> Result<()> {
    let f = File::open(zip_path).wrap_err_with(|| format!("opening {}", zip_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(f).wrap_err_with(|| format!("reading zip {}", zip_path.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .wrap_err_with(|| format!("reading zip entry {i}"))?;
        // `enclosed_name` is the traversal-safe path (rejects `..` and
        // absolute paths) — `name()` would return the raw header bytes.
        let Some(raw) = entry.enclosed_name() else {
            continue;
        };
        let Some(rewritten) = rewrite_archive_path(&raw, strip_prefix) else {
            // The prefix directory entry itself; skip — `into` exists.
            continue;
        };
        let dest = into.join(&rewritten);
        if entry.is_dir() {
            fs::create_dir_all(&dest).wrap_err_with(|| format!("creating {}", dest.display()))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let mut out =
            File::create(&dest).wrap_err_with(|| format!("creating {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .wrap_err_with(|| format!("writing {}", dest.display()))?;
        // Preserve the Unix executable bit if the entry carried mode info
        // (zips built on Unix do; windows-built zips don't).
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// Scan a zip's central directory and return the single common top-level
/// path component if there is exactly one. Returns the empty string when
/// entries are already flat at the archive root, or when there are multiple
/// top-level components (no safe strip is possible).
///
/// Reads only the central directory — no decompression — so this is cheap
/// enough to call before extracting every dist. Packagist's zipballs always
/// wrap contents in `<owner>-<repo>-<short_sha>/`, but the wrapper name isn't
/// predictable across CDNs, so detection beats computation.
pub fn detect_zip_top_level(zip_path: &Path) -> Result<String> {
    let f = File::open(zip_path).wrap_err_with(|| format!("opening {}", zip_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(f).wrap_err_with(|| format!("reading zip {}", zip_path.display()))?;
    let mut top: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .wrap_err_with(|| format!("reading zip entry {i}"))?;
        let Some(raw) = entry.enclosed_name() else {
            continue;
        };
        // First normal path component. `enclosed_name` already rejects `..`
        // and absolute paths so `Component::Normal` is the only kind we can
        // encounter on a non-empty input.
        let Some(first) = raw.components().next() else {
            continue;
        };
        let std::path::Component::Normal(os) = first else {
            continue;
        };
        let Some(s) = os.to_str() else {
            // Non-utf-8 entry name — bail on detection, fall back to no-strip
            // rather than guessing.
            return Ok(String::new());
        };
        match &top {
            None => top = Some(s.to_owned()),
            Some(existing) if existing == s => {}
            Some(_) => return Ok(String::new()),
        }
    }
    Ok(top.unwrap_or_default())
}

/// Apply `strip_prefix` to an archive-internal path. Returns `None` when the
/// rewrite produces an empty path (the prefix directory entry itself — the
/// caller skips it because the destination already exists). Entries that
/// don't start with the prefix are left alone.
fn rewrite_archive_path(path: &Path, strip_prefix: &str) -> Option<PathBuf> {
    let rewritten = if strip_prefix.is_empty() {
        path.to_path_buf()
    } else {
        match path.strip_prefix(strip_prefix) {
            Ok(rest) => rest.to_path_buf(),
            Err(_) => path.to_path_buf(),
        }
    };
    if rewritten.as_os_str().is_empty() {
        None
    } else {
        Some(rewritten)
    }
}
