#import "isss-template.typ": *

#show: isss-doc.with(
  title: "Local Cache & Command Queue",
  subtitle: "Offline-first email-mcp on yabiir and a table layer",
  author: "Javier Fernández",
  contact: "jfernand@me.com",
  date: "2026-10-22",
  docid: "ISSS-TR-0503",
  running: "email-mcp — Local Cache & Command Queue (design draft, rev 4)",
  abstract: [
    #cd[email-mcp] is an MCP server that lets an agent read, write and organize mail across
    several IMAP/SMTP accounts. Every tool currently talks to the server directly: listing a
    16,720-message iCloud INBOX is a network round trip, and every flag change, move or delete
    waits on IMAP. This report proposes a local message cache together with a durable command
    queue: mutations apply to the cache immediately and their IMAP execution is queued, retried
    and reconciled. The two live in #key[separate yabiir stores]. yabiir itself gains only an
    ordered keydir with range scans; everything else — tables, order-preserving keys, per-table
    lz4 compression — lives in a new layer above it, #cd[yabiir-tables]. Instead of atomic
    batches, correctness rests on write ordering: the queue is the source of truth and the cache
    is a disposable view. This is a draft for review; nothing here has been implemented.
  ],
  meta: (
    ("Status", [Draft for review — not scheduled for implementation]),
    ("Revision", [4 (2026-10-22) — see § 1.3]),
    ("Components", [#cd[yabiir] 0.5.0 → 0.6.0; new #cd[yabiir-tables] 0.1.0; #cd[email-mcp] 0.1.0]),
    ("Decisions", [Review Q&A on 2026-10-21 and 2026-10-22 — see § 1.2]),
  ),
)

= Context <sec-context>

== Why a cache

Each #cd[email-mcp] tool opens (or reuses) an IMAP session and asks the server. That is simple
and always correct, but it has three costs that grow with mailbox size and with how much the
agent does:

- #key[Latency.] Listing, searching and reading all pay a network round trip, and a search over a
  large mailbox is a server-side scan the agent waits on.
- #key[Chattiness.] An agent triaging mail issues many small reads in a row; most of them ask for
  data that has not changed since the previous call.
- #key[Fragile mutations.] A delete or move fails outright when the network blips, and the agent
  has to notice and retry.

The goal is a local copy of what each account contains, plus a #hot[command queue]: when the
agent deletes messages, they disappear from the cache at once and the real IMAP operation is
queued, executed in the background, retried on transient failure, and reconciled with the server
if it fails for good.

== Decisions taken <sec-decisions>

#dtable(
  columns: (auto, 1fr),
  ("Topic", "Decision"),
  ([Layering], [Most of the work is a #key[layer above the KV store] that exposes #key[tables]: a new crate #cd[yabiir-tables] in the yabiir workspace, published separately.]),
  ([KV changes], [Only an #key[ordered keydir] with range/prefix scans — #key[the only keydir], replacing the hash map, not an option. No on-disk format change.]),
  ([Value formats], [#key[JSON] (+ lz4) for envelopes and other cache records, #key[postcard] for the frequently rewritten #cd[flags]\; raw for bodies.]),
  ([Stores], [Cache and queue are #key[two directories, two yabiir instances].]),
  ([Atomicity], [#key[No KV batches.] Write ordering instead: the queue is the source of truth, the cache is rebuildable (§ 4.1).]),
  ([Compression], [lz4 (#cd[lz4_flex]) chosen #key[per table] in the table layer.]),
  ([Processes], [#key[One email-mcp at a time]. A shared daemon is the near-term follow-up (§ 7).]),
  ([Queue UX], [Apply locally, enqueue, #key[wait up to ~5 s], then report #cd[done], #cd[failed] or #cd[queued].]),
  ([Meaning of #cd[done]], [#key[Only after the server has confirmed the change, verified by reading it back] — large providers drop requests often enough that an #cd[OK] alone is not trusted. Locally applied but unconfirmed is #cd[queued], never #cd[done] (§ 5.3).]),
  ([Cache scope], [#key[Headers and flags] for mailboxes as they are used; a #key[body is stored the first time it is read].]),
  ([Message storage], [Three tables per message: an #key[immutable envelope] of parsed fields, a #key[small mutable flags record], and the #key[raw body]. Everything else is derived from the raw body (§ 4.3).]),
  ([Indexes], [None in v1: local search scans a mailbox. #key[Secondary indexes may be needed later] (§ 4.4).]),
)

== Changes since revision 1

#dtable(
  columns: (auto, auto, 1fr),
  ("Rev", "Date", "Change"),
  ([1], [2026-10-21], [First draft: one store; yabiir gains atomic batches, ordered keydir and per-record compression (format v2).]),
  ([2], [2026-10-22], [Two stores (cache, queue). New #cd[yabiir-tables] crate with tables, key encoding and per-table codecs. yabiir keeps only the ordered keydir. Batches dropped in favour of write ordering; compression moved to the table layer. Tables identified in keys by numeric ids from the catalog.]),
  ([3], [2026-10-22], [#cd[messages] split into #cd[envelopes] (immutable, parsed and decoded fields) and #cd[flags] (mutable); the summary fetch grows to cover them. Bodies stay raw, with an optional derived #cd[text] table and an optional body-cache size cap. Note that secondary indexes may be needed later.]),
  ([4], [2026-10-22], [#cd[done] means confirmed by the server and verified by read-back. Worker gains per-command timeouts, a verify step, and verify-before-retry when an outcome is unknown. The ordered keydir becomes yabiir's only keydir. JSON for envelopes, postcard for flags confirmed. Freshness fixed at 60 s for all mailboxes; body-cache cap deferred.]),
)

== What exists today

#spec(
  ("yabiir API", [Synchronous #cd[Bitcask] trait (#cd[src/api.rs]): #cd[open / get / put / delete / list_keys / fold / merge / sync / close]\; #cd[Engine] in #cd[src/engine.rs]]),
  ("yabiir limits", [One writer per directory (#cd[lock.rs]); keydir is an ahash #cd[HashMap], so no ordering]),
  ("Workspace", [yabiir is the root package; #cd[xtask] is the only other member]),
  ("async-imap 0.12", [#cd[select_condstore], #cd[Mailbox.highest_modseq], #cd[Fetch.modseq], #cd[idle()] — but no QRESYNC / VANISHED]),
  ("email-mcp", [IMAP ops in #cd[src/imap.rs], sessions in #cd[App::with_session] (#cd[src/app.rs]), tools in #cd[src/server.rs]]),
)

#codepanel(title: "Layers")[
```text
email-mcp      cache store (~/.cache)        queue store (~/.local/share)
                     │                              │
yabiir-tables   Db ─ Table<K,V> ─ Key encoding ─ ValueCodec (json/postcard/raw, lz4)
                     │                              │
yabiir 0.6      Engine, ordered keydir, scan()      (one instance per directory)
```
]

= yabiir 0.6.0: ordered keydir <sec-yabiir>

The cache needs "the newest 20 messages in INBOX" and the queue needs "the oldest pending
command" — both ordered range reads. With a hash keydir the only option is #cd[list_keys] plus a
sort over every key in the store. This is the one change that has to live in the KV layer, because
only the keydir knows where keys are.

#codepanel(title: "Proposed API")[
```rust
pub struct Scan {
    pub start: Bound<Vec<u8>>,
    pub end: Bound<Vec<u8>>,
    pub reverse: bool,
    pub limit: Option<usize>,
}
impl Scan { pub fn prefix(p: &[u8]) -> Self; }

impl Engine {
    pub fn scan(&self, s: Scan) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
    pub fn scan_keys(&self, s: Scan) -> Result<Vec<Vec<u8>>>;
}
```
]

- #key[Ordered is the only keydir.] The ahash #cd[HashMap] in #cd[src/keydir.rs] is replaced
  by a #cd[BTreeMap]\; there is no #cd[KeyOrder] option and no #cd[Hashed] mode, so #cd[scan] is
  always available and #cd[ahash] leaves the dependency list. #cd[list_keys] and #cd[fold] now
  return keys in order as a side effect.
- The range is snapshotted under the read lock with no I/O — the existing "never hold a lock
  across disk I/O" rule — and values are read afterwards.
- #key[No on-disk format change.] Recovery, hint files and merge are untouched; only the
  in-memory index differs, so every existing store reopens unchanged.

#callout(kind: "trap", "The cost to measure")[
  A #cd[BTreeMap] is typically slower than ahash for point #cd[get] / #cd[insert] (log n compares
  of whole keys instead of one hash) and rebuilding it at open is slower too. Before switching,
  #cd[benches/keydir.rs] records #cd[get] / #cd[insert] / open-time rebuild for the old hash
  keydir and the new ordered one, so the regression is known and stated in the 0.6.0 changelog.
]

#callout(kind: "info", "Side benefit")[
  A #cd[BTreeMap] keydir is exactly the deterministic keydir ROADMAP § 1 asks for before
  deterministic simulation testing. The proptest model in #cd[tests/model.rs] gains scans checked
  against a reference #cd[BTreeMap].
]

Release: bump to #cd[0.6.0], update README and ROADMAP, publish.

= yabiir-tables 0.1.0 <sec-tables>

A new workspace member beside #cd[xtask], depending on #cd[yabiir = "0.6"] and published on its
own. It is synchronous like yabiir; async callers wrap it in #cd[spawn_blocking].

== Db and tables

#codepanel(title: "Sketch")[
```rust
pub struct Db { /* one Engine, one directory, one lock */ }
impl Db {
    pub fn open(dir: &Path, opts: Options) -> Result<Db>;
    pub fn table<K: Key, V>(&self, def: &TableDef<K, V>) -> Result<Table<'_, K, V>>;
}

pub struct TableDef<K, V> {
    pub name: &'static str,   // ASCII, non-empty, no NUL
    pub version: u32,
    pub codec: ValueCodec,
    _types: PhantomData<(K, V)>,
}

impl<K: Key, V> Table<'_, K, V> {
    pub fn get(&self, k: &K) -> Result<Option<V>>;
    pub fn put(&self, k: &K, v: &V) -> Result<()>;
    pub fn delete(&self, k: &K) -> Result<()>;
    pub fn range(&self, r: impl RangeBounds<K>) -> Query<K, V>;   // .reverse() .limit(n)
    pub fn prefix(&self, p: &KeyPrefix) -> Query<K, V>;
    pub fn delete_prefix(&self, p: &KeyPrefix) -> Result<usize>;
    pub fn clear(&self) -> Result<usize>;
}
```
]

#spec(
  ("Physical key", [`{table id: u16 BE} {encoded key}` — 2 bytes of overhead per key; tables are disjoint key ranges of one store]),
  ("Table ids", [Assigned by the catalog the first time a table is opened: one more than the highest id ever assigned. Id #cd[0] is the catalog itself. Ids are #key[never reused], so leftovers of a dropped table can never surface in a new one]),
  ("Catalog", [Table #cd[0]: `0x0000 {encoded name}` → #cd[{ id, version, codec }], loaded into memory at #cd[Db::open] (one prefix scan)]),
  ("Dropping", [#cd[Db::drop_table(name)] deletes the id's range first, then the catalog entry; a crash in between is finished on the next drop or open]),
  ("Inspection", [Keys are no longer readable in yabiir's #cd[kv]\; a small #cd[yabiir-tables] CLI (#cd[tables], #cd[dump TABLE]) decodes them via the catalog]),
  ("Schema check", [Reopening a table with a different version or codec returns #cd[Error::SchemaMismatch]\; the caller decides — the cache wipes and rebuilds, the queue migrates]),
)

== Order-preserving keys

#codepanel(title: "Key trait")[
```rust
pub trait Key: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(input: &mut &[u8]) -> Result<Self>;
}
// prefix!(acct, mailbox) builds a KeyPrefix from leading tuple components
```
]

Encodings follow the FoundationDB tuple style, so #key[byte order equals logical order]:

#dtable(
  columns: (auto, 1fr),
  ("Type", "Encoding"),
  ([#cd[u32], #cd[u64]], [Fixed-width big-endian.]),
  ([#cd[i64]], [Big-endian with the sign bit flipped.]),
  ([#cd[String], #cd[&str], `Vec<u8>`], [Bytes with #cd[0x00] escaped as #cd[0x00 0xFF], terminated by #cd[0x00 0x01]. Any string is allowed, and a prefix sorts before its extensions.]),
  ([Tuples (up to 5)], [Components' encodings concatenated, so #cd[prefix!(a, b)] selects every #cd[(a, b, …)].]),
)

== Value codecs (where compression lives)

#codepanel(title: "Per-table codec")[
```rust
pub struct ValueCodec { pub serde: Serde, pub compress: Compress }
pub enum Serde    { Json, Postcard, Raw /* Vec<u8> as-is */ }
pub enum Compress { None, Lz4 { min_size: usize } }
```
]

- The stored value is a #key[1-byte header] followed by the payload. Header bit 0 means lz4
  (#cd[lz4_flex], original size prepended); the other bits are reserved — the obvious next use
  is an encryption stage.
- A record is compressed only if it reaches #cd[min_size] #key[and] actually shrinks, so the
  decision is per record without any KV flag bits.
- Cargo features: #cd[lz4] and #cd[json] on by default, #cd[postcard] optional.

#callout(kind: "trap", "Failure modes to test")[
  Encoded key order equals tuple order for random tuples, and #cd[decode(encode(k)) == k] —
  including strings that contain NUL. A scan of one table never sees another's keys. A schema
  mismatch is caught on reopen. A corrupt lz4 payload surfaces as an #cd[Error], never a panic;
  an incompressible value is stored plain.
]

= email-mcp: two stores <sec-stores>

== One invariant instead of batches <sec-invariant>

#dtable(
  columns: (auto, auto, 1fr),
  ("Store", "Directory", "Role and durability"),
  ([#key[queue]], [`~/.local/share/email-mcp/queue`], [Source of truth for pending changes. Must not be lost: #cd[should_sync_on_put = true].]),
  ([#key[cache]], [`~/.cache/email-mcp/cache`], [A view that can be rebuilt; deleting it costs a resync. #cd[should_sync_on_put = false], synced on a timer.]),
)

#callout(kind: "ok", "The invariant")[
  #key[cache = server state as of the last sync + every pending command's #cd[apply_local].]
  Every #cd[apply_local] is idempotent, so the cache can always be repaired by replaying the
  queue. This replaces cross-store atomic batches.
]

#dtable(
  columns: (auto, 1fr),
  ("Path", "Ordering, and what a crash leaves"),
  ([Enqueue], [(1) Put the #cd[Command] in the queue and fsync — #hot[the commit point]. (2) Apply its local effect to the cache. A crash between the two is repaired at startup, when the cache replays every pending command.]),
  ([Sync], [Write fetched messages, flag changes and expunge deletions; replay pending commands for the mailbox; write #cd[MailboxState] #key[last]. #cd[MailboxState] (uidnext, highestmodseq, last_sync) is the checkpoint: after a crash mid-sync, the old checkpoint makes the next sync redo the work, and every write is an idempotent put or delete.]),
  ([UIDVALIDITY change], [Message keys include uidvalidity, so old entries become unreachable the moment the new #cd[MailboxState] is written; they are cleared afterwards with #cd[delete_prefix]. Commands queued against the old value become #cd[Failed].]),
  ([Worker], [(1) Mark the command #cd[InFlight] (fsync). (2) Execute over IMAP. (3) Verify by reading back. (4) Delete it. A crash after (2) leaves the outcome unknown; the command is verified first and re-run only if needed: #key[at-least-once] (§ 5.3).]),
  ([Corruption or #cd[SchemaMismatch]], [Delete the cache directory, replay the queue, sync on demand. The queue is never wiped.]),
)

#callout(kind: "trap", "At-least-once and duplicates")[
  Re-running #cd[SetFlags], #cd[Move] or #cd[Expunge] is harmless: UID commands ignore UIDs that
  no longer exist. #cd[Copy] and #cd[Append] could duplicate, so an #cd[InFlight] one whose
  outcome is unknown first searches the destination for its Message-ID before retrying — the
  same check used to verify it (§ 5.3).
]

== Tables

#key[Cache store]

#dtable(
  columns: (auto, auto, 1fr, auto),
  ("Table", "Key", "Value", "Codec"),
  ([#cd[mailboxes]], [#cd[(acct, mailbox)]], [#cd[MailboxState { uidvalidity, uidnext, highest_modseq, exists, unread, special_use, last_sync }]], [json]),
  ([#cd[mailbox_list]], [#cd[(acct)]], [`Vec<MailboxInfo>` — the cached LIST], [json]),
  ([#cd[envelopes]], [#cd[(acct, mailbox, uidv: u32, uid: u32)]], [#cd[Envelope] (§ 4.3). Written once per message.], [json + lz4]),
  ([#cd[flags]], [same], [#cd[FlagState { flags, modseq, gmail_labels }]. Small; rewritten on every flag change.], [postcard]),
  ([#cd[bodies]], [same], [Raw RFC 822 bytes. Written once, on first read.], [raw + lz4]),
  ([#cd[text] #hot[optional]], [same], [Plain text extracted from the body at first read, for local body search.], [raw + lz4]),
)

#key[Queue store]

#dtable(
  columns: (auto, auto, 1fr, auto),
  ("Table", "Key", "Value", "Codec"),
  ([#cd[commands]], [#cd[(seq: u64)]], [#cd[Command { seq, account, created, attempts, state, op }] — see § 5], [json]),
)

lz4 tables use #cd[min_size = 512]. The next #cd[seq] comes from a reverse scan with limit 1 at
startup. Newest-first listing is a prefix query on #cd[envelopes]
(#cd[prefix!(acct, mbox, uidv)], reverse, limit 20) plus one #cd[flags] get per row; listing
is in UID order, which is roughly arrival order.

#callout(kind: "note", "Why UIDVALIDITY is in the key")[
  A UID only identifies a message together with its mailbox's UIDVALIDITY. Putting it in the key
  means a UIDVALIDITY change can never make a stale entry look current — the old entries simply
  live under a prefix nothing reads.
]

== What is stored per message <sec-message>

#codepanel(title: "Envelope — immutable, parsed at fetch time")[
```rust
struct Envelope {
    message_id: Option<String>,
    date: Option<i64>,            // Date header, UTC seconds
    internal_date: i64,           // server arrival time
    size: u32,                    // RFC822.SIZE
    from: Vec<Addr>, to: Vec<Addr>, cc: Vec<Addr>, bcc: Vec<Addr>,
    reply_to: Vec<Addr>,
    subject: String,              // RFC 2047-decoded
    in_reply_to: Option<String>,
    references: Vec<String>,
    attachments: u16,             // counted from BODYSTRUCTURE
    gmail: Option<GmailIds>,      // X-GM-MSGID, X-GM-THRID
}
struct Addr { name: Option<String>, email: String }   // email lower-cased for matching
struct FlagState { flags: Vec<String>, modseq: Option<u64>, gmail_labels: Vec<String> }
```
]

#dtable(
  columns: (auto, 1fr),
  ("Field(s)", "Needed by"),
  ([#cd[from], #cd[to], #cd[cc], #cd[subject], #cd[date]], [Listing, and local #cd[search_messages] criteria. Stored #key[parsed and decoded] — raw header strings would not match by display name, case-insensitively, or through #cd[=?UTF-8?…?=] encoding.]),
  ([#cd[reply_to], #cd[in_reply_to], #cd[references], #cd[message_id]], [#cd[reply_to_message] and #cd[forward_message] headers, threading, and the duplicate check for re-run #cd[Copy] / #cd[Append] (§ 4.1).]),
  ([#cd[bcc]], [Sent and Drafts messages.]),
  ([#cd[attachments]], ["Has attachment" in listings and search.]),
  ([#cd[gmail]], [Gmail threads and labels.]),
)

#callout(kind: "ok", "Why envelopes and flags are separate tables")[
  yabiir is append-only: every update writes a whole new copy of the value. With one combined
  record, marking 500 messages read would rewrite 500 compressed envelopes of about 1 KB each.
  Split, a #cd[SetFlags] — queued, replayed, or picked up by #cd[CHANGEDSINCE] — writes only the
  tiny #cd[flags] records, and envelopes are written exactly once.
]

#key[Bodies stay raw.] The raw RFC 822 message is the only representation that can rebuild every
other view, so text, HTML and the attachment list are derived from it on each read with
mail-parser, as #cd[read_message] does today. Two optional additions:

- #key[#cd[text] table] — plain text extracted once at first read, so body-text search can run
  locally instead of going to the server.
- #key[Body-cache size cap] #hot[deferred] — raw bodies include attachments, so a 20 MB
  attachment is a 20 MB cached body. A configurable cap (e.g. #cd[max_body_cache_mb]) evicting
  least-recently-read bodies would keep the cache bounded; envelopes and flags would never be
  evicted. Not in v1: the cache grows with what is read, and #cd[cache_status] reports its size.

== Search and indexes <sec-indexes>

In v1 a local search #key[scans one mailbox]: a prefix query over its #cd[envelopes], joined
with #cd[flags], filtered in memory. For the 16,720-message INBOX that is about 16k small
records — expected to be tens of milliseconds, to be confirmed by measurement.

#callout(kind: "info", "Indexes may be needed in the future")[
  If scans prove too slow — very large mailboxes, cross-mailbox search, or frequent
  date-range and unread queries — #key[secondary indexes] can be added as ordinary tables in the
  cache store, e.g. #cd[by_date (acct, mbox, date, uid)], #cd[by_sender (acct, email, mbox, uid)]
  and #cd[unread (acct, mbox, uid)]. They are maintained by the same idempotent writes as the
  primary tables and, like the rest of the cache, can be rebuilt from scratch. Whether index
  maintenance belongs in #cd[yabiir-tables] (declared indexes) or in email-mcp is left open
  until a need is measured.
]

== Sync <sec-sync>

#cd[sync_mailbox(account, mailbox)] in #cd[src/cache/sync.rs]:

+ #cd[select_condstore] when the server advertises CONDSTORE, otherwise #cd[select]\; compare
  UIDVALIDITY with the cached #cd[MailboxState].
+ #key[New mail:] #cd[UID FETCH {uidnext}:\*] with an expanded summary query, in chunks of 500,
  writing #cd[envelopes] and #cd[flags]. The first sync of a big mailbox goes newest-first in
  windows, so listing works almost immediately; older messages backfill in the background.
+ #key[Flag changes:] with CONDSTORE, #cd[UID FETCH 1:\* (FLAGS) (CHANGEDSINCE modseq)]\; without it,
  all flags (about 1 MB for 16k messages). Only #cd[flags] is written.
+ #key[Expunges:] #cd[UID SEARCH ALL] diffed against the cached UIDs; remove what's gone from every
  message table.
+ #key[Replay] pending commands that touch this mailbox, so a sync never resurrects something the
  agent just deleted.
+ Write #cd[MailboxState] — the checkpoint, always last.

#codepanel(title: "Expanded summary query (today's is at src/imap.rs:185)")[
```text
(UID FLAGS MODSEQ RFC822.SIZE INTERNALDATE BODYSTRUCTURE
 BODY.PEEK[HEADER.FIELDS (FROM TO CC BCC REPLY-TO SUBJECT DATE
                          MESSAGE-ID IN-REPLY-TO REFERENCES)])
+ X-GM-MSGID X-GM-THRID X-GM-LABELS            on Gmail
- MODSEQ                                       without CONDSTORE
```
]

#key[When it runs.] A read tool syncs first if #cd[last_sync] is older than #cd[freshness_secs]
(60 s, one value for every mailbox)\; a #cd[refresh: true] parameter forces it. A background task syncs each account's
INBOX every 5 minutes. The mailbox list is cached with the same freshness rule.

= The command queue <sec-queue>

== Commands

#codepanel(title: "src/cache/queue.rs — sketch")[
```rust
struct Command { seq: u64, account: String, created: i64,
                 attempts: u32, state: State, op: Op }

enum State { Pending, InFlight, Failed { error: String } }

enum Op {
    SetFlags { mailbox: String, uidvalidity: u32, uids: Vec<u32>,
               add: Vec<String>, remove: Vec<String> },
    Move     { from: String, uidvalidity: u32, uids: Vec<u32>, to: String },
    Copy     { from: String, uidvalidity: u32, uids: Vec<u32>, to: String },
    Expunge  { mailbox: String, uidvalidity: u32, uids: Vec<u32> },
    Append   { mailbox: String, flags: Vec<String>, raw: Vec<u8> },
}
```
]

Each #cd[Op] has an #key[idempotent] #cd[apply_local(&CacheTables)], used at enqueue, at
startup replay and during sync:

#dtable(
  columns: (auto, 1fr),
  ("Op", "Local effect"),
  ([#cd[SetFlags]], [Rewrite the #cd[flags] records only.]),
  ([#cd[Move]], [Remove envelope, flags, body and text from the source. Nothing in the destination yet — its UID is unknown until the server answers; the destination fills in on its next sync.]),
  ([#cd[Copy]], [None until the destination syncs.]),
  ([#cd[Expunge]], [Remove envelope, flags, body and text.]),
  ([#cd[Append]], [None until the mailbox syncs (used for the Sent copy).]),
)

== Worker

One tokio task, woken through #cd[Notify] on enqueue, runs commands #key[FIFO per account] —
later commands may depend on earlier ones (flag, then move) — while different accounts proceed in
parallel. It runs each command through the existing #cd[imap::\*] functions under
#cd[App::with_session], after selecting the mailbox and #key[checking UIDVALIDITY still matches].

#dtable(
  columns: (auto, 1fr),
  ("Outcome", "Action"),
  ([Confirmed], [The server answered #cd[OK] #key[and] the read-back verification passed: delete the command from the queue, mark the destination mailbox stale, report #cd[done].]),
  ([Unverified], [#cd[OK] but the read-back disagrees (the request was dropped or only partly applied): treat as transient and retry — every op is safe to repeat once verified.]),
  ([Transient error], [Network, timeout, dropped session: back to #cd[Pending], exponential backoff 1 s → 5 min, #cd[attempts += 1]. The outcome is unknown, so the next attempt #key[verifies before executing].]),
  ([Permanent error], [#cd[NO] response, UIDVALIDITY mismatch, missing mailbox: mark #cd[Failed], then resync the mailbox so the cache reverts to server truth. Replay skips failed commands.]),
)

== Confirmation and verification <sec-confirm>

Large providers are known to drop requests: a session stalls, a command never gets a tagged
response, or an #cd[OK] arrives for a change that does not stick. So #key[#cd[done] means the
server confirmed the change and a read-back agrees] — nothing less. Until then the change is
applied in the cache but reported as #cd[queued].

#dtable(
  columns: (auto, 1fr),
  ("Op", "Verification (read-back)"),
  ([#cd[SetFlags]], [#cd[UID FETCH {uids} (FLAGS)]: every UID has the added flags and none of the removed ones.]),
  ([#cd[Move]], [#cd[UID SEARCH UID {uids}] in the source returns nothing, and the destination's Message-IDs include the moved ones.]),
  ([#cd[Expunge]], [#cd[UID SEARCH UID {uids}] returns nothing.]),
  ([#cd[Copy], #cd[Append]], [The destination contains the Message-ID(s) (#cd[UID SEARCH HEADER Message-ID …]).]),
)

- #key[Timeouts.] Every IMAP command the worker issues has a deadline (default 30 s). On expiry
  the session is dropped and reopened — a stalled connection never blocks the queue — and the
  attempt counts as transient with an unknown outcome.
- #key[Verify before retry.] Whenever an attempt's outcome is unknown (timeout, dropped
  connection, crash while #cd[InFlight]), the worker verifies first. If the change already took
  effect, the command is confirmed without being re-sent; this is also what prevents duplicate
  copies and appends.
- #key[What the agent hears.] After the 5 s wait the tool returns #cd[done] (confirmed),
  #cd[failed] (permanent error, with the reason) or #cd[queued] (applied locally, not yet
  confirmed — retrying in the background). The server instructions tell the agent not to report
  a #cd[queued] change as done, and to check #cd[pending_commands] later.

#callout(kind: "note", "Not queued")[
  SMTP sending, mailbox create / delete / rename, account and login tools, #cd[gmail_labels], and
  drafts (#cd[send_draft] needs the new UID back) stay synchronous. Only the #key[Sent copy] after
  a send goes through the queue.
]

= Tools, lifecycle, security, testing <sec-ops>

== Tool changes

#dtable(
  columns: (auto, 1fr),
  ("Tool(s)", "Behaviour with the cache"),
  ([#cd[list_messages], #cd[list_mailboxes], #cd[read_message]], [Served from the cache after a freshness check; #cd[read_message] stores the body on first read. New #cd[refresh] parameter.]),
  ([#cd[search_messages]], [Header, flag and date criteria run locally. Body-text search and #cd[gmail_query] still go to the server.]),
  ([#cd[update_flags], #cd[move_messages], #cd[copy_messages], #cd[delete_messages]], [Enqueue, wait up to 5 s, return #cd[{ status: done | failed | queued, commands, error? }]. #cd[done] only once the server has confirmed and the read-back agrees (§ 5.3).]),
  ([#cd[send_email], #cd[reply_to_message], #cd[forward_message]], [SMTP directly, as now; the Sent copy is queued.]),
  ([#cd[pending_commands] #hot[new]], [Queued, in-flight and failed commands.]),
  ([#cd[retry_command] #hot[new]], [Re-arm a failed command.]),
  ([#cd[discard_command] #hot[new]], [Drop it, then resync the mailbox to undo its local effect.]),
  ([#cd[sync_mailbox] #hot[new]], [Force a sync.]),
  ([#cd[cache_status] #hot[new]], [Store sizes, last sync per mailbox, queue depth.]),
)

The server instructions gain two lines: changes apply locally at once and survive restarts, but
are #cd[done] only once the server confirms them — a #cd[queued] change must not be reported to
the user as done — and failures show up in #cd[pending_commands]. The #cd[remove_messages] guidance is unchanged — move to
trash or delete, per account — both are now queued.

== Lifecycle and configuration

- #cd[App] gains `cache: Option<Arc<CacheDb>>` and `queue: Option<Arc<QueueDb>>`, thin async
  wrappers over #cd[yabiir-tables] via #cd[spawn_blocking]. #cd[None] means today's direct-IMAP
  path, kept as the fallback.
- New #cd[[cache]] config section: #cd[enabled = true], #cd[freshness_secs = 60].
- If either store is locked by another process, log a warning and run uncached and unqueued. The
  single-instance assumption holds until the daemon exists.
- #cd[remove_account] deletes the account's commands from the queue #key[first], then
  #cd[delete_prefix(prefix!(acct))] on every cache table.
- Each store runs #cd[merge()] independently, hourly, in #cd[spawn_blocking].

== Security

#callout(kind: "trap", "Mail at rest is plaintext")[
  lz4 is not encryption. Cached headers, bodies and queued Sent copies are protected only by
  0700/0600 permissions. The cache now lives under `~/.cache`, which some backup tools copy;
  the README should say so. Encryption at rest — a codec stage in #cd[yabiir-tables] with the key
  in the OS keyring — is future work.
]

== Testability and verification

The IMAP operations sync and the worker need go behind a small #cd[MailServer] trait — the real
implementation wraps #cd[imap::\*] — so queue, replay and sync logic run against an in-memory fake
with injected failures and simulated crashes.

#dtable(
  columns: (auto, 1fr),
  ("Where", "What"),
  ([yabiir], [Proptest model with scans; keydir benches, old hash vs new ordered, for the changelog.]),
  ([email-mcp verification], [Against the fake server: an #cd[OK] that silently drops the change is retried, never reported #cd[done]\; a timeout followed by a change that did apply is confirmed without being re-sent; no duplicate Copy or Append.]),
  ([yabiir-tables], [Key-order property tests; table isolation; codec round-trips; schema mismatch.]),
  ([email-mcp crash ordering], [Kill between queue put and cache apply → replay on reopen repairs the cache. Kill mid-sync → next sync converges. Kill after the IMAP call, before dequeue → no duplicate Copy.]),
  ([email-mcp unit], [Envelope parsing: encoded subjects and display names decode, addresses match case-insensitively; a #cd[SetFlags] writes only #cd[flags]\; #cd[apply_local] is idempotent; replay never resurrects deleted mail; worker retry / failure paths against the fake.]),
  ([Manual, me.com], [Delete a few messages: they vanish at once, status #cd[done] within seconds, Apple Mail agrees. Network off: #cd[queued], drains after reconnecting. Delete `~/.cache/email-mcp`: nothing queued is lost.]),
)

= Phasing and future work <sec-future>

== Phasing

+ #key[yabiir 0.6.0] — ordered keydir and scans; publish.
+ #key[yabiir-tables 0.1.0] — Db, tables, key encoding, codecs, catalog; publish.
+ #key[Read cache] — cache store, sync, cached read tools, #cd[cache_status], #cd[sync_mailbox].
+ #key[Queue] — queue store, worker, replay, the mutating tools, #cd[pending_commands] / #cd[retry_command] / #cd[discard_command].
+ #key[Polish] — background INBOX sync, merge scheduling, README and #cd[--describe] review.

== Future work

- #key[Shared daemon] for several concurrent agent sessions — the near-term follow-up.
- IMAP IDLE push instead of 5-minute polling.
- QRESYNC / VANISHED for cheap expunge detection (needs an async-imap extension).
- Placeholder destination entries so a move shows up in the destination before its sync.
- An offline SMTP outbox.
- Body prefetch, and the body-cache size cap with LRU eviction (§ 4.3).
- Per-mailbox freshness, if 60 s everywhere proves wrong for large archives.
- #key[Secondary indexes] (§ 4.4), if measured scans are too slow.
- An encryption codec stage in #cd[yabiir-tables].
- Per-table lz4 dictionaries trained on mail headers.
- Coalescing queued commands (e.g. a flag set then cleared).
- Transactions in #cd[yabiir-tables], if a use ever needs real atomicity.

#callout(kind: "info", "Open for the next review round")[
  - Should the optional #cd[text] table (local body-text search) be in v1, or deferred like the
    body-cache cap?
]
