//! Build a project's [`PatchPlan`] from its root `composer.json`.
//!
//! The one place both bougie and magebuild get their plan from, so the two
//! apply the same patches the same way:
//!
//! - `extra.patches` / `extra.patches-file` (cweagans), unioned with
//! - the zero-config patches directory (`patches/`, or
//!   `extra.composer-patches.patches-dir`), whose files carry no target: it is
//!   inferred from their diff paths against each locked package's install
//!   path. A file `composer.json` already declares is left out of the scan.
//!
//! Each patch is then materialized: a local file is read and hashed, a URL is
//! downloaded into the patch cache through the caller's [`PatchFetcher`], and a
//! declared `sha256` is checked. A package-scoped patch whose file is missing
//! but lies inside `vendor/` ships in a package that is not installed yet; it
//! becomes a [`DeferredPatch`] for the installer to apply after extraction.
//!
//! Settings follow cweagans' precedence: a [`PlanOptions`] override, then
//! `composer.json` `extra`, then the `COMPOSER_*` environment variable, then
//! cweagans' default.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr, bail};
use serde_json::Value;

use crate::model::{FailureMode, Patch, PatchScope, PatchSource};
use crate::plan::{DeferredPatch, MaterializedPatch, PatchPlan, RootPatch};
use crate::{content_sha256, lock, resolve_patches_dir, resolve_root};

/// Downloads a remote patch. The host supplies it so this crate stays free of
/// an HTTP client.
pub trait PatchFetcher {
    /// Download `url` to `dest`, creating `dest` only once the download is
    /// complete. Called only when `dest` does not exist yet.
    ///
    /// # Errors
    ///
    /// Any failure to download or write the file.
    fn fetch(&self, url: &str, dest: &Path) -> Result<()>;
}

/// A [`PatchFetcher`] that refuses every download, for a project known to
/// declare no remote patches, or a read-only caller.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFetch;

impl PatchFetcher for NoFetch {
    fn fetch(&self, url: &str, _dest: &Path) -> Result<()> {
        bail!("patch `{url}` must be downloaded, and no fetcher was supplied")
    }
}

/// Overrides for [`build_plan`]. `None` defers to `composer.json`, then the
/// environment, then cweagans' default.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Turn patching on or off. Else `extra.enable-patching`, else on.
    pub enable: Option<bool>,
    /// Abort the install on the first patch that fails to apply. Else
    /// `extra.composer-exit-on-patch-failure`, else
    /// `COMPOSER_EXIT_ON_PATCH_FAILURE` being set, else skip and warn.
    pub exit_on_failure: Option<bool>,
    /// Leave `PATCHES.txt` out of patched packages. Else
    /// `extra.composer-patches-skip-reporting`, else
    /// `COMPOSER_PATCHES_SKIP_REPORTING` being set, else write it.
    pub skip_report: Option<bool>,
    /// Write the human-readable `patches` view into `patches.lock.json`
    /// besides the fingerprints.
    pub write_lock: bool,
    /// The patches directory, relative to the project root. Else
    /// `extra.composer-patches.patches-dir`, else `patches`.
    pub patches_dir: Option<String>,
    /// Where downloaded patches are cached. Required only when a patch is
    /// given by URL.
    pub cache_dir: Option<PathBuf>,
}

