//! Reading, searching and organizing messages. Messages are identified by mailbox + UID.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use futures::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};

use crate::compose::{self, Attachment};
use crate::config::{Account, Provider};
use crate::engine::Engine;
use crate::imap;

/// One line of a message listing.
#[derive(Debug, Clone)]
pub struct MessageSummary {
    pub uid: u32,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub subject: String,
    /// The Date header as RFC 3339, falling back to the server's arrival time.
    pub date: Option<String>,
    pub unread: bool,
    pub flagged: bool,
    pub flags: Vec<String>,
    pub size: Option<u32>,
}

/// Messages matching a listing or search, newest first.
#[derive(Debug, Clone)]
pub struct MessagePage {
    /// How many messages matched in total, before paging.
    pub total: usize,
    pub messages: Vec<MessageSummary>,
}

/// Search criteria, combined with AND. Everything empty matches all messages.
#[derive(Debug, Clone, Default)]
pub struct SearchCriteria {
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    /// Anywhere in headers or body.
    pub text: Option<String>,
    /// In the body only.
    pub body: Option<String>,
    /// On or after this date (YYYY-MM-DD).
    pub since: Option<String>,
    /// Before this date (YYYY-MM-DD).
    pub before: Option<String>,
    /// `Some(true)` = only unread, `Some(false)` = only read.
    pub unread: Option<bool>,
    /// `Some(true)` = only flagged, `Some(false)` = only unflagged.
    pub flagged: Option<bool>,
    /// Gmail search syntax (Gmail accounts only).
    pub gmail_query: Option<String>,
    /// Raw IMAP SEARCH criteria, ANDed with the rest.
    pub raw: Option<String>,
}

/// A fetched message: its raw RFC 5322 bytes and flags.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub raw: Vec<u8>,
    pub flags: Vec<String>,
}

/// A parsed message, for display.
#[derive(Debug, Clone)]
pub struct MessageView {
    pub message_id: Option<String>,
    pub from: Vec<String>,
    pub reply_to: Vec<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    /// Plain text: the text part, or the HTML converted to text.
    pub text: String,
    /// HTML body: the HTML part, or the text part converted to HTML.
    pub html: Option<String>,
    /// Whether the message has a real HTML part.
    pub has_html: bool,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone)]
pub struct AttachmentInfo {
    /// 1-based, as used by [`Engine::save_attachments`].
    pub index: usize,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub size: usize,
}

#[derive(Debug, Clone)]
pub struct SavedAttachment {
    pub path: PathBuf,
    pub size: usize,
}

impl SearchCriteria {
    /// The IMAP SEARCH command arguments for these criteria on `provider`'s servers.
    pub fn to_query(&self, provider: Provider) -> Result<String> {
        let mut parts: Vec<String> = vec![];
        for (key, val) in [
            ("FROM", &self.from),
            ("TO", &self.to),
            ("SUBJECT", &self.subject),
            ("TEXT", &self.text),
            ("BODY", &self.body),
        ] {
            if let Some(v) = val {
                parts.push(format!("{key} {}", imap::quote(v)?));
            }
        }
        if let Some(d) = &self.since {
            parts.push(format!("SINCE {}", imap_date(d)?));
        }
        if let Some(d) = &self.before {
            parts.push(format!("BEFORE {}", imap_date(d)?));
        }
        match self.unread {
            Some(true) => parts.push("UNSEEN".into()),
            Some(false) => parts.push("SEEN".into()),
            None => {}
        }
        match self.flagged {
            Some(true) => parts.push("FLAGGED".into()),
            Some(false) => parts.push("UNFLAGGED".into()),
            None => {}
        }
        if let Some(q) = &self.gmail_query {
            if provider != Provider::Gmail {
                bail!("gmail_query only works on Gmail accounts");
            }
            parts.push(format!("X-GM-RAW {}", imap::quote(q)?));
        }
        if let Some(r) = &self.raw {
            if r.contains(['\r', '\n']) {
                bail!("raw criteria cannot contain line breaks");
            }
            parts.push(r.clone());
        }
        if parts.is_empty() {
            parts.push("ALL".into());
        }
        let q = parts.join(" ");
        Ok(if q.is_ascii() {
            q
        } else {
            format!("CHARSET UTF-8 {q}")
        })
    }
}

