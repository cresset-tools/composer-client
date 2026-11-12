use std::cell::Cell;

use super::*;
use crate::model::DepthSpec;
use serde_json::json;
use tempfile::TempDir;

/// A project with `composer.json`, a lock holding `acme/foo` and `acme/bar`,
/// and the given files.
fn project(composer_json: &str, files: &[(&str, &str)]) -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("composer.json"), composer_json).unwrap();
    std::fs::write(
        root.join("composer.lock"),
        r#"{"content-hash":"x","packages":[
            {"name":"acme/foo","version":"1.0.0","type":"library"},
            {"name":"acme/bar","version":"1.0.0","type":"library"}
        ],"packages-dev":[]}"#,
    )
    .unwrap();
    for (path, body) in files {
        let f = root.join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, body).unwrap();
    }
    tmp
}

/// A project-root-relative diff touching one file in `vendor/<pkg>`.
fn diff_for(pkg: &str) -> String {
    format!("--- a/vendor/{pkg}/src/A.php\n+++ b/vendor/{pkg}/src/A.php\n@@ -1 +1 @@\n-old\n+new\n")
}

fn plan(tmp: &TempDir) -> Result<Option<PatchPlan>> {
    build_plan(tmp.path(), &PlanOptions::default(), &NoFetch)
}

#[test]
fn none_when_no_patches_anywhere() {
    let tmp = project(r#"{"name":"acme/app"}"#, &[]);
    assert!(plan(&tmp).unwrap().is_none());
}

#[test]
fn none_without_composer_json() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(plan(&tmp).unwrap().is_none());
}