/// Build the plan for an install of `project_root`, or `None` when there is
/// nothing to apply and nothing to undo.
///
/// With patching off, or nothing declared, a plan still comes back when an
/// earlier install applied patches: the installer must restore those packages
/// to pristine and clear `patches.lock.json`.
///
/// # Errors
///
/// An unparseable `composer.json` or patch declaration, a patches-directory
/// file whose target can't be inferred, a missing local patch outside
/// `vendor/`, a failed download, or a `sha256` mismatch.
pub fn build_plan(
    project_root: &Path,
    opts: &PlanOptions,
    fetcher: &dyn PatchFetcher,
) -> Result<Option<PatchPlan>> {
    let Ok(bytes) = std::fs::read(project_root.join("composer.json")) else {
        return Ok(None);
    };
    let composer_json: Value =
        serde_json::from_slice(&bytes).wrap_err("parsing composer.json for patches")?;

    let applied = lock::read(project_root);
    let skip_report = setting(
        opts.skip_report,
        &composer_json,
        "/extra/composer-patches-skip-reporting",
        "COMPOSER_PATCHES_SKIP_REPORTING",
    );
    let enabled = opts
        .enable
        .or_else(|| {
            composer_json
                .pointer("/extra/enable-patching")
                .and_then(Value::as_bool)
        })
        .unwrap_or(true);
    let patches = if enabled {
        resolve_all(project_root, &composer_json, opts.patches_dir.as_deref())?
    } else {
        Vec::new()
    };

    if patches.is_empty() {
        if applied.is_empty() {
            return Ok(None);
        }
        return Ok(Some(PatchPlan {
            applied,
            failure_mode: FailureMode::SkipAndWarn,
            skip_report,
            write_lock: opts.write_lock,
            ..PatchPlan::default()
        }));
    }

    let failure_mode = if setting(
        opts.exit_on_failure,
        &composer_json,
        "/extra/composer-exit-on-patch-failure",
        "COMPOSER_EXIT_ON_PATCH_FAILURE",
    ) {
        FailureMode::Abort
    } else {
        FailureMode::SkipAndWarn
    };

    let mut by_target: BTreeMap<String, Vec<MaterializedPatch>> = BTreeMap::new();
    let mut root_patches: Vec<RootPatch> = Vec::new();
    let mut deferred: Vec<DeferredPatch> = Vec::new();
    for patch in patches {
        if matches!(patch.scope, PatchScope::Package)
            && let Some(d) = defer_vendor_shipped(project_root, &patch)
        {
            deferred.push(d);
            continue;
        }
        let materialized = materialize(project_root, opts.cache_dir.as_deref(), fetcher, &patch)?;
        match patch.scope {
            PatchScope::Package => by_target
                .entry(patch.target)
                .or_default()
                .push(materialized),
            PatchScope::Root { packages } => root_patches.push(RootPatch {
                patch: materialized,
                packages,
            }),
        }
    }

    Ok(Some(PatchPlan {
        patches: by_target,
        root_patches,
        deferred,
        applied,
        failure_mode,
        skip_report,
        write_lock: opts.write_lock,
    }))
}

/// Every root patch, unmaterialized: `extra.patches` / patches-file, then the
/// patches directory minus the files those declare, then exact duplicates
/// (same target, source and description) dropped. `patches_dir` overrides
/// `extra.composer-patches.patches-dir`. Reads no patch file and downloads
/// nothing, so it suits read-only listings.
///
/// # Errors
///
/// An unparseable patch declaration, or a patches-directory file whose target
/// can't be inferred.
pub fn resolve_all(
    project_root: &Path,
    composer_json: &Value,
    patches_dir: Option<&str>,
) -> Result<Vec<Patch>> {
    let mut patches = resolve_root(composer_json, project_root)?;
    let dir = project_root.join(patches_dir_name(composer_json, patches_dir));
    // A declared file is left out of the scan: the declaration owns its
    // target and depth, and the dedup below can't pair the two (declared
    // sources are relative, scanned ones absolute, and descriptions differ).
    let declared: Vec<PathBuf> = patches
        .iter()
        .filter_map(|p| match &p.source {
            PatchSource::Local(rel) => Some(absolute_in(project_root, rel)),
            PatchSource::Remote(_) => None,
        })
        .collect();
    let install_paths = lock_install_paths(project_root, composer_json);
    patches.extend(resolve_patches_dir(&dir, &install_paths, &declared)?);

    let mut seen = HashSet::new();
    patches.retain(|p| {
        let source = match &p.source {
            PatchSource::Local(path) => path.to_string_lossy().into_owned(),
            PatchSource::Remote(url) => url.clone(),
        };
        seen.insert((p.target.clone(), source, p.description.clone()))
    });
    Ok(patches)
}

/// The patches directory: `overridden`, else
/// `extra.composer-patches.patches-dir`, else `patches`.
#[must_use]
pub fn patches_dir_name(composer_json: &Value, overridden: Option<&str>) -> String {
    overridden
        .or_else(|| {
            composer_json
                .pointer("/extra/composer-patches/patches-dir")
                .and_then(Value::as_str)
        })
        .unwrap_or("patches")
        .to_string()
}

