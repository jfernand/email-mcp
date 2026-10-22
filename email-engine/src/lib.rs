//! A multi-account email engine for iCloud, Gmail and other IMAP/SMTP servers.
//!
//! [`Engine`] holds the configured accounts, their credentials (in the OS keyring) and one
//! IMAP session per account, and offers every operation as a typed async method:
//! accounts and sign-in ([`accounts`]), mailboxes ([`mailboxes`]), reading, searching and
//! organizing messages ([`messages`]), and sending ([`sending`]). The lower layers — IMAP
//! commands, SMTP, message composition, TLS roots — are public for callers that need them.
//!
//! All connections use Mozilla's root store minus the eMudhra/emSign roots ([`tls`]).
//!
//! ```no_run
//! use email_engine::{Engine, tls};
//!
//! # async fn run() -> anyhow::Result<()> {
//! tls::init();
//! let engine = Engine::load()?; // ~/.config/email-mcp/config.toml
//! let account = engine.account(None).await?; // the default account
//! let page = engine.list_messages(&account, "INBOX", false, 0, 10).await?;
//! for m in page.messages {
//!     println!("{} {:?} {}", m.uid, m.from, m.subject);
//! }
//! # Ok(())
//! # }
//! ```

pub mod accounts;
pub mod compose;
pub mod config;
mod engine;
pub mod imap;
pub mod mailboxes;
pub mod messages;
pub mod oauth;
pub mod secrets;
pub mod sending;
pub mod smtp;
pub mod tls;

pub use engine::{Engine, LoginState};
