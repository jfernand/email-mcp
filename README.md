# email-mcp

An [MCP](https://modelcontextprotocol.io) server that lets an AI agent read, write, send and
organize email across several IMAP/SMTP accounts: iCloud, Gmail, and any other IMAP server.
Accounts themselves are managed through the MCP too (add, remove, log in, change settings).

## Install

```sh
cargo install email-mcp
```

Register it with your MCP client, e.g. Claude Code:

```sh
claude mcp add email -- ~/.cargo/bin/email-mcp
```

Then ask the agent to add an account. The first account added is the default.

## Accounts

| Provider | Server settings | Sign-in |
|---|---|---|
| iCloud (me.com, mac.com, icloud.com) | filled in automatically | [app-specific password](https://support.apple.com/en-us/102654) |
| Gmail | filled in automatically | [app password](https://myaccount.google.com/apppasswords), or OAuth with your own Google Cloud "Desktop app" client |
| Anything else | `provider = custom` with IMAP/SMTP hosts and ports | password |

Passwords are checked against the IMAP server before they are stored. Credentials live in the
OS keyring (Secret Service on Linux, Keychain on macOS) under the service name `email-mcp`.
Accounts and settings are stored in `~/.config/email-mcp/config.toml` (or the platform
equivalent; override with `EMAIL_MCP_CONFIG_DIR`).

### Per-account settings

| Setting | Meaning |
|---|---|
| `display_name` | Name shown in the From header |
| `save_sent` | Append a copy of sent mail to Sent (Gmail does this itself) |
| `remove_by` | `move_to_trash` (default) or `delete`: how the agent should remove messages |
| `trash_mailbox` | Folder "trash" refers to, when the server's flagged one isn't the one your clients use |

The tools do not apply `remove_by` themselves. `list_accounts` returns a `remove_messages`
instruction for each account, and the agent follows it by calling either `move_messages` to
trash or `delete_messages` (which always deletes permanently). Asking to "permanently delete"
therefore always works, whatever the setting.

## Tools

Accounts: `list_accounts`, `add_account`, `update_account`, `remove_account`, `login`, `logout`,
`account_status`, `set_google_oauth_client`.
Mailboxes: `list_mailboxes`, `create_mailbox`, `delete_mailbox`, `rename_mailbox`.
Reading: `list_messages`, `search_messages` (including Gmail search syntax), `read_message`,
`save_attachment`.
Writing: `send_email`, `reply_to_message`, `forward_message`, `save_draft`, `send_draft`.
Organizing: `update_flags`, `move_messages`, `copy_messages`, `delete_messages`, `gmail_labels`.

To review exactly what the agent is told:

```sh
email-mcp --describe          # instructions, tools and parameters as text
email-mcp --describe --json   # the raw tool schemas
```

## Structure

The repository is a Cargo workspace with two crates:

| Crate | Role |
|---|---|
| [`email-engine`](email-engine) | The mail engine: accounts, credentials, IMAP, SMTP and composing, as typed async operations. No MCP dependency. |
| `email-mcp` (root) | The MCP server: tool parameter schemas, JSON results, and the guidance the agent receives. |

IMAP calls run under deadlines (connect 30 s, NOOP/SELECT 30 s, a whole tool call 180 s), so a
stalled server fails the call instead of hanging every later call for that account.

## TLS

All connections (IMAP, SMTP, OAuth) use Mozilla's root store from `webpki-root-certs` rather
than the OS store, minus every eMudhra/emSign root following the October 2026 key compromise.

## Future work

- **Local message cache.** Every read currently goes to the IMAP server. Caching headers and
  bodies locally (keyed by mailbox UIDVALIDITY + UID, kept in sync with CONDSTORE/QRESYNC where
  the server supports it) would make listing and searching large mailboxes fast, cut the load
  on the server, and allow offline reading.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
