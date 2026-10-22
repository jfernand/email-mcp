//! Account management: adding, changing and removing accounts, signing in and out.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use crate::config::{
    Account, AuthMethod, GoogleOauthClient, Provider, RemoveBy, SecretStore, SmtpSecurity,
    guess_provider, provider_defaults, validate_id,
};
use crate::engine::{Engine, LoginState};
use crate::imap;
use crate::oauth;
use crate::secrets::{self, SecretKind};
use crate::smtp;

/// A new account. `None` fields take the provider's defaults; custom providers need hosts.
#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    pub email: String,
    /// Defaults to the part of the address before '@', lower-cased, '.' → '-'.
    pub id: Option<String>,
    pub display_name: Option<String>,
    /// Guessed from the email domain when omitted.
    pub provider: Option<Provider>,
    pub auth: Option<AuthMethod>,
    pub username: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SmtpSecurity>,
    pub save_sent: Option<bool>,
    pub remove_by: Option<RemoveBy>,
    /// 0-based position in the account list (0 = default). Defaults to the end.
    pub position: Option<usize>,
}

/// Changes to an account; `None` leaves a setting alone. For the optional text settings
/// (`display_name`, `username`, `trash_mailbox`) an empty string clears the value.
#[derive(Debug, Clone, Default)]
pub struct AccountUpdate {
    pub display_name: Option<String>,
    pub username: Option<String>,
    pub auth: Option<AuthMethod>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SmtpSecurity>,
    pub save_sent: Option<bool>,
    pub remove_by: Option<RemoveBy>,
    /// Must name an existing, selectable mailbox.
    pub trash_mailbox: Option<String>,
    pub position: Option<usize>,
}

/// An account and its place in the list (0 = the default account).
#[derive(Debug, Clone)]
pub struct Positioned {
    pub account: Account,
    pub index: usize,
}

#[derive(Debug, Clone)]
pub struct AccountList {
    pub accounts: Vec<Account>,
    pub google_oauth_configured: bool,
    pub secret_store: SecretStore,
}

/// How a login went.
#[derive(Debug, Clone)]
pub enum LoginStart {
    /// The password was verified and stored.
    LoggedIn,
    /// A Google sign-in was started: the user must open `auth_url`. It completes in the
    /// background; [`Engine::status`] reports how it went.
    BrowserSignIn { auth_url: String },
}

#[derive(Debug, Clone)]
pub struct AccountStatus {
    pub account: Account,
    /// Whether a credential is stored, or why the secret store could not be read.
    pub logged_in: std::result::Result<bool, String>,
    pub pending_login: Option<LoginState>,
    /// Present when a connection test was requested and a credential is stored.
    pub connection: Option<ConnectionCheck>,
}

#[derive(Debug, Clone)]
pub struct ConnectionCheck {
    pub imap: std::result::Result<(), String>,
    pub smtp: std::result::Result<(), String>,
}

impl NewAccount {
    /// The account this describes, with defaults filled in and settings validated.
    pub fn resolve(self) -> Result<Account> {
        let email = self
            .email
            .trim()
            .to_owned();
        let Some((local, _)) = email.split_once('@') else {
            bail!("'{email}' is not an email address");
        };
        let provider = self
            .provider
            .unwrap_or_else(|| guess_provider(&email));
        let defaults = provider_defaults(provider);
        let id = self
            .id
            .unwrap_or_else(|| {
                local
                    .to_ascii_lowercase()
                    .replace('.', "-")
            });
        validate_id(&id)?;
        let auth = self
            .auth
            .or(defaults
                .as_ref()
                .map(|d| d.auth))
            .unwrap_or(AuthMethod::Password);
        if auth == AuthMethod::Oauth2 && provider != Provider::Gmail {
            bail!("OAuth2 is only supported for Gmail accounts");
        }
        let need = |v: Option<String>, d: Option<&'static str>, what: &str| {
            v.or(d.map(str::to_owned))
                .ok_or_else(|| anyhow!("{what} is required for custom providers"))
        };
        Ok(Account {
            id,
            email,
            display_name: self.display_name,
            provider,
            auth,
            username: self.username,
            imap_host: need(
                self.imap_host,
                defaults
                    .as_ref()
                    .map(|d| d.imap_host),
                "imap_host",
            )?,
            imap_port: self
                .imap_port
                .or(defaults
                    .as_ref()
                    .map(|d| d.imap_port))
                .unwrap_or(993),
            smtp_host: need(
                self.smtp_host,
                defaults
                    .as_ref()
                    .map(|d| d.smtp_host),
                "smtp_host",
            )?,
            smtp_port: self
                .smtp_port
                .or(defaults
                    .as_ref()
                    .map(|d| d.smtp_port))
                .unwrap_or(587),
            smtp_security: self
                .smtp_security
                .or(defaults
                    .as_ref()
                    .map(|d| d.smtp_security))
                .unwrap_or(SmtpSecurity::Starttls),
            save_sent: self
                .save_sent
                .or(defaults
                    .as_ref()
                    .map(|d| d.save_sent))
                .unwrap_or(true),
            remove_by: self
                .remove_by
                .unwrap_or_default(),
            trash_mailbox: None,
        })
    }
}

