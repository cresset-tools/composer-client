# composer-install

Install a resolved `composer.lock` into a project — the **install-from-lock** half of a
Composer client, **without running PHP**.

Part of [`composer-client`](https://github.com/cresset-tools/composer-client).

## What it does

Given a `composer.json` + `composer.lock` and a project root, it reproduces what
`composer install` does to the filesystem:

1. verify the lock's content-hash (warn, never block, on drift);
2. preflight the lockfile (reject source-only / non-zip dists; warn about Composer plugins
   and unrun `scripts`);
3. diff against `vendor/composer/installed.json` to skip up-to-date packages and sweep
   stale ones;
4. download + verify (sha1) + extract each changed dist in parallel into
   `vendor/<vendor>/<package>/`;
5. materialize `type: path` packages (symlink-or-copy);
6. apply [cweagans](https://github.com/cweagans/composer-patches)-style patches
   ([`composer-patches`](../composer-patches));
7. run the declarative install plugins — the Magento deploy and Laravel discovery
   ([`composer-installers`](../composer-installers));
8. dump the autoloader ([`composer-autoload`](../composer-autoload));
9. link `vendor/bin` proxies.

The **pubgrub solver is out of scope** — this consumes an already-resolved lock.

## Seams

Three app-specific concerns are injected so the crate carries no HTTP-client,
progress-rendering, or paths policy of its own:

- **`Fetcher`** — the single HTTP GET that populates the dist cache. `ReqwestFetcher` is the
  batteries-included default (Composer `User-Agent`, sha1 verify, atomic placement, bounded
  retry). Bring your own to reuse a client, inject a proxy, or serve from a pre-seeded store.
- **`Progress`** — per-package download / extract callbacks. `NoProgress` is the silent
  default; a CLI implements the trait to drive a progress bar.
- **`cache_root`** (on `InstallEnv`) — where dist archives are cached across runs.

Root-script execution is opted in via the caller-supplied `ScriptHooks` trait; pass `None`
for deterministic scripts-off behavior.

## Example

```rust,no_run
use std::path::Path;
use composer_install::{InstallEnv, InstallOptions, NoProgress, ReqwestFetcher, install_from_lock};

let fetcher = ReqwestFetcher::new()?;
let progress = NoProgress;
let env = InstallEnv {
    fetcher: &fetcher,
    progress: &progress,
    cache_root: Path::new("/tmp/composer-dist-cache"),
};
let summary = install_from_lock(&env, Path::new("."), InstallOptions::default(), None)?;
println!("installed {} package(s)", summary.packages_installed);
# Ok::<(), eyre::Report>(())
```

## License

[EUPL-1.2](../../LICENSE).
