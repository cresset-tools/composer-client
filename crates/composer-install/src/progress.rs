//! Install progress reporting — a caller-supplied seam.
//!
//! The downloader runs a two-phase parallel fan-out (download every dist,
//! then extract every dist). It reports per-package progress through this
//! trait so a CLI can drive a progress bar while a library consumer stays
//! silent. The default [`NoProgress`] does nothing; this crate deliberately
//! depends on no rendering library (`indicatif` etc.) — that's the caller's
//! choice.

use crate::downloader::DistOutcome;

/// Per-package progress callbacks fired by the downloader. Both methods have
/// no-op defaults, so an implementor overrides only the phase it cares about.
/// Called from a rayon parallel iterator, so implementations must be `Sync`
/// and cheap/non-blocking.
pub trait Progress: Sync {
    /// One dist finished its download phase — either freshly fetched or a
    /// cache hit (see [`DistOutcome`]). Fires once per package.
    fn on_download(&self, package: &str, outcome: DistOutcome) {
        let _ = (package, outcome);
    }

    /// One dist finished extracting into its `vendor/` destination. Fires
    /// once per package, after every download has completed.
    fn on_extract(&self, package: &str) {
        let _ = package;
    }
}

/// The silent default: reports nothing. Suitable for library use and for the
/// non-interactive / `--quiet` / machine-output CLI paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {}