impl AccountUpdate {
    /// Apply the changes to `a`. Returns whether the authentication method changed.
    fn apply(self, a: &mut Account) -> Result<bool> {
        let old_auth = a.auth;
        if let Some(v) = self.auth {
            if v == AuthMethod::Oauth2 && a.provider != Provider::Gmail {
                bail!("OAuth2 is only supported for Gmail accounts");
            }
            a.auth = v;
        }
        let non_empty = |v: String| Some(v).filter(|s| !s.is_empty());
        if let Some(v) = self.display_name {
            a.display_name = non_empty(v);
        }
        if let Some(v) = self.username {
            a.username = non_empty(v);
        }
        if let Some(v) = self.imap_host {
            a.imap_host = v;
        }
        if let Some(v) = self.imap_port {
            a.imap_port = v;
        }
        if let Some(v) = self.smtp_host {
            a.smtp_host = v;
        }
        if let Some(v) = self.smtp_port {
            a.smtp_port = v;
        }
        if let Some(v) = self.smtp_security {
            a.smtp_security = v;
        }
        if let Some(v) = self.save_sent {
            a.save_sent = v;
        }
        if let Some(v) = self.remove_by {
            a.remove_by = v;
        }
        if let Some(v) = self.trash_mailbox {
            a.trash_mailbox = non_empty(v);
        }
        Ok(a.auth != old_auth)
    }
}

impl Engine {
    pub async fn accounts(&self) -> AccountList {
        let cfg = self
            .config
            .read()
            .await;
        AccountList {
            accounts: cfg
                .accounts
                .clone(),
            google_oauth_configured: cfg
                .google_oauth
                .is_some(),
            secret_store: cfg.secret_store,
        }
    }

    /// Add an account and save the configuration. Does not log in.
    pub async fn add_account(&self, new: NewAccount) -> Result<Positioned> {
        let position = new.position;
        let account = new.resolve()?;
        let mut cfg = self
            .config
            .write()
            .await;
        if cfg
            .accounts
            .iter()
            .any(|x| {
                x.id == account.id
                    || x.email
                        .eq_ignore_ascii_case(&account.email)
            })
        {
            bail!(
                "an account with id '{}' or email '{}' already exists",
                account.id,
                account.email
            );
        }
        let len = cfg
            .accounts
            .len();
        let index = position
            .unwrap_or(len)
            .min(len);
        cfg.accounts
            .insert(index, account.clone());
        cfg.save()?;
        Ok(Positioned { account, index })
    }

    /// Change an account's settings or position. Returns the updated account and whether its
    /// authentication method changed (in which case it must log in again).
    pub async fn update_account(
        &self,
        key: &str,
        update: AccountUpdate,
    ) -> Result<(Positioned, bool)> {
        if let Some(t) = update
            .trash_mailbox
            .as_deref()
            .filter(|t| !t.is_empty())
        {
            let current = self
                .account(Some(key))
                .await?;
            let boxes = self
                .with_session(&current, None, async |s| imap::list_mailboxes(s).await)
                .await?;
            if !boxes
                .iter()
                .any(|b| b.name == t && b.selectable)
            {
                bail!("no mailbox named '{t}' on account '{}'", current.id);
            }
        }
        let position = update.position;
        let (positioned, auth_changed) = {
            let mut cfg = self
                .config
                .write()
                .await;
            let idx = cfg.index_of(key)?;
            let mut a = cfg.accounts[idx].clone();
            let auth_changed = update.apply(&mut a)?;
            cfg.accounts
                .remove(idx);
            let index = position
                .unwrap_or(idx)
                .min(
                    cfg.accounts
                        .len(),
                );
            cfg.accounts
                .insert(index, a.clone());
            cfg.save()?;
            (Positioned { account: a, index }, auth_changed)
        };
        self.forget(
            &positioned
                .account
                .id,
        )
        .await;
        Ok((positioned, auth_changed))
    }

