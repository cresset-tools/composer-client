//! Typed install errors.
//!
//! Errors travel as [`eyre::Report`]s, but the ones a caller may want to
//! treat specially have an [`InstallError`] at their root, so they can be
//! recovered with `report.downcast_ref::<InstallError>()` (or by walking
//! `report.chain()`) and mapped to an exit code or a tool-specific hint.

use std::fmt;
use std::path::PathBuf;

/// An install failure a caller can match on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallError {
    /// The project has no `composer.json`.
    MissingManifest {
        /// The path that was looked for.
        path: PathBuf,
    },
    /// The project has a `composer.json` but no `composer.lock`.
    MissingLock {
        /// The path that was looked for.
        path: PathBuf,
    },
    /// `composer.json` is not valid JSON.
    InvalidManifest {
        /// The manifest's path.
        path: PathBuf,
        /// The parser's message.
        detail: String,
    },
    /// The lock needs features this installer does not handle (an
    /// unsupported dist type, a non-git source, …). Every blocker is listed,
    /// so the user can fix them in one pass.
    Unsupported {
        /// One human-readable reason per blocking package.
        reasons: Vec<String>,
    },
    /// A `git` command failed while installing a source package.
    Vcs {
        /// What kind of failure, classified from git's stderr.
        failure: VcsFailure,
        /// The git step that failed (`clone`, `fetch`, `checkout`, …).
        operation: String,
        /// The repository URL, or empty when git itself could not run.
        url: String,
        /// git's stderr, trimmed.
        detail: String,
    },
}

/// Why a git command failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VcsFailure {
    /// `git` is not installed or not on `PATH`.
    GitMissing,
    /// The repository is private or the credentials were refused.
    Auth,
    /// The locked commit is not on the remote.
    MissingRef,
    /// The repository could not be reached.
    Unreachable,
    /// Anything else; see the detail.
    Other,
}

impl VcsFailure {
    /// A tool-neutral suggestion for the user.
    pub fn hint(self) -> &'static str {
        match self {
            Self::GitMissing => {
                "install git and make sure it is on PATH; git sources are installed with the \
                 system git"
            }
            Self::Auth => {
                "the repository is private or the credentials are wrong; configure a git \
                 credential helper or ssh key"
            }
            Self::MissingRef => {
                "the locked commit is not on the remote; the lock may point at an unpushed or \
                 force-pushed commit, so update the lock to re-resolve it"
            }
            Self::Unreachable => {
                "the repository could not be reached; check the URL and your network access"
            }
            Self::Other => "see git's output above",
        }
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingManifest { path } => {
                write!(f, "{} not found — not a Composer project", path.display())
            }
            Self::MissingLock { path } => write!(
                f,
                "{} not found — run `composer update` first to generate it",
                path.display()
            ),
            Self::InvalidManifest { path, detail } => {
                write!(f, "parsing {}: {detail}", path.display())
            }
            Self::Unsupported { reasons } => {
                write!(
                    f,
                    "this lockfile requires features this installer does not handle:"
                )?;
                for r in reasons {
                    write!(f, "\n  - {r}")?;
                }
                Ok(())
            }
            Self::Vcs {
                failure,
                operation,
                url,
                detail,
            } => {
                write!(f, "git {operation} failed")?;
                if !url.is_empty() {
                    write!(f, " for {url}")?;
                }
                if !detail.is_empty() {
                    write!(f, ": {detail}")?;
                }
                write!(f, "\nhint: {}", failure.hint())
            }
        }
    }
}

impl std::error::Error for InstallError {}