/// A YYYY-MM-DD date in IMAP's SEARCH format (`01-Feb-2026`).
pub fn imap_date(s: &str) -> Result<String> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow!("invalid date '{s}', expected YYYY-MM-DD"))?;
    Ok(d.format("%d-%b-%Y")
        .to_string())
}

/// The IMAP flag for a friendly name ("seen"/"read", "flagged"/"starred", "answered"/"replied",
/// "draft", "deleted"), or the name itself as a custom keyword if it is a valid atom.
pub fn normalize_flag(name: &str) -> Result<String> {
    let f = match name
        .trim()
        .trim_start_matches('\\')
        .to_ascii_lowercase()
        .as_str()
    {
        "seen" | "read" => "\\Seen".into(),
        "flagged" | "starred" => "\\Flagged".into(),
        "answered" | "replied" => "\\Answered".into(),
        "draft" => "\\Draft".into(),
        "deleted" => "\\Deleted".into(),
        _ => {
            let k = name.trim();
            if k.is_empty()
                || !k
                    .chars()
                    .all(|c| c.is_ascii_graphic() && !"(){%*\"\\]".contains(c))
            {
                bail!("invalid flag/keyword '{name}'");
            }
            k.to_owned()
        }
    };
    Ok(f)
}

fn summarize(s: &imap::Summary) -> MessageSummary {
    let parsed = MessageParser::default().parse(&s.headers);
    let get = |f: fn(&mail_parser::Message) -> Vec<String>| {
        parsed
            .as_ref()
            .map(f)
            .unwrap_or_default()
    };
    MessageSummary {
        uid: s.uid,
        from: get(|m| compose::addresses(m.from())),
        to: get(|m| compose::addresses(m.to())),
        subject: parsed
            .as_ref()
            .and_then(|m| m.subject())
            .unwrap_or("")
            .to_owned(),
        date: parsed
            .as_ref()
            .and_then(|m| m.date())
            .map(|d| d.to_rfc3339())
            .or(s
                .internal_date
                .clone()),
        unread: !s
            .flags
            .iter()
            .any(|f| f == "\\Seen"),
        flagged: s
            .flags
            .iter()
            .any(|f| f == "\\Flagged"),
        flags: s
            .flags
            .clone(),
        size: s.size,
    }
}

impl MessageView {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let msg = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| anyhow!("could not parse message"))?;
        let html = msg
            .body_html(0)
            .map(|h| h.into_owned());
        let has_html = msg
            .html_part(0)
            .is_some_and(|p| p.is_content_type("text", "html"));
        let attachments = msg
            .attachments()
            .enumerate()
            .map(|(i, p)| AttachmentInfo {
                index: i + 1,
                filename: p
                    .attachment_name()
                    .map(str::to_owned),
                content_type: p
                    .content_type()
                    .map(|c| {
                        format!(
                            "{}/{}",
                            c.ctype(),
                            c.subtype()
                                .unwrap_or("*")
                        )
                    }),
                size: p
                    .contents()
                    .len(),
            })
            .collect();
        Ok(Self {
            message_id: msg
                .message_id()
                .map(str::to_owned),
            from: compose::addresses(msg.from()),
            reply_to: compose::addresses(msg.reply_to()),
            to: compose::addresses(msg.to()),
            cc: compose::addresses(msg.cc()),
            subject: msg
                .subject()
                .map(str::to_owned),
            date: msg
                .date()
                .map(|d| d.to_rfc3339()),
            text: compose::best_text(&msg),
            html,
            has_html,
            attachments,
        })
    }
}

/// `name` made safe as a file name: no path separators, NUL or ':', no leading dots.
pub fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned
        .trim_start_matches('.')
        .trim();
    if cleaned.is_empty() {
        "attachment".into()
    } else {
        cleaned.to_owned()
    }
}

/// A path in `dir` for `name` that doesn't exist yet: `name`, else `stem (1).ext`, …
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut path = dir.join(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    path
}

