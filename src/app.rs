use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use tokio::sync::{Mutex, RwLock};

use crate::config::{Account, AuthMethod, Config};
use crate::imap::{self, Credential, Pool, Session};
use crate::oauth::{self, AccessToken};
use crate::secrets::{self, SecretKind};

#[derive(Debug, Clone)]
pub enum LoginState {
    Waiting { auth_url: String },
    Failed(String),
}

#[derive(Default)]
pub struct App {
    pub config: RwLock<Config>,
    pub pool: Pool,
    tokens: Mutex<HashMap<String, AccessToken>>,
    pub logins: Mutex<HashMap<String, LoginState>>,
}

impl App {
    pub fn load() -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            config: RwLock::new(Config::load()?),
            ..Default::default()
        }))
    }

    pub async fn account(&self, key: Option<&str>) -> Result<Account> {
        Ok(self.config.read().await.find(key)?.clone())
    }

    /// Credential for IMAP/SMTP, refreshing the OAuth access token if needed.
    pub async fn credential(&self, account: &Account) -> Result<Credential> {
        let store = self.config.read().await.secret_store;
        match account.auth {
            AuthMethod::Password => secrets::get(store, &account.id, SecretKind::Password)
                .await?
                .map(Credential::Password)
                .ok_or_else(|| anyhow!("account '{}' is not logged in; call login with its app password", account.id)),
            AuthMethod::Oauth2 => {
                if let Some(t) = self.tokens.lock().await.get(&account.id)
                    && t.is_fresh()
                {
                    return Ok(Credential::OauthToken(t.token.clone()));
                }
                let refresh = secrets::get(store, &account.id, SecretKind::OauthRefreshToken)
                    .await?
                    .ok_or_else(|| anyhow!("account '{}' is not logged in; call login to start the Google sign-in", account.id))?;
                let client = self.oauth_client().await?;
                let token = oauth::refresh(&client, &refresh).await?;
                let value = token.token.clone();
                self.tokens.lock().await.insert(account.id.clone(), token);
                Ok(Credential::OauthToken(value))
            }
        }
    }

    pub async fn oauth_client(&self) -> Result<crate::config::GoogleOauthClient> {
        self.config
            .read()
            .await
            .google_oauth
            .clone()
            .ok_or_else(|| anyhow!("no Google OAuth client configured; call set_google_oauth_client first"))
    }

    pub async fn cache_token(&self, account_id: &str, token: AccessToken) {
        self.tokens.lock().await.insert(account_id.to_owned(), token);
    }

    pub async fn forget(&self, account_id: &str) {
        self.pool.drop_session(account_id).await;
        self.tokens.lock().await.remove(account_id);
        self.logins.lock().await.remove(account_id);
    }

    /// Run `f` with a live IMAP session for the account, optionally with `mailbox` selected.
    /// Reconnects once if the cached session turns out to be dead.
    pub async fn with_session<T, F>(&self, account: &Account, mailbox: Option<&str>, f: F) -> Result<T>
    where
        F: for<'s> AsyncFnOnce(&'s mut Session) -> Result<T>,
    {
        let slot = self.pool.slot(&account.id).await;
        let mut guard = slot.lock().await;

        let alive = match guard.as_mut() {
            Some(s) => s.noop().await.is_ok(),
            None => false,
        };
        if !alive {
            let cred = self.credential(account).await?;
            *guard = Some(imap::connect(account, &cred).await?);
        }
        let session = guard.as_mut().unwrap();
        if let Some(mb) = mailbox
            && let Err(e) = session.select(mb).await {
                bail!("could not open mailbox '{mb}': {e}");
            }
        let result = f(session).await;
        if result.is_err() {
            // The connection may be in an unknown state after an error; start fresh next time.
            if let Some(mut s) = guard.take() {
                let _ = s.logout().await;
            }
        }
        result
    }
}
