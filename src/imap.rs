use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::config::Account;
use crate::oauth::xoauth2_string;

pub type Session = async_imap::Session<TlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub enum Credential {
    Password(String),
    OauthToken(String),
}

struct XOauth2(String);

impl async_imap::Authenticator for &XOauth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.0
            .clone()
    }
}

pub async fn connect(account: &Account, cred: &Credential) -> Result<Session> {
    let host = account
        .imap_host
        .as_str();
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((host, account.imap_port)),
    )
    .await
    .map_err(|_| anyhow!("timed out connecting to {host}"))?
    .with_context(|| format!("connecting to {host}:{}", account.imap_port))?;
    let tls = TlsConnector::from(crate::tls::client_config())
        .connect(ServerName::try_from(host.to_owned())?, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}"))?;

    let mut client = async_imap::Client::new(tls);
    client
        .read_response()
        .await?
        .ok_or_else(|| anyhow!("{host} closed the connection before greeting"))?;

    let session = match cred {
        Credential::Password(pw) => client
            .login(account.username(), pw)
            .await
            .map_err(|(e, _)| anyhow!("IMAP login failed: {e}"))?,
        Credential::OauthToken(token) => {
            let auth = XOauth2(xoauth2_string(account.username(), token));
            client
                .authenticate("XOAUTH2", &auth)
                .await
                .map_err(|(e, _)| anyhow!("IMAP XOAUTH2 authentication failed: {e}"))?
        }
    };
    Ok(session)
}

/// One cached IMAP session per account. Sessions are checked with NOOP before
/// reuse and reopened if the server dropped them.
#[derive(Default)]
pub struct Pool {
    sessions: Mutex<HashMap<String, Arc<Mutex<Option<Session>>>>>,
}

impl Pool {
    pub async fn slot(&self, account_id: &str) -> Arc<Mutex<Option<Session>>> {
        self.sessions
            .lock()
            .await
            .entry(account_id.to_owned())
            .or_default()
            .clone()
    }

    pub async fn drop_session(&self, account_id: &str) {
        if let Some(slot) = self
            .sessions
            .lock()
            .await
            .remove(account_id)
            && let Some(mut s) = slot
                .lock()
                .await
                .take()
        {
            let _ = s
                .logout()
                .await;
        }
    }
}

// ---------- mailboxes ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialUse {
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
}

pub struct MailboxInfo {
    pub name: String,
    pub special: Option<SpecialUse>,
    pub selectable: bool,
}

pub async fn list_mailboxes(s: &mut Session) -> Result<Vec<MailboxInfo>> {
    let names: Vec<_> = s
        .list(Some(""), Some("*"))
        .await?
        .try_collect()
        .await?;
    Ok(names
        .iter()
        .map(|n| {
            let mut special = None;
            let mut selectable = true;
            for attr in n.attributes() {
                match attr {
                    NameAttribute::Sent => special = Some(SpecialUse::Sent),
                    NameAttribute::Drafts => special = Some(SpecialUse::Drafts),
                    NameAttribute::Junk => special = Some(SpecialUse::Junk),
                    NameAttribute::Archive => special = Some(SpecialUse::Archive),
                    NameAttribute::All => special = Some(SpecialUse::All),
                    NameAttribute::Trash => special = Some(SpecialUse::Trash),
                    NameAttribute::NoSelect => selectable = false,
                    _ => {}
                }
            }
            MailboxInfo {
                name: n
                    .name()
                    .to_owned(),
                special,
                selectable,
            }
        })
        .collect())
}

/// Find a special-use mailbox, falling back to common names for servers
/// that don't advertise SPECIAL-USE attributes.
pub async fn find_special(s: &mut Session, kind: SpecialUse) -> Result<String> {
    let boxes = list_mailboxes(s).await?;
    if let Some(b) = boxes
        .iter()
        .find(|b| b.special == Some(kind))
    {
        return Ok(b
            .name
            .clone());
    }
    let candidates: &[&str] = match kind {
        SpecialUse::Sent => &["Sent Messages", "Sent", "[Gmail]/Sent Mail", "Sent Items"],
        SpecialUse::Drafts => &["Drafts", "[Gmail]/Drafts"],
        SpecialUse::Trash => &[
            "Deleted Messages",
            "Trash",
            "[Gmail]/Trash",
            "Deleted Items",
        ],
        SpecialUse::Junk => &["Junk", "Spam", "[Gmail]/Spam"],
        SpecialUse::Archive => &["Archive"],
        SpecialUse::All => &["[Gmail]/All Mail"],
    };
    candidates
        .iter()
        .find_map(|c| {
            boxes
                .iter()
                .find(|b| {
                    b.name
                        .eq_ignore_ascii_case(c)
                })
        })
        .map(|b| {
            b.name
                .clone()
        })
        .ok_or_else(|| anyhow!("could not find a {kind:?} mailbox on this account"))
}

