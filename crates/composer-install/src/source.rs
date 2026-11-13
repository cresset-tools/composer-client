//! Git `source` installs — the seam for packages the lock pins to a VCS
//! checkout instead of a dist archive.
//!
//! [`SourceFetcher`] materializes one package's locked revision into its
//! `vendor/` destination. The orchestrator calls it in parallel, once per
//! source package, and treats the result exactly like an extracted dist:
//! patches, the Magento deploy, the autoloader and `vendor/bin` all run over
//! it. Without a `SourceFetcher` a source-only package is a preflight error.
//!
//! [`GitSourceFetcher`] is the default: it shells out to the user's `git`,
//! as Composer does, so ssh keys, credential helpers and proxies work without
//! configuration. Repositories are cached as bare `--mirror` clones and only
//! fetched when the locked commit is missing, so a warm cache installs
//! offline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use eyre::{Result, WrapErr, eyre};
use sha1::Digest as _;

use crate::error::{InstallError, VcsFailure};

/// One package to install from its git `source`.
#[derive(Debug, Clone, Copy)]
pub struct SourceSpec<'a> {
    /// The package name, for error messages.
    pub package_name: &'a str,
    /// Clone URL candidates in preference order (a `preferred` mirror before
    /// the origin host — `LockPackage::source_urls`). Never empty.
    pub urls: &'a [String],
    /// The revision to check out — a full commit sha in a lockfile.
    pub reference: &'a str,
    /// Where the checkout must end up. Any existing contents are replaced.
    pub dest: &'a Path,
}

/// Install one package from its VCS source.
///
/// Implementations must be `Sync`: the orchestrator calls `install` from a
/// rayon parallel iterator.
pub trait SourceFetcher: Sync {
    /// Place a checkout of `spec.reference` at `spec.dest`, trying each URL in
    /// turn. Errors should be (or wrap) [`InstallError::Vcs`] where the cause
    /// is a git failure, so callers can map them.
    fn install(&self, spec: &SourceSpec<'_>) -> Result<()>;
}

/// The default [`SourceFetcher`]: the system `git`, with bare mirrors cached
/// under a caller-chosen directory.
#[derive(Debug)]
pub struct GitSourceFetcher {
    mirror_root: PathBuf,
    git_checked: OnceLock<Result<(), String>>,
}

impl GitSourceFetcher {
    /// Cache bare mirrors under `mirror_root` (created on first use).
    /// Composer's equivalent is `~/.composer/cache/vcs/`.
    pub fn new(mirror_root: impl Into<PathBuf>) -> Self {
        Self {
            mirror_root: mirror_root.into(),
            git_checked: OnceLock::new(),
        }
    }

    /// Fail once, with one clear error, when `git` can't run — instead of one
    /// error per package.
    fn ensure_git(&self) -> Result<()> {
        let checked =
            self.git_checked
                .get_or_init(|| match Command::new("git").arg("--version").output() {
                    Ok(out) if out.status.success() => Ok(()),
                    _ => Err(String::new()),
                });
        checked.clone().map_err(|_| git_missing("invocation"))
    }

    fn install_from(&self, url: &str, reference: &str, dest: &Path) -> Result<()> {
        let mirror = self.ensure_mirror(url, reference)?;

        // `git clone` refuses a non-empty target, so remove any prior tree.
        let _ = std::fs::remove_dir_all(dest);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        run_git(
            "clone",
            url,
            None,
            &[
                "clone".as_ref(),
                "--quiet".as_ref(),
                mirror.as_os_str(),
                dest.as_os_str(),
            ],
        )?;
        run_git(
            "checkout",
            url,
            Some(dest),
            &[
                "checkout".as_ref(),
                "--quiet".as_ref(),
                "--detach".as_ref(),
                reference.as_ref(),
            ],
        )?;
        // Point origin back at the real remote (the clone set it to the local
        // mirror), so the package stays a usable checkout. Best-effort.
        let _ = run_git(
            "set-url",
            url,
            Some(dest),
            &[
                "remote".as_ref(),
                "set-url".as_ref(),
                "origin".as_ref(),
                url.as_ref(),
            ],
        );
        Ok(())
    }

