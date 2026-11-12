//! Unit tests for the parallel dist downloader.
//!
//! Each network test spins up a `wiremock` server on a per-test tokio runtime.
//! The production code is blocking, so the driver constructs the runtime, sets
//! up the mock, then calls the blocking `fetch_and_extract_dists` from the main
//! thread through the default [`ReqwestFetcher`].

use super::*;
use crate::fetch::ReqwestFetcher;
use crate::progress::NoProgress;
use std::io::Write as _;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn fetcher() -> ReqwestFetcher {
    ReqwestFetcher::new().unwrap()
}

fn sha1_hex(bytes: &[u8]) -> String {
    let digest = sha1::Sha1::digest(bytes);
    let mut s = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a small zip whose entries live under `<top>/` so `strip_prefix = top`
/// lands them at the dest root — mirrors Packagist's standard dist layout.
fn build_fixture_zip(top: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/foo"}"#).unwrap();
        zw.start_file(format!("{top}/src/Foo.php"), opts).unwrap();
        zw.write_all(b"<?php class Foo {}\n").unwrap();
        zw.finish().unwrap();
    }
    buf
}

/// A dist cache dir owned by the test.
fn cache_in(tmp: &Path) -> PathBuf {
    let cache = tmp.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    cache
}

#[test]
fn downloads_single_zip_and_extracts_with_strip_prefix() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc1234");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .unwrap();

    assert!(vendor_dest.join("composer.json").is_file());
    assert!(vendor_dest.join("src/Foo.php").is_file());
    assert!(!vendor_dest.join("acme-foo-abc1234").exists());
    let cached = cache_root.join(format!("{hash}.zip"));
    assert!(cached.is_file(), "cache file should be retained for reuse");
}

#[test]
fn falls_back_to_next_candidate_when_primary_url_fails() {
    // Composer's dist-mirror semantics: a 404 from the first candidate must not
    // fail the install — the downloader moves on, and each candidate carries
    // its *own* pre-rendered auth.
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc1234");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/mirror/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // "token:hunter2" — the fallback requires its own credentials.
        Mock::given(method("GET"))
            .and(wm_path("/origin/acme-foo.zip"))
            .and(header("authorization", "Basic dG9rZW46aHVudGVyMg=="))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let primary = format!("{uri}/mirror/acme-foo.zip");
    let fallbacks = [DistCandidate {
        url: format!("{uri}/origin/acme-foo.zip"),
        auth_header: Some("Basic dG9rZW46aHVudGVyMg==".to_owned()),
        auth_header_name: Some("authorization"),
    }];
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &primary,
        sha1: &hash,
        reference: "",
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &fallbacks,
    }];

    let outcomes = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0], DistOutcome::Downloaded { bytes } if bytes > 0),
        "{outcomes:?}"
    );
    assert!(vendor_dest.join("composer.json").is_file());
}

#[test]
fn all_candidates_failing_surfaces_candidate_count() {
    // When every candidate URL fails, the error must say so — the per-URL
    // context alone would name only the *last* URL tried.
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let primary = format!("{uri}/mirror/acme-foo.zip");
    let fallbacks = [DistCandidate {
        url: format!("{uri}/origin/acme-foo.zip"),
        auth_header: None,
        auth_header_name: None,
    }];
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &primary,
        sha1: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        reference: "",
        strip_prefix: None,
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &fallbacks,
    }];

    let err = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .expect_err("every candidate 404s");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("all 2 candidate URLs failed"),
        "expected candidate-count context in error, got: {msg}"
    );
    assert!(
        msg.contains("/origin/acme-foo.zip"),
        "expected last-tried URL in error chain, got: {msg}"
    );
}

