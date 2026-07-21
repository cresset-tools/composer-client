# composer-manifest

The Composer **data model** — the native (no-PHP) representation of a Composer project's
files. Part of [`composer-client`](https://github.com/cresset-tools/composer-client).

## What it holds

- **`lockfile`** — typed `composer.json` / `composer.lock` IO and editing primitives:
  parse the lock into `Lock` / `LockPackage`, read `dist`/`autoload` metadata, edit
  `require`/`platform` entries in place, and recompute `content-hash` — without invoking
  `composer` or re-resolving the dependency graph.
- **`metadata`** — Packagist v2 (`/p2/`) metadata expansion: applies the `composer/2.0`
  minified-diff algorithm (via [`composer-wire`](https://crates.io/crates/composer-wire))
  and materializes each version into a typed `LockPackage`.

Byte-exact JSON round-tripping uses [`composer-php-json`](https://crates.io/crates/composer-php-json);
version algebra uses [`composer-semver`](https://crates.io/crates/composer-semver).

## License

[EUPL-1.2](../../LICENSE).
