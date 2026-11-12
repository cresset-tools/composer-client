//! `ReqwestFetcher` as the remote-patch downloader for
//! `composer_patches::build_plan`, against a local `wiremock` server.

use composer_install::ReqwestFetcher;
use composer_patches::{PlanOptions, build_plan, content_sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn remote_patch_downloads_through_reqwest_fetcher() {
    let body = "--- a/src/A.php\n+++ b/src/A.php\n@@ -1 +1 @@\n-old\n+new\n";
    let sha = content_sha256(body.as_bytes());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/fix.patch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let tmp = tempfile::tempdir().unwrap();
    let url = format!("{uri}/fix.patch");
    let composer_json = serde_json::json!({"extra": {"patches": {"acme/foo": [
        {"description": "Remote fix", "url": url, "sha256": sha}
    ]}}});
    std::fs::write(tmp.path().join("composer.json"), composer_json.to_string()).unwrap();

    let opts = PlanOptions {
        cache_dir: Some(tmp.path().join("patch-cache")),
        ..PlanOptions::default()
    };
    let fetcher = ReqwestFetcher::new().unwrap();
    let plan = build_plan(tmp.path(), &opts, &fetcher)
        .unwrap()
        .expect("a plan");
    let foo = &plan.patches["acme/foo"];
    assert_eq!(foo[0].content_sha256, sha);
    assert_eq!(std::fs::read_to_string(&foo[0].local_path).unwrap(), body);

    // Cached: building again does not hit the server (`expect(1)` above).
    build_plan(tmp.path(), &opts, &fetcher).unwrap();
}