#[test]
fn infers_patches_dir_target_and_depth() {
    let tmp = project(
        r#"{"name":"acme/app"}"#,
        &[("patches/foo.patch", &diff_for("acme/foo"))],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    let foo = &plan.patches["acme/foo"];
    assert_eq!(foo.len(), 1);
    assert_eq!(foo[0].description, "foo.patch");
    // a/(1) + vendor/acme/foo(3) = depth 4.
    assert_eq!(foo[0].depth, DepthSpec::Fixed(4));
}

#[test]
fn unions_composer_json_and_patches_dir() {
    // foo.patch is declared AND in the scanned directory: it appears once,
    // under its declaration. bar.patch is found by the scan.
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":{"Declared fix":"patches/foo.patch"}}}}"#,
        &[
            ("patches/foo.patch", &diff_for("acme/foo")),
            ("patches/bar.patch", &diff_for("acme/bar")),
        ],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    let foo = &plan.patches["acme/foo"];
    assert_eq!(foo.len(), 1);
    assert_eq!(foo[0].description, "Declared fix");
    assert_eq!(plan.patches["acme/bar"].len(), 1);
    assert_eq!(plan.patches.len(), 2);
}

#[test]
fn declared_package_relative_patch_is_not_scanned() {
    // Package-relative headers: the scan alone could not infer a target, so
    // the declaration must keep the file out of the scan entirely.
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":{"Fix A":"patches/foo.patch"}}}}"#,
        &[(
            "patches/foo.patch",
            "--- a/src/A.php\n+++ b/src/A.php\n@@ -1 +1 @@\n-old\n+new\n",
        )],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    assert_eq!(plan.patches["acme/foo"].len(), 1);
    assert_eq!(plan.patches["acme/foo"][0].description, "Fix A");
}

#[test]
fn reads_patches_file_and_custom_dir() {
    let tmp = project(
        r#"{"extra":{"patches-file":"my-patches.json","composer-patches":{"patches-dir":"fixes"}}}"#,
        &[
            (
                "my-patches.json",
                r#"{"patches":{"acme/foo":{"From patches-file":"fixes/foo.patch"}}}"#,
            ),
            ("fixes/foo.patch", &diff_for("acme/foo")),
            ("fixes/bar.patch", &diff_for("acme/bar")),
            // Not the configured directory: ignored.
            ("patches/stray.patch", &diff_for("acme/bar")),
        ],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    assert_eq!(plan.patches["acme/foo"][0].description, "From patches-file");
    assert_eq!(plan.patches["acme/bar"].len(), 1);
}

#[test]
fn patches_dir_option_overrides_composer_json() {
    let tmp = project(
        r#"{"extra":{"composer-patches":{"patches-dir":"fixes"}}}"#,
        &[
            ("fixes/foo.patch", &diff_for("acme/foo")),
            ("mine/bar.patch", &diff_for("acme/bar")),
        ],
    );
    let opts = PlanOptions {
        patches_dir: Some("mine".into()),
        ..PlanOptions::default()
    };
    let plan = build_plan(tmp.path(), &opts, &NoFetch)
        .unwrap()
        .expect("a plan");
    assert!(!plan.patches.contains_key("acme/foo"));
    assert_eq!(plan.patches["acme/bar"].len(), 1);
}

#[test]
fn accepts_bare_list_form() {
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":["patches/foo.patch"]}}}"#,
        &[("patches/foo.patch", &diff_for("acme/foo"))],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    assert_eq!(plan.patches["acme/foo"][0].description, "patches/foo.patch");
}

#[test]
fn infers_target_under_installer_paths() {
    let tmp = project(
        r#"{"extra":{"installer-paths":{"app/code/Acme/Foo":["acme/foo"]}}}"#,
        &[(
            "patches/foo.patch",
            "--- a/app/code/Acme/Foo/src/A.php\n+++ b/app/code/Acme/Foo/src/A.php\n@@ -1 +1 @@\n-old\n+new\n",
        )],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    assert_eq!(plan.patches["acme/foo"].len(), 1);
}

#[test]
fn defers_vendor_shipped_patch() {
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":{"Vendor fix":"vendor/acme/bar/fix.patch"}}}}"#,
        &[],
    );
    let plan = plan(&tmp).unwrap().expect("a plan");
    assert!(plan.patches.is_empty(), "must not be read before install");
    assert_eq!(plan.deferred.len(), 1);
    assert_eq!(plan.deferred[0].target, "acme/foo");
    assert_eq!(plan.deferred[0].origin, "vendor/acme/bar/fix.patch");
}

#[test]
fn missing_patch_outside_vendor_is_an_error() {
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":{"Typo":"patches/typo.patch"}}}}"#,
        &[],
    );
    let err = plan(&tmp).unwrap_err();
    assert!(format!("{err:#}").contains("not found"), "{err:#}");
}

#[test]
fn enable_patching_false_gives_no_plan() {
    let tmp = project(
        r#"{"extra":{"enable-patching":false,"patches":{"acme/foo":{"x":"patches/foo.patch"}}}}"#,
        &[("patches/foo.patch", &diff_for("acme/foo"))],
    );
    assert!(plan(&tmp).unwrap().is_none());

    // The option wins over composer.json.
    let opts = PlanOptions {
        enable: Some(true),
        ..PlanOptions::default()
    };
    let plan = build_plan(tmp.path(), &opts, &NoFetch)
        .unwrap()
        .expect("a plan");
    assert_eq!(plan.patches["acme/foo"].len(), 1);
}

#[test]
fn disabled_with_applied_state_gives_cleanup_plan() {
    let tmp = project(
        r#"{"extra":{"enable-patching":false,"patches":{"acme/foo":{"x":"patches/foo.patch"}}}}"#,
        &[("patches/foo.patch", &diff_for("acme/foo"))],
    );
    let mut applied = BTreeMap::new();
    applied.insert("acme/foo".to_string(), "abc".to_string());
    lock::write(tmp.path(), &applied).unwrap();

    let plan = plan(&tmp).unwrap().expect("a cleanup plan");
    assert!(plan.patches.is_empty());
    assert_eq!(
        plan.applied.get("acme/foo").map(String::as_str),
        Some("abc")
    );
}

#[test]
fn declared_sha256_mismatch_is_an_error() {
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":[{"description":"x","url":"patches/foo.patch","sha256":"00"}]}}}"#,
        &[("patches/foo.patch", &diff_for("acme/foo"))],
    );
    let err = plan(&tmp).unwrap_err();
    assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
}

