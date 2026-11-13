#![cfg(unix)]
//! Hermetic git tests: a throwaway repo in a tempdir, cloned over a
//! `file://` URL — no network.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use super::*;

/// Run `git -C dir <args>` with a fixed identity, asserting success.
fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

pub(crate) fn rev_parse(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .expect("git rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repo with two commits; returns `(url, first_sha, second_sha)`.
pub(crate) fn fixture_repo(root: &Path) -> (String, String, String) {
    let repo = root.join("origin");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q"]);
    std::fs::write(repo.join("composer.json"), r#"{"name":"acme/lib"}"#).unwrap();
    std::fs::write(repo.join("first.php"), "<?php // v1\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-q", "-m", "first"]);
    let first = rev_parse(&repo, "HEAD");

    std::fs::write(repo.join("second.php"), "<?php // v2\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-q", "-m", "second"]);
    let second = rev_parse(&repo, "HEAD");

    (format!("file://{}", repo.display()), first, second)
}

fn install(fetcher: &GitSourceFetcher, url: &str, reference: &str, dest: &Path) -> Result<()> {
    let urls = [url.to_owned()];
    fetcher.install(&SourceSpec {
        package_name: "acme/lib",
        urls: &urls,
        reference,
        dest,
    })
}

fn vcs_failure(report: &eyre::Report) -> (VcsFailure, String) {
    match report.downcast_ref::<InstallError>() {
        Some(InstallError::Vcs {
            failure, operation, ..
        }) => (*failure, operation.clone()),
        other => panic!("expected InstallError::Vcs, got {other:?}"),
    }
}

#[test]
fn checks_out_the_exact_reference() {
    let tmp = TempDir::new().unwrap();
    let (url, first, _second) = fixture_repo(tmp.path());
    let fetcher = GitSourceFetcher::new(tmp.path().join("vcs"));

    let dest = tmp.path().join("vendor/acme/lib");
    install(&fetcher, &url, &first, &dest).unwrap();

    assert!(dest.join("first.php").is_file());
    assert!(!dest.join("second.php").exists(), "at the first commit");
    assert_eq!(rev_parse(&dest, "HEAD"), first);
    let origin = Command::new("git")
        .arg("-C")
        .arg(&dest)
        .args(["remote", "get-url", "origin"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&origin.stdout).trim(), url);
}

#[test]
fn mirror_is_reused_and_dest_is_rewiped() {
    let tmp = TempDir::new().unwrap();
    let (url, first, second) = fixture_repo(tmp.path());
    let fetcher = GitSourceFetcher::new(tmp.path().join("vcs"));

    let dest = tmp.path().join("vendor/acme/lib");
    install(&fetcher, &url, &second, &dest).unwrap();
    std::fs::write(dest.join("stray.txt"), "x").unwrap();

    install(&fetcher, &url, &first, &dest).unwrap();
    assert_eq!(rev_parse(&dest, "HEAD"), first);
    assert!(!dest.join("stray.txt").exists(), "dest is rewiped");
    assert!(!dest.join("second.php").exists());

    let mirrors = std::fs::read_dir(tmp.path().join("vcs")).unwrap().count();
    assert_eq!(mirrors, 1, "exactly one cached mirror");
}

#[test]
fn falls_back_to_the_next_url() {
    let tmp = TempDir::new().unwrap();
    let (url, first, _second) = fixture_repo(tmp.path());
    let fetcher = GitSourceFetcher::new(tmp.path().join("vcs"));
    let dest = tmp.path().join("vendor/acme/lib");
    let missing = format!("file://{}", tmp.path().join("nope").display());
    let urls = [missing, url];
    fetcher
        .install(&SourceSpec {
            package_name: "acme/lib",
            urls: &urls,
            reference: &first,
            dest: &dest,
        })
        .unwrap();
    assert_eq!(rev_parse(&dest, "HEAD"), first);
}

#[test]
fn classifier_maps_auth_and_missing_ref_stderr() {
    let auth = classify_git(
        "clone",
        "https://host/x.git",
        "remote: Authentication failed for 'x'",
    );
    assert_eq!(vcs_failure(&auth).0, VcsFailure::Auth);
    assert!(format!("{auth}").contains("credential helper"));

    let missing = classify_git("checkout", "u", "fatal: reference is not a tree: deadbeef");
    assert_eq!(vcs_failure(&missing).0, VcsFailure::MissingRef);
}

#[test]
fn git_missing_is_typed() {
    let e = git_missing("invocation");
    assert_eq!(
        vcs_failure(&e),
        (VcsFailure::GitMissing, "invocation".to_owned())
    );
    assert!(format!("{e}").contains("install git"));
}

#[test]
fn a_bad_reference_is_a_typed_missing_ref() {
    let tmp = TempDir::new().unwrap();
    let (url, _first, _second) = fixture_repo(tmp.path());
    let fetcher = GitSourceFetcher::new(tmp.path().join("vcs"));
    let dest = tmp.path().join("vendor/acme/lib");
    let err = install(
        &fetcher,
        &url,
        "0000000000000000000000000000000000000000",
        &dest,
    )
    .expect_err("bogus ref must fail");
    assert_eq!(
        vcs_failure(&err),
        (VcsFailure::MissingRef, "checkout".to_owned())
    );
}
