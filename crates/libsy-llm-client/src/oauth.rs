// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Subscription logins read from a local CLI's credential file.
//!
//! The CLI that wrote the file may also refresh it. Refresh tokens are single use,
//! so a refresh always rereads the file first and writes the new tokens back. The
//! other fields in the file are kept as they are.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};

use crate::error::{LlmClientError, Result};

/// Beta flag Anthropic requires on requests authorized with a subscription token.
pub(crate) const CLAUDE_CODE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// Anthropic only serves subscription tokens to requests whose system prompt starts with this.
pub(crate) const CLAUDE_CODE_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
/// Client name the ChatGPT Codex backend expects in the `originator` header.
pub(crate) const CODEX_ORIGINATOR: &str = "codex_cli_rs";

// Refresh this long before expiry so a token does not run out mid-request.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Which CLI wrote the credential file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginKind {
    /// Claude Code, usually `~/.claude/.credentials.json`.
    ClaudeCode,
    /// Codex with a ChatGPT login, usually `~/.codex/auth.json`.
    Codex,
}

impl LoginKind {
    fn token_url(self) -> &'static str {
        match self {
            Self::ClaudeCode => "https://platform.claude.com/v1/oauth/token",
            Self::Codex => "https://auth.openai.com/oauth/token",
        }
    }

    fn client_id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            Self::Codex => "app_EMoamEEZ73f0CkXaXp7hrann",
        }
    }

    // The object in the file that holds the tokens.
    fn section(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claudeAiOauth",
            Self::Codex => "tokens",
        }
    }

    fn access_field(self) -> &'static str {
        match self {
            Self::ClaudeCode => "accessToken",
            Self::Codex => "access_token",
        }
    }

    fn refresh_field(self) -> &'static str {
        match self {
            Self::ClaudeCode => "refreshToken",
            Self::Codex => "refresh_token",
        }
    }
}

/// A token ready to send, with the account it belongs to when the provider needs it.
pub(crate) struct LoginToken {
    pub(crate) access_token: String,
    pub(crate) account_id: Option<String>,
}

/// A subscription login stored in a CLI's credential file.
pub struct SubscriptionLogin {
    kind: LoginKind,
    path: PathBuf,
    token_url: String,
    state: tokio::sync::Mutex<LoginState>,
}

#[derive(Default)]
struct LoginState {
    access_token: Option<String>,
    account_id: Option<String>,
    expires_at_ms: u64,
    // The token the provider rejected; a file still holding it must be refreshed.
    rejected: Option<String>,
}

impl SubscriptionLogin {
    /// A login backed by the credential file at `path`.
    pub fn new(kind: LoginKind, path: impl Into<PathBuf>) -> Self {
        Self::with_token_url(kind, path, kind.token_url())
    }

    pub(crate) fn with_token_url(
        kind: LoginKind,
        path: impl Into<PathBuf>,
        token_url: &str,
    ) -> Self {
        Self {
            kind,
            path: path.into(),
            token_url: token_url.to_string(),
            state: tokio::sync::Mutex::default(),
        }
    }

    /// Which CLI wrote the credential file.
    pub fn kind(&self) -> LoginKind {
        self.kind
    }

    /// The credential file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A current access token, refreshing and saving it when it is about to expire.
    pub(crate) async fn access_token(&self, http: &reqwest::Client) -> Result<LoginToken> {
        let mut state = self.state.lock().await;
        let now = now_ms();
        if let Some(token) = &state.access_token
            && state.expires_at_ms > now + millis(EXPIRY_MARGIN)
        {
            return Ok(LoginToken {
                access_token: token.clone(),
                account_id: state.account_id.clone(),
            });
        }

        let mut file = self.read_file()?;
        let section = self.section(&file)?;
        let access_token = self.string_field(section, self.kind.access_field())?;
        let account_id = section
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let expires_at_ms = match self.kind {
            LoginKind::ClaudeCode => section.get("expiresAt").and_then(Value::as_u64),
            LoginKind::Codex => jwt_expiry_ms(&access_token),
        }
        .unwrap_or(0);
        let rejected = state.rejected.as_deref() == Some(access_token.as_str());
        if !rejected && expires_at_ms > now + millis(EXPIRY_MARGIN) {
            state.access_token = Some(access_token.clone());
            state.account_id = account_id.clone();
            state.expires_at_ms = expires_at_ms;
            return Ok(LoginToken {
                access_token,
                account_id,
            });
        }

        let refreshed = self.refresh(http, section).await?;
        let new_access = self.string_field(&refreshed, "access_token")?;
        let new_expires_at_ms = match refreshed.get("expires_in").and_then(Value::as_u64) {
            Some(expires_in) => now_ms() + expires_in * 1000,
            None => jwt_expiry_ms(&new_access).unwrap_or(0),
        };
        self.save(&mut file, &refreshed, &new_access, new_expires_at_ms)?;

        state.access_token = Some(new_access.clone());
        state.account_id = account_id.clone();
        state.expires_at_ms = new_expires_at_ms;
        state.rejected = None;
        Ok(LoginToken {
            access_token: new_access,
            account_id,
        })
    }

