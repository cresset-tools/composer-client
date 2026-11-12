# Changelog

## [0.2.0](https://github.com/cresset-tools/composer-client/compare/composer-install-v0.1.1...composer-install-v0.2.0) (2026-07-22)


### ⚠ BREAKING CHANGES

* **composer-install:** `InstallOptions` gains a `link_mode` field (construct with `..Default::default()`), and `fetch_and_extract_dists` gains a trailing `link_mode` parameter.

### Features

* **composer-install:** hard-link-from-store install mode ([091396f](https://github.com/cresset-tools/composer-client/commit/091396ffac7f23d31c8eda9b6b8f8ad96a4014f1))

## 0.1.0 (2026-07-21)


### Features

* composer-install — install a composer.lock, decoupled from the app ([ab50a9d](https://github.com/cresset-tools/composer-client/commit/ab50a9db4801560e2184b6afa03975a11ea35228))
