use anyhow::{Context, Result, anyhow};
use lettre::message::header::ContentType;
use lettre::message::{Attachment as LettreAttachment, Mailbox, MultiPart, SinglePart};
use lettre::Message;
use mail_parser::{Address, MimeHeaders};

use crate::config::Account;

#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

impl Attachment {
    pub fn from_path(path: &str) -> Result<Self> {
        let p = std::path::Path::new(path);
        let data = std::fs::read(p).with_context(|| format!("reading attachment {path}"))?;
        let filename = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        Ok(Self {
            content_type: guess_mime(&filename).into(),
            filename,
            data,
        })
    }
}

fn guess_mime(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ics" => "text/calendar",
        _ => "application/octet-stream",
    }
}

/// Everything needed to build an outgoing message.
#[derive(Debug, Clone, Default)]
pub struct Compose {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

pub fn sender(account: &Account) -> Result<Mailbox> {
    let addr = account
        .email
        .parse()
        .with_context(|| format!("invalid account email {}", account.email))?;
    Ok(Mailbox::new(account.display_name.clone(), addr))
}

fn parse_mailbox(s: &str) -> Result<Mailbox> {
    s.trim()
        .parse()
        .map_err(|e| anyhow!("invalid address '{s}': {e}"))
}

/// Build the message. `keep_bcc` retains the Bcc header (needed for drafts).
pub fn build(account: &Account, c: &Compose, keep_bcc: bool) -> Result<Message> {
    let mut b = Message::builder().from(sender(account)?).subject(&c.subject);
    for a in &c.to {
        b = b.to(parse_mailbox(a)?);
    }
    for a in &c.cc {
        b = b.cc(parse_mailbox(a)?);
    }
    for a in &c.bcc {
        b = b.bcc(parse_mailbox(a)?);
    }
    if keep_bcc {
        b = b.keep_bcc();
    }
    if let Some(id) = &c.in_reply_to {
        b = b.in_reply_to(id.clone());
    }
    if !c.references.is_empty() {
        b = b.references(c.references.join(" "));
    }

    let text = c.text.clone().unwrap_or_default();
    let body = match &c.html {
        Some(html) => MultiPart::alternative_plain_html(text, html.clone()),
        None => MultiPart::mixed().singlepart(SinglePart::plain(text)),
    };
    let body = if c.attachments.is_empty() {
        body
    } else {
        let mut mixed = MultiPart::mixed().multipart(body);
        for a in &c.attachments {
            let ct = ContentType::parse(&a.content_type)
                .unwrap_or_else(|_| ContentType::parse("application/octet-stream").unwrap());
            mixed = mixed.singlepart(LettreAttachment::new(a.filename.clone()).body(a.data.clone(), ct));
        }
        mixed
    };
    Ok(b.multipart(body)?)
}

// ---------- helpers for replies / forwards / drafts ----------

pub fn addresses(a: Option<&Address>) -> Vec<String> {
    a.map(|a| {
        a.iter()
            .filter_map(|addr| {
                let email = addr.address()?;
                Some(match addr.name() {
                    Some(n) if !n.is_empty() => format!("\"{}\" <{email}>", n.replace('"', "")),
                    _ => email.to_owned(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

fn bare_email(s: &str) -> String {
    let s = s.trim();
    match (s.rfind('<'), s.rfind('>')) {
        (Some(l), Some(r)) if l < r => s[l + 1..r].to_ascii_lowercase(),
        _ => s.to_ascii_lowercase(),
    }
}

fn dedupe_excluding(list: Vec<String>, exclude: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = exclude.iter().map(|e| bare_email(e)).collect();
    list.into_iter()
        .filter(|a| {
            let e = bare_email(a);
            if seen.contains(&e) {
                false
            } else {
                seen.push(e);
                true
            }
        })
        .collect()
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let lower = subject.trim_start().to_ascii_lowercase();
    if lower.starts_with(&format!("{}:", prefix.to_ascii_lowercase())) {
        subject.to_owned()
    } else {
        format!("{prefix}: {subject}")
    }
}

pub fn best_text(msg: &mail_parser::Message) -> String {
    if let Some(t) = msg.body_text(0) {
        return t.into_owned();
    }
    msg.body_html(0)
        .map(|h| html_to_text(&h))
        .unwrap_or_default()
}

pub fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100).unwrap_or_else(|_| html.to_owned())
}

fn references_of(msg: &mail_parser::Message) -> Vec<String> {
    msg.references()
        .as_text_list()
        .map(|l| l.iter().map(|r| format!("<{r}>")).collect())
        .or_else(|| msg.references().as_text().map(|r| vec![format!("<{r}>")]))
        .unwrap_or_default()
}

fn reference_chain(orig: &mail_parser::Message) -> (Option<String>, Vec<String>) {
    let id = orig.message_id().map(|i| format!("<{i}>"));
    let mut refs = references_of(orig);
    if let Some(i) = &id {
        refs.push(i.clone());
    }
    (id, refs)
}

fn attribution(orig: &mail_parser::Message) -> String {
    let from = addresses(orig.from()).join(", ");
    match orig.date() {
        Some(d) => format!("On {}, {from} wrote:", d.to_rfc822()),
        None => format!("{from} wrote:"),
    }
}

pub fn reply(
    account: &Account,
    orig: &mail_parser::Message,
    body: &str,
    html: Option<String>,
    reply_all: bool,
) -> Compose {
    let me = vec![account.email.clone()];
    let to = {
        let rt = addresses(orig.reply_to());
        if rt.is_empty() { addresses(orig.from()) } else { rt }
    };
    let (to, cc) = if reply_all {
        let to = dedupe_excluding(to, &me);
        let mut exclude = me.clone();
        exclude.extend(to.iter().cloned());
        let mut others = addresses(orig.to());
        others.extend(addresses(orig.cc()));
        (to, dedupe_excluding(others, &exclude))
    } else {
        (to, vec![])
    };

    let quoted: String = best_text(orig)
        .lines()
        .map(|l| format!("> {l}\n"))
        .collect();
    let (in_reply_to, references) = reference_chain(orig);
    Compose {
        to,
        cc,
        subject: prefixed("Re", orig.subject().unwrap_or("")),
        text: Some(format!("{body}\n\n{}\n{quoted}", attribution(orig))),
        html,
        in_reply_to,
        references,
        ..Default::default()
    }
}

pub fn forward(orig: &mail_parser::Message, to: Vec<String>, body: &str, include_attachments: bool) -> Compose {
    let header = format!(
        "---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n{}",
        addresses(orig.from()).join(", "),
        orig.date().map(|d| d.to_rfc822()).unwrap_or_default(),
        orig.subject().unwrap_or(""),
        addresses(orig.to()).join(", "),
        {
            let cc = addresses(orig.cc());
            if cc.is_empty() { String::new() } else { format!("Cc: {}\n", cc.join(", ")) }
        }
    );
    let attachments = if include_attachments { attachments_of(orig) } else { vec![] };
    let (_, references) = reference_chain(orig);
    Compose {
        to,
        subject: prefixed("Fwd", orig.subject().unwrap_or("")),
        text: Some(format!("{body}\n\n{header}\n{}", best_text(orig))),
        attachments,
        references,
        ..Default::default()
    }
}

pub fn attachments_of(msg: &mail_parser::Message) -> Vec<Attachment> {
    msg.attachments()
        .enumerate()
        .map(|(i, p)| Attachment {
            filename: p
                .attachment_name()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("attachment-{}", i + 1)),
            content_type: p
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(st) => format!("{}/{st}", ct.ctype()),
                    None => ct.ctype().to_owned(),
                })
                .unwrap_or_else(|| "application/octet-stream".into()),
            data: p.contents().to_vec(),
        })
        .collect()
}

/// Reconstruct a Compose from a stored draft so it can be sent or edited.
pub fn from_draft(msg: &mail_parser::Message) -> Compose {
    let has_html = msg
        .html_part(0)
        .is_some_and(|p| p.is_content_type("text", "html"));
    Compose {
        to: addresses(msg.to()),
        cc: addresses(msg.cc()),
        bcc: addresses(msg.bcc()),
        subject: msg.subject().unwrap_or("").to_owned(),
        text: msg.body_text(0).map(|t| t.into_owned()),
        html: if has_html { msg.body_html(0).map(|h| h.into_owned()) } else { None },
        attachments: attachments_of(msg),
        in_reply_to: msg.in_reply_to().as_text().map(|i| format!("<{i}>")),
        references: references_of(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthMethod, Provider, SmtpSecurity};
    use mail_parser::MessageParser;

    fn account() -> Account {
        Account {
            id: "me".into(),
            email: "me@me.com".into(),
            display_name: Some("Me".into()),
            provider: Provider::Icloud,
            auth: AuthMethod::Password,
            username: None,
            imap_host: "imap.mail.me.com".into(),
            imap_port: 993,
            smtp_host: "smtp.mail.me.com".into(),
            smtp_port: 587,
            smtp_security: SmtpSecurity::Starttls,
            save_sent: true,
            remove_by: Default::default(),
            trash_mailbox: None,
        }
    }

    const ORIGINAL: &[u8] = b"From: Bob <bob@example.com>\r\n\
To: Me <ME@me.com>, carol@example.com\r\n\
Cc: dave@example.com, bob@example.com\r\n\
Subject: Lunch\r\n\
Date: Tue, 20 Oct 2026 12:00:00 +0000\r\n\
Message-ID: <msg2@example.com>\r\n\
References: <msg1@example.com>\r\n\
\r\n\
Tomorrow?\r\nAt noon.\r\n";

    fn emails(list: &[String]) -> Vec<String> {
        list.iter().map(|a| bare_email(a)).collect()
    }

    #[test]
    fn reply_goes_to_sender_and_threads() {
        let orig = MessageParser::default().parse(ORIGINAL).unwrap();
        let c = reply(&account(), &orig, "Sure", None, false);
        assert_eq!(emails(&c.to), ["bob@example.com"]);
        assert!(c.cc.is_empty());
        assert_eq!(c.subject, "Re: Lunch");
        assert_eq!(c.in_reply_to.as_deref(), Some("<msg2@example.com>"));
        assert_eq!(c.references, ["<msg1@example.com>", "<msg2@example.com>"]);
        let text = c.text.unwrap();
        assert!(text.starts_with("Sure\n"));
        assert!(text.contains("> Tomorrow?\n> At noon."));
    }

    #[test]
    fn reply_all_excludes_self_and_duplicates() {
        let orig = MessageParser::default().parse(ORIGINAL).unwrap();
        let c = reply(&account(), &orig, "Sure", None, true);
        assert_eq!(emails(&c.to), ["bob@example.com"]);
        // Me (case-insensitively) and Bob (already in To) are dropped from Cc.
        assert_eq!(emails(&c.cc), ["carol@example.com", "dave@example.com"]);
    }

    #[test]
    fn subject_prefix_not_doubled() {
        assert_eq!(prefixed("Re", "RE: Lunch"), "RE: Lunch");
        assert_eq!(prefixed("Fwd", "Lunch"), "Fwd: Lunch");
    }

    #[test]
    fn forward_keeps_original_and_attachments() {
        let raw = b"From: bob@example.com\r\nTo: me@me.com\r\nSubject: Report\r\n\
MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=XX\r\n\r\n\
--XX\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
--XX\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=r.pdf\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n--XX--\r\n";
        let orig = MessageParser::default().parse(&raw[..]).unwrap();
        let c = forward(&orig, vec!["eve@example.com".into()], "FYI", true);
        assert_eq!(c.subject, "Fwd: Report");
        assert!(c.text.as_ref().unwrap().contains("See attached."));
        assert_eq!(c.attachments.len(), 1);
        assert_eq!(c.attachments[0].filename, "r.pdf");
        assert_eq!(c.attachments[0].data, b"%PDF");
    }

    #[test]
    fn draft_round_trip_keeps_bcc_and_threading() {
        let c = Compose {
            to: vec!["Bob <bob@example.com>".into()],
            bcc: vec!["secret@example.com".into()],
            subject: "Hi".into(),
            text: Some("Body".into()),
            in_reply_to: Some("<m1@example.com>".into()),
            references: vec!["<m1@example.com>".into()],
            attachments: vec![Attachment {
                filename: "a.txt".into(),
                content_type: "text/plain".into(),
                data: b"hello".to_vec(),
            }],
            ..Default::default()
        };
        let raw = build(&account(), &c, true).unwrap().formatted();
        let parsed = MessageParser::default().parse(&raw).unwrap();
        let back = from_draft(&parsed);
        assert_eq!(emails(&back.to), ["bob@example.com"]);
        assert_eq!(emails(&back.bcc), ["secret@example.com"]);
        assert_eq!(back.subject, "Hi");
        assert_eq!(back.text.as_deref().map(str::trim), Some("Body"));
        assert_eq!(back.html, None);
        assert_eq!(back.in_reply_to.as_deref(), Some("<m1@example.com>"));
        assert_eq!(back.attachments.len(), 1);
        assert_eq!(back.attachments[0].data, b"hello");
    }

    #[test]
    fn sent_message_omits_bcc_header() {
        let c = Compose {
            to: vec!["bob@example.com".into()],
            bcc: vec!["secret@example.com".into()],
            subject: "Hi".into(),
            text: Some("Body".into()),
            ..Default::default()
        };
        let msg = build(&account(), &c, false).unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(!raw.contains("secret@example.com"));
        // ...but the envelope still delivers to the Bcc recipient.
        assert!(msg.envelope().to().iter().any(|a| a.to_string() == "secret@example.com"));
    }
}
