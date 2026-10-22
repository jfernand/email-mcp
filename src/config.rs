use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Well-known providers; each fills in server defaults for its accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// iCloud Mail (me.com, mac.com, icloud.com). Requires an app-specific password.
    Icloud,
    /// Gmail / Google Workspace. Supports OAuth2 or an app password.
    Gmail,
    /// Any other IMAP/SMTP server; hosts and ports must be given.
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// Username + password (for iCloud and Gmail this must be an app-specific password).
    Password,
    /// OAuth2 (XOAUTH2). Currently supported for Gmail only.
    Oauth2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Implicit TLS (usually port 465).
    Tls,
    /// STARTTLS upgrade (usually port 587).
    Starttls,
}

/// What delete_messages does by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoveBy {
    /// Move messages to the Trash mailbox; they can be recovered from there.
    #[default]
    MoveToTrash,
    /// Delete messages permanently (expunge). They cannot be recovered.
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Short unique identifier used to refer to the account in tool calls.
    pub id: String,
    pub email: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub provider: Provider,
    pub auth: AuthMethod,
    /// Login name; defaults to the email address.
    #[serde(default)]
    pub username: Option<String>,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: SmtpSecurity,
    /// Whether to APPEND a copy of sent mail to the Sent mailbox.
    /// Gmail does this server-side; iCloud and most others do not.
    pub save_sent: bool,
    /// Remove messages by moving them to Trash or by deleting them permanently.
    #[serde(default)]
    pub remove_by: RemoveBy,
    /// Mailbox that deleted mail goes to, overriding the server's \Trash folder.
    /// iCloud flags "Deleted Messages", but Apple Mail clients may use "Trash".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_mailbox: Option<String>,
}

impl Account {
    pub fn username(&self) -> &str {
        self.username.as_deref().unwrap_or(&self.email)
    }
}

/// OAuth client credentials (a Google Cloud "Desktop app" client).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoogleOauthClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretStore {
    /// OS keyring (Secret Service on Linux, Keychain on macOS).
    #[default]
    Keyring,
    /// A 0600-permission JSON file next to the config. Use only if no keyring is available.
    File,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub secret_store: SecretStore,
    #[serde(default)]
    pub google_oauth: Option<GoogleOauthClient>,
    /// Ordered list; the first account is the default.
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("EMAIL_MCP_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("", "", "email-mcp")
        .ok_or_else(|| anyhow!("could not determine a config directory"))?;
    Ok(dirs.config_dir().to_path_buf())
}

impl Config {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Resolve an account by id or email; `None` selects the default (first) account.
    pub fn find(&self, key: Option<&str>) -> Result<&Account> {
        match key {
            None => self
                .accounts
                .first()
                .ok_or_else(|| anyhow!("no accounts configured; use add_account first")),
            Some(k) => self
                .accounts
                .iter()
                .find(|a| a.id.eq_ignore_ascii_case(k) || a.email.eq_ignore_ascii_case(k))
                .ok_or_else(|| {
                    let known: Vec<_> = self.accounts.iter().map(|a| a.id.as_str()).collect();
                    anyhow!("unknown account '{k}' (known: {})", known.join(", "))
                }),
        }
    }

    pub fn index_of(&self, key: &str) -> Result<usize> {
        let id = self.find(Some(key))?.id.clone();
        Ok(self.accounts.iter().position(|a| a.id == id).unwrap())
    }
}

/// Server defaults for a provider, given the email address.
pub struct ProviderDefaults {
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub smtp_security: SmtpSecurity,
    pub save_sent: bool,
    pub auth: AuthMethod,
}

pub fn provider_defaults(provider: Provider) -> Option<ProviderDefaults> {
    match provider {
        Provider::Icloud => Some(ProviderDefaults {
            imap_host: "imap.mail.me.com",
            imap_port: 993,
            smtp_host: "smtp.mail.me.com",
            smtp_port: 587,
            smtp_security: SmtpSecurity::Starttls,
            save_sent: true,
            auth: AuthMethod::Password,
        }),
        Provider::Gmail => Some(ProviderDefaults {
            imap_host: "imap.gmail.com",
            imap_port: 993,
            smtp_host: "smtp.gmail.com",
            smtp_port: 465,
            smtp_security: SmtpSecurity::Tls,
            save_sent: false,
            auth: AuthMethod::Password,
        }),
        Provider::Custom => None,
    }
}

/// Guess the provider from an email domain.
pub fn guess_provider(email: &str) -> Provider {
    let domain = email.rsplit('@').next().unwrap_or("").to_ascii_lowercase();
    match domain.as_str() {
        "me.com" | "mac.com" | "icloud.com" => Provider::Icloud,
        "gmail.com" | "googlemail.com" => Provider::Gmail,
        _ => Provider::Custom,
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("account id must be non-empty and contain only letters, digits, '-' or '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_without_new_settings_still_loads() {
        let cfg: Config = toml::from_str(
            r#"
            [[account]]
            id = "me"
            email = "me@me.com"
            provider = "icloud"
            auth = "password"
            imap_host = "imap.mail.me.com"
            imap_port = 993
            smtp_host = "smtp.mail.me.com"
            smtp_port = 587
            smtp_security = "starttls"
            save_sent = true
            "#,
        )
        .unwrap();
        let a = &cfg.accounts[0];
        assert_eq!(a.remove_by, RemoveBy::MoveToTrash);
        assert_eq!(a.trash_mailbox, None);
        // Unset trash_mailbox is not written back.
        assert!(!toml::to_string(&cfg).unwrap().contains("trash_mailbox"));
    }
}
