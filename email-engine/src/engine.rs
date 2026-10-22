use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;

use crate::config::{Account, AuthMethod, Config, GoogleOauthClient, SecretStore};
use crate::error::SetupNeeded;
use crate::imap::{self, Credential, Pool, Session};
use crate::oauth::{self, AccessToken};
use crate::secrets::{self, SecretKind};

fn not_logged_in(account: &Account) -> anyhow::Error {
    SetupNeeded::NotLoggedIn {
        account: account
            .id
            .clone(),
        auth: account.auth,
    }
    .into()
}

/// State of a Google sign-in started by [`Engine::login`] and still finishing in the background.
#[derive(Debug, Clone)]
pub enum LoginState {
    Waiting { auth_url: String },
    Failed(String),
}

/// The mail engine: configured accounts, their credentials, and one cached IMAP session per
/// account. Every operation takes an account key (id or email; `None` = the default account).
#[derive(Default)]
pub struct Engine {
    pub(crate) config: RwLock<Config>,
    pool: Pool,
    tokens: Mutex<HashMap<String, AccessToken>>,
    pub(crate) logins: Mutex<HashMap<String, LoginState>>,
}

impl Engine {
    /// Load the account configuration from disk (see [`crate::config::config_dir`]).
    pub fn load() -> Result<Arc<Self>> {
        Ok(Arc::new(Self::with_config(Config::load()?)))
    }

    /// An engine over an in-memory configuration, for tests and embedding.
    pub fn with_config(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            ..Default::default()
        }
    }

    /// The account `key` names (id or email), or the default account for `None`.
    pub async fn account(&self, key: Option<&str>) -> Result<Account> {
        Ok(self
            .config
            .read()
            .await
            .find(key)?
            .clone())
    }

    pub(crate) async fn secret_store(&self) -> SecretStore {
        self.config
            .read()
            .await
            .secret_store
    }

    /// Credential for IMAP/SMTP, refreshing the OAuth access token if needed.
    pub(crate) async fn credential(&self, account: &Account) -> Result<Credential> {
        let store = self
            .secret_store()
            .await;
        match account.auth {
            AuthMethod::Password => secrets::get(store, &account.id, SecretKind::Password)
                .await?
                .map(Credential::Password)
                .ok_or_else(|| not_logged_in(account)),
            AuthMethod::Oauth2 => {
                if let Some(t) = self
                    .tokens
                    .lock()
                    .await
                    .get(&account.id)
                    && t.is_fresh()
                {
                    return Ok(Credential::OauthToken(
                        t.token
                            .clone(),
                    ));
                }
                let refresh = secrets::get(store, &account.id, SecretKind::OauthRefreshToken)
                    .await?
                    .ok_or_else(|| not_logged_in(account))?;
                let client = self
                    .oauth_client()
                    .await?;
                let token = oauth::refresh(&client, &refresh).await?;
                let value = token
                    .token
                    .clone();
                self.cache_token(&account.id, token)
                    .await;
                Ok(Credential::OauthToken(value))
            }
        }
    }

    pub(crate) async fn oauth_client(&self) -> Result<GoogleOauthClient> {
        self.config
            .read()
            .await
            .google_oauth
            .clone()
            .ok_or_else(|| SetupNeeded::NoGoogleOauthClient.into())
    }

    pub(crate) async fn cache_token(&self, account_id: &str, token: AccessToken) {
        self.tokens
            .lock()
            .await
            .insert(account_id.to_owned(), token);
    }

    /// Drop the account's session, cached token and any pending sign-in.
    pub(crate) async fn forget(&self, account_id: &str) {
        self.pool
            .drop_session(account_id)
            .await;
        self.tokens
            .lock()
            .await
            .remove(account_id);
        self.logins
            .lock()
            .await
            .remove(account_id);
    }

    /// Run `f` with a live IMAP session for the account, optionally with `mailbox` selected.
    /// Reconnects once if the cached session turns out to be dead.
    pub async fn with_session<T, F>(
        &self,
        account: &Account,
        mailbox: Option<&str>,
        f: F,
    ) -> Result<T>
    where
        F: for<'s> AsyncFnOnce(&'s mut Session) -> Result<T>,
    {
        let slot = self
            .pool
            .slot(&account.id)
            .await;
        let mut guard = slot
            .lock()
            .await;

        let alive = match guard.as_mut() {
            Some(s) => matches!(timeout(imap::COMMAND_TIMEOUT, s.noop()).await, Ok(Ok(_))),
            None => false,
        };
        if !alive {
            // A stalled session is simply dropped; logging out of it could stall too.
            guard.take();
            let cred = self
                .credential(account)
                .await?;
            *guard = Some(imap::connect(account, &cred).await?);
        }
        let session = guard
            .as_mut()
            .expect("session was just checked or opened");
        if let Some(mb) = mailbox {
            match timeout(imap::COMMAND_TIMEOUT, session.select(mb)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => bail!("could not open mailbox '{mb}': {e}"),
                Err(_) => {
                    guard.take();
                    bail!(
                        "the server did not respond within {}s while opening mailbox '{mb}'; the connection was reset",
                        imap::COMMAND_TIMEOUT.as_secs()
                    );
                }
            }
        }
        let result = match timeout(imap::CALL_TIMEOUT, f(session)).await {
            Ok(r) => r,
            Err(_) => {
                guard.take();
                bail!(
                    "the server did not respond within {}s and the connection was reset; \
                     the operation may or may not have taken effect, so check before retrying",
                    imap::CALL_TIMEOUT.as_secs()
                );
            }
        };
        if result.is_err() {
            // The connection may be in an unknown state after an error; start fresh next time.
            if let Some(s) = guard.take() {
                imap::logout(s).await;
            }
        }
        result
    }
}
