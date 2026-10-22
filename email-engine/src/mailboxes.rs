//! Mailboxes (folders; Gmail labels appear as mailboxes).

use anyhow::{Result, bail};

use crate::config::Account;
use crate::engine::Engine;
use crate::imap::{self, SpecialUse};

#[derive(Debug, Clone)]
pub struct Mailbox {
    pub name: String,
    pub special_use: Option<SpecialUse>,
    pub selectable: bool,
    /// Present when counts were requested and the server answered STATUS.
    pub counts: Option<MailboxCounts>,
}

#[derive(Debug, Clone, Copy)]
pub struct MailboxCounts {
    pub messages: u32,
    /// `None` when the server did not report UNSEEN.
    pub unread: Option<u32>,
}

/// The special-use mailbox a well-known name ("archive", "trash", "junk", "spam", "sent",
/// "drafts", any case) refers to, or `None` for an ordinary mailbox name.
pub fn special_name(name: &str) -> Option<SpecialUse> {
    match name
        .to_ascii_lowercase()
        .as_str()
    {
        "archive" => Some(SpecialUse::Archive),
        "trash" => Some(SpecialUse::Trash),
        "junk" | "spam" => Some(SpecialUse::Junk),
        "sent" => Some(SpecialUse::Sent),
        "drafts" => Some(SpecialUse::Drafts),
        _ => None,
    }
}

impl Engine {
    /// All mailboxes, with message and unread counts if `with_counts` (one STATUS each).
    pub async fn list_mailboxes(
        &self,
        account: &Account,
        with_counts: bool,
    ) -> Result<Vec<Mailbox>> {
        self.with_session(account, None, async |s| {
            let boxes = imap::list_mailboxes(s).await?;
            let mut out = Vec::with_capacity(boxes.len());
            for b in boxes {
                let counts = if with_counts && b.selectable {
                    s.status(&b.name, "(MESSAGES UNSEEN)")
                        .await
                        .ok()
                        .map(|st| MailboxCounts {
                            messages: st.exists,
                            unread: st.unseen,
                        })
                } else {
                    None
                };
                out.push(Mailbox {
                    name: b.name,
                    special_use: b.special,
                    selectable: b.selectable,
                    counts,
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn create_mailbox(&self, account: &Account, name: &str) -> Result<()> {
        self.with_session(account, None, async |s| {
            Ok(s.create(name)
                .await?)
        })
        .await
    }

    /// Delete a mailbox and everything in it. INBOX is refused.
    pub async fn delete_mailbox(&self, account: &Account, name: &str) -> Result<()> {
        if name.eq_ignore_ascii_case("INBOX") {
            bail!("refusing to delete INBOX");
        }
        self.with_session(account, None, async |s| {
            Ok(s.delete(name)
                .await?)
        })
        .await
    }

    pub async fn rename_mailbox(
        &self,
        account: &Account,
        name: &str,
        new_name: &str,
    ) -> Result<()> {
        self.with_session(account, None, async |s| {
            Ok(s.rename(name, new_name)
                .await?)
        })
        .await
    }

    /// The real mailbox `name` refers to. Well-known names (see [`special_name`]) resolve to
    /// the account's `trash_mailbox` setting or the server's special-use folder, unless a
    /// mailbox with exactly that name exists; any other name is returned as is.
    pub async fn resolve_mailbox(&self, account: &Account, name: &str) -> Result<String> {
        let Some(kind) = special_name(name) else {
            return Ok(name.to_owned());
        };
        if kind == SpecialUse::Trash
            && let Some(t) = &account.trash_mailbox
        {
            return Ok(t.clone());
        }
        self.with_session(account, None, async |s| {
            let boxes = imap::list_mailboxes(s).await?;
            if let Some(b) = boxes
                .iter()
                .find(|b| b.name == name)
            {
                return Ok(b
                    .name
                    .clone());
            }
            imap::find_special(s, kind).await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_names_resolve_case_insensitively() {
        assert_eq!(special_name("Trash"), Some(SpecialUse::Trash));
        assert_eq!(special_name("SPAM"), Some(SpecialUse::Junk));
        assert_eq!(special_name("junk"), Some(SpecialUse::Junk));
        assert_eq!(special_name("drafts"), Some(SpecialUse::Drafts));
        assert_eq!(special_name("INBOX"), None);
        assert_eq!(special_name("Trash/2026"), None);
    }
}
