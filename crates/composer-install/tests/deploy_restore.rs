//! Root files a Magento component maps (`index.php`, `bin/magento`, …) are
//! untracked, so `git clean` deletes them while `vendor/` survives. A later
//! install with nothing to download must put the missing ones back, and must
//! leave the ones that are there (possibly edited) alone.

use std::io::Write;
use std::path::Path;

use composer_install::{
    InstallEnv, InstallOptions, InstallSummary, NoProgress, ReqwestFetcher,
    install_from_lock_with_patches,
};
use tempfile::TempDir;

fn install(cache_root: &Path, project_root: &Path) -> eyre::Result<InstallSummary> {
    let fetcher = ReqwestFetcher::new().unwrap();
    let progress = NoProgress;
    let env = InstallEnv::new(&fetcher, &progress, cache_root);
    install_from_lock_with_patches(&env, project_root, InstallOptions::default(), None, None)
}

fn make_zip(dest: &Path, wrapper: &str, files: &[(&str, &str)]) {
    let f = std::fs::File::create(dest).unwrap();
    let mut zip = zip::ZipWriter::new(f);
    let opts: zip::write::FileOptions<'_, '_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (rel, content) in files {
        zip.start_file(format!("{wrapper}/{rel}"), opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

fn json_path(path: &Path) -> String {
    let quoted = serde_json::to_string(&path.to_string_lossy()).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

#[test]
fn reinstall_restores_deleted_mapped_files_and_keeps_edits() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    let artifact = tmp.path().join("base.zip");
    make_zip(
        &artifact,
        "base-1.0.0",
        &[
            ("index.php", "<?php // index"),
            ("pub/index.php", "<?php // pub"),
            ("pub/.htaccess", "pristine"),
            ("bin/magento", "#!/usr/bin/env php"),
        ],
    );

    let cj = r#"{ "name": "acme/store", "require": { "acme/base": "^1.0" } }"#;
    std::fs::write(proj.join("composer.json"), cj).unwrap();
    let hash = composer_manifest::lockfile::content_hash(cj.as_bytes()).unwrap();
    let lock = format!(
        r#"{{ "content-hash": "{hash}",
            "packages": [
                {{ "name": "acme/base", "version": "1.0.0", "type": "magento2-component",
                   "extra": {{
                       "map": [["index.php", "index.php"], ["pub", "pub"], ["bin/magento", "bin/magento"]],
                       "chmod": [{{ "mask": "0755", "path": "bin/magento" }}]
                   }},
                   "dist": {{ "type": "zip", "url": "{}", "shasum": "" }} }}
            ], "packages-dev": [] }}"#,
        json_path(&artifact)
    );
    std::fs::write(proj.join("composer.lock"), lock).unwrap();

    let first = install(&cache, &proj).unwrap();
    assert_eq!(first.packages_installed, 1);
    assert!(proj.join("bin/magento").is_file());
    assert!(proj.join("app/etc/vendor_path.php").is_file());

    // What `git clean -fd` does to an untracked root, plus a hand edit.
    for f in [
        "index.php",
        "pub/index.php",
        "bin/magento",
        "app/etc/vendor_path.php",
    ] {
        std::fs::remove_file(proj.join(f)).unwrap();
    }
    std::fs::write(proj.join("pub/.htaccess"), "edited").unwrap();

    let second = install(&cache, &proj).unwrap();
    assert_eq!(second.packages_installed, 0, "nothing to download");
    assert_eq!(second.files_deployed, 3);
    let read = |p: &str| std::fs::read_to_string(proj.join(p)).unwrap();
    assert_eq!(read("index.php"), "<?php // index");
    assert_eq!(read("pub/index.php"), "<?php // pub");
    assert_eq!(read("bin/magento"), "#!/usr/bin/env php");
    assert_eq!(read("pub/.htaccess"), "edited", "existing files are kept");
    assert!(proj.join("app/etc/vendor_path.php").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(proj.join("bin/magento"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    let third = install(&cache, &proj).unwrap();
    assert_eq!(third.files_deployed, 0, "nothing missing, nothing copied");
}