    /// Marks `token` as rejected so the next call refreshes instead of reusing it.
    pub(crate) async fn reject(&self, token: &str) {
        let mut state = self.state.lock().await;
        if state.access_token.as_deref() == Some(token) {
            state.access_token = None;
        }
        state.rejected = Some(token.to_string());
    }

    async fn refresh(&self, http: &reqwest::Client, section: &Value) -> Result<Value> {
        let refresh_token = self.string_field(section, self.kind.refresh_field())?;
        let scope = match self.kind {
            LoginKind::ClaudeCode => section
                .get("scopes")
                .and_then(Value::as_array)
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default(),
            LoginKind::Codex => "openid profile email".to_string(),
        };
        let mut body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": self.kind.client_id(),
        });
        if !scope.is_empty() {
            body["scope"] = Value::String(scope);
        }
        let response = http
            .post(&self.token_url)
            .json(&body)
            .send()
            .await
            .map_err(|error| self.error(format!("token refresh failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.error(format!(
                "token refresh failed with HTTP {status}; log in again with the CLI"
            )));
        }
        response
            .json()
            .await
            .map_err(|error| self.error(format!("token refresh returned invalid JSON: {error}")))
    }

    fn save(
        &self,
        file: &mut Value,
        refreshed: &Value,
        new_access: &str,
        new_expires_at_ms: u64,
    ) -> Result<()> {
        let kind = self.kind;
        let section = file
            .get_mut(kind.section())
            .and_then(Value::as_object_mut)
            .ok_or_else(|| self.error(format!("missing {}", kind.section())))?;
        section.insert(kind.access_field().into(), new_access.into());
        if let Some(new_refresh) = refreshed.get("refresh_token").and_then(Value::as_str) {
            section.insert(kind.refresh_field().into(), new_refresh.into());
        }
        match kind {
            LoginKind::ClaudeCode => {
                section.insert("expiresAt".into(), json!(new_expires_at_ms));
            }
            LoginKind::Codex => {
                if let Some(id_token) = refreshed.get("id_token").and_then(Value::as_str) {
                    section.insert("id_token".into(), id_token.into());
                }
                if let Some(file) = file.as_object_mut() {
                    file.insert("last_refresh".into(), rfc3339_now().into());
                }
            }
        }
        self.write_file(file)
    }

    fn section<'a>(&self, file: &'a Value) -> Result<&'a Value> {
        file.get(self.kind.section())
            .filter(|section| section.is_object())
            .ok_or_else(|| {
                self.error(format!(
                    "missing {}; log in with the CLI first",
                    self.kind.section()
                ))
            })
    }

    fn string_field(&self, value: &Value, name: &str) -> Result<String> {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| self.error(format!("missing {name}")))
    }

    fn read_file(&self) -> Result<Value> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| self.error(format!("cannot read credential file: {error}")))?;
        serde_json::from_str(&text)
            .map_err(|error| self.error(format!("credential file is not valid JSON: {error}")))
    }

    // Writes beside the target and renames, so a reader never sees half a file.
    fn write_file(&self, file: &Value) -> Result<()> {
        let text = serde_json::to_string_pretty(file)
            .map_err(|error| self.error(format!("cannot encode credential file: {error}")))?;
        let mut temp = self.path.clone().into_os_string();
        temp.push(".switchyard-tmp");
        let temp = PathBuf::from(temp);
        write_private(&temp, text.as_bytes())
            .and_then(|()| std::fs::rename(&temp, &self.path))
            .map_err(|error| self.error(format!("cannot save credential file: {error}")))
    }

    fn error(&self, message: String) -> LlmClientError {
        LlmClientError::Configuration {
            message: format!("login {}: {message}", self.path.display()),
        }
    }
}

impl fmt::Debug for SubscriptionLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionLogin")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .finish()
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

// The `exp` claim of a JWT, in milliseconds. The signature is not checked; the
// value only decides when to refresh.
fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let payload = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Map<String, Value> = serde_json::from_slice(&payload).ok()?;
    claims.get("exp")?.as_u64()?.checked_mul(1000)
}

