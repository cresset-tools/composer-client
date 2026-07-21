# composer-installers

Native Rust reimplementation of the **declarative** Composer install plugins that PHP
projects (Magento / Mage-OS especially) rely on. It reproduces their on-disk effect —
without running PHP, the plugins, or any Composer script.

Part of [`composer-client`](https://github.com/cresset-tools/composer-client).

## What it covers

- **`composer/installers`** — a generic package-`type` → install-path router
  (`magento-theme` → `app/design/frontend/{$name}/`, `wordpress-plugin` →
  `wp-content/plugins/{$name}/`, Drupal / Shopware / PrestaShop, …), honoring root
  `extra.installer-paths` overrides. Pure relocation; no copying.
- **`magento/magento-composer-installer`** — a `magento2-component` package (canonically
  `magento/magento2-base`) declares an `extra.map` of `[source, dest]` pairs copied into the
  project root (`index.php`, `pub/`, the `app/etc/*` skeleton), plus an `extra.chmod` list;
  also generates `app/etc/vendor_path.php`.
- **Laravel `post-autoload-dump`** — writes the `bootstrap/cache/packages.php` package
  discovery manifest the framework would otherwise generate.

Every input lives in `composer.json`, which is what makes a native, PHP-free port possible.

## License

[EUPL-1.2](../../LICENSE).
