# email-engine

The mail engine behind [email-mcp](https://github.com/jfernand/email-mcp): several IMAP/SMTP
accounts (iCloud, Gmail, any other server) with credentials in the OS keyring, offered as typed
async operations with no MCP dependency.

```rust
use email_engine::{Engine, tls};

tls::init();
let engine = Engine::load()?; // ~/.config/email-mcp/config.toml
let account = engine.account(None).await?; // the default account
let page = engine.list_messages(&account, "INBOX", false, 0, 10).await?;
for m in page.messages {
    println!("{} {:?} {}", m.uid, m.from, m.subject);
}
```

| Module | What it does |
|---|---|
| `accounts` | Add, update and remove accounts; password and Google OAuth sign-in; status checks |
| `mailboxes` | List, create, delete and rename mailboxes; resolve "trash", "archive", … |
| `messages` | List, search, fetch and parse messages; save attachments; flags, move, copy, delete, Gmail labels |
| `sending` | Send over SMTP with a Sent copy, drafts, replies, forwards |
| `imap`, `smtp`, `compose` | The lower layers, for callers that need them |
| `config`, `secrets`, `oauth`, `tls` | Configuration file, keyring access, Google OAuth (PKCE), the TLS root store |

Every IMAP call runs under deadlines (connect 30 s, NOOP/SELECT 30 s, a whole operation 180 s),
and a stalled session is dropped rather than blocking the account. All connections use Mozilla's
root store minus the eMudhra/emSign roots.

Enable the `schema` feature for `schemars::JsonSchema` on the account-setting enums.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
