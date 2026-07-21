//! Composer (the PHP package manager) data model.
//!
//! Reimplements the Composer surface natively — no phar, no PHP. This crate
//! holds the Composer-format data models a native implementation reads and
//! writes: `composer.json` / `composer.lock` ([`lockfile`]) and the Packagist
//! v2 metadata wire format ([`metadata`]).

pub mod lockfile;
pub mod metadata;

/// Re-export of the [`composer_php_json`] crate (the byte-exact PHP
/// `json_encode`, from the shared `composer-rs` workspace), available here as
/// `composer_manifest::php_json`. New consumers can also depend on
/// `composer-php-json` directly.
pub use composer_php_json as php_json;
