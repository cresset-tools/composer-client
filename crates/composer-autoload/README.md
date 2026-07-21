# composer-autoload

Generate Composer-compatible `vendor/composer/autoload_*.php` from a resolved dependency
tree — **byte-equivalent** to `composer dump-autoload` (pinned to a specific upstream
Composer version), without running PHP or Composer.

Part of [`composer-client`](https://github.com/cresset-tools/composer-client).

## What it emits

Every file `composer dump-autoload` writes under `vendor/`:
`autoload.php`, `composer/autoload_{namespaces,psr4,classmap,files,real,static}.php`,
the vendored `ClassLoader.php` / `InstalledVersions.php` / `LICENSE`, and
`installed.{json,php}`. Conditional features are wired in: `--optimize`,
`--classmap-authoritative`, `--no-dev`, `--apcu-autoloader`, the
`config.autoloader-suffix` override, and `config.platform-check` → `platform_check.php`.

Performance-first: parallel file scan, SIMD byte search in the classmap pipeline, lazy I/O.

Built on [`composer-installers`](https://github.com/cresset-tools/composer-client/tree/main/crates/composer-installers)
(install-path resolution), [`composer-php-json`](https://crates.io/crates/composer-php-json)
(byte-exact PHP `json_encode`), and [`composer-semver`](https://crates.io/crates/composer-semver)
(the platform-check constraint math).

## License

[EUPL-1.2](../../LICENSE).