// ---------- messages ----------

pub struct Summary {
    pub uid: u32,
    pub flags: Vec<String>,
    pub size: Option<u32>,
    pub internal_date: Option<String>,
    pub headers: Vec<u8>,
}

pub fn flag_name(f: &Flag) -> String {
    match f {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Recent => "\\Recent".into(),
        Flag::MayCreate => "\\*".into(),
        Flag::Custom(c) => c.to_string(),
    }
}

const SUMMARY_QUERY: &str = "(UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM TO CC SUBJECT DATE MESSAGE-ID)])";

fn to_summary(f: &Fetch) -> Option<Summary> {
    Some(Summary {
        uid: f.uid?,
        flags: f
            .flags()
            .map(|fl| flag_name(&fl))
            .collect(),
        size: f.size,
        internal_date: f
            .internal_date()
            .map(|d| d.to_rfc3339()),
        headers: f
            .header()
            .unwrap_or_default()
            .to_vec(),
    })
}

/// Fetch summaries for the given UIDs, newest (highest UID) first.
pub async fn fetch_summaries(s: &mut Session, uids: &[u32]) -> Result<Vec<Summary>> {
    if uids.is_empty() {
        return Ok(vec![]);
    }
    let fetches: Vec<Fetch> = s
        .uid_fetch(uid_set(uids), SUMMARY_QUERY)
        .await?
        .try_collect()
        .await?;
    let mut out: Vec<_> = fetches
        .iter()
        .filter_map(to_summary)
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.uid));
    Ok(out)
}

/// UIDs matching `query` in the currently selected mailbox, newest first.
pub async fn search(s: &mut Session, query: &str) -> Result<Vec<u32>> {
    let mut uids: Vec<u32> = s
        .uid_search(query)
        .await?
        .into_iter()
        .collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(uids)
}

pub async fn fetch_raw(s: &mut Session, uid: u32) -> Result<(Vec<u8>, Vec<String>)> {
    let fetches: Vec<Fetch> = s
        .uid_fetch(uid.to_string(), "(UID FLAGS BODY.PEEK[])")
        .await?
        .try_collect()
        .await?;
    let f = fetches
        .iter()
        .find(|f| f.uid == Some(uid))
        .ok_or_else(|| anyhow!("no message with UID {uid} in the selected mailbox"))?;
    let body = f
        .body()
        .ok_or_else(|| anyhow!("server returned no body for UID {uid}"))?
        .to_vec();
    Ok((
        body,
        f.flags()
            .map(|fl| flag_name(&fl))
            .collect(),
    ))
}

pub fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub async fn store_flags(s: &mut Session, uids: &[u32], op: &str, flags: &[String]) -> Result<()> {
    if flags.is_empty() || uids.is_empty() {
        return Ok(());
    }
    let q = format!("{op}FLAGS.SILENT ({})", flags.join(" "));
    let _: Vec<Fetch> = s
        .uid_store(uid_set(uids), q)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

pub async fn move_messages(s: &mut Session, uids: &[u32], to: &str) -> Result<()> {
    let caps = s
        .capabilities()
        .await?;
    if caps.has_str("MOVE") {
        s.uid_mv(uid_set(uids), to)
            .await?;
    } else {
        s.uid_copy(uid_set(uids), to)
            .await?;
        store_flags(s, uids, "+", &["\\Deleted".into()]).await?;
        expunge_uids(s, uids).await?;
    }
    Ok(())
}

pub async fn expunge_uids(s: &mut Session, uids: &[u32]) -> Result<()> {
    let caps = s
        .capabilities()
        .await?;
    if caps.has_str("UIDPLUS") {
        let _: Vec<_> = s
            .uid_expunge(uid_set(uids))
            .await?
            .try_collect()
            .await?;
    } else {
        let _: Vec<_> = s
            .expunge()
            .await?
            .try_collect()
            .await?;
    }
    Ok(())
}

/// Quote a string for use as an IMAP astring in SEARCH etc.
pub fn quote(s: &str) -> Result<String> {
    if s.contains(['\r', '\n']) {
        bail!("search terms cannot contain line breaks");
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
    ))
}