// The current UTC time as RFC 3339, the format Codex reads in `last_refresh`.
fn rfc3339_now() -> String {
    let secs = now_ms() / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Converts days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis)
        .unwrap_or(0)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn temp_file(name: &str, contents: &Value) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "switchyard-oauth-{name}-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        std::fs::write(&path, contents.to_string()).expect("write temp credential file");
        path
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_default()).unwrap_or_default()
    }

    fn claude_file(expires_at_ms: u64) -> Value {
        json!({
            "mcpOAuth": {"other": {"accessToken": "keep-me"}},
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": "old-refresh",
                "expiresAt": expires_at_ms,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max"
            }
        })
    }

    fn jwt(exp_secs: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(json!({"exp": exp_secs}).to_string());
        format!("header.{payload}.signature")
    }

    fn codex_file(access_token: &str) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "old-id",
                "access_token": access_token,
                "refresh_token": "old-refresh",
                "account_id": "account-1"
            },
            "last_refresh": "2026-01-01T00:00:00Z"
        })
    }

    #[tokio::test]
    async fn valid_token_is_used_without_refresh() -> Result<()> {
        let path = temp_file("valid", &claude_file(now_ms() + 3_600_000));
        let login =
            SubscriptionLogin::with_token_url(LoginKind::ClaudeCode, &path, "http://127.0.0.1:9");
        let token = login.access_token(&reqwest::Client::new()).await;
        let _ = std::fs::remove_file(&path);
        assert_eq!(token?.access_token, "old-access");
        Ok(())
    }

    #[tokio::test]
    async fn expired_token_is_refreshed_and_saved() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "grant_type": "refresh_token",
                "refresh_token": "old-refresh",
                "client_id": LoginKind::ClaudeCode.client_id(),
                "scope": "user:inference user:profile"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        let path = temp_file("expired", &claude_file(0));
        let login = SubscriptionLogin::with_token_url(LoginKind::ClaudeCode, &path, &server.uri());
        let http = reqwest::Client::new();
        let first = login.access_token(&http).await;
        let second = login.access_token(&http).await;
        let saved = read_json(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(first?.access_token, "new-access");
        assert_eq!(second?.access_token, "new-access");
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "new-access");
        assert_eq!(saved["claudeAiOauth"]["refreshToken"], "new-refresh");
        assert_eq!(saved["claudeAiOauth"]["subscriptionType"], "max");
        assert_eq!(saved["mcpOAuth"]["other"]["accessToken"], "keep-me");
        assert!(saved["claudeAiOauth"]["expiresAt"].as_u64() > Some(now_ms()));
        Ok(())
    }

    #[tokio::test]
    async fn rejected_token_is_refreshed_even_before_expiry() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "new-access",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        let path = temp_file("rejected", &claude_file(now_ms() + 3_600_000));
        let login = SubscriptionLogin::with_token_url(LoginKind::ClaudeCode, &path, &server.uri());
        let http = reqwest::Client::new();
        let first = login.access_token(&http).await?;
        login.reject(&first.access_token).await;
        let second = login.access_token(&http).await;
        let saved = read_json(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(second?.access_token, "new-access");
        // The provider sent no new refresh token, so the old one stays.
        assert_eq!(saved["claudeAiOauth"]["refreshToken"], "old-refresh");
        Ok(())
    }

    #[tokio::test]
    async fn failed_refresh_names_the_file_but_not_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid_grant"))
            .mount(&server)
            .await;

        let path = temp_file("failed", &claude_file(0));
        let login = SubscriptionLogin::with_token_url(LoginKind::ClaudeCode, &path, &server.uri());
        let error = login
            .access_token(&reqwest::Client::new())
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
        let _ = std::fs::remove_file(&path);

        let Err(error) = error else {
            panic!("expected refresh failure");
        };
        assert!(error.contains("log in again"), "{error}");
        assert!(!error.contains("old-refresh"), "{error}");
    }

    #[tokio::test]
    async fn codex_token_expiry_comes_from_the_jwt() -> Result<()> {
        let valid = jwt(now_ms() / 1000 + 3_600);
        let path = temp_file("codex-valid", &codex_file(&valid));
        let login =
            SubscriptionLogin::with_token_url(LoginKind::Codex, &path, "http://127.0.0.1:9");
        let token = login.access_token(&reqwest::Client::new()).await;
        let _ = std::fs::remove_file(&path);

        let token = token?;
        assert_eq!(token.access_token, valid);
        assert_eq!(token.account_id.as_deref(), Some("account-1"));
        Ok(())
    }

    #[tokio::test]
    async fn expired_codex_token_is_refreshed_and_saved() -> Result<()> {
        let fresh = jwt(now_ms() / 1000 + 3_600);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "grant_type": "refresh_token",
                "refresh_token": "old-refresh",
                "client_id": LoginKind::Codex.client_id()
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id_token": "new-id",
                "access_token": fresh,
                "refresh_token": "new-refresh"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let path = temp_file("codex-expired", &codex_file(&jwt(1)));
        let login = SubscriptionLogin::with_token_url(LoginKind::Codex, &path, &server.uri());
        let token = login.access_token(&reqwest::Client::new()).await;
        let saved = read_json(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(token?.access_token, fresh);
        assert_eq!(saved["tokens"]["access_token"], fresh);
        assert_eq!(saved["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(saved["tokens"]["id_token"], "new-id");
        assert_eq!(saved["tokens"]["account_id"], "account-1");
        assert_eq!(saved["auth_mode"], "chatgpt");
        assert_ne!(saved["last_refresh"], "2026-01-01T00:00:00Z");
        Ok(())
    }

    #[test]
    fn rfc3339_formats_a_utc_timestamp() {
        let now = rfc3339_now();
        assert_eq!(now.len(), "2026-10-03T12:00:00Z".len());
        assert!(now.ends_with('Z') && now.as_bytes()[10] == b'T', "{now}");
    }
}
