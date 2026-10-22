#import "isss-template.typ": *

#show: isss-doc.with(
  title: "Quality Review",
  subtitle: "email-mcp 0.1.0 — structure, formatting and tests",
  author: "Javier Fernández",
  contact: "jfernand@me.com",
  date: "2026-10-22",
  docid: "ISSS-TR-0504",
  running: "email-mcp — Quality Review",
  abstract: [
    A review of #cd[email-mcp] 0.1.0 before the cache and command-queue work of ISSS-TR-0503.
    Formatting now follows #cd[chain_width = 0], matching yabiir, and clippy is clean. The code is
    sound in its security-sensitive parts (TLS roots, OAuth, credential storage) but thinly
    tested: line coverage is #key[24.8 %], and the IMAP, session and tool layers have no tests at
    all. Two behaviours can hurt a real mailbox — a fallback #cd[EXPUNGE] that removes more than
    asked, and the absence of command timeouts — and both should be fixed before the queue is
    built on top of them. The companion review of yabiir is in that repository's
    #cd[docs/quality-review.md].
  ],
  meta: (
    ("Status", [Observations — fixes not yet made, except formatting]),
    ("Scope", [#cd[email-mcp] at #cd[10d978f] plus the formatting change]),
    ("Related", [ISSS-TR-0503 (cache and queue design)\; yabiir #cd[docs/quality-review.md]]),
  ),
)

= Status <sec-status>

#dtable(
  columns: (auto, 1fr),
  ("Check", "Result"),
  ([Formatting], [Added #cd[rustfmt.toml] with #cd[chain_width = 0] (same as yabiir) and ran #cd[cargo fmt]: all 11 source files reformatted, #cd[cargo fmt --check] now clean.]),
  ([Lints], [#cd[cargo clippy --all-targets] clean.]),
  ([Tests], [13 pass, 1 ignored (needs network)\; 0 integration tests.]),
  ([Coverage], [#key[24.8 %] of lines (#cd[cargo llvm-cov]).]),
  ([CI], [#key[None.] yabiir has fmt, clippy, test and proptest jobs\; email-mcp has no workflow.]),
)

#dtable(
  columns: (auto, auto, 1fr),
  ("File", "Lines covered", "What the tests cover"),
  ([#cd[describe.rs]], [95.6 %], [Text and JSON rendering of the tool schemas.]),
  ([#cd[compose.rs]], [84.7 %], [Building, replying, forwarding, attachments.]),
  ([#cd[tls.rs]], [41.9 %], [Root store excludes eMudhra/emSign\; live endpoint check is ignored.]),
  ([#cd[config.rs]], [24.7 %], [Loading a config without the newer settings.]),
  ([#cd[server.rs]], [4.6 %], [Only #cd[remove_guidance].]),
  ([#cd[app.rs], #cd[imap.rs], #cd[oauth.rs], #cd[secrets.rs], #cd[smtp.rs], #cd[main.rs]], [0 %], [Nothing.]),
)

= Correctness and safety <sec-correctness>

#callout(kind: "trap", "Fallback EXPUNGE removes more than asked")[
  #cd[imap::expunge_uids] uses #cd[UID EXPUNGE] when the server has UIDPLUS, but otherwise sends a
  plain #cd[EXPUNGE], which permanently removes #key[every] message flagged #cd[\\Deleted] in the
  mailbox — including ones another client flagged and meant to keep or undo. The same path backs
  #cd[move_messages] on servers without MOVE. Without UIDPLUS the safe choice is to refuse (or to
  move to Trash instead) rather than expunge the whole mailbox. iCloud and Gmail both advertise
  UIDPLUS, so this bites only on other servers — exactly the ones least tested.
]

#callout(kind: "trap", "No timeouts after connecting")[
  Only the TCP connect (30 s), SMTP (60 s) and the OAuth login wait (10 min) have deadlines. Once
  a session is open, a stalled IMAP command blocks the tool call indefinitely #key[and] holds that
  account's session mutex, so every later call for the account hangs too. ISSS-TR-0503 rev 4
  already requires per-command timeouts for the worker\; they belong in #cd[App::with_session] for
  every tool, now.
]

- #key[UID sets are never compressed.] #cd[imap::uid_set] joins every UID with commas. Acting on
  thousands of messages produces a command line of tens of kilobytes, beyond what many servers
  accept. Collapse runs into ranges (#cd[1:500,502]) and chunk very large sets.
- #key[A NOOP before every call.] #cd[with_session] checks liveness with a round trip each time.
  Skipping it when the session was used in the last few seconds halves the round trips of an
  agent issuing calls back to back.
- #key[Production #cd[unwrap]s were reviewed] (#cd[config.rs], #cd[secrets.rs], #cd[app.rs],
  #cd[server.rs]): each relies on an invariant that holds (a path with a parent, an index just
  found, a slot just filled, JSON that always serializes). None needs to change, though
  #cd[expect] with the invariant spelled out would read better.
- #key[Security-sensitive parts are in good shape:] OAuth uses PKCE (S256) with a #cd[state]
  check on a loopback listener\; credentials are verified before they are stored in the keyring\;
  search terms are quoted and line breaks rejected (#cd[imap::quote]) — though #cd[quote] itself
  has no test.

= Structure <sec-structure>

#dtable(
  columns: (auto, 1fr),
  ("Observation", "Improvement"),
  ([#key[Binary-only crate.] There is no #cd[lib.rs], so #cd[tests/] can only hold fixtures — nothing outside #cd[src] can call the code.], [Split into a library (#cd[src/lib.rs] exposing the modules) and a thin #cd[main.rs]. Integration tests then live in #cd[tests/] and the cache work can be tested from outside.]),
  ([#key[#cd[server.rs] is 2,016 lines]: 27 argument structs, helpers, and all 26 tools in one #cd[impl].], [A #cd[server/] module: #cd[args.rs], #cd[accounts.rs], #cd[mailboxes.rs], #cd[messages.rs], #cd[compose.rs], with helpers in their own file. rmcp routers combine (#cd[Self::accounts_router() + Self::messages_router()]), so the split needs no behaviour change.]),
  ([#key[Tools call IMAP directly] (20 #cd[with_session] calls).], [The #cd[MailServer] trait planned as step 3.1 of ISSS-TR-0503 is also what makes the tools testable. Doing it first serves both.]),
  ([#key[No CI.]], [Copy yabiir's workflow: fmt, clippy #cd[-D warnings], tests on Linux and macOS, plus an MSRV job — #cd[rust-version = "1.89"] is declared but has never been built with 1.89.]),
)

= Testing <sec-testing>

The cheapest gains are pure functions that already exist and carry real risk. In order:

+ #key[Pure helpers, no network] — #cd[imap::quote] (escaping, line-break rejection),
  #cd[imap::uid_set] (and the range compression above), #cd[imap::flag_name], #cd[to_summary]
  on a canned #cd[Fetch]\; #cd[server.rs] #cd[imap_flag], #cd[imap_date], #cd[safe_filename],
  #cd[unique_path], #cd[truncate], #cd[account_json]\; #cd[config] #cd[find] / #cd[index_of] /
  #cd[validate_id] / #cd[guess_provider] and an atomic #cd[save] round trip in a temp dir\;
  #cd[oauth::xoauth2_string], #cd[AccessToken::is_fresh] and the loopback callback parsing
  (code, error, wrong #cd[state]).
+ #key[Library split] (§ 3), so the following can live in #cd[tests/].
+ #key[Tool tests against a fake] — once the #cd[MailServer] trait exists: argument validation,
  mailbox resolution (#cd[trash_mailbox]), #cd[remove_messages] guidance, error messages, and the
  expunge and move fallbacks.
+ #key[Ignored integration tests] against a local GreenMail or Dovecot container: login, list,
  search, move, delete, append, with and without MOVE / UIDPLUS / CONDSTORE.
+ #key[Secrets] — the keyring backend behind a trait with an in-memory implementation, so
  #cd[login] / #cd[logout] flows are testable without touching the real keyring.

#callout(kind: "info", "Target")[
  Aim for the pure-helper and fake-server tests to bring line coverage above 70 % before the cache
  work starts, and keep new cache and queue code at the level yabiir's core has (≥ 95 %).
]

= Suggested order <sec-order>

+ Fix the #cd[EXPUNGE] fallback and add command timeouts (§ 2).
+ Add CI with an MSRV job.
+ Library split, then the #cd[server/] module split.
+ Pure-helper tests, UID-set compression.
+ #cd[MailServer] trait and fake — this is also step 3.1 of the cache plan, so the cache work
  starts from a tested base.
