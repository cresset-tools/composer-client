//! Native reimplementations of the declarative Composer install plugins that
//! PHP projects (Magento / Mage-OS especially) depend on. This crate never runs
//! the PHP plugins or Composer scripts — it reproduces their declarative on-disk
//! effect natively. Every input lives in `composer.json`, which is what makes a
//! native port possible.
//!
//! - [`paths`] — `composer/installers`. A generic package-`type` → install-path
//!   router (e.g. `magento-theme` → `app/design/frontend/{$name}/`,
//!   `wordpress-plugin` → `wp-content/plugins/{$name}/`), with root
//!   `extra.installer-paths` overrides. Pure relocation; no copying.
//!
//! - [`deploy`] — `magento/magento-composer-installer`. A `magento2-component`
//!   package (canonically `magento/magento2-base`) declares an `extra.map` of
//!   `[source, dest]` pairs copied into the project root (this is how
//!   `index.php`, `pub/`, the `app/etc/*` skeleton land) plus an `extra.chmod`
//!   list of permission masks. Also generates `app/etc/vendor_path.php`, which
//!   Magento's bootstrap reads to locate `vendor/`.
//!
//! - [`laravel`] — Laravel's `post-autoload-dump` package discovery: writes the
//!   `bootstrap/cache/packages.php` manifest the framework would otherwise
//!   generate via `Illuminate\Foundation\ComposerScripts::postAutoloadDump`.

pub mod deploy;
pub mod laravel;
pub mod paths;

pub use deploy::{ChmodEntry, DeployPlan, DeployStats, VENDOR_PATH_PHP, apply_deploy, plan_deploy};
pub use laravel::{
    PACKAGES_CACHE, STALE_CACHES, blocking_post_autoload_dump, build_package_manifest,
    clear_compiled, only_reproduced_scripts, render_packages_php,
};
pub use paths::{
    InstallerPaths, install_path, install_path_relative_to_repo, unsupported_framework,
};
