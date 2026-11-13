//! Per-host repository credentials, read from the same sources Composer uses.
//!
//! The credential type and its parsing live in [`composer_auth`]; this module
//! finds the sources and merges them. Dist URLs sitting behind the same auth as the metadata (Magento's
//! `/archives/…`, private satis, GitLab CI Composer ZIPs) need an
//! `Authorization` header; the orchestrator resolves it per dist host from the
//! merged map [`read_all_auth`] returns.
//!
//! Sources, lowest to highest precedence (later wins): the global Composer
//! `auth.json` (`$COMPOSER_HOME`/XDG/legacy locations), composer.json's
//! `config` block, the project-level `auth.json`, and the `COMPOSER_AUTH`
//! environment variable — mirroring `Composer\Factory`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use composer_auth::AuthCredentials;
use eyre::{Result, eyre};
use serde_json::{Map, Value};

/// [`composer_auth::parse_auth_object`], with its error as an eyre report.
fn parse_auth_object(
    obj: &Map<String, Value>,
    source: &str,
) -> Result<HashMap<String, AuthCredentials>> {
    composer_auth::parse_auth_object(obj, source).map_err(|e| eyre!("{e}"))
}

/// Read auth from composer.json's `config.http-basic` / `config.bearer` (and
/// the other supported keys). The lowest-precedence non-machine source.
pub fn read_auth_from_composer_json(
    composer_json: &Value,
) -> Result<HashMap<String, AuthCredentials>> {
    let Some(obj) = composer_json.as_object() else {
        return Ok(HashMap::new());
    };
    let Some(config) = obj.get("config").and_then(Value::as_object) else {
        return Ok(HashMap::new());
    };
    parse_auth_object(config, "composer.json config")
}

/// Read auth from a project-level `auth.json` (next to composer.json). Same
/// shape as composer.json's `config` section but at the top level. Empty map
/// when the file doesn't exist.
pub fn read_auth_json(project_root: &Path) -> Result<HashMap<String, AuthCredentials>> {
    read_auth_json_at(&project_root.join("auth.json"))
}