/// Each locked package (runtime and dev) with its install path relative to the
/// project root: `vendor/<name>`, or where `extra.installer-paths` and the
/// package type put it. An absent or unparseable `composer.lock` gives no
/// paths, and a patches-directory file then fails to infer its target.
#[must_use]
pub fn lock_install_paths(project_root: &Path, composer_json: &Value) -> Vec<(String, String)> {
    let Some(lock) = std::fs::read(project_root.join("composer.lock"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Vec::new();
    };
    let installer_paths = composer_installers::InstallerPaths::parse(composer_json);
    let mut out = Vec::new();
    for key in ["packages", "packages-dev"] {
        for p in lock
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                continue;
            };
            let ptype = p.get("type").and_then(Value::as_str);
            let rel = composer_installers::install_path(name, ptype, &installer_paths);
            out.push((name.to_string(), rel));
        }
    }
    out
}

/// A boolean setting: the override, else `composer.json` at `pointer`, else
/// whether `env` is set.
fn setting(overridden: Option<bool>, composer_json: &Value, pointer: &str, env: &str) -> bool {
    overridden
        .or_else(|| composer_json.pointer(pointer).and_then(Value::as_bool))
        .unwrap_or_else(|| std::env::var_os(env).is_some())
}

fn absolute_in(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// A package-scoped local patch whose file is missing and lies inside
/// `vendor/`: it ships in a package that is not installed yet. `None` for
/// anything else, including a missing file outside `vendor/`, which stays an
/// error at materialize time so a mistyped project patch fails before the
/// install.
fn defer_vendor_shipped(project_root: &Path, patch: &Patch) -> Option<DeferredPatch> {
    let PatchSource::Local(rel) = &patch.source else {
        return None;
    };
    let abs = absolute_in(project_root, rel);
    if abs.exists() || !abs.starts_with(project_root.join("vendor")) {
        return None;
    }
    Some(DeferredPatch {
        target: patch.target.clone(),
        description: patch.description.clone(),
        origin: rel.to_string_lossy().into_owned(),
        local_path: abs,
        depth: patch.depth,
        declared_sha256: patch.sha256.clone(),
    })
}

/// Read one patch's bytes (a project file, or a URL downloaded into the cache)
/// and check them against a declared `sha256`.
fn materialize(
    project_root: &Path,
    cache_dir: Option<&Path>,
    fetcher: &dyn PatchFetcher,
    patch: &Patch,
) -> Result<MaterializedPatch> {
    let (local_path, origin) = match &patch.source {
        PatchSource::Local(rel) => (
            absolute_in(project_root, rel),
            rel.to_string_lossy().into_owned(),
        ),
        PatchSource::Remote(url) => {
            let Some(cache_dir) = cache_dir else {
                bail!(
                    "patch `{}` for `{}` is a URL, and no patch cache was configured",
                    patch.description,
                    patch.target
                );
            };
            (
                download(cache_dir, fetcher, url, patch.sha256.as_deref())?,
                url.clone(),
            )
        }
    };
    let bytes = std::fs::read(&local_path).wrap_err_with(|| {
        format!(
            "patch file `{}` for `{}` not found",
            local_path.display(),
            patch.target
        )
    })?;
    let sha = content_sha256(&bytes);
    if let Some(declared) = &patch.sha256
        && !declared.eq_ignore_ascii_case(&sha)
    {
        if matches!(patch.source, PatchSource::Remote(_)) {
            // Don't leave a bad download in the cache under the declared sha.
            let _ = std::fs::remove_file(&local_path);
        }
        bail!(
            "patch `{}` for `{}`: sha256 mismatch (declared {declared}, actual {sha})",
            patch.description,
            patch.target
        );
    }
    Ok(MaterializedPatch {
        description: patch.description.clone(),
        origin,
        local_path,
        content_sha256: sha,
        depth: patch.depth,
    })
}

/// The cached copy of `url`, downloaded first if absent. The cache key is the
/// declared sha256 when there is one (content-addressed), else a hash of the
/// URL, so an undeclared patch is trusted on first use.
fn download(
    cache_dir: &Path,
    fetcher: &dyn PatchFetcher,
    url: &str,
    declared_sha: Option<&str>,
) -> Result<PathBuf> {
    let key = declared_sha.map_or_else(|| content_sha256(url.as_bytes()), str::to_ascii_lowercase);
    let dest = cache_dir.join(format!("{key}.patch"));
    if !dest.exists() {
        std::fs::create_dir_all(cache_dir)
            .wrap_err_with(|| format!("creating patch cache `{}`", cache_dir.display()))?;
        fetcher
            .fetch(url, &dest)
            .wrap_err_with(|| format!("downloading patch `{url}`"))?;
    }
    Ok(dest)
}

#[cfg(test)]
mod tests;
