//! Sending mail, drafts, replies and forwards.

use anyhow::{Result, anyhow, bail};
use mail_parser::MessageParser;

use crate::compose::{self, Compose};
use crate::config::Account;
use crate::engine::Engine;
use crate::imap::{self, SpecialUse};
use crate::smtp;

/// What happened besides the send itself.
#[derive(Debug, Clone, Default)]
pub struct SendReport {
    /// E.g. where the Sent copy went, or why saving it failed.
    pub notes: Vec<String>,
}

impl Engine {
    /// Send over SMTP, file a copy in Sent if the account's `save_sent` is on, and mark
    /// `answered` (mailbox, UID) as answered. Failing to file the copy does not fail the send.
    pub async fn send(
        &self,
        account: &Account,
        c: &Compose,
        answered: Option<(&str, u32)>,
    ) -> Result<SendReport> {
        if c.to
            .is_empty()
            && c.cc
                .is_empty()
            && c.bcc
                .is_empty()
        {
            bail!("no recipients");
        }
        let msg = compose::build(account, c, false)?;
        let raw = msg.formatted();
        let cred = self
            .credential(account)
            .await?;
        smtp::send(account, &cred, msg.envelope(), &raw).await?;

        let mut report = SendReport::default();
        if account.save_sent {
            let r = self
                .with_session(account, None, async |s| {
                    let sent = imap::find_special(s, SpecialUse::Sent).await?;
                    s.append(&sent, Some("(\\Seen)"), None, &raw)
                        .await?;
                    Ok(sent)
                })
                .await;
            report
                .notes
                .push(match r {
                    Ok(sent) => format!("copy saved to {sent}"),
                    Err(e) => format!("sent, but saving a copy to Sent failed: {e:#}"),
                });
        }
        if let Some((mailbox, uid)) = answered {
            let _ = self
                .with_session(account, Some(mailbox), async |s| {
                    imap::store_flags(s, &[uid], "+", &["\\Answered".into()]).await
                })
                .await;
        }
        Ok(report)
    }

    /// Save a draft, replacing draft `replace_uid` if given. Returns the Drafts mailbox.
    pub async fn save_draft(
        &self,
        account: &Account,
        c: &Compose,
        replace_uid: Option<u32>,
    ) -> Result<String> {
        let raw = compose::build(account, c, true)?.formatted();
        self.with_session(account, None, async |s| {
            let drafts = imap::find_special(s, SpecialUse::Drafts).await?;
            s.append(&drafts, Some("(\\Draft \\Seen)"), None, &raw)
                .await?;
            if let Some(old) = replace_uid {
                s.select(&drafts)
                    .await?;
                imap::store_flags(s, &[old], "+", &["\\Deleted".into()]).await?;
                imap::expunge_uids(s, &[old]).await?;
            }
            Ok(drafts)
        })
        .await
    }

    /// A threaded reply to message `uid` in `mailbox`, quoting the original.
    pub async fn reply_to(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        body: &str,
        html: Option<String>,
        reply_all: bool,
    ) -> Result<Compose> {
        let raw = self
            .fetch_message(account, mailbox, uid, false)
            .await?
            .raw;
        let orig = MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| anyhow!("could not parse original message"))?;
        Ok(compose::reply(account, &orig, body, html, reply_all))
    }

    /// A forward of message `uid` in `mailbox` to `to`, with `body` above it.
    pub async fn forward(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        to: Vec<String>,
        body: &str,
        include_attachments: bool,
    ) -> Result<Compose> {
        let raw = self
            .fetch_message(account, mailbox, uid, false)
            .await?
            .raw;
        let orig = MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| anyhow!("could not parse original message"))?;
        Ok(compose::forward(&orig, to, body, include_attachments))
    }

    /// Send draft `uid` from the Drafts mailbox, then remove the draft. Returns what was sent,
    /// the send report, and whether the draft was removed.
    pub async fn send_draft(
        &self,
        account: &Account,
        uid: u32,
    ) -> Result<(Compose, SendReport, bool)> {
        let drafts = self
            .resolve_mailbox(account, "drafts")
            .await?;
        let raw = self
            .fetch_message(account, &drafts, uid, false)
            .await?
            .raw;
        let msg = MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| anyhow!("could not parse draft"))?;
        let c = compose::from_draft(&msg);
        let report = self
            .send(account, &c, None)
            .await?;
        let removed = self
            .with_session(account, Some(&drafts), async |s| {
                imap::store_flags(s, &[uid], "+", &["\\Deleted".into()]).await?;
                imap::expunge_uids(s, &[uid]).await
            })
            .await
            .is_ok();
        Ok((c, report, removed))
    }
}
