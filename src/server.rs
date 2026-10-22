//! The MCP front end: tool parameter schemas, JSON results, and the guidance the agent gets.
//! All mail work is done by [`email_engine::Engine`].

use std::sync::Arc;

use email_engine::accounts::{AccountStatus, AccountUpdate, LoginStart, NewAccount};
use email_engine::compose::{Attachment, Compose};
use email_engine::config::{Account, AuthMethod, Provider, RemoveBy, SmtpSecurity};
use email_engine::messages::{MessagePage, MessageSummary, MessageView, SearchCriteria};
use email_engine::sending::SendReport;
use email_engine::{Engine, LoginState, oauth};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

type ToolResult = Result<String, String>;

fn done(v: Value) -> ToolResult {
    Ok(serde_json::to_string_pretty(&v).expect("a JSON value always serializes"))
}

fn fail(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[derive(Clone)]
pub struct EmailServer {
    engine: Arc<Engine>,
    tool_router: ToolRouter<Self>,
}

impl EmailServer {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            tool_router: Self::tool_router(),
        }
    }

    pub fn tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router
            .list_all()
    }

    async fn account(&self, key: Option<&str>) -> Result<Account, String> {
        self.engine
            .account(key)
            .await
            .map_err(fail)
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

// ---------- JSON results ----------

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

fn summary_json(s: &MessageSummary) -> Value {
    json!({
        "uid": s.uid,
        "from": s.from,
        "to": s.to,
        "subject": s.subject,
        "date": s.date,
        "unread": s.unread,
        "flagged": s.flagged,
        "flags": s.flags,
        "size": s.size,
    })
}

fn page_json(page: &MessagePage) -> Vec<Value> {
    page.messages
        .iter()
        .map(summary_json)
        .collect()
}

fn status_json(st: &AccountStatus) -> Value {
    let mut v = json!({
        "account": st.account.id,
        "email": st.account.email,
        "auth": st.account.auth,
        "logged_in": match &st.logged_in {
            Ok(b) => json!(b),
            Err(e) => json!(format!("error reading secret store: {e}")),
        },
    });
    if let Some(state) = &st.pending_login {
        v["pending_login"] = match state {
            LoginState::Waiting { auth_url } => {
                json!({ "status": "waiting", "auth_url": auth_url })
            }
            LoginState::Failed(e) => json!({ "status": "failed", "error": e }),
        };
    }
    if let Some(c) = &st.connection {
        let show = |r: &Result<(), String>| match r {
            Ok(()) => json!("ok"),
            Err(e) => json!(format!("error: {e}")),
        };
        v["connection"] = json!({ "imap": show(&c.imap), "smtp": show(&c.smtp) });
    }
    v
}

fn sent_json(account: &Account, c: &Compose, report: &SendReport) -> Value {
    json!({
        "sent": true,
        "account": account.id,
        "to": c.to, "cc": c.cc, "bcc": c.bcc,
        "subject": c.subject,
        "notes": report.notes,
    })
}

fn draft_json(mailbox: &str, replace_uid: Option<u32>, c: &Compose) -> Value {
    json!({
        "saved": true,
        "mailbox": mailbox,
        "replaced_uid": replace_uid,
        "subject": c.subject,
    })
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
    /// Send `c`, or save it as a draft, and describe the outcome.
    async fn send_or_draft(
        &self,
        account: &Account,
        c: &Compose,
        draft_only: bool,
        answered: Option<(&str, u32)>,
    ) -> ToolResult {
        if draft_only {
            let mailbox = self
                .engine
                .save_draft(account, c, None)
                .await
                .map_err(fail)?;
            done(draft_json(&mailbox, None, c))
        } else {
            let report = self
                .engine
                .send(account, c, answered)
                .await
                .map_err(fail)?;
            done(sent_json(account, c, &report))
        }
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
        let list = self
            .engine
            .accounts()
            .await;
        let accounts: Vec<_> = list
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| account_json(a, i))
            .collect();
        done(json!({
            "accounts": accounts,
            "google_oauth_client_configured": list.google_oauth_configured,
            "secret_store": list.secret_store,
        }))
    }

    #[tool(
        description = "Add an email account. Server settings are filled in automatically for iCloud (me.com/mac.com/icloud.com) and Gmail. \
        iCloud requires an app-specific password (appleid.apple.com → Sign-In and Security → App-Specific Passwords). \
        Gmail can use an app password (auth=password) or OAuth (auth=oauth2, requires set_google_oauth_client). \
        Pass `password` to log in immediately, or call `login` afterwards."
    )]
    async fn add_account(&self, Parameters(a): Parameters<AddAccountArgs>) -> ToolResult {
        let added = self
            .engine
            .add_account(NewAccount {
                email: a.email,
                id: a.id,
                display_name: a.display_name,
                provider: a.provider,
                auth: a.auth,
                username: a.username,
                imap_host: a.imap_host,
                imap_port: a.imap_port,
                smtp_host: a.smtp_host,
                smtp_port: a.smtp_port,
                smtp_security: a.smtp_security,
                save_sent: a.save_sent,
                remove_by: a.remove_by,
                position: a.position,
            })
            .await
            .map_err(fail)?;
        let mut result = json!({ "added": account_json(&added.account, added.index) });
        if let Some(pw) = a.password {
            result["login"] = match self
                .login(Parameters(LoginArgs {
                    account: added
                        .account
                        .id,
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
        let (updated, auth_changed) = self
            .engine
            .update_account(
                &u.account,
                AccountUpdate {
                    display_name: u.display_name,
                    username: u.username,
                    auth: u.auth,
                    imap_host: u.imap_host,
                    imap_port: u.imap_port,
                    smtp_host: u.smtp_host,
                    smtp_port: u.smtp_port,
                    smtp_security: u.smtp_security,
                    save_sent: u.save_sent,
                    remove_by: u.remove_by,
                    trash_mailbox: u.trash_mailbox,
                    position: u.position,
                },
            )
            .await
            .map_err(fail)?;
        let mut out = json!({ "updated": account_json(&updated.account, updated.index) });
        if auth_changed {
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
        let removed = self
            .engine
            .remove_account(&account)
            .await
            .map_err(fail)?;
        done(json!({ "removed": removed.id, "email": removed.email }))
    }

    #[tool(
        description = "Log in to an account. For password accounts, pass the (app-specific) password; it is verified against IMAP and then stored in the OS keyring. \
        For Gmail OAuth accounts, this returns a Google sign-in URL (and tries to open it in the browser); finish signing in there, then check account_status."
    )]
    async fn login(&self, Parameters(l): Parameters<LoginArgs>) -> ToolResult {
        let (account, start) = self
            .engine
            .login(&l.account, l.password)
            .await
            .map_err(fail)?;
        match start {
            LoginStart::LoggedIn => done(json!({ "account": account.id, "logged_in": true })),
            LoginStart::BrowserSignIn { auth_url } => {
                open_browser(&auth_url);
                done(json!({
                    "account": account.id,
                    "status": "waiting_for_browser_sign_in",
                    "auth_url": auth_url,
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
            .engine
            .logout(&account)
            .await
            .map_err(fail)?;
        done(json!({ "account": account.id, "logged_in": false }))
    }

    #[tool(
        description = "Show whether accounts are logged in, any pending OAuth sign-in, and optionally test IMAP/SMTP connectivity."
    )]
    async fn account_status(&self, Parameters(a): Parameters<StatusArgs>) -> ToolResult {
        let statuses = self
            .engine
            .status(
                a.account
                    .as_deref(),
                a.test_connection,
            )
            .await
            .map_err(fail)?;
        done(json!(
            statuses
                .iter()
                .map(status_json)
                .collect::<Vec<_>>()
        ))
    }

    #[tool(
        description = "Configure the Google OAuth client used for Gmail accounts with auth=oauth2. Create it in Google Cloud Console: \
        enable the Gmail API, configure the OAuth consent screen (add yourself as a test user), then create an OAuth client ID of type 'Desktop app'."
    )]
    async fn set_google_oauth_client(
        &self,
        Parameters(c): Parameters<OauthClientArgs>,
    ) -> ToolResult {
        self.engine
            .set_google_oauth_client(&c.client_id, &c.client_secret)
            .await
            .map_err(fail)?;
        done(json!({ "google_oauth_client_configured": true }))
    }

    // ----- mailboxes -----

    #[tool(
        description = "List mailboxes (folders; Gmail labels appear as mailboxes) with their special use (sent, drafts, trash, junk, archive)."
    )]
    async fn list_mailboxes(&self, Parameters(a): Parameters<ListMailboxesArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let boxes = self
            .engine
            .list_mailboxes(&account, a.with_counts)
            .await
            .map_err(fail)?;
        let out: Vec<_> = boxes
            .iter()
            .map(|b| {
                let mut v = json!({
                    "name": b.name,
                    "special_use": b.special_use.map(|k| format!("{k:?}").to_lowercase()),
                });
                if !b.selectable {
                    v["selectable"] = json!(false);
                } else if let Some(c) = b.counts {
                    v["messages"] = json!(c.messages);
                    v["unread"] = json!(c.unread);
                }
                v
            })
            .collect();
        done(json!({ "account": account.id, "mailboxes": out }))
    }

    #[tool(
        description = "Create a mailbox (folder). Use the server's hierarchy delimiter (usually '/') for nesting."
    )]
    async fn create_mailbox(&self, Parameters(a): Parameters<MailboxArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        self.engine
            .create_mailbox(&account, &a.name)
            .await
            .map_err(fail)?;
        done(json!({ "created": a.name }))
    }

    #[tool(description = "Delete a mailbox (folder) and all messages in it.")]
    async fn delete_mailbox(&self, Parameters(a): Parameters<MailboxArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        self.engine
            .delete_mailbox(&account, &a.name)
            .await
            .map_err(fail)?;
        done(json!({ "deleted": a.name }))
    }

    #[tool(description = "Rename a mailbox (folder).")]
    async fn rename_mailbox(&self, Parameters(a): Parameters<RenameMailboxArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        self.engine
            .rename_mailbox(&account, &a.name, &a.new_name)
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
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let page = self
            .engine
            .list_messages(&account, &a.mailbox, a.unread_only, a.offset, a.limit)
            .await
            .map_err(fail)?;
        done(json!({
            "account": account.id,
            "mailbox": a.mailbox,
            "total_matching": page.total,
            "offset": a.offset,
            "messages": page_json(&page),
        }))
    }

    #[tool(
        description = "Search a mailbox. All given criteria are combined with AND. Results are newest first."
    )]
    async fn search_messages(&self, Parameters(a): Parameters<SearchArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let criteria = SearchCriteria {
            from: a.from,
            to: a.to,
            subject: a.subject,
            text: a.text,
            body: a.body,
            since: a.since,
            before: a.before,
            unread: a.unread,
            flagged: a.flagged,
            gmail_query: a.gmail_query,
            raw: a.raw,
        };
        let (query, page) = self
            .engine
            .search(&account, &a.mailbox, &criteria, a.limit)
            .await
            .map_err(fail)?;
        done(json!({
            "account": account.id,
            "mailbox": a.mailbox,
            "query": query,
            "total_matching": page.total,
            "messages": page_json(&page),
        }))
    }

    #[tool(
        description = "Read a message: headers, body, and a list of attachments. Does not mark it read unless mark_read is true."
    )]
    async fn read_message(&self, Parameters(a): Parameters<ReadMessageArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let fetched = self
            .engine
            .fetch_message(&account, &a.mailbox, a.uid, a.mark_read)
            .await
            .map_err(fail)?;
        let max = a
            .max_chars
            .unwrap_or(20_000);

        if a.format == BodyFormat::Raw {
            let (body, truncated) =
                truncate(String::from_utf8_lossy(&fetched.raw).into_owned(), max);
            return done(json!({ "uid": a.uid, "raw": body, "truncated": truncated }));
        }
        let view = MessageView::parse(&fetched.raw).map_err(fail)?;
        let body = match a.format {
            BodyFormat::Html => view
                .html
                .unwrap_or_default(),
            _ => view.text,
        };
        let (body, truncated) = truncate(body, max);
        let attachments: Vec<_> = view
            .attachments
            .iter()
            .map(|p| {
                json!({
                    "index": p.index,
                    "filename": p.filename,
                    "content_type": p.content_type,
                    "size": p.size,
                })
            })
            .collect();
        done(json!({
            "account": account.id,
            "mailbox": a.mailbox,
            "uid": a.uid,
            "message_id": view.message_id,
            "from": view.from,
            "reply_to": view.reply_to,
            "to": view.to,
            "cc": view.cc,
            "subject": view.subject,
            "date": view.date,
            "flags": fetched.flags,
            "has_html": view.has_html,
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
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let saved = self
            .engine
            .save_attachments(
                &account,
                &a.mailbox,
                a.uid,
                a.index,
                a.directory
                    .map(Into::into),
            )
            .await
            .map_err(fail)?;
        let saved: Vec<_> = saved
            .iter()
            .map(|s| json!({ "path": s.path, "size": s.size }))
            .collect();
        done(json!({ "saved": saved }))
    }

    // ----- writing & sending -----

    #[tool(description = "Compose and send a new email.")]
    async fn send_email(&self, Parameters(a): Parameters<SendArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let c = Compose {
            to: a.to,
            cc: a.cc,
            bcc: a.bcc,
            subject: a.subject,
            text: Some(a.body),
            html: a.html,
            attachments: Attachment::from_paths(&a.attachments).map_err(fail)?,
            ..Default::default()
        };
        self.send_or_draft(&account, &c, false, None)
            .await
    }

    #[tool(
        description = "Reply to a message (threaded, with the original quoted). Set draft_only to save it to Drafts instead of sending."
    )]
    async fn reply_to_message(&self, Parameters(a): Parameters<ReplyArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let mut c = self
            .engine
            .reply_to(&account, &a.mailbox, a.uid, &a.body, a.html, a.reply_all)
            .await
            .map_err(fail)?;
        c.attachments = Attachment::from_paths(&a.attachments).map_err(fail)?;
        self.send_or_draft(&account, &c, a.draft_only, Some((&a.mailbox, a.uid)))
            .await
    }

    #[tool(
        description = "Forward a message, optionally with a note and its attachments. Set draft_only to save it to Drafts instead of sending."
    )]
    async fn forward_message(&self, Parameters(a): Parameters<ForwardArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let mut c = self
            .engine
            .forward(
                &account,
                &a.mailbox,
                a.uid,
                a.to,
                &a.body,
                a.include_attachments,
            )
            .await
            .map_err(fail)?;
        c.cc = a.cc;
        self.send_or_draft(&account, &c, a.draft_only, None)
            .await
    }

    #[tool(
        description = "Save a draft to the Drafts mailbox, or replace an existing draft by passing replace_uid. Read drafts with list_messages(mailbox=\"drafts\")."
    )]
    async fn save_draft(&self, Parameters(a): Parameters<DraftArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let c = Compose {
            to: a.to,
            cc: a.cc,
            bcc: a.bcc,
            subject: a.subject,
            text: Some(a.body),
            html: a.html,
            attachments: Attachment::from_paths(&a.attachments).map_err(fail)?,
            ..Default::default()
        };
        let mailbox = self
            .engine
            .save_draft(&account, &c, a.replace_uid)
            .await
            .map_err(fail)?;
        done(draft_json(&mailbox, a.replace_uid, &c))
    }

    #[tool(description = "Send an existing draft from the Drafts mailbox, then remove the draft.")]
    async fn send_draft(&self, Parameters(a): Parameters<SendDraftArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let (c, report, removed) = self
            .engine
            .send_draft(&account, a.uid)
            .await
            .map_err(fail)?;
        let mut result = sent_json(&account, &c, &report);
        result["draft_removed"] = json!(removed);
        done(result)
    }

    // ----- modifying -----

    #[tool(
        description = "Add or remove flags on messages: seen (read), flagged (starred), answered, draft, or custom keywords. E.g. mark unread with remove=[\"seen\"]."
    )]
    async fn update_flags(&self, Parameters(a): Parameters<FlagArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let (added, removed) = self
            .engine
            .update_flags(&account, &a.mailbox, &a.uids, &a.add, &a.remove)
            .await
            .map_err(fail)?;
        done(json!({ "uids": a.uids, "added": added, "removed": removed }))
    }

    #[tool(
        description = "Move messages to another mailbox. Destination may be a mailbox name or one of: archive, trash, junk."
    )]
    async fn move_messages(&self, Parameters(a): Parameters<MoveArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let dest = self
            .engine
            .move_messages(&account, &a.mailbox, &a.uids, &a.destination)
            .await
            .map_err(fail)?;
        done(json!({ "moved": a.uids, "from": a.mailbox, "to": dest }))
    }

    #[tool(description = "Copy messages to another mailbox (for Gmail, this adds a label).")]
    async fn copy_messages(&self, Parameters(a): Parameters<MoveArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        let dest = self
            .engine
            .copy_messages(&account, &a.mailbox, &a.uids, &a.destination)
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
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        self.engine
            .delete_messages(&account, &a.mailbox, &a.uids)
            .await
            .map_err(fail)?;
        done(json!({ "deleted_permanently": a.uids, "mailbox": a.mailbox }))
    }

    #[tool(description = "Gmail only: add or remove labels on messages without moving them.")]
    async fn gmail_labels(&self, Parameters(a): Parameters<LabelArgs>) -> ToolResult {
        let account = self
            .account(
                a.account
                    .as_deref(),
            )
            .await?;
        self.engine
            .gmail_labels(&account, &a.mailbox, &a.uids, &a.add, &a.remove)
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
    use email_engine::config::provider_defaults;

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

    #[test]
    fn account_json_marks_only_the_first_account_default() {
        let a = account(RemoveBy::MoveToTrash, None);
        assert_eq!(account_json(&a, 0)["default"], json!(true));
        assert_eq!(account_json(&a, 1)["default"], json!(false));
        assert_eq!(account_json(&a, 0)["imap"], json!("imap.mail.me.com:993"));
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("héllo".into(), 10), ("héllo".into(), false));
        assert_eq!(truncate("héllo".into(), 2), ("hé".into(), true));
    }
}