    /// Remove an account and delete its stored credentials. Mail on the server is untouched.
    pub async fn remove_account(&self, key: &str) -> Result<Account> {
        let (removed, store) = {
            let mut cfg = self
                .config
                .write()
                .await;
            let idx = cfg.index_of(key)?;
            let removed = cfg
                .accounts
                .remove(idx);
            cfg.save()?;
            (removed, cfg.secret_store)
        };
        self.forget(&removed.id)
            .await;
        secrets::delete_all(store, &removed.id).await?;
        Ok(removed)
    }

    /// Log in. A password is verified against IMAP before it is stored, so a typo never
    /// replaces a working password. OAuth accounts start a Google sign-in instead.
    pub async fn login(
        self: &Arc<Self>,
        key: &str,
        password: Option<String>,
    ) -> Result<(Account, LoginStart)> {
        let account = self
            .account(Some(key))
            .await?;
        let store = self
            .secret_store()
            .await;
        self.forget(&account.id)
            .await;
        match account.auth {
            AuthMethod::Password => {
                let pw = password
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| anyhow!("password is required for this account"))?;
                let session = imap::connect(&account, &imap::Credential::Password(pw.clone()))
                    .await
                    .map_err(|e| {
                        let hint = match account.provider {
                            Provider::Icloud => " (iCloud requires an app-specific password, not your Apple ID password)",
                            Provider::Gmail => " (Gmail requires an app password with 2-Step Verification enabled, or use auth=oauth2)",
                            Provider::Custom => "",
                        };
                        anyhow!("{e:#}{hint}")
                    })?;
                imap::logout(session).await;
                secrets::set(store, &account.id, SecretKind::Password, pw).await?;
                Ok((account, LoginStart::LoggedIn))
            }
            AuthMethod::Oauth2 => {
                let auth_url = self
                    .begin_oauth(&account, store)
                    .await?;
                Ok((account, LoginStart::BrowserSignIn { auth_url }))
            }
        }
    }

    async fn begin_oauth(
        self: &Arc<Self>,
        account: &Account,
        store: SecretStore,
    ) -> Result<String> {
        let client = self
            .oauth_client()
            .await?;
        let pending = oauth::begin_login(&client, &account.email).await?;
        let url = pending
            .auth_url
            .clone();
        let id = account
            .id
            .clone();
        self.logins
            .lock()
            .await
            .insert(
                id.clone(),
                LoginState::Waiting {
                    auth_url: url.clone(),
                },
            );
        // The sign-in finishes in the browser, long after this call returns.
        let engine = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = async {
                let (refresh, access) = pending
                    .complete(&client)
                    .await?;
                secrets::set(store, &id, SecretKind::OauthRefreshToken, refresh).await?;
                engine
                    .cache_token(&id, access)
                    .await;
                anyhow::Ok(())
            }
            .await;
            let mut logins = engine
                .logins
                .lock()
                .await;
            match outcome {
                Ok(()) => {
                    logins.remove(&id);
                }
                Err(e) => {
                    logins.insert(id, LoginState::Failed(format!("{e:#}")));
                }
            }
        });
        Ok(url)
    }

    /// Close connections and delete the account's stored password or tokens.
    pub async fn logout(&self, key: &str) -> Result<Account> {
        let account = self
            .account(Some(key))
            .await?;
        let store = self
            .secret_store()
            .await;
        self.forget(&account.id)
            .await;
        secrets::delete_all(store, &account.id).await?;
        Ok(account)
    }

    /// Login state of one account (`Some(key)`) or all, optionally testing IMAP and SMTP.
    pub async fn status(
        &self,
        key: Option<&str>,
        test_connection: bool,
    ) -> Result<Vec<AccountStatus>> {
        let (accounts, store) = {
            let cfg = self
                .config
                .read()
                .await;
            let list = match key {
                Some(k) => vec![
                    cfg.find(Some(k))?
                        .clone(),
                ],
                None => cfg
                    .accounts
                    .clone(),
            };
            (list, cfg.secret_store)
        };
        let mut out = vec![];
        for account in accounts {
            let kind = match account.auth {
                AuthMethod::Password => SecretKind::Password,
                AuthMethod::Oauth2 => SecretKind::OauthRefreshToken,
            };
            let logged_in = secrets::get(store, &account.id, kind)
                .await
                .map(|s| s.is_some())
                .map_err(|e| format!("{e:#}"));
            let pending_login = self
                .logins
                .lock()
                .await
                .get(&account.id)
                .cloned();
            let connection = if test_connection && logged_in == Ok(true) {
                Some(
                    self.check_connection(&account)
                        .await,
                )
            } else {
                None
            };
            out.push(AccountStatus {
                account,
                logged_in,
                pending_login,
                connection,
            });
        }
        Ok(out)
    }

    /// Connect to IMAP and SMTP with the stored credential.
    pub async fn check_connection(&self, account: &Account) -> ConnectionCheck {
        let imap = self
            .with_session(account, None, async |s| {
                Ok(s.noop()
                    .await?)
            })
            .await
            .map_err(|e| format!("{e:#}"));
        let smtp = match self
            .credential(account)
            .await
        {
            Ok(cred) => smtp::test(account, &cred)
                .await
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("{e:#}")),
        };
        ConnectionCheck { imap, smtp }
    }

    /// Set the Google OAuth "Desktop app" client used by Gmail accounts with OAuth2.
    pub async fn set_google_oauth_client(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Result<()> {
        let mut cfg = self
            .config
            .write()
            .await;
        cfg.google_oauth = Some(GoogleOauthClient {
            client_id: client_id
                .trim()
                .to_owned(),
            client_secret: client_secret
                .trim()
                .to_owned(),
        });
        cfg.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(email: &str) -> NewAccount {
        NewAccount {
            email: email.into(),
            ..Default::default()
        }
    }

    #[test]
    fn icloud_accounts_get_provider_defaults() {
        let a = new(" Jane.Doe@me.com ")
            .resolve()
            .unwrap();
        assert_eq!(a.id, "jane-doe");
        assert_eq!(a.email, "Jane.Doe@me.com");
        assert_eq!(a.provider, Provider::Icloud);
        assert_eq!(a.imap_host, "imap.mail.me.com");
        assert_eq!(a.smtp_port, 587);
        assert_eq!(a.remove_by, RemoveBy::MoveToTrash);
        assert!(a.save_sent);
    }

    #[test]
    fn explicit_settings_override_defaults() {
        let a = NewAccount {
            id: Some("work".into()),
            save_sent: Some(false),
            smtp_port: Some(2525),
            ..new("me@gmail.com")
        }
        .resolve()
        .unwrap();
        assert_eq!(a.id, "work");
        assert_eq!(a.provider, Provider::Gmail);
        assert!(!a.save_sent);
        assert_eq!(a.smtp_port, 2525);
    }

    #[test]
    fn custom_providers_need_hosts() {
        let err = new("me@example.org")
            .resolve()
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("imap_host"),
            "{err}"
        );
        let a = NewAccount {
            imap_host: Some("imap.example.org".into()),
            smtp_host: Some("smtp.example.org".into()),
            ..new("me@example.org")
        }
        .resolve()
        .unwrap();
        assert_eq!(a.imap_port, 993);
        assert_eq!(a.smtp_security, SmtpSecurity::Starttls);
    }

    #[test]
    fn invalid_accounts_are_rejected() {
        assert!(
            new("not-an-address")
                .resolve()
                .is_err()
        );
        assert!(
            NewAccount {
                id: Some("has space".into()),
                ..new("a@me.com")
            }
            .resolve()
            .is_err()
        );
        assert!(
            NewAccount {
                auth: Some(AuthMethod::Oauth2),
                ..new("a@me.com")
            }
            .resolve()
            .is_err()
        );
    }

    #[test]
    fn updates_change_only_what_is_given_and_empty_clears() {
        let mut a = NewAccount {
            display_name: Some("Jane".into()),
            ..new("jane@me.com")
        }
        .resolve()
        .unwrap();
        let changed = AccountUpdate {
            display_name: Some(String::new()),
            trash_mailbox: Some("Trash".into()),
            remove_by: Some(RemoveBy::Delete),
            ..Default::default()
        }
        .apply(&mut a)
        .unwrap();
        assert!(!changed);
        assert_eq!(a.display_name, None);
        assert_eq!(
            a.trash_mailbox
                .as_deref(),
            Some("Trash")
        );
        assert_eq!(a.remove_by, RemoveBy::Delete);
        assert_eq!(a.imap_host, "imap.mail.me.com");
    }

    #[test]
    fn oauth_is_refused_for_non_gmail_updates() {
        let mut a = new("jane@me.com")
            .resolve()
            .unwrap();
        let err = AccountUpdate {
            auth: Some(AuthMethod::Oauth2),
            ..Default::default()
        }
        .apply(&mut a);
        assert!(err.is_err());
        assert_eq!(a.auth, AuthMethod::Password);
    }
}