#[test]
fn cache_hit_short_circuits_network() {
    // Pre-populate the cache; the mock server returns 500 if hit, so any HTTP
    // attempt would fail the test loudly.
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-deadbeef");
    let hash = sha1_hex(&body);
    std::fs::write(cache_root.join(format!("{hash}.zip")), &body).unwrap();

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        strip_prefix: Some("acme-foo-deadbeef"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let outcomes = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .unwrap();
    assert_eq!(outcomes, vec![DistOutcome::CacheHit]);
    assert!(vendor_dest.join("composer.json").is_file());
}

#[test]
fn hash_mismatch_aborts_install_cleanly() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-aaaa");
    // Claim the wrong hash — the fetcher must reject the bytes and leave
    // neither a `.partial` nor a cached zip behind.
    let wrong_hash = "0000000000000000000000000000000000000000";

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: wrong_hash,
        reference: "",
        strip_prefix: Some("acme-foo-aaaa"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .expect_err("hash mismatch must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("sha1") || msg.contains("hash"),
        "expected hash-mismatch context in error, got: {msg}"
    );

    // No cached zip, no leftover `.partial`.
    let cached = cache_root.join(format!("{wrong_hash}.zip"));
    let partial = cache_root.join(format!("{wrong_hash}.partial"));
    assert!(!cached.exists(), "no cached zip for failed hash");
    assert!(!partial.exists(), "no leftover .partial");
    assert!(!vendor_dest.exists());
}

#[test]
fn parallel_four_dists_share_one_run() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());

    // Four distinct packages, each with its own fixture zip and dest.
    let pkgs: Vec<(String, String, Vec<u8>)> = (0..4)
        .map(|i| {
            let top = format!("acme-pkg{i}-aaaa");
            let body = build_fixture_zip(&top);
            let hash = sha1_hex(&body);
            (top, hash, body)
        })
        .collect();

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        for (i, (_, _, body)) in pkgs.iter().enumerate() {
            Mock::given(method("GET"))
                .and(wm_path(format!("/p{i}.zip")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                .mount(&server)
                .await;
        }
        let uri = server.uri();
        (uri, server)
    });

    let urls: Vec<String> = (0..4).map(|i| format!("{uri}/p{i}.zip")).collect();
    let names: Vec<String> = (0..4).map(|i| format!("acme/pkg{i}")).collect();
    let dests: Vec<PathBuf> = (0..4)
        .map(|i| {
            tmp.path()
                .join("vendor")
                .join("acme")
                .join(format!("pkg{i}"))
        })
        .collect();

    let dists: Vec<DistRequest<'_>> = (0..4)
        .map(|i| DistRequest {
            package_name: &names[i],
            url: &urls[i],
            sha1: &pkgs[i].1,
            reference: "",
            strip_prefix: Some(&pkgs[i].0),
            vendor_dest: &dests[i],
            auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
        })
        .collect();

    fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .unwrap();

    for dest in &dests {
        assert!(dest.join("composer.json").is_file());
        assert!(dest.join("src/Foo.php").is_file());
    }
}

#[test]
fn extract_strips_top_level_directory_via_auto_detect() {
    // Verify that *no* file with the top-level directory component survives in
    // `vendor_dest`. Also exercises the `strip_prefix = None` branch.
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("strip");

    let top = "acme-strip-9876543";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/d.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let url = format!("{uri}/d.zip");
    let dists = [DistRequest {
        package_name: "acme/strip",
        url: &url,
        sha1: &hash,
        reference: "",
        strip_prefix: None,
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];
    let outcomes = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0], DistOutcome::Downloaded { bytes } if bytes > 0),
        "{outcomes:?}"
    );

    // Walk vendor_dest; assert no path component equals the stripped top-level
    // name.
    let mut count = 0usize;
    for entry in walkdir(&vendor_dest) {
        count += 1;
        for part in entry.components() {
            if let std::path::Component::Normal(p) = part {
                assert_ne!(p.to_str(), Some(top), "stripped prefix leaked: {entry:?}");
            }
        }
    }
    assert!(count > 0, "no files extracted");
}

