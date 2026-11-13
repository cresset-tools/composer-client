//! Dist archive extraction — the HTTP-free half of the downloader.
//!
//! The [`Fetcher`](crate::Fetcher) trait only *downloads* (its default impl
//! wraps `reqwest`); extraction has no HTTP or app coupling, so it lives here
//! and runs the same regardless of who fetched the bytes. Composer dists are
//! `zip` (nearly always) or `tar` (plain, gzip or zstd); the wrapping
//! directory (`<vendor>-<package>-<short_sha>/`) is detected and stripped.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr, eyre};

/// The archive format of a dist, from the lock's `dist.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveKind {
    /// `dist.type: zip` — every Packagist and VCS-driver dist.
    Zip,
    /// `dist.type: tar` — a tarball: plain, gzip or zstd (Composer's
    /// `TarDownloader` reads the first two through `PharData`).
    Tar,
}

impl ArchiveKind {
    /// Map a lock `dist.type` to an archive kind, or `None` for a type this
    /// crate can't extract (`xz`, `rar`, `gzip` single files, `path`, …).
    pub fn from_dist_type(kind: &str) -> Option<Self> {
        match kind {
            "zip" => Some(Self::Zip),
            "tar" => Some(Self::Tar),
            _ => None,
        }
    }

    /// File extension used for the cached archive.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
        }
    }
}

/// Extract an archive of `kind` into `into`, stripping `strip_prefix`, or
/// the detected single top-level directory when `strip_prefix` is `None`.
pub fn extract_dist(
    kind: ArchiveKind,
    archive: &Path,
    into: &Path,
    strip_prefix: Option<&str>,
) -> Result<()> {
    let detected: String;
    let strip = if let Some(s) = strip_prefix {
        s
    } else {
        detected = match kind {
            ArchiveKind::Zip => detect_zip_top_level(archive),
            ArchiveKind::Tar => detect_tar_top_level(archive),
        }
        .wrap_err_with(|| format!("detecting top-level dir in {}", archive.display()))?;
        detected.as_str()
    };
    match kind {
        ArchiveKind::Zip => extract_zip(archive, into, strip),
        ArchiveKind::Tar => extract_tar(archive, into, strip),
    }
}

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

/// Open a tarball for reading, picking the codec from its leading magic
/// bytes: gzip (`1f 8b`), zstd (`28 b5 2f fd`), else a plain POSIX tar. The
/// lock never records a tar dist's codec, so it is sniffed, as PHP's
/// `PharData` does. An unsupported codec surfaces as a tar parse error.
fn open_tar_reader(archive: &Path) -> Result<Box<dyn Read>> {
    let mut f = File::open(archive).wrap_err_with(|| format!("opening {}", archive.display()))?;
    let mut magic = [0u8; 4];
    let mut filled = 0;
    while filled < magic.len() {
        let n = f
            .read(&mut magic[filled..])
            .wrap_err_with(|| format!("reading {}", archive.display()))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(0))
        .wrap_err_with(|| format!("rewinding {}", archive.display()))?;
    let magic = &magic[..filled];
    if magic.starts_with(&[0x1f, 0x8b]) {
        Ok(Box::new(flate2::read::GzDecoder::new(f)))
    } else if magic == [0x28, 0xb5, 0x2f, 0xfd] {
        let decoder = zstd::stream::read::Decoder::new(f)
            .wrap_err_with(|| format!("reading zstd stream {}", archive.display()))?;
        Ok(Box::new(decoder))
    } else if magic.starts_with(b"BZh") {
        Err(eyre!(
            "{} is a bzip2 tarball, which this installer does not extract",
            archive.display()
        ))
    } else {
        Ok(Box::new(f))
    }
}

/// Extract a tarball (plain, gzip or zstd) into `into`, stripping `strip_prefix`
/// the same way [`extract_zip`] does.
///
/// Tar entry paths come straight off the header and `Entry::unpack` does not
/// sanitize them, so every entry path, hard-link target and symlink target is
/// checked to stay inside `into`.
#[tracing::instrument(skip_all, fields(into = %into.display()))]
pub fn extract_tar(archive: &Path, into: &Path, strip_prefix: &str) -> Result<()> {
    let mut tar = tar::Archive::new(open_tar_reader(archive)?);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    for entry in tar
        .entries()
        .wrap_err_with(|| format!("reading entries from {}", archive.display()))?
    {
        let mut entry = entry.wrap_err("reading archive entry")?;
        let path = entry.path().wrap_err("reading entry path")?.into_owned();
        // pax global headers and similar carry no file.
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let Some(rewritten) = rewrite_archive_path(&path, strip_prefix) else {
            continue;
        };
        if !is_safe_archive_path(&rewritten) {
            return Err(eyre!(
                "archive entry {} escapes the extraction root",
                path.display()
            ));
        }
        let dest = into.join(&rewritten);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        if kind.is_hard_link() {
            // The header names the link source by its archive path, so it
            // needs the same prefix strip before it can be linked on disk.
            let link_name = entry
                .link_name()
                .wrap_err("reading hardlink target")?
                .ok_or_else(|| eyre!("hardlink entry {} has no target", path.display()))?
                .into_owned();
            let target_rel = rewrite_archive_path(&link_name, strip_prefix)
                .filter(|p| is_safe_archive_path(p))
                .ok_or_else(|| {
                    eyre!(
                        "hardlink target {} escapes the extraction root",
                        link_name.display()
                    )
                })?;
            let target = into.join(target_rel);
            let _ = fs::remove_file(&dest);
            fs::hard_link(&target, &dest)
                .wrap_err_with(|| format!("linking {} → {}", dest.display(), target.display()))?;
            continue;
        }
        if kind.is_symlink() {
            let link_name = entry
                .link_name()
                .wrap_err("reading symlink target")?
                .ok_or_else(|| eyre!("symlink entry {} has no target", path.display()))?
                .into_owned();
            if symlink_target_escapes(&rewritten, &link_name) {
                return Err(eyre!(
                    "symlink entry {} → {} escapes the extraction root",
                    path.display(),
                    link_name.display()
                ));
            }
        }
        entry
            .unpack(&dest)
            .wrap_err_with(|| format!("unpacking {} → {}", path.display(), dest.display()))?;
    }
    Ok(())
}

