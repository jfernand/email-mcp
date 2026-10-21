use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::{SecretStore, config_dir};

const SERVICE: &str = "email-mcp";

#[derive(Debug, Clone, Copy)]
pub enum SecretKind {
    Password,
    OauthRefreshToken,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::OauthRefreshToken => "oauth-refresh-token",
        }
    }
}

fn key(account_id: &str, kind: SecretKind) -> String {
    format!("{account_id}:{}", kind.suffix())
}

pub async fn get(store: SecretStore, account_id: &str, kind: SecretKind) -> Result<Option<String>> {
    let key = key(account_id, kind);
    run_blocking(move || match store {
        SecretStore::Keyring => match keyring::Entry::new(SERVICE, &key)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading from OS keyring"),
        },
        SecretStore::File => Ok(read_file()?.remove(&key)),
    })
    .await
}

pub async fn set(store: SecretStore, account_id: &str, kind: SecretKind, value: String) -> Result<()> {
    let key = key(account_id, kind);
    run_blocking(move || match store {
        SecretStore::Keyring => keyring::Entry::new(SERVICE, &key)?
            .set_password(&value)
            .context("writing to OS keyring"),
        SecretStore::File => {
            let mut map = read_file()?;
            map.insert(key, value);
            write_file(&map)
        }
    })
    .await
}

pub async fn delete(store: SecretStore, account_id: &str, kind: SecretKind) -> Result<()> {
    let key = key(account_id, kind);
    run_blocking(move || match store {
        SecretStore::Keyring => match keyring::Entry::new(SERVICE, &key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("deleting from OS keyring"),
        },
        SecretStore::File => {
            let mut map = read_file()?;
            if map.remove(&key).is_some() {
                write_file(&map)?;
            }
            Ok(())
        }
    })
    .await
}

pub async fn delete_all(store: SecretStore, account_id: &str) -> Result<()> {
    delete(store, account_id, SecretKind::Password).await?;
    delete(store, account_id, SecretKind::OauthRefreshToken).await
}

async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

fn file_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("secrets.json"))
}

fn read_file() -> Result<BTreeMap<String, String>> {
    let path = file_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_file(map: &BTreeMap<String, String>) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let path = file_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(map)?.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