#[test]
fn dist_request_auth_header_is_sent_on_get() {
    // Wiremock only responds with the ZIP when the request carries the exact
    // `Authorization` header the caller pre-rendered.
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let auth = "Basic dXNlcjpwYXNz";
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        strip_prefix: Some("acme-foo-abc"),
        vendor_dest: &vendor_dest,
        auth_header: Some(auth),
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .expect("download must succeed when the Authorization header matches");
    assert!(vendor_dest.join("composer.json").is_file());
}

#[test]
fn dist_request_without_auth_fails_when_server_requires_it() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        strip_prefix: Some("acme-foo-abc"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .expect_err("unauthenticated request must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "{msg}");
}

#[test]
fn rewrite_github_api_zipball_to_codeload() {
    let url = "https://api.github.com/repos/Seldaek/monolog/zipball/c915e2634718dbc8a4a15c61b0e62e7a44e14448";
    let rewritten = rewrite_github_dist_url(url);
    assert_eq!(
        rewritten,
        "https://codeload.github.com/Seldaek/monolog/legacy.zip/c915e2634718dbc8a4a15c61b0e62e7a44e14448",
    );
}

#[test]
fn rewrite_leaves_non_github_urls_unchanged() {
    let urls = [
        "https://repo.packagist.org/archives/vendor/pkg.zip",
        "https://gitlab.example.com/api/v4/projects/1/packages/composer/archives/foo.zip",
        "https://example.test/acme-foo.zip",
    ];
    for url in urls {
        assert_eq!(rewrite_github_dist_url(url).as_ref(), url);
    }
}

#[test]
fn rewrite_leaves_github_non_zipball_urls_unchanged() {
    let url = "https://api.github.com/repos/owner/repo/tarball/abc123";
    assert_eq!(rewrite_github_dist_url(url).as_ref(), url);
}

#[test]
fn rewrite_handles_org_scoped_repos() {
    let url = "https://api.github.com/repos/symfony/console/zipball/3156577f46a38aa1b9323aad223de7a9cd426782";
    assert_eq!(
        rewrite_github_dist_url(url),
        "https://codeload.github.com/symfony/console/legacy.zip/3156577f46a38aa1b9323aad223de7a9cd426782",
    );
}

#[test]
fn local_artifact_dist_is_copied_and_extracted() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp
        .path()
        .join("vendor")
        .join("vsourz")
        .join("imagegallery");

    let body = build_fixture_zip("acme-foo-abc1234");
    let hash = sha1_hex(&body);

    // Project layout: `<root>/artifacts/vsourz-imagegallery-1.0.1-p1.zip`, with
    // the URL stored relative — how Composer's `type: artifact` repository
    // serializes into composer.lock.
    let artifacts_dir = tmp.path().join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    let zip_path = artifacts_dir.join("vsourz-imagegallery-1.0.1-p1.zip");
    std::fs::write(&zip_path, &body).unwrap();

    let url = "artifacts/vsourz-imagegallery-1.0.1-p1.zip";
    let dists = [DistRequest {
        package_name: "vsourz/imagegallery",
        url,
        sha1: &hash,
        reference: "",
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .unwrap();

    assert!(vendor_dest.join("composer.json").is_file());
    assert!(vendor_dest.join("src/Foo.php").is_file());
    let cached = cache_root.join(format!("{hash}.zip"));
    assert!(
        cached.is_file(),
        "expected cached copy at {}",
        cached.display()
    );
}

#[test]
fn local_artifact_dist_missing_file_errors_clearly() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("missing");

    let dists = [DistRequest {
        package_name: "acme/missing",
        url: "artifacts/does-not-exist.zip",
        sha1: "0000000000000000000000000000000000000000",
        reference: "",
        strip_prefix: Some("x"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .expect_err("missing artifact must surface as an error");
    let msg = format!("{err:#}");
    assert!(msg.contains("type: artifact"), "{msg}");
    assert!(msg.contains("does-not-exist.zip"), "{msg}");
}

#[test]
fn local_artifact_dist_sha1_mismatch_errors() {
    let tmp = TempDir::new().unwrap();
    let cache_root = cache_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc1234");
    let artifacts_dir = tmp.path().join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    let zip_path = artifacts_dir.join("acme-foo.zip");
    std::fs::write(&zip_path, &body).unwrap();

    let dists = [DistRequest {
        package_name: "acme/foo",
        url: "artifacts/acme-foo.zip",
        sha1: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        reference: "",
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(
        &fetcher(),
        &cache_root,
        &dists,
        &NoProgress,
        LinkMode::Extract,
    )
    .expect_err("sha1 mismatch must surface");
    let msg = format!("{err:#}");
    assert!(msg.contains("sha1 mismatch"), "{msg}");
}

/// Minimal recursive walk: every path under `root` relative to `root`.
fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(rel);
            }
        }
    }
    out
}

#[test]
fn cache_key_distinguishes_dists_without_shasum_or_reference() {
    // A Composer `type: package` repo entry can carry only `dist.url` +
    // `dist.type` — no shasum, no reference. The key must fold in the package
    // name + URL so distinct packages differ.
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path();
    let vendor = tmp.path().join("vendor");
    let proj = tmp.path();

    let js = DistRequest {
        package_name: "acme/js-widget",
        url: "https://example.test/js-widget.zip",
        sha1: "",
        reference: "",
        strip_prefix: None,
        vendor_dest: &vendor,
        auth_header: None,
        auth_header_name: None,
        project_root: proj,
        fallbacks: &[],
    };
    let css = DistRequest {
        package_name: "acme/css-kit",
        url: "https://example.test/css-kit.zip",
        ..js
    };

    let js_path = cache_path_for(cache_root, &js);
    let css_path = cache_path_for(cache_root, &css);
    assert_ne!(
        js_path, css_path,
        "distinct no-shasum/no-reference dists must not share a cache file"
    );

    // A shasum still content-addresses (and wins over the url fallback).
    let hashed = DistRequest {
        sha1: "abc123def456",
        ..js
    };
    assert_eq!(
        cache_path_for(cache_root, &hashed),
        cache_root.join("abc123def456.zip")
    );
}

// ---- LinkMode::Hardlink (extracted store) ----

/// Seed `<cache>/<key>.zip` with a fixture whose entries live under `top/`, and
/// return a store-mode DistRequest keyed on `key` (sha1 verbatim).
fn seed_zip(cache: &Path, key: &str, top: &str) {
    std::fs::write(cache.join(format!("{key}.zip")), build_fixture_zip(top)).unwrap();
}

#[test]
#[cfg(unix)]
fn hardlink_store_shares_inodes_and_writes_marker() {
    use std::os::unix::fs::MetadataExt as _;
    let tmp = TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    let vendor = tmp.path().join("vendor").join("acme").join("foo");
    let top = "acme-foo-abc1234";
    let key = "deadbeefcafe";
    seed_zip(&cache, key, top);
    let dist = DistRequest {
        package_name: "acme/foo",
        url: "",
        sha1: key,
        reference: "",
        strip_prefix: Some(top),
        vendor_dest: &vendor,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    };

    install_from_store(&cache, &dist).unwrap();

    let store = cache.join("extracted").join(key);
    assert!(store.join("composer.json").is_file());
    assert!(store.join("src/Foo.php").is_file());
    assert!(
        cache
            .join("extracted")
            .join(format!("{key}.complete"))
            .is_file(),
        "a complete extraction must write its marker"
    );
    assert!(vendor.join("composer.json").is_file());
    assert!(vendor.join("src/Foo.php").is_file());

    let store_ino = std::fs::metadata(store.join("src/Foo.php")).unwrap().ino();
    let vendor_ino = std::fs::metadata(vendor.join("src/Foo.php")).unwrap().ino();
    assert_eq!(
        store_ino, vendor_ino,
        "vendor file should hard-link the store file"
    );

    // A second install (marker present) still materializes vendor without
    // re-extracting — and still shares inodes.
    let _ = std::fs::remove_dir_all(&vendor);
    install_from_store(&cache, &dist).unwrap();
    assert_eq!(
        std::fs::metadata(store.join("src/Foo.php")).unwrap().ino(),
        std::fs::metadata(vendor.join("src/Foo.php")).unwrap().ino(),
    );
}

#[test]
#[cfg(unix)]
fn atomic_rewrite_breaks_only_the_patched_file() {
    use std::os::unix::fs::MetadataExt as _;
    let tmp = TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    let vendor = tmp.path().join("vendor").join("acme").join("foo");
    let top = "acme-foo-abc1234";
    let key = "feedface0001";
    seed_zip(&cache, key, top);
    let dist = DistRequest {
        package_name: "acme/foo",
        url: "",
        sha1: key,
        reference: "",
        strip_prefix: Some(top),
        vendor_dest: &vendor,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    };
    install_from_store(&cache, &dist).unwrap();
    let store = cache.join("extracted").join(key);

    // Mimic the patcher: temp file in the same dir + rename over the target.
    let target = vendor.join("src/Foo.php");
    let tmpf = vendor.join("src/.Foo.php.patch");
    std::fs::write(&tmpf, b"<?php class Foo { public $patched = true; }\n").unwrap();
    std::fs::rename(&tmpf, &target).unwrap();

    // The store's copy is untouched...
    assert_eq!(
        std::fs::read_to_string(store.join("src/Foo.php")).unwrap(),
        "<?php class Foo {}\n",
        "the shared store must not see the patch",
    );
    // ...the patched file now has its own inode...
    assert_ne!(
        std::fs::metadata(&target).unwrap().ino(),
        std::fs::metadata(store.join("src/Foo.php")).unwrap().ino(),
    );
    // ...and every un-patched file still shares with the store.
    assert_eq!(
        std::fs::metadata(vendor.join("composer.json"))
            .unwrap()
            .ino(),
        std::fs::metadata(store.join("composer.json"))
            .unwrap()
            .ino(),
    );
}

#[test]
fn store_without_marker_is_reextracted() {
    let tmp = TempDir::new().unwrap();
    let cache = cache_in(tmp.path());
    let vendor = tmp.path().join("vendor").join("acme").join("foo");
    let top = "acme-foo-abc1234";
    let key = "0badc0de0002";
    seed_zip(&cache, key, top);
    let dist = DistRequest {
        package_name: "acme/foo",
        url: "",
        sha1: key,
        reference: "",
        strip_prefix: Some(top),
        vendor_dest: &vendor,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    };

    // A store dir left over from a crashed run — present, but no marker, and
    // holding stale junk. It must be wiped and re-extracted, not trusted.
    let store = cache.join("extracted").join(key);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("STALE"), b"junk").unwrap();

    install_from_store(&cache, &dist).unwrap();

    assert!(store.join("composer.json").is_file());
    assert!(
        !store.join("STALE").exists(),
        "a marker-less store must be re-extracted clean"
    );
    assert!(
        cache
            .join("extracted")
            .join(format!("{key}.complete"))
            .is_file()
    );
    assert!(vendor.join("composer.json").is_file());
}

#[test]
fn copy_fallback_reproduces_the_tree() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(src.join("nested")).unwrap();
    std::fs::write(src.join("a.txt"), b"aaa").unwrap();
    std::fs::write(src.join("nested/b.txt"), b"bbb").unwrap();
    let dst = tmp.path().join("dst");
    std::fs::create_dir_all(&dst).unwrap();

    // can_link = false forces the copy path (the EXDEV / no-hard-link fallback).
    link_tree_inner(&src, &dst, false).unwrap();

    assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "aaa");
    assert_eq!(
        std::fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
        "bbb"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_ne!(
            std::fs::metadata(src.join("a.txt")).unwrap().ino(),
            std::fs::metadata(dst.join("a.txt")).unwrap().ino(),
            "copy mode must produce independent inodes",
        );
    }
}
