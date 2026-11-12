//! Install a resolved `composer.lock` into a project — the install-from-lock
//! half of a Composer client, without running PHP.
//!
//! Given a `composer.json` + `composer.lock` and a project root, this crate
//! reproduces what `composer install` does to the filesystem:
//!
//! 1. verify the lock's content-hash (warn, never block, on drift);
//! 2. preflight the lockfile (reject source-only / non-zip dists; warn about
//!    Composer plugins and unrun `scripts`);
//! 3. diff against `vendor/composer/installed.json` to skip up-to-date
//!    packages and sweep stale ones;
//! 4. download + verify + extract each changed dist in parallel into
//!    `vendor/<vendor>/<package>/`;
//! 5. materialize `type: path` packages (symlink-or-copy);
//! 6. apply cweagans-style patches ([`composer_patches`]);
//! 7. run the declarative install plugins — the Magento deploy and Laravel
//!    discovery ([`composer_installers`]);
//! 8. dump the autoloader ([`composer_autoload`]);
//! 9. link `vendor/bin` proxies.
//!
//! The **pubgrub solver is out of scope** — this consumes an already-resolved
//! lock. It is a sibling of [`composer_manifest`] (the lock model),
//! [`composer_installers`] (install-path routing + deploy + Laravel),
//! [`composer_autoload`] (`dump-autoload`), and [`composer_patches`].
//!
//! # Seams
//!
//! Three app-specific concerns are injected so the crate carries no
//! HTTP-client, progress-rendering, or paths policy of its own:
//!
//! - [`Fetcher`] — the single HTTP GET that populates the dist cache.
//!   [`ReqwestFetcher`] is the batteries-included default (Composer User-Agent,
//!   sha1 verify, atomic placement, bounded retry).
//! - [`Progress`] — per-package download/extract callbacks. [`NoProgress`] is
//!   the silent default.
//! - `cache_root` (on [`InstallEnv`]) — where dist archives are cached.
//!
//! Root-script execution is opted in via [`ScriptHooks`]; pass `None` for the
//! deterministic scripts-off behavior.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use composer_install::{InstallEnv, InstallOptions, NoProgress, ReqwestFetcher, install_from_lock};
//!
//! # fn main() -> eyre::Result<()> {
//! let fetcher = ReqwestFetcher::new()?;
//! let progress = NoProgress;
//! let env = InstallEnv {
//!     fetcher: &fetcher,
//!     progress: &progress,
//!     cache_root: Path::new("/tmp/composer-dist-cache"),
//! };
//! let summary = install_from_lock(&env, Path::new("."), InstallOptions::default(), None)?;
//! println!("installed {} package(s)", summary.packages_installed);
//! # Ok(())
//! # }
//! ```

pub mod archive;
pub mod auth;
pub mod downloader;
pub mod fetch;
pub mod progress;

mod bin_proxy;
mod orchestrate;
mod path_link;

pub use archive::{detect_zip_top_level, extract_zip};
pub use auth::{
    AuthCredentials, parse_composer_auth_env, read_all_auth, read_auth_from_composer_json,
    read_auth_json, read_composer_auth_env, read_global_auth_json,
};
pub use downloader::{DistCandidate, DistOutcome, DistRequest, fetch_and_extract_dists};
pub use fetch::{FetchSpec, Fetcher, ReqwestFetcher};
pub use orchestrate::{
    InstallEnv, InstallOptions, InstallSummary, LinkMode, ScriptHooks, install_from_lock,
    install_from_lock_with_patches,
};
pub use progress::{NoProgress, Progress};