/// Read auth from a specific `auth.json` path. Empty map when it doesn't exist.
fn read_auth_json_at(path: &Path) -> Result<HashMap<String, AuthCredentials>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let bytes = std::fs::read(path).map_err(|e| eyre!("reading {}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| eyre!("parsing {}: {e}", path.display()))?;
    let Some(obj) = value.as_object() else {
        return Ok(HashMap::new());
    };
    parse_auth_object(obj, &path.display().to_string())
}

/// Candidate locations for the **global** Composer `auth.json`, in lookup
/// order. Composer reads `$COMPOSER_HOME/auth.json`; the XDG-strict and legacy
/// locations are added because that's where the file lives across distros.
/// First existing file wins.
///
/// `env` is a closure so this stays pure for tests (no `set_var` races); the
/// public reader wires it to `std::env::var`.
fn global_auth_json_candidates(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = env("COMPOSER_HOME") {
        out.push(PathBuf::from(h).join("auth.json"));
    }
    if let Some(h) = env("XDG_CONFIG_HOME") {
        out.push(PathBuf::from(h).join("composer").join("auth.json"));
    }
    if let Some(h) = env("HOME") {
        out.push(
            PathBuf::from(&h)
                .join(".config")
                .join("composer")
                .join("auth.json"),
        );
        out.push(PathBuf::from(&h).join(".composer").join("auth.json"));
    }
    out
}

/// Read auth from the global Composer `auth.json`, if present. Empty map when
/// no candidate exists. The high-value source: users already keep their
/// GitHub / private-Packagist credentials here for Composer itself, so this
/// inherits a working credential store with no reconfiguration.
pub fn read_global_auth_json() -> Result<HashMap<String, AuthCredentials>> {
    for candidate in global_auth_json_candidates(|k| std::env::var(k).ok()) {
        if candidate.is_file() {
            return read_auth_json_at(&candidate);
        }
    }
    Ok(HashMap::new())
}

/// Parse the JSON body of the `COMPOSER_AUTH` environment variable — same
/// shape as `auth.json`. Empty input → empty map.
pub fn parse_composer_auth_env(raw: &str) -> Result<HashMap<String, AuthCredentials>> {
    composer_auth::parse_auth_json(raw, "COMPOSER_AUTH").map_err(|e| eyre!("{e}"))
}

/// Read auth from the `COMPOSER_AUTH` environment variable — the canonical way
/// to inject credentials in CI without committing an `auth.json`. Empty map
/// when unset or empty.
pub fn read_composer_auth_env() -> Result<HashMap<String, AuthCredentials>> {
    match std::env::var("COMPOSER_AUTH") {
        Ok(s) => parse_composer_auth_env(&s),
        Err(_) => Ok(HashMap::new()),
    }
}

/// Collect credentials from every source and merge in Composer's documented
/// order (later wins): global `auth.json` → composer.json `config` → project
/// `auth.json` → `COMPOSER_AUTH`.
///
/// Intuition: global is the machine default; composer.json `config` is the
/// project's committed intent; project `auth.json` is the developer's
/// per-checkout override; `COMPOSER_AUTH` is the CI/runtime override.
pub fn read_all_auth(
    composer_json: &Value,
    project_root: &Path,
) -> Result<HashMap<String, AuthCredentials>> {
    read_all_auth_with::<std::hash::RandomState>(composer_json, project_root, None)
}

/// [`read_all_auth`], plus `machine` credentials from a caller's own store.
/// They sit in the machine-wide slot: above the global `auth.json` (they win
/// for the same host) and below every project source.
pub fn read_all_auth_with<S: std::hash::BuildHasher>(
    composer_json: &Value,
    project_root: &Path,
    machine: Option<&HashMap<String, AuthCredentials, S>>,
) -> Result<HashMap<String, AuthCredentials>> {
    Ok(merge_auth_sources(
        machine_slot(read_global_auth_json()?, machine),
        read_auth_from_composer_json(composer_json)?,
        read_auth_json(project_root)?,
        read_composer_auth_env()?,
    ))
}

/// The machine-wide slot: the global `auth.json`, with the caller's own
/// store winning for the same host.
fn machine_slot<S: std::hash::BuildHasher>(
    mut global: HashMap<String, AuthCredentials>,
    machine: Option<&HashMap<String, AuthCredentials, S>>,
) -> HashMap<String, AuthCredentials> {
    if let Some(machine) = machine {
        global.extend(machine.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    global
}

/// Pure merger so the precedence order can be unit-tested without env-var or
/// filesystem races. Arguments are lowest- to highest-precedence.
fn merge_auth_sources(
    global: HashMap<String, AuthCredentials>,
    composer_json_config: HashMap<String, AuthCredentials>,
    project_auth_json: HashMap<String, AuthCredentials>,
    composer_auth_env: HashMap<String, AuthCredentials>,
) -> HashMap<String, AuthCredentials> {
    let mut out = global;
    out.extend(composer_json_config);
    out.extend(project_auth_json);
    out.extend(composer_auth_env);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_http_basic_and_bearer() {
        let obj = json!({
            "http-basic": { "repo.example.com": { "username": "u", "password": "p" } },
            "bearer": { "api.example.com": "tok" }
        });
        let map = parse_auth_object(obj.as_object().unwrap(), "test").unwrap();
        assert!(matches!(
            map.get("repo.example.com"),
            Some(AuthCredentials::Basic { .. })
        ));
        assert!(matches!(
            map.get("api.example.com"),
            Some(AuthCredentials::Bearer { .. })
        ));
    }

    #[test]
    fn later_source_wins_per_host() {
        let mut global = HashMap::new();
        global.insert(
            "h".to_string(),
            AuthCredentials::Bearer {
                token: "old".into(),
            },
        );
        let mut env = HashMap::new();
        env.insert(
            "h".to_string(),
            AuthCredentials::Bearer {
                token: "new".into(),
            },
        );
        let merged = merge_auth_sources(global, HashMap::new(), HashMap::new(), env);
        match merged.get("h") {
            Some(AuthCredentials::Bearer { token }) => assert_eq!(token, "new"),
            other => panic!("expected the env token to win, got {other:?}"),
        }
    }

    fn bearer(token: &str) -> HashMap<String, AuthCredentials> {
        HashMap::from([(
            "h".to_string(),
            AuthCredentials::Bearer {
                token: token.into(),
            },
        )])
    }

    fn token_for_h(map: &HashMap<String, AuthCredentials>) -> &str {
        match map.get("h") {
            Some(AuthCredentials::Bearer { token }) => token,
            other => panic!("expected a bearer token, got {other:?}"),
        }
    }

    #[test]
    fn machine_store_beats_global_auth_json_but_not_the_project() {
        let store = bearer("store");
        let slot = machine_slot(bearer("global"), Some(&store));
        assert_eq!(token_for_h(&slot), "store");

        let merged = merge_auth_sources(slot, bearer("config"), HashMap::new(), HashMap::new());
        assert_eq!(token_for_h(&merged), "config");
    }
}