impl Engine {
    /// The newest messages in `mailbox`, skipping `offset`, at most `limit`.
    pub async fn list_messages(
        &self,
        account: &Account,
        mailbox: &str,
        unread_only: bool,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        let query = if unread_only { "UNSEEN" } else { "ALL" };
        self.page(account, mailbox, query, offset, limit)
            .await
    }

    /// Messages in `mailbox` matching `criteria`, newest first, at most `limit`.
    /// Also returns the IMAP query that was run.
    pub async fn search(
        &self,
        account: &Account,
        mailbox: &str,
        criteria: &SearchCriteria,
        limit: usize,
    ) -> Result<(String, MessagePage)> {
        let query = criteria.to_query(account.provider)?;
        let page = self
            .page(account, mailbox, &query, 0, limit)
            .await?;
        Ok((query, page))
    }

    async fn page(
        &self,
        account: &Account,
        mailbox: &str,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Result<MessagePage> {
        self.with_session(account, Some(mailbox), async |s| {
            let uids = imap::search(s, query).await?;
            let total = uids.len();
            let page: Vec<u32> = uids
                .into_iter()
                .skip(offset)
                .take(limit)
                .collect();
            let messages = imap::fetch_summaries(s, &page)
                .await?
                .iter()
                .map(summarize)
                .collect();
            Ok(MessagePage { total, messages })
        })
        .await
    }

    /// Fetch one message without marking it read, unless `mark_read`.
    pub async fn fetch_message(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        mark_read: bool,
    ) -> Result<FetchedMessage> {
        self.with_session(account, Some(mailbox), async |s| {
            let (raw, flags) = imap::fetch_raw(s, uid).await?;
            if mark_read {
                imap::store_flags(s, &[uid], "+", &["\\Seen".into()]).await?;
            }
            Ok(FetchedMessage { raw, flags })
        })
        .await
    }

    /// Save attachment `index` (1-based), or all of them, into `dir` (default: the user's
    /// Downloads folder). Existing files are never overwritten.
    pub async fn save_attachments(
        &self,
        account: &Account,
        mailbox: &str,
        uid: u32,
        index: Option<usize>,
        dir: Option<PathBuf>,
    ) -> Result<Vec<SavedAttachment>> {
        let raw = self
            .fetch_message(account, mailbox, uid, false)
            .await?
            .raw;
        let msg = MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| anyhow!("could not parse message"))?;
        let all = compose::attachments_of(&msg);
        let chosen: Vec<&Attachment> = match index {
            Some(i) => vec![
                all.get(i.wrapping_sub(1))
                    .ok_or_else(|| anyhow!("no attachment #{i} (message has {})", all.len()))?,
            ],
            None => all
                .iter()
                .collect(),
        };
        if chosen.is_empty() {
            bail!("message has no attachments");
        }
        let dir = match dir {
            Some(d) => d,
            None => directories::UserDirs::new()
                .and_then(|u| {
                    u.download_dir()
                        .map(Path::to_path_buf)
                })
                .ok_or_else(|| {
                    anyhow!("no Downloads directory found; give a directory to save into")
                })?,
        };
        std::fs::create_dir_all(&dir)?;
        let mut saved = vec![];
        for att in chosen {
            let path = unique_path(&dir, &safe_filename(&att.filename));
            std::fs::write(&path, &att.data)
                .map_err(|e| anyhow!("writing {}: {e}", path.display()))?;
            saved.push(SavedAttachment {
                path,
                size: att
                    .data
                    .len(),
            });
        }
        Ok(saved)
    }

    /// Add and remove flags (friendly names or keywords, see [`normalize_flag`]).
    /// Returns the IMAP flags added and removed.
    pub async fn update_flags(
        &self,
        account: &Account,
        mailbox: &str,
        uids: &[u32],
        add: &[String],
        remove: &[String],
    ) -> Result<(Vec<String>, Vec<String>)> {
        let add = add
            .iter()
            .map(|f| normalize_flag(f))
            .collect::<Result<Vec<_>>>()?;
        let remove = remove
            .iter()
            .map(|f| normalize_flag(f))
            .collect::<Result<Vec<_>>>()?;
        self.with_session(account, Some(mailbox), async |s| {
            imap::store_flags(s, uids, "+", &add).await?;
            imap::store_flags(s, uids, "-", &remove).await
        })
        .await?;
        Ok((add, remove))
    }

