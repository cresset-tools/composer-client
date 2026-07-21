# composer-patches

Native Rust reimplementation of [`cweagans/composer-patches`](https://github.com/cweagans/composer-patches):
resolve, download, and apply patch files to installed packages during a Composer install —
**without running the PHP plugin** or shelling out to `git`/`patch`.

Part of [`composer-client`](https://github.com/cresset-tools/composer-client).

## What it does

- Parses `patches/*.patch` (unified/git diffs, multi-file aware) via the
  [`flickzeug`](https://crates.io/crates/flickzeug) fuzzy-matching diff library.
- Owns `-p` strip inference, file routing, and atomic on-disk application.
- Records a re-application invariant (`patches.lock.json`) with content fingerprints so a
  patched tree is detected and not double-applied.
- Generates diffs (the inverse) for a "create patch" flow via [`similar`](https://crates.io/crates/similar).

The core is filesystem/PHP-agnostic: it parses, plans, and applies; the host decides *when*.

## License

[EUPL-1.2](../../LICENSE).
