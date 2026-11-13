//! Typed install errors.
//!
//! Errors travel as [`eyre::Report`]s, but the ones a caller may want to
//! treat specially have an [`InstallError`] at their root, so they can be
//! recovered with `report.downcast_ref::<InstallError>()` (or by walking
//! `report.chain()`) and mapped to an exit code or a tool-specific hint.

use std::fmt;

/// An install failure a caller can match on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallError {
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