    /// Move messages; `destination` may be a well-known name (see
    /// [`Engine::resolve_mailbox`]). Returns the real destination mailbox.
    pub async fn move_messages(
        &self,
        account: &Account,
        mailbox: &str,
        uids: &[u32],
        destination: &str,
    ) -> Result<String> {
        let dest = self
            .resolve_mailbox(account, destination)
            .await?;
        self.with_session(account, Some(mailbox), async |s| {
            imap::move_messages(s, uids, &dest).await
        })
        .await?;
        Ok(dest)
    }

    /// Copy messages (on Gmail this adds a label). Returns the real destination mailbox.
    pub async fn copy_messages(
        &self,
        account: &Account,
        mailbox: &str,
        uids: &[u32],
        destination: &str,
    ) -> Result<String> {
        let dest = self
            .resolve_mailbox(account, destination)
            .await?;
        self.with_session(account, Some(mailbox), async |s| {
            Ok(s.uid_copy(imap::uid_set(uids), &dest)
                .await?)
        })
        .await?;
        Ok(dest)
    }

    /// Permanently delete messages (`\Deleted` + expunge of exactly these UIDs).
    pub async fn delete_messages(
        &self,
        account: &Account,
        mailbox: &str,
        uids: &[u32],
    ) -> Result<()> {
        self.with_session(account, Some(mailbox), async |s| {
            imap::store_flags(s, uids, "+", &["\\Deleted".into()]).await?;
            imap::expunge_uids(s, uids).await
        })
        .await
    }

