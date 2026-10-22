use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use mail_parser::{MessageParser, MimeHeaders};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{App, LoginState};
use crate::compose::{self, Attachment, Compose};
use crate::config::{
    Account, AuthMethod, GoogleOauthClient, Provider, RemoveBy, SmtpSecurity, guess_provider,
    provider_defaults, validate_id,
};
use crate::imap::{self, SpecialUse};
use crate::oauth;
use crate::secrets::{self, SecretKind};
use crate::smtp;

type ToolResult = Result<String, String>;

fn done(v: Value) -> ToolResult {
    Ok(serde_json::to_string_pretty(&v).unwrap())
}

fn fail(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[derive(Clone)]
pub struct EmailServer {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
}

impl EmailServer {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }

    pub fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router
            .list_all()
    }
}

// ---------- parameter types ----------

#[derive(Deserialize, JsonSchema)]
pub struct RequiredAccountArg {
    /// Account id or email address.
    pub account: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct AddAccountArgs {
    /// Email address of the account.
    pub email: String,
    /// Short id used to refer to the account (letters, digits, '-', '_'). Defaults to the part before '@'.
    pub id: Option<String>,
    /// Name shown in the From header.
    pub display_name: Option<String>,
    /// Provider; guessed from the email domain when omitted (me.com/icloud.com → icloud, gmail.com → gmail).
    pub provider: Option<Provider>,
    /// Authentication method. Defaults to password (an app-specific password for iCloud/Gmail).
    pub auth: Option<AuthMethod>,
    /// Login name if different from the email address.
    pub username: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SmtpSecurity>,
    /// Save a copy of sent mail to the Sent mailbox (defaults per provider).
    pub save_sent: Option<bool>,
    /// How to remove messages from this account: move_to_trash (recoverable) or delete (permanent).
    /// This is guidance returned by list_accounts; the tools themselves never apply it.
    pub remove_by: Option<RemoveBy>,
    /// 0-based position in the account list; 0 makes it the default. Defaults to the end.
    pub position: Option<usize>,
    /// Password to store and verify immediately (same as calling login afterwards).
    pub password: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateAccountArgs {
    /// Account id or email address.
    pub account: String,
    pub display_name: Option<String>,
    pub username: Option<String>,
    pub auth: Option<AuthMethod>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SmtpSecurity>,
    pub save_sent: Option<bool>,
    /// How to remove messages from this account: move_to_trash (recoverable) or delete (permanent).
    /// This is guidance returned by list_accounts; the tools themselves never apply it.
    pub remove_by: Option<RemoveBy>,
    /// Mailbox that "trash" refers to, instead of the one the server marks as Trash.
    /// Must already exist. Empty string reverts to the server's choice.
    pub trash_mailbox: Option<String>,
    /// New 0-based position in the account list; 0 makes it the default.
    pub position: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LoginArgs {
    /// Account id or email address.
    pub account: String,
    /// For password accounts: the (app-specific) password. Ignored for OAuth accounts.
    pub password: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StatusArgs {
    /// Account id or email. Omit for all accounts.
    pub account: Option<String>,
    /// Also connect to IMAP and SMTP to verify the credentials work.
    #[serde(default)]
    pub test_connection: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct OauthClientArgs {
    /// OAuth client id of a Google Cloud "Desktop app" client.
    pub client_id: String,
    /// The matching client secret.
    pub client_secret: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListMailboxesArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    /// Include total/unread counts (one extra request per mailbox).
    #[serde(default)]
    pub with_counts: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct MailboxArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    /// Mailbox name, e.g. "Archive/2026".
    pub name: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RenameMailboxArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    pub name: String,
    pub new_name: String,
}

fn inbox() -> String {
    "INBOX".into()
}
fn default_limit() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
pub struct ListMessagesArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    /// Mailbox to list (default INBOX).
    #[serde(default = "inbox")]
    pub mailbox: String,
    /// Maximum number of messages to return, newest first (default 20).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Number of newest messages to skip, for paging.
    #[serde(default)]
    pub offset: usize,
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    /// Mailbox to search (default INBOX). For Gmail, "[Gmail]/All Mail" searches everything.
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    /// Matches anywhere in headers or body.
    pub text: Option<String>,
    /// Matches in the body only.
    pub body: Option<String>,
    /// Messages on or after this date (YYYY-MM-DD).
    pub since: Option<String>,
    /// Messages before this date (YYYY-MM-DD).
    pub before: Option<String>,
    /// true = only unread, false = only read.
    pub unread: Option<bool>,
    /// true = only flagged/starred, false = only unflagged.
    pub flagged: Option<bool>,
    /// Gmail accounts only: a Gmail search query, e.g. "has:attachment from:bob newer_than:7d".
    pub gmail_query: Option<String>,
    /// Raw IMAP SEARCH criteria, ANDed with the other filters.
    pub raw: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BodyFormat {
    /// Plain text (HTML converted to text if there is no text part).
    #[default]
    Text,
    /// The HTML part, if any.
    Html,
    /// The full raw RFC 5322 source.
    Raw,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadMessageArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    /// Message UID (from list_messages or search_messages).
    pub uid: u32,
    #[serde(default)]
    pub format: BodyFormat,
    /// Mark the message as read.
    #[serde(default)]
    pub mark_read: bool,
    /// Truncate the body to this many characters (default 20000).
    pub max_chars: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SaveAttachmentArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub uid: u32,
    /// 1-based attachment index as shown by read_message. Omit to save all attachments.
    pub index: Option<usize>,
    /// Directory to save into (default: the Downloads folder).
    pub directory: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    /// Recipients, e.g. "bob@example.com" or "Bob <bob@example.com>".
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    /// Plain-text body.
    pub body: String,
    /// Optional HTML alternative body.
    pub html: Option<String>,
    /// Local file paths to attach.
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DraftArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    pub html: Option<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
    /// UID of an existing draft (in the Drafts mailbox) to replace with this content.
    pub replace_uid: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReplyArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    /// UID of the message being replied to.
    pub uid: u32,
    /// Reply text; the original is quoted below it.
    pub body: String,
    pub html: Option<String>,
    /// Reply to all recipients, not just the sender.
    #[serde(default)]
    pub reply_all: bool,
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Save as a draft instead of sending.
    #[serde(default)]
    pub draft_only: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, JsonSchema)]
pub struct ForwardArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub uid: u32,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    /// Text to put above the forwarded message.
    #[serde(default)]
    pub body: String,
    /// Include the original's attachments (default true).
    #[serde(default = "yes")]
    pub include_attachments: bool,
    /// Save as a draft instead of sending.
    #[serde(default)]
    pub draft_only: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendDraftArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    /// UID of the draft in the Drafts mailbox.
    pub uid: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct FlagArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub uids: Vec<u32>,
    /// Flags to set: "seen" (read), "flagged" (starred), "answered", "draft", or any custom keyword.
    #[serde(default)]
    pub add: Vec<String>,
    /// Flags to clear, same names as `add`.
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MoveArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub uids: Vec<u32>,
    /// Destination mailbox. Special names "archive", "trash", "junk" resolve to the account's folders.
    pub destination: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeleteArgs {
    /// Account id or email address. Omit to use the default (first) account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub uids: Vec<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LabelArgs {
    /// Account id or email address (must be a Gmail account). Omit for the default account.
    pub account: Option<String>,
    #[serde(default = "inbox")]
    pub mailbox: String,
    pub uids: Vec<u32>,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

// ---------- helpers ----------

fn account_json(a: &Account, index: usize) -> Value {
    json!({
        "id": a.id,
        "email": a.email,
        "display_name": a.display_name,
        "provider": a.provider,
        "auth": a.auth,
        "default": index == 0,
        "imap": format!("{}:{}", a.imap_host, a.imap_port),
        "smtp": format!("{}:{} ({:?})", a.smtp_host, a.smtp_port, a.smtp_security),
        "save_sent": a.save_sent,
        "remove_by": a.remove_by,
        "trash_mailbox": a.trash_mailbox,
        "remove_messages": remove_guidance(a),
    })
}

/// How the agent should remove messages from this account. The tools don't apply
/// `remove_by` themselves; this text tells the agent which call to make.
fn remove_guidance(a: &Account) -> String {
    let trash = a
        .trash_mailbox
        .as_deref()
        .unwrap_or("trash");
    match a.remove_by {
        RemoveBy::MoveToTrash => format!(
            "Remove messages with move_messages destination=\"{trash}\". \
             Use delete_messages only if the user explicitly asks to delete permanently."
        ),
        RemoveBy::Delete => format!(
            "Remove messages with delete_messages (permanent). \
             Use move_messages destination=\"{trash}\" if the user asks to keep a recoverable copy."
        ),
    }
}

fn summary_json(s: &imap::Summary) -> Value {
    let parsed = MessageParser::default().parse(&s.headers);
    let get = |f: fn(&mail_parser::Message) -> Vec<String>| {
        parsed
            .as_ref()
            .map(f)
            .unwrap_or_default()
    };
    json!({
        "uid": s.uid,
        "from": get(|m| compose::addresses(m.from())),
        "to": get(|m| compose::addresses(m.to())),
        "subject": parsed.as_ref().and_then(|m| m.subject()).unwrap_or(""),
        "date": parsed.as_ref().and_then(|m| m.date()).map(|d| d.to_rfc3339()).or(s.internal_date.clone()),
        "unread": !s.flags.iter().any(|f| f == "\\Seen"),
        "flagged": s.flags.iter().any(|f| f == "\\Flagged"),
        "flags": s.flags,
        "size": s.size,
    })
}

fn imap_flag(name: &str) -> Result<String> {
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

fn imap_date(s: &str) -> Result<String> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow!("invalid date '{s}', expected YYYY-MM-DD"))?;
    Ok(d.format("%d-%b-%Y")
        .to_string())
}

fn load_attachments(paths: &[String]) -> Result<Vec<Attachment>> {
    paths
        .iter()
        .map(|p| Attachment::from_path(p))
        .collect()
}

fn truncate(s: String, max: usize) -> (String, bool) {
    if s.chars()
        .count()
        <= max
    {
        (s, false)
    } else {
        (
            s.chars()
                .take(max)
                .collect(),
            true,
        )
    }
}

fn safe_filename(name: &str) -> String {
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

fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let mut path = dir.join(name);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_owned(), format!(".{e}")),
        None => (name.to_owned(), String::new()),
    };
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    path
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

impl EmailServer {
    async fn resolve_mailbox(&self, account: &Account, name: &str) -> Result<String> {
        let kind = match name
            .to_ascii_lowercase()
            .as_str()
        {
            "archive" => SpecialUse::Archive,
            "trash" => SpecialUse::Trash,
            "junk" | "spam" => SpecialUse::Junk,
            "sent" => SpecialUse::Sent,
            "drafts" => SpecialUse::Drafts,
            _ => return Ok(name.to_owned()),
        };
        if kind == SpecialUse::Trash
            && let Some(t) = &account.trash_mailbox
        {
            return Ok(t.clone());
        }
        self.app
            .with_session(account, None, async |s| {
                // Prefer an exact mailbox name match before special-use resolution.
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

    async fn fetch_parsed(&self, account: &Account, mailbox: &str, uid: u32) -> Result<Vec<u8>> {
        self.app
            .with_session(account, Some(mailbox), async |s| {
                Ok(imap::fetch_raw(s, uid)
                    .await?
                    .0)
            })
            .await
    }

    /// Send a composed message, file a copy in Sent if the provider doesn't, and
    /// optionally mark the message being replied to as answered.
    async fn send_compose(
        &self,
        account: &Account,
        c: &Compose,
        answered: Option<(&str, u32)>,
    ) -> Result<Value> {
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
            .app
            .credential(account)
            .await?;
        smtp::send(account, &cred, msg.envelope(), &raw).await?;

        let mut notes = vec![];
        if account.save_sent {
            let r = self
                .app
                .with_session(account, None, async |s| {
                    let sent = imap::find_special(s, SpecialUse::Sent).await?;
                    s.append(&sent, Some("(\\Seen)"), None, &raw)
                        .await?;
                    Ok(sent)
                })
                .await;
            match r {
                Ok(sent) => notes.push(format!("copy saved to {sent}")),
                Err(e) => notes.push(format!("sent, but saving a copy to Sent failed: {e:#}")),
            }
        }
        if let Some((mailbox, uid)) = answered {
            let _ = self
                .app
                .with_session(account, Some(mailbox), async |s| {
                    imap::store_flags(s, &[uid], "+", &["\\Answered".into()]).await
                })
                .await;
        }
        Ok(json!({
            "sent": true,
            "account": account.id,
            "to": c.to, "cc": c.cc, "bcc": c.bcc,
            "subject": c.subject,
            "notes": notes,
        }))
    }

    async fn save_draft_compose(
        &self,
        account: &Account,
        c: &Compose,
        replace_uid: Option<u32>,
    ) -> Result<Value> {
        let raw = compose::build(account, c, true)?.formatted();
        self.app
            .with_session(account, None, async |s| {
                let drafts = imap::find_special(s, SpecialUse::Drafts).await?;
                s.append(&drafts, Some("(\\Draft \\Seen)"), None, &raw)
                    .await?;
                if let Some(old) = replace_uid {
                    s.select(&drafts)
                        .await?;
                    imap::store_flags(s, &[old], "+", &["\\Deleted".into()]).await?;
                    imap::expunge_uids(s, &[old]).await?;
                }
                Ok(json!({
                    "saved": true,
                    "mailbox": drafts,
                    "replaced_uid": replace_uid,
                    "subject": c.subject,
                }))
            })
            .await
    }

    async fn verify(&self, account: &Account) -> Value {
        let imap = match self
            .app
            .with_session(account, None, async |s| {
                Ok(s.noop()
                    .await?)
            })
            .await
        {
            Ok(()) => json!("ok"),
            Err(e) => json!(format!("error: {e:#}")),
        };
        let smtp = match self
            .app
            .credential(account)
            .await
        {
            Ok(cred) => match smtp::test(account, &cred).await {
                Ok(()) => json!("ok"),
                Err(e) => json!(format!("error: {e:#}")),
            },
            Err(e) => json!(format!("error: {e:#}")),
        };
        json!({ "imap": imap, "smtp": smtp })
    }
}

// ---------- tools ----------

#[tool_router]
impl EmailServer {
    // ----- account management -----

    #[tool(
        description = "List configured email accounts. The first account is the default used when a tool's `account` is omitted."
    )]
    async fn list_accounts(&self) -> ToolResult {
        let cfg = self
            .app
            .config
            .read()
            .await;
        let accounts: Vec<_> = cfg
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| account_json(a, i))
            .collect();
        done(json!({
            "accounts": accounts,
            "google_oauth_client_configured": cfg.google_oauth.is_some(),
            "secret_store": cfg.secret_store,
        }))
    }

    #[tool(
        description = "Add an email account. Server settings are filled in automatically for iCloud (me.com/mac.com/icloud.com) and Gmail. \
        iCloud requires an app-specific password (appleid.apple.com → Sign-In and Security → App-Specific Passwords). \
        Gmail can use an app password (auth=password) or OAuth (auth=oauth2, requires set_google_oauth_client). \
        Pass `password` to log in immediately, or call `login` afterwards."
    )]
    async fn add_account(&self, Parameters(a): Parameters<AddAccountArgs>) -> ToolResult {
        let email = a
            .email
            .trim()
            .to_owned();
        if !email.contains('@') {
            return Err(format!("'{email}' is not an email address"));
        }
        let provider = a
            .provider
            .unwrap_or_else(|| guess_provider(&email));
        let defaults = provider_defaults(provider);
        let id =
            a.id.unwrap_or_else(|| {
                email
                    .split('@')
                    .next()
                    .unwrap()
                    .to_ascii_lowercase()
                    .replace('.', "-")
            });
        validate_id(&id).map_err(fail)?;
        let auth = a
            .auth
            .or(defaults
                .as_ref()
                .map(|d| d.auth))
            .unwrap_or(AuthMethod::Password);
        if auth == AuthMethod::Oauth2 && provider != Provider::Gmail {
            return Err("OAuth2 is only supported for Gmail accounts".into());
        }
        let need = |v: Option<String>, d: Option<&'static str>, what: &str| {
            v.or(d.map(str::to_owned))
                .ok_or_else(|| format!("{what} is required for custom providers"))
        };
        let account = Account {
            id: id.clone(),
            email: email.clone(),
            display_name: a.display_name,
            provider,
            auth,
            username: a.username,
            imap_host: need(
                a.imap_host,
                defaults
                    .as_ref()
                    .map(|d| d.imap_host),
                "imap_host",
            )?,
            imap_port: a
                .imap_port
                .or(defaults
                    .as_ref()
                    .map(|d| d.imap_port))
                .unwrap_or(993),
            smtp_host: need(
                a.smtp_host,
                defaults
                    .as_ref()
                    .map(|d| d.smtp_host),
                "smtp_host",
            )?,
            smtp_port: a
                .smtp_port
                .or(defaults
                    .as_ref()
                    .map(|d| d.smtp_port))
                .unwrap_or(587),
            smtp_security: a
                .smtp_security
                .or(defaults
                    .as_ref()
                    .map(|d| d.smtp_security))
                .unwrap_or(SmtpSecurity::Starttls),
            save_sent: a
                .save_sent
                .or(defaults
                    .as_ref()
                    .map(|d| d.save_sent))
                .unwrap_or(true),
            remove_by: a
                .remove_by
                .unwrap_or_default(),
            trash_mailbox: None,
        };

        let pos = {
            let mut cfg = self
                .app
                .config
                .write()
                .await;
            if cfg
                .accounts
                .iter()
                .any(|x| {
                    x.id == id
                        || x.email
                            .eq_ignore_ascii_case(&email)
                })
            {
                return Err(format!(
                    "an account with id '{id}' or email '{email}' already exists"
                ));
            }
            let pos = a
                .position
                .unwrap_or(
                    cfg.accounts
                        .len(),
                )
                .min(
                    cfg.accounts
                        .len(),
                );
            cfg.accounts
                .insert(pos, account.clone());
            cfg.save()
                .map_err(fail)?;
            pos
        };

        let mut result = json!({ "added": account_json(&account, pos) });
        if let Some(pw) = a.password {
            result["login"] = match self
                .login(Parameters(LoginArgs {
                    account: id,
                    password: Some(pw),
                }))
                .await
            {
                Ok(s) => serde_json::from_str(&s).unwrap_or(json!(s)),
                Err(e) => json!(format!("error: {e}")),
            };
        } else {
            result["next_step"] = json!("call login to authenticate this account");
        }
        done(result)
    }

