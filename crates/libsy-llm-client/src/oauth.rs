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

use serde_json::{Value, json};

use crate::error::{LlmClientError, Result};

const CLAUDE_CODE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLAUDE_CODE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Beta flag Anthropic requires on requests authorized with a subscription token.
pub(crate) const CLAUDE_CODE_OAUTH_BETA: &str = "oauth-2025-04-20";
/// Anthropic only serves subscription tokens to requests whose system prompt starts with this.
pub(crate) const CLAUDE_CODE_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

// Refresh this long before expiry so a token does not run out mid-request.
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);

/// A Claude Code subscription login stored in `~/.claude/.credentials.json`.
pub struct ClaudeCodeLogin {
    path: PathBuf,
    token_url: String,
    state: tokio::sync::Mutex<LoginState>,
}

#[derive(Default)]
struct LoginState {
    access_token: Option<String>,
    expires_at_ms: u64,
    // The token the provider rejected; a file still holding it must be refreshed.
    rejected: Option<String>,
}

impl ClaudeCodeLogin {
    /// A login backed by the credential file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_token_url(path, CLAUDE_CODE_TOKEN_URL)
    }

    pub(crate) fn with_token_url(path: impl Into<PathBuf>, token_url: &str) -> Self {
        Self {
            path: path.into(),
            token_url: token_url.to_string(),
            state: tokio::sync::Mutex::default(),
        }
    }

    /// The credential file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A current access token, refreshing and saving it when it is about to expire.
    pub(crate) async fn access_token(&self, http: &reqwest::Client) -> Result<String> {
        let mut state = self.state.lock().await;
        let now = now_ms();
        if let Some(token) = &state.access_token
            && state.expires_at_ms > now + millis(EXPIRY_MARGIN)
        {
            return Ok(token.clone());
        }

        let mut file = self.read_file()?;
        let oauth = claude_oauth(&file, &self.path)?;
        let access_token = string_field(oauth, "accessToken", &self.path)?;
        let expires_at_ms = oauth.get("expiresAt").and_then(Value::as_u64).unwrap_or(0);
        let rejected = state.rejected.as_deref() == Some(access_token.as_str());
        if !rejected && expires_at_ms > now + millis(EXPIRY_MARGIN) {
            state.access_token = Some(access_token.clone());
            state.expires_at_ms = expires_at_ms;
            return Ok(access_token);
        }

        let refresh_token = string_field(oauth, "refreshToken", &self.path)?;
        let scope = oauth
            .get("scopes")
            .and_then(Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let mut body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_CODE_CLIENT_ID,
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
        let refreshed: Value = response
            .json()
            .await
            .map_err(|error| self.error(format!("token refresh returned invalid JSON: {error}")))?;
        let new_access = string_field(&refreshed, "access_token", &self.path)?;
        let expires_in = refreshed
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let new_expires_at_ms = now_ms() + expires_in * 1000;

        let oauth = file
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| self.error("missing claudeAiOauth".to_string()))?;
        oauth.insert("accessToken".into(), Value::String(new_access.clone()));
        oauth.insert("expiresAt".into(), json!(new_expires_at_ms));
        if let Some(new_refresh) = refreshed.get("refresh_token").and_then(Value::as_str) {
            oauth.insert(
                "refreshToken".into(),
                Value::String(new_refresh.to_string()),
            );
        }
        self.write_file(&file)?;

        state.access_token = Some(new_access.clone());
        state.expires_at_ms = new_expires_at_ms;
        state.rejected = None;
        Ok(new_access)
    }

    /// Marks `token` as rejected so the next call refreshes instead of reusing it.
    pub(crate) async fn reject(&self, token: &str) {
        let mut state = self.state.lock().await;
        if state.access_token.as_deref() == Some(token) {
            state.access_token = None;
        }
        state.rejected = Some(token.to_string());
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

impl fmt::Debug for ClaudeCodeLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeCodeLogin")
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

fn claude_oauth<'a>(file: &'a Value, path: &Path) -> Result<&'a Value> {
    file.get("claudeAiOauth")
        .ok_or_else(|| LlmClientError::Configuration {
            message: format!(
                "login {}: missing claudeAiOauth; log in with Claude Code first",
                path.display()
            ),
        })
}

fn string_field(value: &Value, name: &str, path: &Path) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| LlmClientError::Configuration {
            message: format!("login {}: missing {name}", path.display()),
        })
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

    fn credential_file(expires_at_ms: u64) -> Value {
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

    #[tokio::test]
    async fn valid_token_is_used_without_refresh() -> Result<()> {
        let path = temp_file("valid", &credential_file(now_ms() + 3_600_000));
        let login = ClaudeCodeLogin::with_token_url(&path, "http://127.0.0.1:9/unused");
        let token = login.access_token(&reqwest::Client::new()).await;
        let _ = std::fs::remove_file(&path);
        assert_eq!(token?, "old-access");
        Ok(())
    }

    #[tokio::test]
    async fn expired_token_is_refreshed_and_saved() -> Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({
                "grant_type": "refresh_token",
                "refresh_token": "old-refresh",
                "client_id": CLAUDE_CODE_CLIENT_ID,
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

        let path = temp_file("expired", &credential_file(0));
        let login = ClaudeCodeLogin::with_token_url(&path, &server.uri());
        let http = reqwest::Client::new();
        let first = login.access_token(&http).await;
        let second = login.access_token(&http).await;
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
                .unwrap_or_default();
        let _ = std::fs::remove_file(&path);

        assert_eq!(first?, "new-access");
        assert_eq!(second?, "new-access");
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

        let path = temp_file("rejected", &credential_file(now_ms() + 3_600_000));
        let login = ClaudeCodeLogin::with_token_url(&path, &server.uri());
        let http = reqwest::Client::new();
        let first = login.access_token(&http).await?;
        login.reject(&first).await;
        let second = login.access_token(&http).await;
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
                .unwrap_or_default();
        let _ = std::fs::remove_file(&path);

        assert_eq!(second?, "new-access");
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

        let path = temp_file("failed", &credential_file(0));
        let login = ClaudeCodeLogin::with_token_url(&path, &server.uri());
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
}