    /// Ensure a bare mirror of `url` exists and contains `reference`. Clones
    /// on first use; otherwise fetches only when the revision is absent.
    fn ensure_mirror(&self, url: &str, reference: &str) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.mirror_root)
            .wrap_err_with(|| format!("creating {}", self.mirror_root.display()))?;
        let mirror = self.mirror_root.join(mirror_dir_name(url));
        if mirror.join("HEAD").is_file() {
            if !reference.is_empty() && !commit_present(&mirror, reference) {
                run_git(
                    "fetch",
                    url,
                    Some(&mirror),
                    &["remote".as_ref(), "update".as_ref(), "--prune".as_ref()],
                )?;
            }
        } else {
            let _ = std::fs::remove_dir_all(&mirror);
            run_git(
                "clone",
                url,
                None,
                &[
                    "clone".as_ref(),
                    "--mirror".as_ref(),
                    "--quiet".as_ref(),
                    url.as_ref(),
                    mirror.as_os_str(),
                ],
            )?;
        }
        Ok(mirror)
    }
}

impl SourceFetcher for GitSourceFetcher {
    fn install(&self, spec: &SourceSpec<'_>) -> Result<()> {
        self.ensure_git()?;
        let mut last_err = None;
        for url in spec.urls {
            match self.install_from(url, spec.reference, spec.dest) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| eyre!("no clone URL")))
    }
}

/// Install every source package in parallel. Returns how many were installed
/// (all of them; any failure aborts).
pub(crate) fn materialize_sources(
    fetcher: &dyn SourceFetcher,
    specs: &[SourceSpec<'_>],
) -> Result<usize> {
    use rayon::prelude::*;
    specs.par_iter().try_for_each(|spec| {
        if spec.urls.is_empty() {
            return Err(eyre!(
                "package `{}` has a git source but no clone URL",
                spec.package_name
            ));
        }
        fetcher
            .install(spec)
            .wrap_err_with(|| format!("installing `{}` from git source", spec.package_name))
    })?;
    Ok(specs.len())
}

/// `true` when `reference` resolves to a commit in the mirror.
fn commit_present(mirror: &Path, reference: &str) -> bool {
    run_git(
        "cat-file",
        "",
        Some(mirror),
        &[
            "cat-file".as_ref(),
            "-e".as_ref(),
            format!("{reference}^{{commit}}").as_ref(),
        ],
    )
    .is_ok()
}

/// A filesystem-safe directory name for a repository URL: the sanitized tail
/// of the URL plus a hash, so two URLs that sanitize alike never share a
/// mirror.
fn mirror_dir_name(url: &str) -> String {
    let sanitized: String = url
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let skip = sanitized.chars().count().saturating_sub(60);
    let tail: String = sanitized.chars().skip(skip).collect();
    let digest = sha1::Sha1::digest(url.as_bytes());
    let mut short = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(short, "{b:02x}");
    }
    format!("{tail}-{short}")
}

/// Run `git [-C cwd] <args...>`: a spawn failure means git is missing, and a
/// non-zero exit is classified from its stderr.
fn run_git(
    operation: &str,
    url: &str,
    cwd: Option<&Path>,
    args: &[&std::ffi::OsStr],
) -> Result<()> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args);
    let out = cmd.output().map_err(|_| git_missing(operation))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(classify_git(
            operation,
            url,
            &String::from_utf8_lossy(&out.stderr),
        ))
    }
}

fn git_missing(operation: &str) -> eyre::Report {
    InstallError::Vcs {
        failure: VcsFailure::GitMissing,
        operation: operation.to_owned(),
        url: String::new(),
        detail: "`git` could not be run".to_owned(),
    }
    .into()
}

/// Classify a failed git command from its stderr.
fn classify_git(operation: &str, url: &str, stderr: &str) -> eyre::Report {
    let s = stderr.to_ascii_lowercase();
    let failure = if [
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied",
        "access denied",
        "403",
        "terminal prompts disabled",
    ]
    .iter()
    .any(|m| s.contains(m))
    {
        VcsFailure::Auth
    } else if [
        "couldn't find remote ref",
        "did not match any",
        "reference is not a tree",
        "unable to read tree",
        "not a valid object name",
        "bad object",
        "pathspec",
    ]
    .iter()
    .any(|m| s.contains(m))
    {
        VcsFailure::MissingRef
    } else if [
        "repository not found",
        "could not read from remote repository",
        "not found",
    ]
    .iter()
    .any(|m| s.contains(m))
    {
        VcsFailure::Unreachable
    } else {
        VcsFailure::Other
    };
    InstallError::Vcs {
        failure,
        operation: operation.to_owned(),
        url: url.to_owned(),
        detail: stderr.trim().to_owned(),
    }
    .into()
}

#[cfg(test)]
pub(crate) mod tests;
