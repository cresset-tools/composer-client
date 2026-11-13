# Changelog

## [0.2.0](https://github.com/cresset-tools/composer-client/compare/composer-patches-v0.1.0...composer-patches-v0.2.0) (2026-11-13)


### ⚠ BREAKING CHANGES

* **composer-patches:** PatchPlan has a new public field, deferred, so struct literals must add it (or use ..Default::default()).

### Features

* **composer-patches:** bare-list patches, vendor-shipped patches, bougie's output names ([707d605](https://github.com/cresset-tools/composer-client/commit/707d605938e9dd8e56be39038496901afedf45df))
* **composer-patches:** build_plan, the patch-plan builder bougie and magebuild share ([c53858d](https://github.com/cresset-tools/composer-client/commit/c53858dc8f5babd01972f3b38b06b87aa44c6c95))


### Bug Fixes

* **composer-patches:** give a created file the patch's line endings on every platform ([ebd1217](https://github.com/cresset-tools/composer-client/commit/ebd12177b17a34d95fad28a6b87e10ad14146518))

## 0.1.0 (2026-07-21)


### Features

* composer-patches — native cweagans/composer-patches ([b0c5960](https://github.com/cresset-tools/composer-client/commit/b0c59600bbfa60e12f2dfc18163acdd82159a686))