/// The tar counterpart of [`detect_zip_top_level`]. It has to decompress to
/// read entry names, but dists are small and this runs once per package.
pub fn detect_tar_top_level(archive: &Path) -> Result<String> {
    let mut tar = tar::Archive::new(open_tar_reader(archive)?);
    let mut top: Option<String> = None;
    for entry in tar
        .entries()
        .wrap_err_with(|| format!("reading entries from {}", archive.display()))?
    {
        let entry = entry.wrap_err("reading archive entry")?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        let path = entry.path().wrap_err("reading entry path")?;
        let mut components = path.components();
        let first = loop {
            match components.next() {
                Some(std::path::Component::CurDir) => {}
                other => break other,
            }
        };
        let Some(first) = first else {
            continue;
        };
        let std::path::Component::Normal(os) = first else {
            return Ok(String::new());
        };
        let Some(s) = os.to_str() else {
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

/// Whether `path` is relative with only normal components — the tar-side
/// equivalent of the `zip` crate's `enclosed_name`.
fn is_safe_archive_path(path: &Path) -> bool {
    use std::path::Component;
    path.components().all(|c| match c {
        Component::Normal(_) | Component::CurDir => true,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    })
}

/// Whether a symlink at archive-relative `link_rel` pointing at `target`
/// resolves outside the extraction root. Purely lexical: an absolute target
/// escapes, and a relative one escapes when its `..` segments climb above the
/// root. Interior `..` that stays inside the tree is fine.
fn symlink_target_escapes(link_rel: &Path, target: &Path) -> bool {
    use std::path::Component;
    if target.is_absolute() {
        return true;
    }
    let mut depth: usize = link_rel.parent().map_or(0, |p| {
        p.components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count()
    });
    for c in target.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                let Some(next) = depth.checked_sub(1) else {
                    return true;
                };
                depth = next;
            }
            Component::RootDir | Component::Prefix(_) => return true,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum Codec {
        Plain,
        Gzip,
        Zstd,
    }

    fn tar_with(entries: &[(&str, &[u8])], codec: Codec) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *body).unwrap();
        }
        let raw = builder.into_inner().unwrap();
        match codec {
            Codec::Plain => raw,
            Codec::Gzip => {
                let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                std::io::Write::write_all(&mut gz, &raw).unwrap();
                gz.finish().unwrap()
            }
            Codec::Zstd => zstd::stream::encode_all(raw.as_slice(), 1).unwrap(),
        }
    }

    #[test]
    fn every_tar_codec_extracts_with_the_wrapper_stripped() {
        for codec in [Codec::Plain, Codec::Gzip, Codec::Zstd] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = tmp.path().join("dist.tar");
            fs::write(
                &archive,
                tar_with(
                    &[
                        ("acme-lib-abc123/composer.json", b"{}"),
                        ("acme-lib-abc123/src/Lib.php", b"<?php"),
                    ],
                    codec,
                ),
            )
            .unwrap();
            assert_eq!(detect_tar_top_level(&archive).unwrap(), "acme-lib-abc123");
            let into = tmp.path().join("out");
            fs::create_dir_all(&into).unwrap();
            extract_dist(ArchiveKind::Tar, &archive, &into, None).unwrap();
            assert_eq!(fs::read(into.join("src/Lib.php")).unwrap(), b"<?php");
            assert!(into.join("composer.json").is_file());
        }
    }

    #[test]
    fn tar_symlink_escaping_the_root_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "pkg/evil", "../../etc/passwd")
            .unwrap();
        let archive = tmp.path().join("dist.tar");
        fs::write(&archive, builder.into_inner().unwrap()).unwrap();
        let into = tmp.path().join("out");
        fs::create_dir_all(&into).unwrap();
        let err = extract_tar(&archive, &into, "pkg").unwrap_err();
        assert!(format!("{err:#}").contains("escapes the extraction root"));
    }

    #[test]
    fn symlink_escape_is_lexical() {
        assert!(symlink_target_escapes(Path::new("a"), Path::new("/abs")));
        assert!(symlink_target_escapes(Path::new("a"), Path::new("../x")));
        assert!(!symlink_target_escapes(
            Path::new("bin/x"),
            Path::new("../lib/y")
        ));
    }

    #[test]
    fn dist_types_map_to_archive_kinds() {
        assert_eq!(ArchiveKind::from_dist_type("zip"), Some(ArchiveKind::Zip));
        assert_eq!(ArchiveKind::from_dist_type("tar"), Some(ArchiveKind::Tar));
        assert_eq!(ArchiveKind::from_dist_type("xz"), None);
    }
}