    /// Gmail only: add and remove labels without moving the messages.
    pub async fn gmail_labels(
        &self,
        account: &Account,
        mailbox: &str,
        uids: &[u32],
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        if account.provider != Provider::Gmail {
            bail!("account '{}' is not a Gmail account", account.id);
        }
        let quote_all = |ls: &[String]| -> Result<String> {
            Ok(ls
                .iter()
                .map(|l| imap::quote(l))
                .collect::<Result<Vec<_>>>()?
                .join(" "))
        };
        let add = quote_all(add)?;
        let remove = quote_all(remove)?;
        self.with_session(account, Some(mailbox), async |s| {
            let set = imap::uid_set(uids);
            if !add.is_empty() {
                let _: Vec<_> = s
                    .uid_store(&set, format!("+X-GM-LABELS.SILENT ({add})"))
                    .await?
                    .try_collect()
                    .await?;
            }
            if !remove.is_empty() {
                let _: Vec<_> = s
                    .uid_store(&set, format!("-X-GM-LABELS.SILENT ({remove})"))
                    .await?
                    .try_collect()
                    .await?;
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_criteria_match_everything() {
        let q = SearchCriteria::default()
            .to_query(Provider::Icloud)
            .unwrap();
        assert_eq!(q, "ALL");
    }

    #[test]
    fn criteria_combine_in_a_fixed_order() {
        let c = SearchCriteria {
            from: Some("bob".into()),
            subject: Some(r#"say "hi""#.into()),
            since: Some("2026-02-01".into()),
            unread: Some(true),
            flagged: Some(false),
            raw: Some("LARGER 1000".into()),
            ..Default::default()
        };
        assert_eq!(
            c.to_query(Provider::Custom)
                .unwrap(),
            r#"FROM "bob" SUBJECT "say \"hi\"" SINCE 01-Feb-2026 UNSEEN UNFLAGGED LARGER 1000"#
        );
    }

    #[test]
    fn non_ascii_criteria_declare_utf8() {
        let c = SearchCriteria {
            subject: Some("café".into()),
            ..Default::default()
        };
        assert_eq!(
            c.to_query(Provider::Icloud)
                .unwrap(),
            "CHARSET UTF-8 SUBJECT \"café\""
        );
    }

    #[test]
    fn gmail_query_is_gmail_only() {
        let c = SearchCriteria {
            gmail_query: Some("has:attachment".into()),
            ..Default::default()
        };
        assert_eq!(
            c.to_query(Provider::Gmail)
                .unwrap(),
            "X-GM-RAW \"has:attachment\""
        );
        assert!(
            c.to_query(Provider::Icloud)
                .is_err()
        );
    }

    #[test]
    fn criteria_reject_line_breaks() {
        let raw = SearchCriteria {
            raw: Some("ALL\r\nA1 DELETE INBOX".into()),
            ..Default::default()
        };
        assert!(
            raw.to_query(Provider::Custom)
                .is_err()
        );
        let from = SearchCriteria {
            from: Some("a\nb".into()),
            ..Default::default()
        };
        assert!(
            from.to_query(Provider::Custom)
                .is_err()
        );
    }

    #[test]
    fn imap_dates_use_the_search_format() {
        assert_eq!(imap_date("2026-10-22").unwrap(), "22-Oct-2026");
        assert!(imap_date("22/10/2026").is_err());
        assert!(imap_date("2026-02-30").is_err());
    }

    #[test]
    fn friendly_flag_names_map_to_system_flags() {
        assert_eq!(normalize_flag("read").unwrap(), "\\Seen");
        assert_eq!(normalize_flag("\\Seen").unwrap(), "\\Seen");
        assert_eq!(normalize_flag("Starred").unwrap(), "\\Flagged");
        assert_eq!(normalize_flag("replied").unwrap(), "\\Answered");
        assert_eq!(normalize_flag("$Label1").unwrap(), "$Label1");
    }

    #[test]
    fn invalid_keywords_are_rejected() {
        for bad in ["", "  ", "two words", "a(b", "x\"y", "back\\slash", "star*"] {
            assert!(normalize_flag(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn filenames_cannot_escape_the_directory() {
        assert_eq!(safe_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_filename("C:\\x.txt"), "C__x.txt");
        assert_eq!(safe_filename(".hidden"), "hidden");
        assert_eq!(safe_filename("..."), "attachment");
        assert_eq!(safe_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn unique_path_never_reuses_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_path(dir.path(), "a.txt");
        assert_eq!(
            first,
            dir.path()
                .join("a.txt")
        );
        std::fs::write(&first, b"1").unwrap();
        let second = unique_path(dir.path(), "a.txt");
        assert_eq!(
            second,
            dir.path()
                .join("a (1).txt")
        );
        std::fs::write(&second, b"2").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path()
                .join("a (2).txt")
        );
        std::fs::write(
            dir.path()
                .join("noext"),
            b"",
        )
        .unwrap();
        assert_eq!(
            unique_path(dir.path(), "noext"),
            dir.path()
                .join("noext (1)")
        );
    }

    #[test]
    fn message_view_parses_headers_body_and_attachments() {
        let raw = b"From: Bob <bob@example.com>\r\n\
To: me@me.com\r\n\
Subject: Hello\r\n\
Message-ID: <m1@example.com>\r\n\
Date: Wed, 21 Oct 2026 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain\r\n\
\r\n\
Hi there\r\n\
--b\r\n\
Content-Type: text/csv; name=\"data.csv\"\r\n\
Content-Disposition: attachment; filename=\"data.csv\"\r\n\
\r\n\
a,b\r\n\
--b--\r\n";
        let v = MessageView::parse(raw).unwrap();
        assert_eq!(
            v.subject
                .as_deref(),
            Some("Hello")
        );
        assert_eq!(
            v.message_id
                .as_deref(),
            Some("m1@example.com")
        );
        assert_eq!(v.from, vec!["\"Bob\" <bob@example.com>"]);
        assert_eq!(
            v.text
                .trim(),
            "Hi there"
        );
        assert!(!v.has_html);
        assert_eq!(
            v.attachments
                .len(),
            1
        );
        assert_eq!(v.attachments[0].index, 1);
        assert_eq!(
            v.attachments[0]
                .filename
                .as_deref(),
            Some("data.csv")
        );
        assert_eq!(
            v.attachments[0]
                .content_type
                .as_deref(),
            Some("text/csv")
        );
    }
}
