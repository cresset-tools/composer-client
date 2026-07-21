# composer-client

Client-side Composer behavior for the [cresset-tools](https://github.com/cresset-tools)
family — the layer **above** [`composer-rs`](https://github.com/cresset-tools/composer-rs).

`composer-rs` models the Composer **contract** (version algebra, wire/metadata format,
PHP-exact JSON encoding — the parts defined by Composer's spec, shared by clients *and*
servers). `composer-client` holds the client **behavior** that more than one client needs —
today [`bougie`](https://github.com/cresset-tools/bougie) (the package manager) and
`magebuild` (the build orchestrator): declarative install plugins, autoload generation, and
install-from-lock. A repository server never dumps an autoloader or installs to paths, so
these stay out of `composer-rs` by its own contract-vs-behavior boundary.

Everything here reproduces the **declarative on-disk effect** of Composer's install-time
machinery natively — no PHP runtime, no plugin execution, no Composer phar.

## Crates

| Crate | What it is | Status |
|-------|------------|--------|
| [`composer-installers`](crates/composer-installers) | Declarative install-plugin reimplementations: `composer/installers` (package-`type` → install-path routing), `magento/magento-composer-installer` (`extra.map` copy + chmod + `vendor_path.php`), and Laravel `post-autoload-dump` discovery. | available |
| `composer-autoload` | Byte-equivalent `composer dump-autoload` (the `vendor/composer/autoload_*.php` set). | planned |
| `composer-install` | Install-from-lock primitive: parallel dist download + extract + verify → populated `vendor/`, over pluggable `Fetcher`/`Layout` traits. | planned |

## License

[EUPL-1.2](LICENSE).