    #[tool(
        description = "Change an account's settings or its position in the list (position 0 = default account)."
    )]
    async fn update_account(&self, Parameters(u): Parameters<UpdateAccountArgs>) -> ToolResult {
        if let Some(t) = u
            .trash_mailbox
            .as_deref()
            .filter(|t| !t.is_empty())
        {
            let current = self
                .app
                .account(Some(&u.account))
                .await
                .map_err(fail)?;
            let boxes = self
                .app
                .with_session(&current, None, async |s| imap::list_mailboxes(s).await)
                .await
                .map_err(fail)?;
            if !boxes
                .iter()
                .any(|b| b.name == t && b.selectable)
            {
                return Err(format!(
                    "no mailbox named '{t}' on account '{}'",
                    current.id
                ));
            }
        }
        let (account, changed_auth) = {
            let mut cfg = self
                .app
                .config
                .write()
                .await;
            let idx = cfg
                .index_of(&u.account)
                .map_err(fail)?;
            let mut a = cfg
                .accounts
                .remove(idx);
            let old_auth = a.auth;
            if let Some(v) = u.display_name {
                a.display_name = Some(v).filter(|s| !s.is_empty());
            }
            if let Some(v) = u.username {
                a.username = Some(v).filter(|s| !s.is_empty());
            }
            if let Some(v) = u.auth {
                if v == AuthMethod::Oauth2 && a.provider != Provider::Gmail {
                    cfg.accounts
                        .insert(idx, a);
                    return Err("OAuth2 is only supported for Gmail accounts".into());
                }
                a.auth = v;
            }
            if let Some(v) = u.imap_host {
                a.imap_host = v;
            }
            if let Some(v) = u.imap_port {
                a.imap_port = v;
            }
            if let Some(v) = u.smtp_host {
                a.smtp_host = v;
            }
            if let Some(v) = u.smtp_port {
                a.smtp_port = v;
            }
            if let Some(v) = u.smtp_security {
                a.smtp_security = v;
            }
            if let Some(v) = u.save_sent {
                a.save_sent = v;
            }
            if let Some(v) = u.remove_by {
                a.remove_by = v;
            }
            if let Some(v) = u.trash_mailbox {
                a.trash_mailbox = Some(v).filter(|s| !s.is_empty());
            }
            let pos = u
                .position
                .unwrap_or(idx)
                .min(
                    cfg.accounts
                        .len(),
                );
            cfg.accounts
                .insert(pos, a.clone());
            cfg.save()
                .map_err(fail)?;
            (
                a,
                old_auth
                    != u.auth
                        .unwrap_or(old_auth),
            )
        };
        self.app
            .forget(&account.id)
            .await;
        let idx = self
            .app
            .config
            .read()
            .await
            .index_of(&account.id)
            .map_err(fail)?;
        let mut out = json!({ "updated": account_json(&account, idx) });
        if changed_auth {
            out["next_step"] = json!("authentication method changed; call login");
        }
        done(out)
    }

    #[tool(
        description = "Remove an account and delete its stored credentials. Does not touch any mail on the server."
    )]
    async fn remove_account(
        &self,
        Parameters(RequiredAccountArg { account }): Parameters<RequiredAccountArg>,
    ) -> ToolResult {
        let (removed, store) = {
            let mut cfg = self
                .app
                .config
                .write()
                .await;
            let idx = cfg
                .index_of(&account)
                .map_err(fail)?;
            let removed = cfg
                .accounts
                .remove(idx);
            cfg.save()
                .map_err(fail)?;
            (removed, cfg.secret_store)
        };
        self.app
            .forget(&removed.id)
            .await;
        secrets::delete_all(store, &removed.id)
            .await
            .map_err(fail)?;
        done(json!({ "removed": removed.id, "email": removed.email }))
    }

    #[tool(
        description = "Log in to an account. For password accounts, pass the (app-specific) password; it is verified against IMAP and then stored in the OS keyring. \
        For Gmail OAuth accounts, this returns a Google sign-in URL (and tries to open it in the browser); finish signing in there, then check account_status."
    )]
    async fn login(&self, Parameters(l): Parameters<LoginArgs>) -> ToolResult {
        let account = self
            .app
            .account(Some(&l.account))
            .await
            .map_err(fail)?;
        let store = self
            .app
            .config
            .read()
            .await
            .secret_store;
        self.app
            .forget(&account.id)
            .await;

        match account.auth {
            AuthMethod::Password => {
                let pw = l
                    .password
                    .filter(|p| !p.is_empty())
                    .ok_or("password is required for this account")?;
                // Verify before storing so a typo doesn't replace a working password.
                let mut session = imap::connect(&account, &imap::Credential::Password(pw.clone()))
                    .await
                    .map_err(|e| {
                        let hint = match account.provider {
                            Provider::Icloud => " (iCloud requires an app-specific password, not your Apple ID password)",
                            Provider::Gmail => " (Gmail requires an app password with 2-Step Verification enabled, or use auth=oauth2)",
                            Provider::Custom => "",
                        };
                        format!("{e:#}{hint}")
                    })?;
                let _ = session
                    .logout()
                    .await;
                secrets::set(store, &account.id, SecretKind::Password, pw)
                    .await
                    .map_err(fail)?;
                done(json!({ "account": account.id, "logged_in": true }))
            }
            AuthMethod::Oauth2 => {
                let client = self
                    .app
                    .oauth_client()
                    .await
                    .map_err(fail)?;
                let pending = oauth::begin_login(&client, &account.email)
                    .await
                    .map_err(fail)?;
                let url = pending
                    .auth_url
                    .clone();
                self.app
                    .logins
                    .lock()
                    .await
                    .insert(
                        account
                            .id
                            .clone(),
                        LoginState::Waiting {
                            auth_url: url.clone(),
                        },
                    );

                let app = self
                    .app
                    .clone();
                let id = account
                    .id
                    .clone();
                tokio::spawn(async move {
                    let outcome = async {
                        let (refresh, access) = pending
                            .complete(&client)
                            .await?;
                        secrets::set(store, &id, SecretKind::OauthRefreshToken, refresh).await?;
                        app.cache_token(&id, access)
                            .await;
                        anyhow::Ok(())
                    }
                    .await;
                    let mut logins = app
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

                open_browser(&url);
                done(json!({
                    "account": account.id,
                    "status": "waiting_for_browser_sign_in",
                    "auth_url": url,
                    "instructions": format!(
                        "Open the URL and sign in as {}. The sign-in completes automatically; it expires in {} minutes. Then call account_status.",
                        account.email, oauth::LOGIN_TIMEOUT.as_secs() / 60
                    ),
                }))
            }
        }
    }

    #[tool(
        description = "Log out of an account: closes connections and deletes its stored password/tokens. The account stays configured."
    )]
    async fn logout(
        &self,
        Parameters(RequiredAccountArg { account }): Parameters<RequiredAccountArg>,
    ) -> ToolResult {
        let account = self
            .app
            .account(Some(&account))
            .await
            .map_err(fail)?;
        let store = self
            .app
            .config
            .read()
            .await
            .secret_store;
        self.app
            .forget(&account.id)
            .await;
        secrets::delete_all(store, &account.id)
            .await
            .map_err(fail)?;
        done(json!({ "account": account.id, "logged_in": false }))
    }

    #[tool(
        description = "Show whether accounts are logged in, any pending OAuth sign-in, and optionally test IMAP/SMTP connectivity."
    )]
    async fn account_status(&self, Parameters(a): Parameters<StatusArgs>) -> ToolResult {
        let (accounts, store) = {
            let cfg = self
                .app
                .config
                .read()
                .await;
            let list = match &a.account {
                Some(k) => vec![
                    cfg.find(Some(k))
                        .map_err(fail)?
                        .clone(),
                ],
                None => cfg
                    .accounts
                    .clone(),
            };
            (list, cfg.secret_store)
        };
        let mut out = vec![];
        for acct in accounts {
            let kind = match acct.auth {
                AuthMethod::Password => SecretKind::Password,
                AuthMethod::Oauth2 => SecretKind::OauthRefreshToken,
            };
            let has_secret = secrets::get(store, &acct.id, kind).await;
            let mut v = json!({
                "account": acct.id,
                "email": acct.email,
                "auth": acct.auth,
                "logged_in": match &has_secret { Ok(s) => json!(s.is_some()), Err(e) => json!(format!("error reading secret store: {e:#}")) },
            });
            if let Some(state) = self
                .app
                .logins
                .lock()
                .await
                .get(&acct.id)
            {
                v["pending_login"] = match state {
                    LoginState::Waiting { auth_url } => {
                        json!({ "status": "waiting", "auth_url": auth_url })
                    }
                    LoginState::Failed(e) => json!({ "status": "failed", "error": e }),
                };
            }
            if a.test_connection && matches!(has_secret, Ok(Some(_))) {
                v["connection"] = self
                    .verify(&acct)
                    .await;
            }
            out.push(v);
        }
        done(json!(out))
    }

    #[tool(
        description = "Configure the Google OAuth client used for Gmail accounts with auth=oauth2. Create it in Google Cloud Console: \
        enable the Gmail API, configure the OAuth consent screen (add yourself as a test user), then create an OAuth client ID of type 'Desktop app'."
    )]
    async fn set_google_oauth_client(
        &self,
        Parameters(c): Parameters<OauthClientArgs>,
    ) -> ToolResult {
        let mut cfg = self
            .app
            .config
            .write()
            .await;
        cfg.google_oauth = Some(GoogleOauthClient {
            client_id: c
                .client_id
                .trim()
                .to_owned(),
            client_secret: c
                .client_secret
                .trim()
                .to_owned(),
        });
        cfg.save()
            .map_err(fail)?;
        done(json!({ "google_oauth_client_configured": true }))
    }

    // ----- mailboxes -----

    #[tool(
        description = "List mailboxes (folders; Gmail labels appear as mailboxes) with their special use (sent, drafts, trash, junk, archive)."
    )]
    async fn list_mailboxes(&self, Parameters(a): Parameters<ListMailboxesArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, None, async |s| {
                let boxes = imap::list_mailboxes(s).await?;
                let mut out = vec![];
                for b in boxes {
                    let mut v = json!({
                        "name": b.name,
                        "special_use": b.special.map(|k| format!("{k:?}").to_lowercase()),
                    });
                    if !b.selectable {
                        v["selectable"] = json!(false);
                    } else if a.with_counts
                        && let Ok(st) = s
                            .status(&b.name, "(MESSAGES UNSEEN)")
                            .await
                    {
                        v["messages"] = json!(st.exists);
                        v["unread"] = json!(st.unseen);
                    }
                    out.push(v);
                }
                Ok(json!({ "account": account.id, "mailboxes": out }))
            })
            .await
            .map_err(fail)
            .and_then(done)
    }

    #[tool(
        description = "Create a mailbox (folder). Use the server's hierarchy delimiter (usually '/') for nesting."
    )]
    async fn create_mailbox(&self, Parameters(a): Parameters<MailboxArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, None, async |s| {
                Ok(s.create(&a.name)
                    .await?)
            })
            .await
            .map_err(fail)?;
        done(json!({ "created": a.name }))
    }

    #[tool(description = "Delete a mailbox (folder) and all messages in it.")]
    async fn delete_mailbox(&self, Parameters(a): Parameters<MailboxArgs>) -> ToolResult {
        if a.name
            .eq_ignore_ascii_case("INBOX")
        {
            return Err("refusing to delete INBOX".into());
        }
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, None, async |s| {
                Ok(s.delete(&a.name)
                    .await?)
            })
            .await
            .map_err(fail)?;
        done(json!({ "deleted": a.name }))
    }

    #[tool(description = "Rename a mailbox (folder).")]
    async fn rename_mailbox(&self, Parameters(a): Parameters<RenameMailboxArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, None, async |s| {
                Ok(s.rename(&a.name, &a.new_name)
                    .await?)
            })
            .await
            .map_err(fail)?;
        done(json!({ "renamed": a.name, "to": a.new_name }))
    }

    // ----- reading -----

    #[tool(
        description = "List the newest messages in a mailbox (headers only). Returns UIDs to use with read_message and other tools."
    )]
    async fn list_messages(&self, Parameters(a): Parameters<ListMessagesArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let query = if a.unread_only { "UNSEEN" } else { "ALL" };
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                let uids = imap::search(s, query).await?;
                let total = uids.len();
                let page: Vec<u32> = uids
                    .into_iter()
                    .skip(a.offset)
                    .take(a.limit)
                    .collect();
                let msgs: Vec<_> = imap::fetch_summaries(s, &page)
                    .await?
                    .iter()
                    .map(summary_json)
                    .collect();
                Ok(json!({
                    "account": account.id,
                    "mailbox": a.mailbox,
                    "total_matching": total,
                    "offset": a.offset,
                    "messages": msgs,
                }))
            })
            .await
            .map_err(fail)
            .and_then(done)
    }

    #[tool(
        description = "Search a mailbox. All given criteria are combined with AND. Results are newest first."
    )]
    async fn search_messages(&self, Parameters(a): Parameters<SearchArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let build = || -> Result<String> {
            let mut parts: Vec<String> = vec![];
            for (key, val) in [
                ("FROM", &a.from),
                ("TO", &a.to),
                ("SUBJECT", &a.subject),
                ("TEXT", &a.text),
                ("BODY", &a.body),
            ] {
                if let Some(v) = val {
                    parts.push(format!("{key} {}", imap::quote(v)?));
                }
            }
            if let Some(d) = &a.since {
                parts.push(format!("SINCE {}", imap_date(d)?));
            }
            if let Some(d) = &a.before {
                parts.push(format!("BEFORE {}", imap_date(d)?));
            }
            match a.unread {
                Some(true) => parts.push("UNSEEN".into()),
                Some(false) => parts.push("SEEN".into()),
                None => {}
            }
            match a.flagged {
                Some(true) => parts.push("FLAGGED".into()),
                Some(false) => parts.push("UNFLAGGED".into()),
                None => {}
            }
            if let Some(q) = &a.gmail_query {
                if account.provider != Provider::Gmail {
                    bail!("gmail_query only works on Gmail accounts");
                }
                parts.push(format!("X-GM-RAW {}", imap::quote(q)?));
            }
            if let Some(r) = &a.raw {
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
        };
        let query = build().map_err(fail)?;
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                let uids = imap::search(s, &query).await?;
                let total = uids.len();
                let page: Vec<u32> = uids
                    .into_iter()
                    .take(a.limit)
                    .collect();
                let msgs: Vec<_> = imap::fetch_summaries(s, &page)
                    .await?
                    .iter()
                    .map(summary_json)
                    .collect();
                Ok(json!({
                    "account": account.id,
                    "mailbox": a.mailbox,
                    "query": query,
                    "total_matching": total,
                    "messages": msgs,
                }))
            })
            .await
            .map_err(fail)
            .and_then(done)
    }

    #[tool(
        description = "Read a message: headers, body, and a list of attachments. Does not mark it read unless mark_read is true."
    )]
    async fn read_message(&self, Parameters(a): Parameters<ReadMessageArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let (raw, flags) = self
            .app
            .with_session(&account, Some(&a.mailbox), async |s| {
                let r = imap::fetch_raw(s, a.uid).await?;
                if a.mark_read {
                    imap::store_flags(s, &[a.uid], "+", &["\\Seen".into()]).await?;
                }
                Ok(r)
            })
            .await
            .map_err(fail)?;
        let max = a
            .max_chars
            .unwrap_or(20_000);

        if a.format == BodyFormat::Raw {
            let (body, truncated) = truncate(String::from_utf8_lossy(&raw).into_owned(), max);
            return done(json!({ "uid": a.uid, "raw": body, "truncated": truncated }));
        }
        let msg = MessageParser::default()
            .parse(&raw)
            .ok_or("could not parse message")?;
        let body = match a.format {
            BodyFormat::Html => msg
                .body_html(0)
                .map(|h| h.into_owned())
                .unwrap_or_default(),
            _ => compose::best_text(&msg),
        };
        let (body, truncated) = truncate(body, max);
        let attachments: Vec<_> = msg
            .attachments()
            .enumerate()
            .map(|(i, p)| {
                json!({
                    "index": i + 1,
                    "filename": p.attachment_name(),
                    "content_type": p.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("*"))),
                    "size": p.contents().len(),
                })
            })
            .collect();
        done(json!({
            "account": account.id,
            "mailbox": a.mailbox,
            "uid": a.uid,
            "message_id": msg.message_id(),
            "from": compose::addresses(msg.from()),
            "reply_to": compose::addresses(msg.reply_to()),
            "to": compose::addresses(msg.to()),
            "cc": compose::addresses(msg.cc()),
            "subject": msg.subject(),
            "date": msg.date().map(|d| d.to_rfc3339()),
            "flags": flags,
            "has_html": msg.html_part(0).is_some_and(|p| p.is_content_type("text", "html")),
            "body": body,
            "body_truncated": truncated,
            "attachments": attachments,
        }))
    }

    #[tool(
        description = "Save one or all attachments of a message to a local directory (default: Downloads). Returns the saved paths."
    )]
    async fn save_attachment(&self, Parameters(a): Parameters<SaveAttachmentArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let raw = self
            .fetch_parsed(&account, &a.mailbox, a.uid)
            .await
            .map_err(fail)?;
        let msg = MessageParser::default()
            .parse(&raw)
            .ok_or("could not parse message")?;
        let all = compose::attachments_of(&msg);
        let chosen: Vec<&Attachment> = match a.index {
            Some(i) => vec![
                all.get(i.wrapping_sub(1))
                    .ok_or_else(|| format!("no attachment #{i} (message has {})", all.len()))?,
            ],
            None => all
                .iter()
                .collect(),
        };
        if chosen.is_empty() {
            return Err("message has no attachments".into());
        }
        let dir = match a.directory {
            Some(d) => std::path::PathBuf::from(d),
            None => directories::UserDirs::new()
                .and_then(|u| {
                    u.download_dir()
                        .map(|p| p.to_path_buf())
                })
                .ok_or("no Downloads directory found; pass `directory`")?,
        };
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut saved = vec![];
        for att in chosen {
            let path = unique_path(&dir, &safe_filename(&att.filename));
            std::fs::write(&path, &att.data)
                .map_err(|e| format!("writing {}: {e}", path.display()))?;
            saved.push(json!({ "path": path, "size": att.data.len() }));
        }
        done(json!({ "saved": saved }))
    }

    // ----- writing & sending -----

    #[tool(description = "Compose and send a new email.")]
    async fn send_email(&self, Parameters(a): Parameters<SendArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let c = Compose {
            to: a.to,
            cc: a.cc,
            bcc: a.bcc,
            subject: a.subject,
            text: Some(a.body),
            html: a.html,
            attachments: load_attachments(&a.attachments).map_err(fail)?,
            ..Default::default()
        };
        self.send_compose(&account, &c, None)
            .await
            .map_err(fail)
            .and_then(done)
    }

    #[tool(
        description = "Reply to a message (threaded, with the original quoted). Set draft_only to save it to Drafts instead of sending."
    )]
    async fn reply_to_message(&self, Parameters(a): Parameters<ReplyArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let raw = self
            .fetch_parsed(&account, &a.mailbox, a.uid)
            .await
            .map_err(fail)?;
        let orig = MessageParser::default()
            .parse(&raw)
            .ok_or("could not parse original message")?;
        let mut c = compose::reply(&account, &orig, &a.body, a.html, a.reply_all);
        c.attachments = load_attachments(&a.attachments).map_err(fail)?;
        if a.draft_only {
            self.save_draft_compose(&account, &c, None)
                .await
        } else {
            self.send_compose(&account, &c, Some((&a.mailbox, a.uid)))
                .await
        }
        .map_err(fail)
        .and_then(done)
    }

    #[tool(
        description = "Forward a message, optionally with a note and its attachments. Set draft_only to save it to Drafts instead of sending."
    )]
    async fn forward_message(&self, Parameters(a): Parameters<ForwardArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let raw = self
            .fetch_parsed(&account, &a.mailbox, a.uid)
            .await
            .map_err(fail)?;
        let orig = MessageParser::default()
            .parse(&raw)
            .ok_or("could not parse original message")?;
        let mut c = compose::forward(&orig, a.to, &a.body, a.include_attachments);
        c.cc = a.cc;
        if a.draft_only {
            self.save_draft_compose(&account, &c, None)
                .await
        } else {
            self.send_compose(&account, &c, None)
                .await
        }
        .map_err(fail)
        .and_then(done)
    }

    #[tool(
        description = "Save a draft to the Drafts mailbox, or replace an existing draft by passing replace_uid. Read drafts with list_messages(mailbox=\"drafts\")."
    )]
    async fn save_draft(&self, Parameters(a): Parameters<DraftArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let c = Compose {
            to: a.to,
            cc: a.cc,
            bcc: a.bcc,
            subject: a.subject,
            text: Some(a.body),
            html: a.html,
            attachments: load_attachments(&a.attachments).map_err(fail)?,
            ..Default::default()
        };
        self.save_draft_compose(&account, &c, a.replace_uid)
            .await
            .map_err(fail)
            .and_then(done)
    }

    #[tool(description = "Send an existing draft from the Drafts mailbox, then remove the draft.")]
    async fn send_draft(&self, Parameters(a): Parameters<SendDraftArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let drafts = self
            .resolve_mailbox(&account, "drafts")
            .await
            .map_err(fail)?;
        let raw = self
            .fetch_parsed(&account, &drafts, a.uid)
            .await
            .map_err(fail)?;
        let msg = MessageParser::default()
            .parse(&raw)
            .ok_or("could not parse draft")?;
        let c = compose::from_draft(&msg);
        let mut result = self
            .send_compose(&account, &c, None)
            .await
            .map_err(fail)?;
        let removed = self
            .app
            .with_session(&account, Some(&drafts), async |s| {
                imap::store_flags(s, &[a.uid], "+", &["\\Deleted".into()]).await?;
                imap::expunge_uids(s, &[a.uid]).await
            })
            .await;
        result["draft_removed"] = json!(removed.is_ok());
        done(result)
    }

    // ----- modifying -----

    #[tool(
        description = "Add or remove flags on messages: seen (read), flagged (starred), answered, draft, or custom keywords. E.g. mark unread with remove=[\"seen\"]."
    )]
    async fn update_flags(&self, Parameters(a): Parameters<FlagArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let add = a
            .add
            .iter()
            .map(|f| imap_flag(f))
            .collect::<Result<Vec<_>>>()
            .map_err(fail)?;
        let remove = a
            .remove
            .iter()
            .map(|f| imap_flag(f))
            .collect::<Result<Vec<_>>>()
            .map_err(fail)?;
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                imap::store_flags(s, &a.uids, "+", &add).await?;
                imap::store_flags(s, &a.uids, "-", &remove).await
            })
            .await
            .map_err(fail)?;
        done(json!({ "uids": a.uids, "added": add, "removed": remove }))
    }

    #[tool(
        description = "Move messages to another mailbox. Destination may be a mailbox name or one of: archive, trash, junk."
    )]
    async fn move_messages(&self, Parameters(a): Parameters<MoveArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let dest = self
            .resolve_mailbox(&account, &a.destination)
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                imap::move_messages(s, &a.uids, &dest).await
            })
            .await
            .map_err(fail)?;
        done(json!({ "moved": a.uids, "from": a.mailbox, "to": dest }))
    }

    #[tool(description = "Copy messages to another mailbox (for Gmail, this adds a label).")]
    async fn copy_messages(&self, Parameters(a): Parameters<MoveArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        let dest = self
            .resolve_mailbox(&account, &a.destination)
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                Ok(s.uid_copy(imap::uid_set(&a.uids), &dest)
                    .await?)
            })
            .await
            .map_err(fail)?;
        done(json!({ "copied": a.uids, "from": a.mailbox, "to": dest }))
    }

    #[tool(
        description = "Permanently delete messages (\\Deleted + expunge); they cannot be recovered. \
        Before removing messages, follow the account's `remove_messages` guidance from list_accounts: \
        if it says to move them to trash, call move_messages instead, unless the user explicitly asks to delete permanently."
    )]
    async fn delete_messages(&self, Parameters(a): Parameters<DeleteArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                imap::store_flags(s, &a.uids, "+", &["\\Deleted".into()]).await?;
                imap::expunge_uids(s, &a.uids).await
            })
            .await
            .map_err(fail)?;
        done(json!({ "deleted_permanently": a.uids, "mailbox": a.mailbox }))
    }

    #[tool(description = "Gmail only: add or remove labels on messages without moving them.")]
    async fn gmail_labels(&self, Parameters(a): Parameters<LabelArgs>) -> ToolResult {
        let account = self
            .app
            .account(
                a.account
                    .as_deref(),
            )
            .await
            .map_err(fail)?;
        if account.provider != Provider::Gmail {
            return Err(format!("account '{}' is not a Gmail account", account.id));
        }
        let quote_all = |ls: &[String]| -> Result<String> {
            Ok(ls
                .iter()
                .map(|l| imap::quote(l))
                .collect::<Result<Vec<_>>>()?
                .join(" "))
        };
        let add = quote_all(&a.add).map_err(fail)?;
        let remove = quote_all(&a.remove).map_err(fail)?;
        self.app
            .with_session(&account, Some(&a.mailbox), async |s| {
                use futures::TryStreamExt;
                let set = imap::uid_set(&a.uids);
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
            .map_err(fail)?;
        done(json!({ "uids": a.uids, "added": a.add, "removed": a.remove }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for EmailServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("email-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read, write, send and organize email across several IMAP/SMTP accounts (iCloud, Gmail, others). \
                 Every mail tool takes an optional `account` (id or email); omitted means the first, default account. \
                 Messages are identified by mailbox + UID, as returned by list_messages/search_messages. \
                 Set up accounts with add_account then login; check with account_status. \
                 Each account's settings and how to apply them are returned by list_accounts \
                 (e.g. `remove_messages` says whether removing means move_messages to trash or delete_messages); \
                 check them before removing messages, and change them with update_account. \
                 Sending email and deleting messages act on the user's real mailbox, so confirm recipients and content with the user first.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(remove_by: RemoveBy, trash: Option<&str>) -> Account {
        let d = provider_defaults(Provider::Icloud).unwrap();
        Account {
            id: "me".into(),
            email: "me@me.com".into(),
            display_name: None,
            provider: Provider::Icloud,
            auth: d.auth,
            username: None,
            imap_host: d
                .imap_host
                .into(),
            imap_port: d.imap_port,
            smtp_host: d
                .smtp_host
                .into(),
            smtp_port: d.smtp_port,
            smtp_security: d.smtp_security,
            save_sent: d.save_sent,
            remove_by,
            trash_mailbox: trash.map(Into::into),
        }
    }

    #[test]
    fn remove_guidance_names_the_right_call() {
        let g = remove_guidance(&account(RemoveBy::MoveToTrash, Some("Trash")));
        assert!(
            g.starts_with("Remove messages with move_messages destination=\"Trash\""),
            "{g}"
        );
        let g = remove_guidance(&account(RemoveBy::MoveToTrash, None));
        assert!(g.contains("destination=\"trash\""), "{g}");
        let g = remove_guidance(&account(RemoveBy::Delete, None));
        assert!(g.starts_with("Remove messages with delete_messages"), "{g}");
    }
}