#[test]
fn failure_mode_and_skip_report_precedence() {
    let tmp = project(
        r#"{"extra":{"composer-exit-on-patch-failure":true,"composer-patches-skip-reporting":true,
            "patches":{"acme/foo":{"x":"patches/foo.patch"}}}}"#,
        &[("patches/foo.patch", &diff_for("acme/foo"))],
    );
    let p = plan(&tmp).unwrap().expect("a plan");
    assert_eq!(p.failure_mode, FailureMode::Abort);
    assert!(p.skip_report);

    let opts = PlanOptions {
        exit_on_failure: Some(false),
        skip_report: Some(false),
        write_lock: true,
        ..PlanOptions::default()
    };
    let p = build_plan(tmp.path(), &opts, &NoFetch)
        .unwrap()
        .expect("a plan");
    assert_eq!(p.failure_mode, FailureMode::SkipAndWarn);
    assert!(!p.skip_report);
    assert!(p.write_lock);
}

/// Serves one fixed body and counts downloads.
struct FakeFetch {
    body: String,
    calls: Cell<u32>,
}

impl PatchFetcher for FakeFetch {
    fn fetch(&self, _url: &str, dest: &Path) -> Result<()> {
        self.calls.set(self.calls.get() + 1);
        std::fs::write(dest, &self.body)?;
        Ok(())
    }
}

#[test]
fn remote_patch_is_downloaded_once_and_verified() {
    let body = diff_for("acme/foo");
    let sha = content_sha256(body.as_bytes());
    let tmp = project(
        &json!({"extra":{"patches":{"acme/foo":[
            {"description":"Remote","url":"https://example.test/foo.patch","sha256": sha}
        ]}}})
        .to_string(),
        &[],
    );
    let fetch = FakeFetch {
        body: body.clone(),
        calls: Cell::new(0),
    };
    let opts = PlanOptions {
        cache_dir: Some(tmp.path().join("cache")),
        ..PlanOptions::default()
    };
    let plan = build_plan(tmp.path(), &opts, &fetch)
        .unwrap()
        .expect("a plan");
    let foo = &plan.patches["acme/foo"];
    assert_eq!(foo[0].origin, "https://example.test/foo.patch");
    assert_eq!(foo[0].content_sha256, sha);
    assert_eq!(
        foo[0].local_path,
        tmp.path().join(format!("cache/{sha}.patch"))
    );

    // Cached: a second plan does not download again.
    build_plan(tmp.path(), &opts, &fetch).unwrap();
    assert_eq!(fetch.calls.get(), 1);
}

#[test]
fn bad_remote_download_is_rejected_and_not_cached() {
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":[
            {"description":"Remote","url":"https://example.test/foo.patch","sha256":"00"}
        ]}}}"#,
        &[],
    );
    let fetch = FakeFetch {
        body: "tampered".into(),
        calls: Cell::new(0),
    };
    let opts = PlanOptions {
        cache_dir: Some(tmp.path().join("cache")),
        ..PlanOptions::default()
    };
    let err = build_plan(tmp.path(), &opts, &fetch).unwrap_err();
    assert!(format!("{err:#}").contains("sha256 mismatch"), "{err:#}");
    assert!(!tmp.path().join("cache/00.patch").exists());
}

#[test]
fn remote_patch_needs_a_cache_dir() {
    let tmp = project(
        r#"{"extra":{"patches":{"acme/foo":{"Remote":"https://example.test/foo.patch"}}}}"#,
        &[],
    );
    let err = plan(&tmp).unwrap_err();
    assert!(format!("{err:#}").contains("no patch cache"), "{err:#}");
}
