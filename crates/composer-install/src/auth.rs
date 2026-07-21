//! Per-host repository credentials, read from the same sources Composer uses.
//!
//! Relocated from the resolver (it carried no solver logic — pure fs + serde).
//! Dist URLs sitting behind the same auth as the metadata (Magento's
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

use base64::Engine;
use eyre::{Result, eyre};
use serde_json::{Map, Value};

/// Auth credentials for a single repository host. Skipped from `Debug` output
/// so secrets never leak into logs / error messages.
#[derive(Clone)]
pub enum AuthCredentials {
    /// HTTP Basic — Composer's `http-basic` shape.
    Basic { username: String, password: String },
    /// Bearer token — Composer's `bearer` shape.
    Bearer { token: String },
    /// GitHub OAuth — sends `Authorization: token <tok>`.
    GitHubToken { token: String },
    /// GitLab private/CI token — sends `PRIVATE-TOKEN: <tok>`.
    GitLabToken { token: String },
}

impl std::fmt::Debug for AuthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &"<redacted>")
                .finish(),
            Self::GitHubToken { .. } => f
                .debug_struct("GitHubToken")
                .field("token", &"<redacted>")
                .finish(),
            Self::GitLabToken { .. } => f
                .debug_struct("GitLabToken")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

impl AuthCredentials {
    /// Render the credentials as the HTTP header *value*.
    pub fn header_value(&self) -> String {
        match self {
            Self::Basic { username, password } => {
                let raw = format!("{username}:{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
                format!("Basic {encoded}")
            }
            Self::Bearer { token } => format!("Bearer {token}"),
            Self::GitHubToken { token } => format!("token {token}"),
            Self::GitLabToken { token } => token.clone(),
        }
    }

    /// The HTTP header *name* for this credential type.
    pub fn header_name(&self) -> &'static str {
        match self {
            Self::Basic { .. } | Self::Bearer { .. } | Self::GitHubToken { .. } => "authorization",
            Self::GitLabToken { .. } => "private-token",
        }
    }
}

/// Parse an `{http-basic, bearer, github-oauth, gitlab-token, gitlab-oauth}`
/// auth object — the shape that lives at the top level of `auth.json` and
/// (nested under `config`) inside `composer.json`. Error messages prefix each
/// path with `source` so diagnostics name the file/env-var the bad value came
/// from.
fn parse_auth_object(
    obj: &Map<String, Value>,
    source: &str,
) -> Result<HashMap<String, AuthCredentials>> {
    let mut out: HashMap<String, AuthCredentials> = HashMap::new();
    if let Some(bearer) = obj.get("bearer").and_then(Value::as_object) {
        for (host, val) in bearer {
            let token = val
                .as_str()
                .ok_or_else(|| eyre!("{source}: bearer.{host} must be a string token"))?;
            out.insert(
                host.clone(),
                AuthCredentials::Bearer {
                    token: token.to_owned(),
                },
            );
        }
    }
    if let Some(github) = obj.get("github-oauth").and_then(Value::as_object) {
        for (host, val) in github {
            let token = val
                .as_str()
                .ok_or_else(|| eyre!("{source}: github-oauth.{host} must be a string token"))?;
            out.insert(
                host.clone(),
                AuthCredentials::GitHubToken {
                    token: token.to_owned(),
                },
            );
        }
    }
    if let Some(gitlab) = obj.get("gitlab-oauth").and_then(Value::as_object) {
        for (host, val) in gitlab {
            let token = val
                .as_str()
                .ok_or_else(|| eyre!("{source}: gitlab-oauth.{host} must be a string token"))?;
            out.insert(
                host.clone(),
                AuthCredentials::Bearer {
                    token: token.to_owned(),
                },
            );
        }
    }
    if let Some(gitlab) = obj.get("gitlab-token").and_then(Value::as_object) {
        for (host, val) in gitlab {
            let token = if let Some(s) = val.as_str() {
                s.to_owned()
            } else if let Some(obj) = val.as_object() {
                obj.get("token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        eyre!("{source}: gitlab-token.{host}.token is missing or not a string")
                    })?
                    .to_owned()
            } else {
                return Err(eyre!(
                    "{source}: gitlab-token.{host} must be a string or object with `token`"
                ));
            };
            out.insert(host.clone(), AuthCredentials::GitLabToken { token });
        }
    }
    if let Some(http_basic) = obj.get("http-basic").and_then(Value::as_object) {
        for (host, val) in http_basic {
            let entry = val.as_object().ok_or_else(|| {
                eyre!(
                    "{source}: http-basic.{host} must be an object with `username` and `password`"
                )
            })?;
            let username = entry
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    eyre!("{source}: http-basic.{host}.username is missing or not a string")
                })?;
            let password = entry
                .get("password")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    eyre!("{source}: http-basic.{host}.password is missing or not a string")
                })?;
            out.insert(
                host.clone(),
                AuthCredentials::Basic {
                    username: username.to_owned(),
                    password: password.to_owned(),
                },
            );
        }
    }
    Ok(out)
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
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashMap::new());
    }
    let value: Value =
        serde_json::from_str(trimmed).map_err(|e| eyre!("COMPOSER_AUTH is not valid JSON: {e}"))?;
    let Some(obj) = value.as_object() else {
        return Err(eyre!("COMPOSER_AUTH must decode to a JSON object"));
    };
    parse_auth_object(obj, "COMPOSER_AUTH")
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
    Ok(merge_auth_sources(
        read_global_auth_json()?,
        read_auth_from_composer_json(composer_json)?,
        read_auth_json(project_root)?,
        read_composer_auth_env()?,
    ))
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
    fn basic_header_is_base64_userpass() {
        let creds = AuthCredentials::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        assert_eq!(creds.header_value(), "Basic dXNlcjpwYXNz");
        assert_eq!(creds.header_name(), "authorization");
    }

    #[test]
    fn gitlab_token_uses_private_token_header() {
        let creds = AuthCredentials::GitLabToken {
            token: "abc".into(),
        };
        assert_eq!(creds.header_value(), "abc");
        assert_eq!(creds.header_name(), "private-token");
    }

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

    #[test]
    fn debug_redacts_secrets() {
        let creds = AuthCredentials::Basic {
            username: "user".into(),
            password: "s3cret".into(),
        };
        let rendered = format!("{creds:?}");
        assert!(rendered.contains("user"));
        assert!(!rendered.contains("s3cret"), "{rendered}");
    }
}
