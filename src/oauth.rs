//! Google OAuth2 "installed app" flow with a loopback redirect and PKCE.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::GoogleOauthClient;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://mail.google.com/";
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: Instant,
}

impl AccessToken {
    pub fn is_fresh(&self) -> bool {
        self.expires_at > Instant::now() + Duration::from_secs(60)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

/// A login waiting for the browser to hit the loopback redirect.
pub struct PendingLogin {
    pub auth_url: String,
    listener: TcpListener,
    redirect_uri: String,
    verifier: String,
    state: String,
}

fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

pub async fn begin_login(client: &GoogleOauthClient, login_hint: &str) -> Result<PendingLogin> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding loopback listener for OAuth redirect")?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}",
        listener
            .local_addr()?
            .port()
    );
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token();

    let auth_url = url::Url::parse_with_params(
        AUTH_URL,
        &[
            (
                "client_id",
                client
                    .client_id
                    .as_str(),
            ),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("login_hint", login_hint),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
        ],
    )?
    .to_string();

    Ok(PendingLogin {
        auth_url,
        listener,
        redirect_uri,
        verifier,
        state,
    })
}

impl PendingLogin {
    /// Wait for the redirect, exchange the code, and return (refresh_token, access_token).
    pub async fn complete(self, client: &GoogleOauthClient) -> Result<(String, AccessToken)> {
        let code = tokio::time::timeout(LOGIN_TIMEOUT, self.wait_for_code())
            .await
            .map_err(|_| anyhow!("timed out waiting for the browser sign-in"))??;
        let resp = token_request(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", &self.verifier),
        ])
        .await?;
        let refresh = resp
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("Google did not return a refresh token"))?;
        Ok((refresh, to_access_token(resp)))
    }

    async fn wait_for_code(&self) -> Result<String> {
        loop {
            let (mut sock, _) = self
                .listener
                .accept()
                .await?;
            let mut buf = vec![0u8; 8192];
            let n = sock
                .read(&mut buf)
                .await?;
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req
                .lines()
                .next()
                .and_then(|l| {
                    l.split_whitespace()
                        .nth(1)
                })
                .unwrap_or("/");
            let url = url::Url::parse(&format!("http://localhost{path}"))?;
            let param = |k: &str| {
                url.query_pairs()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.into_owned())
            };

            // Browsers also request /favicon.ico and the like; ignore anything without our state.
            if param("state").as_deref()
                != Some(
                    self.state
                        .as_str(),
                )
            {
                respond(&mut sock, 404, "Not found").await;
                continue;
            }
            if let Some(err) = param("error") {
                respond(
                    &mut sock,
                    400,
                    &format!("Sign-in failed: {err}. You can close this tab."),
                )
                .await;
                bail!("authorization denied: {err}");
            }
            let Some(code) = param("code") else {
                respond(&mut sock, 400, "Missing authorization code.").await;
                bail!("redirect did not include an authorization code");
            };
            respond(
                &mut sock,
                200,
                "Signed in to email-mcp. You can close this tab.",
            )
            .await;
            return Ok(code);
        }
    }
}

async fn respond(sock: &mut tokio::net::TcpStream, status: u16, msg: &str) {
    let body = format!("<!doctype html><meta charset=utf-8><title>email-mcp</title><p>{msg}</p>");
    let resp = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock
        .write_all(resp.as_bytes())
        .await;
    let _ = sock
        .shutdown()
        .await;
}

pub async fn refresh(client: &GoogleOauthClient, refresh_token: &str) -> Result<AccessToken> {
    let resp = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
    ])
    .await?;
    Ok(to_access_token(resp))
}

fn to_access_token(resp: TokenResponse) -> AccessToken {
    AccessToken {
        token: resp.access_token,
        expires_at: Instant::now() + Duration::from_secs(resp.expires_in),
    }
}

async fn token_request(form: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = crate::tls::http_client()?
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await
        .context("contacting Google token endpoint")?;
    let status = resp.status();
    let body = resp
        .text()
        .await?;
    if !status.is_success() {
        if let Ok(e) = serde_json::from_str::<TokenError>(&body) {
            bail!(
                "Google token endpoint: {}{}",
                e.error,
                e.error_description
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            );
        }
        bail!("Google token endpoint returned {status}: {body}");
    }
    Ok(serde_json::from_str(&body)?)
}

/// SASL XOAUTH2 initial response (before base64 encoding).
pub fn xoauth2_string(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}
