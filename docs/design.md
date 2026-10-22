# Shep: design sketch

Ent-style graph data as a shared service, in Rust, on its own replicated
log (`shep-raft`, a multi-raft forked from openraft) and LSM storage
(SlateDB).

Status: fifth revision. The second revision replaced the single ordered
log with dotted version vectors, change queries, and materialized queries.
The third added app-declared operations, live queries computed in two
phases, ad-hoc access for people, node keys, admission control, files,
text search, actors, and universes addressed by domain. The fourth made
the developer's surface smaller: one identity word (actor), indexes
derived from operations, placement separate from keys, one way to follow
changes, and freshness checks. This one replaces Postgres with Shep's own
storage, and changes nothing apps see:

- **The log is the database.** Each partition's write-ahead log is a Raft
  group with its own log, and a dot is a position in it. Dotted version vectors
  stay the core of consistency: every fact is stamped with a dot, and a
  fact is visible at a version exactly when its stamp is in it. Holes,
  voids, and the frontier machinery are gone.
- **Materializers** apply the logs into SlateDB, an LSM tree on object
  storage, and serve reads at any causally closed version. Index
  tombstones and per-field dots come from keeping stamped versions in the
  keys.
- **Concurrency control is optimistic,** validated by each partition's
  leader. Every read behind a decision that could go stale is validated;
  the planner proves the rest safe from declared disciplines and CALM-style
  analysis, and skips them (§7.6). Mutations that span placement groups
  use two-phase commit, which is one dot.
- **Placement is static.** A node's partition comes from its key, groups
  split only at their boundaries, and whether an operation needs
  two-phase commit is decided at deploy.
- **Every component runs in one binary or many:** the WAL cluster,
  materializers, search indexers, view maintainers, and front ends. One
  binary on one server, with replication factor 1, is a supported
  deployment that loses only fault tolerance (§9.6).

A review of this revision simplified it further: one rule for consistent
versions (causal closure over stamps), derived per-node metadata, no
placement directory, and splits only at group boundaries. Later passes
added a consistency planner (§7.6), replaced per-schema actor mappings
with credentials that carry who acts for whom (§6.3), and replaced the
schema language, the Rust derives, CEL, and the policy language with one
language, ShepQL, a dialect of EdgeQL (§4.0). With it, policies combine as
union minus deny instead of ordered rules (§6.1).

The document records proposed decisions and the reasons for them. Open
questions are collected at the end.

---

## Overview

Shep is a graph data service for an organization's internal apps. There is
one shared graph of nodes and edges, described by versioned
**schemas** that apps own and share, under declarative privacy policies.
Schemas, policies, and operations are written in one language, ShepQL, a
dialect of EdgeQL (§4.0).

- **Schemas are shared; operations are each app's own.** Each app declares
  the queries, live queries, and mutations it runs in its own **operation
  set**, against pinned schema versions. Shep plans and prices every
  operation before any request arrives, and changing an app's operations
  never changes a schema. People can also query ad hoc from the CLI, as
  themselves.
- **Every response states the version it reflects.** Shep answers "what
  changed since this version" and keeps live queries current, applying
  each subscriber's privacy rules.
- **Admission control** keeps any one app, operation, or person from
  overloading the shared service.
- **Large content lives in an object store.** A built-in `File` type
  tracks each upload's state and gives readers download links and
  searchable text.

```
  Rust app backends            Browser apps (JS/TS)              People
  SDL + shep::query!           served JS / GraphQL               shep CLI, GraphiQL
         │                            │                                │
         │ declared operations        │ declared operations            │ ad-hoc queries,
         │ live queries (HTTP/2)      │ live queries (WebSocket)       │ as themselves
         ▼                            ▼                                ▼
 ┌──────────── Front ends: N instances, soft state only (sessions, live-query topics) ────────────┐
 │ token verification │ admission control: measured resources, priorities, budgets, shedding     │
 │ registry + checker │ privacy │ planner │ mutation executor │ delta engine │ live queries       │
 └───────┬───────────────────────────────┬─────────────────────────────────────▲──────────────────┘
         │ propose: delta + read set     │ reads at a version                  │ tail the logs;
         ▼ (2PC across partitions)       ▼ (at_least V)                        │ view, search reads
 ┌─── WAL cluster (shep-raft) ──┐  ┌─── Materializers (SlateDB) ───┐  ┌────────┴──────────────────┐
 │ one Raft group per partition │  │ facts, edges, local indexes,  │  │ view maintainers (DBSP)   │
 │ 3 replicas; the leader       │  │ ghosts, per-field dots;       │  │ search indexers (Tantivy) │
 │ validates and appends        │  │ reads at any version          │  │ each with its version     │
 │ dot = (partition, index)     │  │                               │  │                           │
 └───────────────┬──────────────┘  └───────────────▲───────────────┘  └───────────▲───────────────┘
                 ├─────────────────────────────────┘
                 └── committed entries, in log order ─────────────────────────────┘

 Object store (S3): archived log segments, SlateDB SSTs and checkpoints, search segments, blobs (§16)
 WAL controller (Raft, every WAL node): nodes, universes, partitions, replica sets (§9.6)
 Meta partition per universe (Raft): registry (versions, policies, operation sets), barriers, frontiers
 Auth service (outside Shep): identities, act-for consent, credentials; issues the tokens Shep verifies
 Every component also runs in one binary on one node, replication factor 1: in production or in tests (§9.6)
```

**Major capabilities**

| Capability | In brief | Section |
|---|---|---|
| Versioned schemas | Apps register schema versions. Storage follows the newest version, and proven adapters keep every older client's interface working, with lossy reads, hidden rows, and rejected writes where a change requires them | §4 |
| Cross-schema links | A schema links to another schema's node types at a pinned version | §5 |
| Privacy | Allow and deny policies combined as union minus deny, with total guards, compiled to relations, so Shep can also answer "who can see this?" | §6 |
| Operation sets | Each app declares its queries, live queries, and mutations against pinned schema versions, apart from the shared schemas. People query ad hoc from the CLI, as themselves | §7.1, §7.2 |
| Language | ShepQL, an EdgeQL dialect, for schemas (SDL), checks, policies, adapters, queries, and mutations, type-checked offline | §4.0 |
| Clients | Rust (types generated from SDL, `shep::query!`), the `shep` CLI for every language, served JS modules and GraphQL for browsers | §4.1, §7 |
| Transactions | Conditional multi-step mutations, declared in the app's operation set and run atomically on the server, with two-phase commit when they span placement groups | §8 |
| Versions | A dot is a position in a partition's replicated log, every fact is stamped with one, and a version is a dotted version vector. Every response states the version it reflects, and client libraries track them for the app | §9 |
| Mutation log | Every commit as a Z-set delta with its dot and causal context, in a write-ahead log replicated with Raft. It is the source of truth: storage is materialized from it, and it is replayable and archived to S3 | §3, §10 |
| Change queries | "What changed in Q between V1 and V2". Apps use them through live queries, and app-owned fan-out through `shep::fanout` | §11 |
| Materialized queries | Maintained incrementally with DBSP. Slightly behind the log, always tagged with their exact version | §12 |
| Live queries | Clients subscribe to declared live queries, or poll the same stream as a feed. Shep computes each change once per topic, then applies each subscriber's privacy rules | §13 |
| Universes | Isolated graphs: production, plus short-lived dev and test universes | §14 |
| Admission control | Measured resources, priority classes, and budgets keep the shared service from overloading; per-app quotas are open | §15 |
| Files | Large content in an object store, behind a built-in `File` type with upload states, download links checked on use, and extracted text. The same bytes are never stored twice | §16.1–§16.5 |
| Text search | Word and substring search over text fields and files' text, on Rust search indexers (Tantivy) that follow the log. Same privacy rules as any read | §16.6 |

**How a request flows**

- **A mutation:**
  1. It arrives at any front end, names a declared operation, and is
     admitted against that operation's cost (§15).
  2. The front end reads what it needs from materializers at a version,
     evaluates its reads and guards, applying validation, privacy rules,
     and adapters, and computes its Z-set delta and read set.
  3. It proposes the delta to the WAL leader of the partition that holds
     the data. The leader checks that nothing the mutation read has
     changed since, and appends the entry to the partition's Raft log. Its
     position is the mutation's dot, which stamps everything it writes.
  4. It commits once a quorum of replicas has the entry. A mutation that
     writes, or must validate reads in, several placement groups uses
     two-phase commit instead, and is one dot all the same (§8).

  The response carries the results and a version token, plus the observed
  changes when the mutation declares `observe` (§11.3).
- **A read** is served by materializers at one causally closed version,
  which it returns as its token.
- **A subscription** stays on the front end that holds its connection.
  That front end tails the logs, computes each topic's change once,
  applies each subscriber's privacy, and pushes updates in version order
  (§13).
- **Materializers** tail the logs and apply entries in log order (§3).
  **View maintainers and search indexers** step from one causally closed
  version to the next (§9.7, §12, §16.6).

Nothing on the write path waits on a global sequence, so adding
partitions adds write capacity.

**What Shep deliberately doesn't do**

- **Run undeclared operations for apps.** Ad-hoc queries are for people.
- **Carry ephemeral signals** such as typing indicators and presence.
  Live queries carry graph data only.
- **Make cross-group mutations cheap.** They exist, through two-phase
  commit (§8), but placement keeps data that's written together in one
  group, and admission control prices them higher.
- **Multi-tenancy.** Shep is internal-only.
- **Vector search.** Text search matches words and substrings (§16.6).
- **Byzantine fault tolerance.** Shep trusts its own processes and
  machines, and survives crashes, not malicious nodes. Durability comes
  from Raft replication of the WAL and the archived log (§10.4), never from
  consensus among mutually distrusting parties. There are no blockchain or
  ledger features, no proof-of-anything, and no validator sets.

## 1. Goals

- **One shared graph, many applications.** Nodes and edges live in one
  physical store. Each application sees the graph through its own **schema**.
- **Untyped storage, typed access.** Storage does not know about
  application types. Schemas are registered with the service at runtime, and
  every read and write goes through a schema.
- **Versioned schemas that are provably compatible, forever.** Old clients
  may never upgrade. Storage always follows the newest version, and a new
  version is accepted only if every older version in service gets proven
  adapters (read and write transformations) that keep its interface
  working. That's an interface guarantee: old clients' reads can be lossy,
  rows can be hidden from them, and writes the new head can't represent
  are rejected (§4.2).
- **Cross-schema links pinned to a version.** `issues@3` can link to
  `users::User` *as defined by `users@5`*. That link keeps working for as long
  as `users@5` is in service, even after the users team ships v6, v7, and so
  on.
- **Rich validation.** Declarative constraints that the checker can reason
  about, with an escape hatch for arbitrary logic.
- **Declarative privacy.** Allow and deny policies on every node and edge
  type, combined as union minus deny, with default deny and guards that
  are always true or false (§6.1), evaluated for the viewer a request is
  for. Read policies and write policies are separate.
- **One language.** Schemas, checks, policies, adapters, queries, and
  mutations are all ShepQL, a dialect of EdgeQL that Shep owns (§4.0).
- **Server-side transactions.** Clients can't hold a transaction open, so
  conditional multi-step graph mutations are declared as single mutations
  in the app's operation set and run atomically on the server.
- **Declared operations, owned by apps.** Every query, live query, and
  mutation an app runs is declared in the app's operation set, so Shep can
  plan, check, and price it at registration (§7.1). Operation sets are
  separate from schemas: changing one never bumps a schema version. People
  can query ad hoc, as themselves, under the same policies (§7.2).
- **Consistent change tracking and live queries.** Every response states
  the version it reflects. Clients subscribe to declared live queries, and
  Shep computes each change once and applies each subscriber's privacy
  rules (§13). Apps that run their own fan-out can ask what changed since
  a version, on its own or atomically with a mutation (§11). A Slack clone, where every
  message, DM, and conversation is a graph node, is the reference case.
- **Protected from overload.** Every connection, request, and subscription
  is admitted against measured resources and runs within a budget (§15).
- **Horizontal scale.** Each component scales on its own: partitions
  split by size and load, each with its own Raft log, and materializers,
  search indexers, and front ends are added as needed. Front ends keep no
  durable state; their subscriptions are soft state that any front end
  can rebuild. Versions are vectors of per-partition log positions (§9),
  so no global sequence sits on the write path.
- **Materialized queries.** Queries maintained incrementally with DBSP.
  Each lags slightly but knows its exact version (§12).
- **Files and text search.** Large content, such as Slack uploads and
  PDFs, lives in an object store behind a built-in `File` type. Text
  fields and files' extracted text are searchable through Rust search
  indexers that follow the log, under the same
  privacy rules as any read (§16).
- **Target workloads.** These shape the scaling decisions more than chat
  does:
  - a Slack clone
  - **agent orchestration**: task state, task claims, and dashboards
  - **SCM metadata**: commits, files, PRs, and CI results, which means
    write-heavy ingestion and very large edge lists
  - possibly more
- **Rust first, but not Rust only.**
  - **Rust.** Most clients will be Rust, so the main interface is a
    first-class Rust client, with a Toasty-style API (`get_by_*`,
    `update!`, `.exec(&handle)`). Its types are generated from SDL by
    `shep-build`, and `shep::query!` type-checks operations at compile
    time, offline (§4.1, §7.1).
  - **Not Rust schemas.** Earlier revisions wrote Rust schemas with
    Toasty-style derive macros. They're dropped: one schema source is
    simpler, and a proc macro can read committed SDL but can't see
    sibling `#[derive]` structs, so offline query checking needs the
    schema as text (§4.1).
  - **Everyone else.** Apps in other languages, especially browser
    JS/TS apps, write the same `.esdl` and `.edgeql` files, and check and
    register them with the same `shep` CLI, a Rust binary installed with
    `cargo install`.
  - **Browsers** talk to GraphQL generated for each app's operation set.
    The service itself serves ergonomic JS client modules generated from
    the same operation set, so a browser app needs no npm package from
    Shep.

- **The log is the database.** Every committed change is logged as a
  Z-set delta, with its dot and causal context, in a replicated
  write-ahead log. Everything else, including the stored state Shep reads
  from, is materialized from it. Older entries are archived to object
  storage, and replaying the log in causal order rebuilds a universe
  (§3, §10).
- **Universes.** Production is one universe. Staging, dev, and tests get
  their own isolated, often short-lived universes. Each test universe
  is production's registry plus the candidate schema versions, policies,
  and operation sets under test.
  Apps are global across all universes.

**Internal only.** Shep serves one organization's own apps, and that
organization controls all the data. There is no multi-tenancy and no
personal-data regime (PII handling, right-to-erasure) to design around.

Non-goals for v1: cross-region replication, vector search, OCR,
Byzantine fault tolerance (Shep trusts its own processes and machines,
and survives crashes, not malicious nodes), and analytics queries beyond
materialized queries.

## 2. Vocabulary

**Terms app developers use.**

| Term | Meaning |
|---|---|
| **Schema version** | An immutable *view*: names, types, constraints, and edges over the node and edge types the schema owns or depends on. Shep numbers versions when they're registered. A version holds no operations and no indexes (§4.1.4, §7.1). |
| **Head** | The newest activated version of a schema. Stored data must always be valid for the head. A new version becomes the head at the end of its cutover (§4.2). |
| **In service** | A schema version is in service while any active operation set, policy, or client uses it, or any in-service schema version depends on it. Dormant operation sets don't count, but a version one pins isn't retired (§4.6). This set drives every compatibility decision. |
| **Adapter** | A proven pair of read/write functions between two consecutive versions. Chains of adapters give every older version its view of head-shaped storage (§4.3). |
| **Deprecated field** | A field removed from new versions' views but still declared, so its owner can set policies for the older versions that still use it (§4.1.4). |
| **Node key** | A node's identity within its type: generated, app-declared, composite, or a content hash. A key says nothing about where the node is stored (§3). |
| **ShepQL** | Shep's one language, a dialect of EdgeQL: SDL for schemas and policies (`.esdl`), queries and mutations for operations (`.edgeql`). Every difference from EdgeQL is specified and comes with a checker error (§4.0). |
| **Placement** | Which placement group a node is in. `constraint shep::placement on (.channel)` keeps a node with another one, so they're written in one partition without two-phase commit. A node's partition is computed from its key, with no lookup (§9.6). |
| **Actor** | Who is doing the work, as the token says: a node, such as a `users::User` signed in through SSO or an `agents::Agent` an app runs, or the calling app itself (`actor = app`, with no viewer). Read-your-writes and audit are per actor. Policies see it only in schemas that declare their permitted actors (§6.3). |
| **Actor handle** | A client-side value, such as `db.with_token(&token)` or `db.as_user(id).by(agent)`, that every request runs through. It presents a token whose claims name the actor and viewer, and tracks the actor's causal position (§7.3). |
| **Token** | A bearer JWT the external auth service signs and Shep verifies, naming the universe, app, actor, viewer, and scopes (§6.3). |
| **Viewer** | Whom a request is for. It defaults to the actor; a credential can name another, such as the user an agent acts for. Everything access-related is per viewer. A viewer not of a schema's viewer type is denied by that schema (§6.3). |
| **Policy** | An `access policy … allow\|deny <ops> using (<guard>)` on a node or edge type, or on a field. An operation is allowed when some allow's guard holds and no deny's does; guards are always true or false (§6.1). |
| **Function** | A named, typed, Pure ShepQL function in a policy file, exported to dependent policies and inlined when policies compile (§6.1). |
| **Check** | A constraint in the schema: a built-in such as `max_len_value(200)`, or `constraint expression on (…)` over the node's own fields. Empty means satisfied (§4.5). |
| **App** | A registered application. Apps live in one global namespace shared by all universes. An app owns schemas, policies, and operation sets; its name is registered in production's registry, and tokens name it (§6.3, §14.1). |
| **Universe** | A fully isolated instance of the graph (data, registry, policies, mutation log), such as production, staging, a dev environment, or a test run. Production is `prod` at `shep.internal`; every other universe is at `<universe>.shep.internal` (§14). |
| **Operation** | A query, live query, or mutation an app declares in a ShepQL `.edgeql` file or `shep::query!`, against pinned schema versions. Apps can run only declared operations, and Shep builds the indexes they need (§7.1). |
| **Primitive** | An operation every app gets for each node type it uses: get, create, update, delete, restore, link, unlink, paged edge loads, and uploads for file fields (§7.1). |
| **Walk** | A query step, `shep::walk`, that follows an edge repeatedly, up to a declared depth, such as a commit's ancestors through `parents` (§7.1). |
| **Discipline** | A declared contract on how a field, edge, or node type may change: `readonly` (frozen: never changes after create) or `extending shep::Permanent` (never deleted). Every writer is checked against it, and the planner uses it to skip coordination (§4.1.2, §7.6). |
| **Freshness check** | `shep::unchanged(ops::q(…), since := V)`: a test of whether a declared query's inputs changed since a version, answered from the dots on what it reads. A mutation can be conditioned on it (§11.7). |
| **Placeholder** | A node that something links to before it exists, such as a parent commit not imported yet. It reads as absent everywhere until an ordinary create fills it (§3). |
| **Operation set** | All of one app's declared operations, with the schema versions they're written against. Identified by content hash, not numbered, and registered apart from schemas (§7.1). |
| **Bookmark** | The one version token apps handle on the common path: an actor handle's causal position, serialized to carry read-your-writes across a process boundary (§7.3). |
| **Revision** | Which version of a node was read: the newest stamp among its keys, opaque, used for opt-in compare-and-swap (`if_revision`). Not a version token (§3). |
| **Live query** | A query marked `#! shep: live`. Clients subscribe to it, or poll a feed of the same changes (§13). |
| **Feed** | The polling form of a subscription: each poll returns what changed since the previous poll (§7.3, §13.1). |
| **Materialized query** | A query Shep maintains incrementally from the log, requested with `#! shep: materialize`, tagged with the version it reflects (§12). |
| **Audience** | Everyone who can perform an operation on a node: a compiled policy restricted to that node (§6.8). |
| **File** | A node of the built-in type `shep::File`, held by one file field of one node. Its content is a blob, and Shep tracks its state: `Uploading`, `Processing`, `Ready`, or `Failed` (§16.2). |
| **Blob** | Immutable content in the object store, identified by its SHA-256 and stored once per universe. Any number of files can point to one blob (§16.2). |
| **Upload** | A short-lived, private target for a file's bytes, bound to one app, one actor, and one file field. Attaching it to a node creates a file (§16.3). |

**Internals.** App code never handles these, but the rest of this document
uses them.

| Term | Meaning |
|---|---|
| **Node type** | Storage-level identity such as `users::User`. It is owned by one schema, never changes, and outlives any version of that schema. |
| **Edge type** | Storage-level identity for a kind of edge, such as `issues::Issue.assignee`. It is owned by the schema that declares it, which may differ from the schema that owns the nodes at either end. |
| **Field ID** | A stable numeric identity for a field within a node or edge type, like a protobuf field number. Names belong to versions. Field IDs belong to storage. |
| **Valid set `S(v)`** | The set of states that version `v` considers valid. |
| **Representable** | A stored value is representable for version `v` if reading it through `v`'s adapters and writing it back unchanged leaves it unchanged. Old-version writes to non-representable fields are rejected by default (§4.3). |
| **Partition** | A range of placement groups, with its own replicated log. Partitions split at group boundaries by size and load (§9.6). |
| **WAL** | The write-ahead log: one Raft group per partition, each with its own log, three replicas (one on a single-node deployment), run by `shep-raft`. The source of truth. Its leader validates and appends mutations (§3, §10.3). |
| **Materializer** | A process that applies partitions' logs into SlateDB and serves reads at any version (§3). |
| **Front end** | A process that holds sessions and live-query topics, plans and executes operations, and proposes mutations to the WAL. Soft state only (§9.6). |
| **Dot** | `(partition, index)`: a position in a partition's log. A mutation's dot is its entry's; a two-phase commit's is its decision's. One dot per mutation (§9.1). |
| **Stamp** | The dot recorded on each version of a stored key: the mutation that wrote it. A fact is visible at a version exactly when its stamp is in it (§3, §9.2). |
| **Version** | A dotted version vector: per partition, a contiguous prefix of its log plus extra dots beyond it. A usable version is causally closed (§9.2). Client libraries track versions inside actor handles and subscriptions. |
| **Frontier** | A recent causally closed version that the meta partition publishes, so derived consumers can step between versions without computing closure themselves (§9.7). |
| **Read set** | The keys and ranges a mutation read in a partition. The partition's leader rejects the mutation if anything with a stamp not in its read version wrote into them (§10.3). |
| **Context** | The version a mutation's snapshot contained: its causal past (§9.5). |
| **Mutation log** | A partition's log. A mutation's entry holds its context and a Z-set delta; other entries, such as two-phase commit records and registry barriers, stamp nothing but still carry contexts (§10). |
| **Fact** | One leaf value of a node, `(node, path, value)`; or a node's existence or deletion; or one edge. Base data is a set of facts (§10.1). |
| **Z-set** | DBSP's representation of a collection with integer weights. A delta is a Z-set of facts: `+1` for facts added, `−1` for facts removed (§10.1). |
| **Session** | A connection bound to one operation set at connect (§7.1). Ad-hoc sessions bind schema versions instead (§7.2). |
| **Change query** | The change in a query's result between two versions. Live queries are built on it, and `shep::fanout` exposes it (§11, §13.6). |
| **Live range** | The part of a live query's result that a subscription or feed holds: the first page, grown as the client pages (§13.1). |
| **Resync** | An update that replaces a client's copy of a result instead of changing it (§11.4, §13.1). |
| **Topic** | The subscriptions to one live query with the same query IR, arguments, and pinned schema versions, and the same app when its policies read `global shep::app` or the schemas grant the apps differently. Their shared change is computed once (§13.2). |
| **Search index** | One Tantivy index per node type and shard, kept by search indexers that tail the log, with a version like a materialized query. It holds one indexed field per (field, analyzer, adapted form) that some operation searches, and those indexed fields are what operations share, build, and pay for (§16.6). |
| **Hashed keyspace** | Partitions keyed by a hash, for data with no placement group of its own: global `exclusive` entries and upload records (§3). |
| **Consistency class** | How one read point is planned: skipped, validated, validated in another group through two-phase commit, or under the horizon. Decided at deploy and shown by `shep ops check` (§7.6). |

## 3. Storage model

Shep owns its storage. The source of truth is the **write-ahead log
(WAL)**: one replicated log per **partition**, run by `shep-raft`, Shep's
multi-raft (§17). Everything
else is derived from it. **Materializers** apply each partition's log into
SlateDB, an LSM tree on object storage, and serve reads from it; search
indexers (§16.6) and view maintainers (§12) follow the same logs.

```
  front ends ──── propose (delta, read set, read version) ────▶ WAL: one Raft group per partition
     ▲                                                          (3 replicas; leader validates and appends)
     │ snapshot reads at a version                                        │ committed entries
     │                                                                    ▼
  materializers: SlateDB per partition ◀── apply in log order ── tail ────┤
  (facts, edges, local indexes, ghosts)                                   ├──▶ view maintainers (§12)
                                                                          └──▶ search indexers (§16.6)
  object store: archived log segments, SlateDB SSTs and manifests, search segments, blobs (§16)
```

**The components** (§9.6 covers how they're deployed):

- **WAL.** One Raft group per partition, three replicas across failure
  domains, or one on a single-node deployment (§9.6). Each group has its
  own log, as in Redpanda; no write-ahead log is shared between a core's
  groups. A log entry is a committed mutation: its Z-set delta with old
  values, its context, and its intent metadata (§10.1), or another kind of
  entry, such as a two-phase commit record (§8) or a registry barrier
  (§10.3). The leader validates every proposed mutation before appending
  it (§10.3). There is no other write-ahead log: SlateDB's own WAL is off,
  because the Raft log already holds everything.
- **Materializers.** Each applies one or more partitions' committed
  entries, in log order, into one SlateDB database per partition, and
  serves snapshot reads, `at_least` waits, and freshness checks (§11.7).
  - **It derives what the log doesn't carry.** Deltas carry facts only.
    The materializer derives index entries and ghosts from them, using the
    index definitions in force at that point in the log (§10.3), and keeps
    each partition's causal frontier (§9.4).
  - **It records its progress** in the same SlateDB write batch as each
    entry, so after a restart it resumes from there (§10.4).
- **Object store.** Sealed log segments, SlateDB's SSTs and manifests,
  search segment copies (§16.6), and blobs (§16.2).

**Key layout.** One SlateDB database per partition, under the universe's
object-store prefix. Keys are ordered bytes. Every key ends with its
**physical version** `(gen, pos)`: the generation of the partition that
wrote it, which is its partition ID (§9.1), and the position of the entry in that partition's log,
inverted so the newest sorts first. Every value records its **stamp**:
the dot that makes it visible (§9.1). The database holds every version of
every key down to a retention floor:

```
f / type / node key / field id / ¬ver        → (stamp, a field's value)
n / type / node key / ¬ver                   → (stamp, lifecycle: created, deleted (when, by whom), placeholder)
l / edge type / src / dst / ¬ver             → (stamp, the link: its sort key and fields; or a ghost, unlinked)
e / edge type / src / sort key / dst / ¬ver  → (stamp, traversal order for the link; or a ghost)
i / edge type / dst / src / ¬ver             → (stamp, the inverse edge, for co-placed ends only, below)
x / index id / value / node key / ¬ver       → (stamp, a local index entry; or a ghost: the node left `value`)
t / gen / pos                                → the commit time of the entry at `pos` (for a release, its decision's)
m / applied                                  → the last log index applied
m / cf / ¬pos                                → the causal frontier cf(p, pos), kept down to the floor
```

- **Visibility and physical order are separate.** A version is
  *visible* at `V` when its stamp is in `V` (§9.2). Its *physical order*
  is `(gen, pos)`. Reading at `V` takes, for each key, the physically
  newest version whose stamp is in `V`.
- **The physical winner is the causally latest write in `V`.** That holds
  because of three facts:
  - one partition writes a key at a time (§9.5, condition 2)
  - writes to one key are causally ordered (§9.5, condition 3), and within
    one partition's log the later write has the later position
  - a split's children have a larger generation than their parent, and
    their writes are causally after the split entry (§9.6)

  So `(gen, pos)` order and causal order agree for every key, and a
  version whose stamp is in a causally closed `V` has every physically
  older version's stamp in it too.
  - **Position alone would be wrong.** A child's log starts again at 1, but
    it shares the parent's SSTs, which keep their keys. A parent write at
    position 100 would sort ahead of the child's later write at position
    2. The generation breaks that tie, and equal positions in different
    generations are distinct.
  - **Two-phase commit parts** are written at the participant's release
    entry, so their physical version is that entry's, while their stamp is
    the decision dot (§8).
  - **Proven.** `Lineage.lean` proves the winner rule through repeated
    splits (`winner_correct`), and that ordering by position alone is
    wrong (`localPos_unsound`). `Split.tla` (finding SR-1) checks it with
    checkpoint loading and compaction (§18).
- **Most stamps are the partition's own.** A single-partition mutation
  stamps its writes with its own entry's dot. A two-phase commit's writes
  are stamped with its home's decision dot, so they can come from another
  partition (§8).
- **The registry lives in the universe's meta partition**: schemas,
  versions, field IDs, dependencies, operation sets, policies and their
  pins (§6.6), node and edge types, and index definitions. The
  partition map is the WAL controller's (§9.6). It's an ordinary partition, with its own log, so every
  registry change is a log entry with a dot (§10.1).
- **Blob bytes aren't in SlateDB.** Blobs live only in the object store,
  keyed by their SHA-256, and a file's fields record its blob's hash
  (§16.2).

Proposed decisions:

- **One key per field.** A field's value is its own key, so changing one
  field writes one key, however wide the node is, and the stamp in that
  key is the field's **dot**: the mutation that last changed it.
  - **Reading a node** is a prefix scan over its keys, taking each key's
    newest version visible at the read's version.
  - **Nested data shares its field's key.** A map, set, embedded struct,
    list, or `Document` is stored whole under its top-level field, and the
    facts inside it come from diffing (§10.2). A change anywhere inside it
    moves the field's dot. That's coarser than the facts, and it can only
    make a freshness check answer "changed" when nothing a query read
    changed, never the reverse (§11.7).
  - **Every field's whole value has a size cap,** nested data included:
    64 KiB by default, raised with `@max_size` up to 1 MiB, and a node's
    fields together are capped at 4 MiB (§16.1). A field is one key, read,
    validated, logged with its old value, and pushed to subscribers whole,
    so the cap bounds each of those. Collections that grow without bound
    belong in edges, and large content in files.
  - **Writing the same value moves nothing.** A write that leaves a field
    unchanged logs no fact (§10.2), writes no key, and leaves its dot
    alone.
  - **Dots of a node, derived, never stored separately.**
    - **The lifecycle dot** is the stamp of the node's newest `n` version:
      creation, deletion, restore, and a placeholder's fill. Only those
      write `n`.
    - **An edge set's dot** is the newest stamp in its source's `l` range
      for that edge type, ghosts included.
    - **`revision`** is the newest stamp among the node's field and
      lifecycle keys. It's opaque, and two revisions of one node compare
      in causal order. `if_revision(r)` compares against it (§7.3).
    - **`updated_at`** is the commit time recorded at that version's
      physical position, in the node's own partition's `t` keys. A
      two-phase commit's release carries its decision's commit time, so
      this never needs another partition's keys (§8).

    Nothing is written to keep these current, so two updates to different
    fields of one node never touch a shared key.
  - **What dots are for.** Freshness checks (§11.7) and field-level
    compare-and-swap (§7.3).
- **Per-type key prefixes, indexes only where operations need them.**
  Every key starts with its type, so a type's data is contiguous within a
  partition. Indexes are the `exclusive` constraints a schema declares
  and the indexes Shep builds for declared operations
  (§7.1), as `x` keys in the partitions that hold the type.
  - **Index entries are derived, not proposed.** A mutation's delta
    carries facts. The materializer writes the index entries and ghosts
    they imply, under the index definitions in force at that entry: a
    registry change takes effect in a partition at its barrier (§10.3). So
    every index entry is a function of the facts and the registry, and
    replay reproduces it.
  - **What a write pays for.** A write maintains only the indexes on the
    type it writes. An index another app's operations need, on a type this
    app writes, does cost this app's writes. Shep charges the index's
    storage and build to the apps that use it (§15.1), and the write
    overhead shows in each operation's measured cost.
  - **Each index records its barrier position in every partition**: the
    index of the registry barrier that started it there. Ghosts exist only
    from then on (§11.7).
- **Uniqueness is decided where the data is.**
  - **Local, the default.** The checker requires every `exclusive`
    constraint to include its type's placement group: `exclusive on
    ((.project, .name))` on a `Label` placed with its project. All of a group's nodes are in one
    partition, so the unique index is an `x` range there, validated like
    any other read.
  - **Global, when it can't be local.** A constraint that can't include
    the group, such as a user's email, lives in
    the **hashed keyspace**: partitions keyed by a hash of (index, value),
    holding one entry per value. A mutation that writes such a value makes
    that partition a two-phase commit participant (§8), and `shep ops
    check` warns about it, with its cost.
  - **The same answers either way.** A create whose unique value has a
    live entry fails with `conflict`. An entry of a deleted node doesn't
    count. Deleting or restoring a node with a global unique updates its
    entry in the same two-phase commit.
- **A node's identity is its type plus a key.** The key is scoped to the
  node type, and every key in storage starts with both.
  - **Clients see only the key.** The type comes from context. Every
    operation parameter, result row, and edge is typed by the schema
    versions the client's session binds (§7.1). So a reference to a
    `Commit` travels as just its encoded key, and the server pairs it with
    the type it already knows. Numeric type IDs stay inside Shep, like
    every other storage identity.
  - **GraphQL is typed the same way.** Every field and argument fixes the
    type of the IDs it takes or returns, within the app's operation set.
    Shep doesn't generate Relay's untyped `node(id:)` root field. If an
    app ever needs Relay, `node(id:)` can be generated for that app alone,
    with IDs that encode the type name.
  - **Only an untyped lookup names the type.** Keys are unique only
    within a type, so a bare key pasted into `shep get` is ambiguous. There
    the person writes `Commit:<key>` (§7.2). Inside a typed query, the
    argument gives the type.
  - **Keys are ordinary fields.** A node type is keyed by the built-in
    `id`, or names its key fields with `constraint shep::key` (§4.1.1).
  - **Generated keys.** The built-in `id` is a UUID v7,
    which is time-ordered. For a type placed with another node, the
    generated key also carries its group's routing hash (§9.6).
    Auto-increment integers aren't offered: they would need a counter
    shared across partitions, which §9 avoids.
  - **App keys.** A key can be any hashable value or tuple of values: a
    string, bytes, an integer, a fixed-size hash, an enum, or a newtype
    over one of these. Floats, nullable values, lists, and documents can't
    be key components. An app-key type is placed by its key or a key
    field, never with another node, so its key alone routes it (§9.6).
  - **Key fields are frozen.** They can't change after a node is created.
    Across versions, the checker forbids adapters, type changes, and
    deprecation on key fields.
    Otherwise an old client would compute a different key for the same
    node.
  - **Composite keys.** `(repo, sha)` is a key made of two fields. It is
    identity; placement may use one of its fields (§9.6). A query over every
    commit of a repository is served by an index on `repo`, which Shep
    builds when an operation needs it.
  - **Content-addressed nodes.** A key can be a hash of the node's content,
    either one the app computes (a git SHA) or one Shep computes over
    declared fields. Identical content is then the same node, which makes
    ingestion idempotent. The hashed fields are frozen, and a hash Shep
    computes uses a canonical encoding fixed per node type.
    - **Create returns only the key.** It succeeds whether or not the node
      already existed, and never says which, so it can't reveal a node the
      actor can't read (§6.5).
    - **Creating content whose node is deleted** runs the `restore` policy
      for that actor. If the actor can't restore it, the create fails
      with `deleted` only when the read policy admits the actor (§6.5).
      Otherwise it fails with `denied`, the same error as any create the
      actor isn't allowed to make.
  - **Keys are never reused.** An app key stays bound to its node even
    after deletion, unlike `exclusive` fields, whose index entries for
    deleted nodes don't count. A future physical purge deletes the node's
    edges too, and leaves a key ghost with the purge's dot, so a later node
    with the same key never inherits them, and "absent now" still means
    "absent then" (§11.7).
  - **Creating an app key that's taken** fails with `conflict`, or with
    `deleted` when the node is deleted and the read policy admits the
    actor. Like any uniqueness constraint, this tells the caller the key
    is taken. A deleted node comes back only through `restore`.
  - **Placeholders: edges to nodes that don't exist yet.** Imported
    history arrives in any order: a child commit names its parent's SHA
    before the parent is imported.
    - **Opt in per type** with `extending shep::Placeholders`. Only
      types whose key the app supplies qualify: app keys, composite keys,
      and hashes the app computes, such as a git SHA. A type keyed by the
      generated `id`, or by a hash Shep computes, doesn't, since nobody
      can know its key before its content exists. Such types are placed by
      their key or a key field, so a placeholder's partition comes from its
      key. The checker enforces this.
    - **Creating one.** Linking to a key with no node creates the target
      as a placeholder. In ShepQL, a key cast names the target:
      `update c set { parents += <Commit>(repo := r, sha := s) }`. On a
      type without placeholders, the same cast to a missing key fails
      with `not_found`. That's the only way to create one, so every placeholder
      exists because something points to it. The source's update policy,
      which governs linking, covers it. If the target's group is placed
      apart from the source's, the link is a two-phase commit like any
      other write to two groups (§8); a repository's commits are placed
      together, so history import stays in one group.
    - **Reading.** A placeholder reads as absent everywhere, exactly like
      a hidden or deleted node (§6.5): in lists, single links, counts, walks
      (which stop at it), search, live queries, and audiences. No reader
      code changes, and no reader can tell a placeholder from a node
      that's absent, hidden, or deleted.
    - **Nothing can read `@placeholder`.** No policy, filter, or
      projection may read the placeholder fact, or test existence in a way
      that tells a placeholder from absence, and data facts never use the
      reserved lifecycle paths. In ShepQL, `exists`, `.__type__`, key
      casts, and every path read treat a placeholder as `{}`; ShepQL has
      no other way to reach the lifecycle facts. With that rule, every query returns the
      same result for a placeholder as for an absent key, and filling one
      produces the same change as creating it (`query_placeholder_eq_absent`
      and `fill_changes_eq_create` in `formal/lean`; for ShepQL, in both
      read contexts, `evalQ_placeholder_eq_absent` in
      `PlaceholdersQL.lean`).
    - **Filling.** An ordinary create with the placeholder's key fills it.
      Validation, checks, and the target's create policy run then, with
      the full data. Required edges and checks aren't enforced on the
      placeholder itself, only once it's filled. On a placeholder-enabled
      type, a create is:
      - on a placeholder: a fill
      - on a filled node with equal data: a no-op
      - on a filled node with different data: `conflict`

      This extends the content-addressed create above, which returns
      only the key and never says whether the node existed.
    - **Placeholders never become visible without a fill.** Unlinking the
      edge that created one leaves it in place, and a later fill still
      works. Nothing deletes or restores a placeholder.
    - **Shown to their owners.** `shep schema status` reports placeholders
      per type and their age, so an import that leaves orphans is
      visible. `shep ops check` notes operations that link to a
      placeholder-enabled type.
    - **Other uses.** A gitlink, a submodule's pinned commit, can be an
      edge to a placeholder in another repository's history.
  - **Edges hold keys.** An edge type fixes its source and destination
    types, so an edge key stores only the two keys.
  - **Changing a key is a new node type.** A type's key declaration is part
    of its storage identity. The compatibility checker rejects changing it,
    and no adapter can.
  - **Size limit.** An encoded key is at most 256 bytes, so keys stay cheap
    in edge and index keys.
- **Edges live with their source.** An edge's keys are in its source's
  partition.
  - **One link per pair.** The `l` key, `(edge type, src, dst)`, is the
    link's identity: whether it exists, its sort key, and its fields.
    Linking or unlinking reads and writes it, so two concurrent links of
    the same pair conflict, which is condition 3 of §9.5. Links into one
    source from different destinations touch different `l` keys and never
    conflict, so thousands of people can join one channel at once.
  - **Traversal order.** The `e` key orders a source's links by sort key,
    so a forward traversal is one range scan. The materializer derives it
    from `l`, as it derives index entries.
  - **Inverse edges, decided at deploy.** When an edge type's two ends are
    always in one placement group (the edge is the source's `with` edge,
    or both types share a group root), the materializer keeps `i` keys,
    and the inverse is as current as the forward direction. Otherwise the
    inverse direction is always a view of the logs (§12), maintained like
    any view and read at its version, as TAO maintains inverse
    associations, but with exact versions. The planner's choice comes from
    placement, so it never changes when partitions split.
- **Ghosts: what left still leaves a trace.** A freshness check has to see
  rows that left the range a query reads (§11.7), and the LSM keeps that
  trace naturally: a removal is a new version, not an erasure.
  - **Ghost index entries.** When an update moves a node's indexed value
    out of an index range, or deletes the node, the old `x` entry gets a
    ghost version with the moving mutation's stamp.
  - **Ghost edges.** Unlinking writes ghost versions of the link's `l` and
    `e` keys with the unlink's stamp. Linking it again writes live ones.
  - **Normal reads skip ghosts.** Only freshness checks visit them.
  - **Compaction.** Shep's compaction keeps, for each key, the newest
    version at or below the retention floor (live or ghost) and every
    version above it, and drops the rest (§10.4). That is exact for every
    causally closed version whose prefix of the partition reaches the
    floor: such a version contains every stamp written at or below the
    floor, because a release's context names its decision (§8,
    `FrontierMaterialize_relctx`). Compacting ghosts themselves further is
    open question 17.
  - **Splits** carry facts and ghosts with their original stamps (§9.6).
- **Nodes are never physically deleted (for now).** Deleting a node writes
  a new lifecycle version with `deleted_at`. Edges to and from it stay in
  place, and index entries of deleted nodes and placeholders are kept as
  ghosts, so they don't slow down hot lookups. §6.5 describes how reads
  treat deleted nodes. Physical purge comes later; §6.5 explains why it
  fits without a compatibility change.
- **The hashed keyspace.** Data that has no placement group of its own
  lives in partitions keyed by a hash: global unique entries (above) and
  upload records (§16.3). It's split like any other range (§9.6).
- **Every component runs in one process or many.** A single binary
  composes the components (WAL, materializer, search indexer, view
  maintainer, front end), with in-process transport and a local or
  in-memory object store, for tests and local runs. The distributed
  deployment runs the same code with one role per process (§9.6, §17).
  Where a component has real and test implementations, such as the
  transport or the object store, it's an enum with a real and a test
  variant, not a trait.
- **The WAL is thread-per-core.** The WAL service is a specialized Rust
  implementation of Redpanda's model, after Seastar: one tokio
  `current_thread` runtime per core, and minimal sharing. Each core owns
  its Raft groups, their logs, a per-core metadata store, and its peer
  connections; cores talk only through bounded channels. Core 0 hosts
  the WAL controller (§9.6).
  - **Raft is `shep-raft`,** Shep's own multi-raft, forked from openraft
    0.10.0-alpha.37. It keeps openraft's engine (elections, vote rules,
    log matching, commit, and joint consensus) and its runtime, which
    already pipelines appends to each follower, runs heartbeats as a
    separate command with a worker per follower, and tracks submitted
    I/O apart from flushed I/O. The fork changes only what Redpanda does
    differently: heartbeats batched per node pair in Shep's network
    layer, then lightweight heartbeats; recovery admitted by the follower;
    and the flush rules (commit over flushed offsets, a follower's commit
    clamped to its own flushed index, group flush). Each Raft group runs
    on its owning core's runtime (§9.9).
  - **Validation and append are one step.** A partition's leader
    validates a proposal and appends it in one step of its core's loop
    (§10.3), so no other append can come between them.
  - **A request on the wrong core hops once.** Each node assigns its
    replicas to cores (§9.6); a request that reaches a node on a core
    that doesn't own the target replica is forwarded once, over the
    node's bounded channels, to the owning core. This applies to the WAL service; the other
  components use an ordinary multi-threaded tokio runtime unless a
  section says otherwise.

## 4. Schemas, versions, and compatibility

This part is the core of the system and the hardest to get right, so it gets
the most detail.

### 4.0 The language: ShepQL

Shep has one language: **ShepQL, a dialect of EdgeQL**, the query language
of Gel (formerly EdgeDB). Schemas, checks, privacy policies, adapters,
policy tests, queries, and mutations are all written in it. EdgeQL fits
Shep's model closely: object types with links (Shep's nodes and edges),
nested result shapes, `insert`/`update`/`delete` with upserts, `assert`
with named errors, `if … then … else`, and static cardinality (one,
optional, set). Every Shep-specific construct is ShepQL too: paths,
constraints, mixins, and enums, never strings that hold a second language.
The reference, with every `shep::` signature, is §21.

- **Shep owns the dialect.** Gel Data shut down in December 2025, and
  Gel 7.1 is its last release, so there's no upstream to track. ShepQL
  borrows EdgeQL's grammar and semantics (Apache-2.0) and documents every
  difference. It's named separately so it doesn't imply Gel
  compatibility (open question 24).
- **What Shep reuses.** The grammar as a reference, and the tokenizer.
  Gel's type and cardinality inference is Python, and every Gel codegen
  path needs a running server, so Shep writes its own checker in Rust
  (`shep-ql`, §17): parsing, name resolution, type, cardinality, and
  volatility inference, under EdgeQL's simple scoping only. The 2025
  formal semantics of EdgeQL (Sullivan et al., arXiv 2507.16089) serves as
  a reference.
- **Files.**
  - `shep/schema/*.esdl`: the schema, in Gel-style SDL (§4.1.1).
  - `shep/adapters/*.esdl`: adapters between consecutive versions (§4.3).
    The lockfile stores only their hashes.
  - `shep/policies/*.esdl`: policies and policy functions (§6).
  - `shep/policies/tests/*.edgeql`: policy tests, written with `assert`
    (§6.7).
  - `shep/ops/*.edgeql`: an app's operations, one per file (§7.1).

**Two read contexts.** Where an expression runs decides what its paths
see:
- **Policies read raw data.** A guard, a policy function, and a check see
  every node and field as stored, as in Gel since 3.0. Visibility is
  explicit, through `shep::visible` (§6.1).
- **Operations read through visibility.** A query or mutation sees only
  what the request's viewer may read (§6.5): every hop, link, and key cast
  is filtered by the policies.
- **Policy functions are policy-only.** They're declared in policy files
  and callable only from policies and other policy functions. An
  operation can't call one, so no function is ever checked under both
  contexts.

**Taken unchanged from EdgeQL and SDL:**
- `select` and shapes, `filter`, `order by … empty first|last then`,
  `limit`; `with` bindings; `for … union`; `group … using … by`
- `if … then … else`; `insert`, `update … set { := += -= }`, `delete`,
  `insert … unless conflict on … else (…)`
- `assert`, `assert_exists`, `assert_single`, `exists`, `??`, `?=`,
  `[is T]`, backlinks (`.<link[is T]`), link properties
- typed scalar parameters, including `<optional T>$x`
- `type` and `abstract type` mixins, `required`/`single`/`multi`,
  computeds, `readonly`, `default`
- the standard constraints, `constraint expression on (…)`,
  `exclusive on (…)`, `errmessage`, `rewrite insert|update`
- `annotation` (including `deprecated`), `function`, `enum` scalars, and
  the standard scalar and string functions
- `access policy … allow|deny … using (…)`

**Removed:**
- DDL and migrations. Schema evolution is Shep's diff, adapters, and
  lockfile (§4.2–§4.4).
- `configure`, `set global`, `start transaction`, `describe`.
- Triggers. Invariants are checks, derived data is materialized queries,
  and side effects belong to apps.
- `index` and `fts::index`: operations decide indexes (§7.1, §16.6).
  Also `sequence` and user-defined globals.
- `offset`, a non-literal top-level `limit`, and unbounded `for`.
- Legacy path factoring: simple scoping only.
- Reopening another schema's type with `type users::User { … }`;
  `extend type` replaces it (below).
- Volatile, clock, and random functions outside mutations.

**Semantic changes**, each paired with a checker error, lint, or report:
1. **Decisions are total.** Policy guards, `assert` conditions, `if`
   conditions, and walk `stop_at` conditions must have inferred
   cardinality exactly One. The checker rejects anything that can be
   empty or a set. Partial functions (division, string casts, array
   indexing, `assert_single`) aren't allowed in them, so a decision is
   always true or false.
   - **The fix depends on polarity.** The checker suggests `?? false` in
     an allow, an `if`, or an `assert`, and `?? true` in a deny, so the
     default never widens access; or `exists`.
   - **`?=` is linted when both sides can be empty,** since `{} ?= {}` is
     true.
   - **Absent principals, per atom.** Where a request has no viewer or no
     actor, every comparison, `in`, or `is` atom that reads
     `global viewer` or `global actor` is false. `not`, `and`, and `or`
     then apply as usual, so `not (global viewer in .blocked)` is true
     with no viewer. `exists global viewer` is an ordinary, total atom.
     This is the only way an absent principal enters a decision (§6.1).
   - **It's a typing rule.** A comparison, `in`, or `is` atom whose only
     possibly-empty operand is `global viewer` or `global actor` has
     cardinality One, so `global viewer in .members` is a valid guard
     with no `??` (`viewerIn_one`). Plain EdgeQL would type it
     AtMostOne; the two readings agree on the atom but differ under
     `not` (`viewerIn_not_differs`), which is why the rule is Shep's.
     The linter flags `?? false` on such an atom as redundant. An atom
     with another optional operand, such as `i.assignee = global
     viewer`, is still AtMostOne and needs the polarity's default.
2. **Policies combine as union minus deny**, with default deny (§6.1).
3. **`required` describes storage.** A link read is optional even when
   `required`, because visibility can hide its target (§6.2). Gel raises
   an error here; Shep returns `None`. The exception: when the checker
   proves a required single link is always visible to anyone who can read
   its source, the link is typed One (§4.1.1), in operations. In
   policies, which read raw data, a required single link is One, except
   a hop to a type that extends `shep::Placeholders`, which is AtMostOne
   even when `required`, since its target may be a placeholder
   (`placeholder_hop_not_total`).
4. **Key casts are optional.** `<Issue>$issue` is AtMostOne: it's `{}`
   for a key that's missing, hidden, deleted, or a placeholder, and those
   look alike. Only `assert_exists` raises `not_found`. A placeholder is
   created only explicitly, with `shep::placeholder(<Commit>(…))` (§3).
5. **Every `assert` binding runs, in order,** before any write. Gel skips
   `with` bindings nothing references.
6. **Reads see `S`; writes merge.** Every read in a mutation sees its
   snapshot `S`, except the bindings of DML results and
   `shep::observe`, which see the merged post-state (§8).
   - All patches to one node merge first, in statement order; `update
     write` policies and checks then run once, on the merged state.
   - Assigning the same field twice is a compile error when the targets
     are statically the same node (Gel silently keeps one), and an
     `invalid` error at run time when two updates may alias and do.
7. **`multi` properties are sets,** not multisets.
8. **Single-target writes fail loudly.** An `update` or `delete` of one
   node (a key cast or a `with` binding) that its policy denies raises
   `denied`, or `not_found` if the actor can't read the target. A
   set-valued `update … filter` filters: it writes the nodes the actor may
   update and skips the rest, as Gel does. The result types follow.
9. **Functions are inlined element-wise,** as `for v in {arg} union
   (body)` for each ordinary parameter; a `set of` parameter is bound
   whole. They may not recurse.
10. **A Pure volatility class.** Pure is Immutable plus graph path reads
    plus the principal globals. Policies, functions, checks, adapters,
    defaults, and filters of `#! shep: live` queries must be Pure.
    Mutations may also use `shep::now()`, the commit time.
11. **`global viewer`'s cardinality comes from the principal shapes**
    (§7.1). It's One in an operation set whose shapes all have a viewer,
    AtMostOne when some shape has none (`db.as_app()`), and using it is an
    error when no shape has one. In policies it's AtMostOne.
12. **Assigning an optional parameter** to a field needs `?? .f` (keep
    the value when the argument is absent) or an explicit `shep::clear()`.
    Shep doesn't guess whether absent means "unchanged" or "unset".
    Comparisons with optional parameters in filters are linted (§7.1).

**Added,** as functions, constraints, mixins, enums, and types, so the
grammar barely changes (signatures in §21):
- **Functions:** `shep::visible`, `can`, `walk`, `search`, `snippet`,
  `matches`, `with_deleted`, `placeholder`, `claim`, `renew`, `complete`,
  `observe`, `audience`, `unchanged`, `now`, `clear`, `restore`,
  `bounded`, `content_hash`, `wasm`, and the adapter built-ins
  `truncate`, `clamp`, `saturate`, `map`, `first`, `head`, `reject`,
  `redact`.
- **Path constraints:** `shep::key`, `shep::placement`,
  `shep::contained_in`, `shep::independent_of`, `shep::redactable`,
  `shep::strict_revocation`; and the value constraints `shep::max_size`
  and `shep::file`.
- **Mixins:** `shep::Node` (the system fields, on every type),
  `shep::Permanent`, `shep::Placeholders`, `shep::Embed`.
- **Enums:** `shep::Op` (`select`, `insert`, `update_read`,
  `update_write`, `delete`, `restore`), `shep::Analyzer`, `shep::Event`
  (for acknowledgements, §6.7).
- **Types:** `File`, `Document`, `Lease`, `Version`, `Upload`, `App`,
  `Sha1`, `Sha256`.
- **Annotations,** only for plain metadata: `deprecated` (Gel's) and
  `shep::renamed_from`.

**Grammar delta:**

```
SDL:
  SchemaPin       := 'using' 'schema' Name 'version' StringConst ';'
  TypeExpr       +:= 'map' '<' TypeExpr ',' TypeExpr '>'
  ExtendType      := 'extend' 'type' Name '{'
                       ( AccessPolicy
                       | PointerRef '{' AccessPolicy* '}'     # field and edge policies
                       | PathConstraint                       # contained_in, independent_of, strict_revocation
                       | ComputedBacklink )* '}'              # the last only in schema files, on foreign types
  AccessKind     +:= 'restore'
  AccessPolicyBody +:= 'shep::ack' '(' EnumLiteral ',' Path ')' ';'
  GlobalDecl       : object and union types for the reserved names viewer, actor
  AdapterDecl     := 'adapter' 'from' 'version' StringConst '{'
                       ( Path '{' ( ('get' | 'put' | 'hide' | 'fields') ':=' Expr ';' )* '}' )* '}'
ShepQL:
  OpHeader        := '#!' 'shep:' Option (',' Option)*      # a comment to other tools
  KeyCast         := '<' NodeType '>' ( Param | NamedTuple | StrConst )
  ParamType      +:= 'array' '<' NodeType '>'
  OpCall          := 'ops::' Name '(' ( Name ':=' Expr ),* ')' # only as an argument of shep::observe / shep::unchanged
  EmbedValue      := '<' EmbedType '>' NamedTuple             # a value; equality is field by field
  Special forms    : shep::walk and shep::search rebind `.` in their path arguments
  PathProperty     : `@depth` on shep::walk results
```

- **Binding in policy blocks.** In an `extend type` block, `.` is the
  node. In a field or edge policy, `.` is still the node, and
  `__subject__` is the field's value or the link's target.
- **Binding in adapters.** `__subject__` is the field's value, `.` is the
  node, `__old__` the stored value, and `__specified__` the fields a patch
  names (§4.3).
- **The rest of the delta** (`limit` as `max_first`, the key ordering
  rule, `shep::with_deleted` name resolution, deprecated fields, Pure
  defaults, `wasm`, `renamed_from`) is in §21.

### 4.1 Authoring

**One schema source.** A schema is a set of `.esdl` files in ShepQL's
SDL. The CLI checks and registers it, and every client is generated from
it: `shep-build` generates Rust types (§4.1.2), and the service generates
TypeScript (§7.4). The registry accepts only the canonical IR, and the CLI
prints any registered version back as SDL (`shep schema show users@5`), so
diffs, the registry UI, and compatibility reports use one readable form.

Earlier revisions also accepted Rust schemas written with Toasty-style
derive macros. One source is simpler, and it's what lets `shep::query!`
type-check operations offline: a proc macro can read committed schema
files, but it can't see sibling `#[derive]` structs (§7.1). The generated
Rust client keeps Toasty's API shape (§7.3).

#### 4.1.1 Schemas in SDL

```
# shep/schema/issues.esdl
using schema users version '5';             # pinned dependency (§4.1.4)

module issues {
  global viewer: users::User;               # whom requests are for, in issues' policies (§6.3)

  scalar type State extending enum<Open, Closed>;

  type Project {
    required name: str { constraint min_len_value(1); constraint max_len_value(100); }
    multi members: users::User;             # who works on it; policies check it
    group: users::Group;
    multi issues := .<project[is Issue];    # inverse edge
    multi labels := .<project[is Label];
  }

  type Issue {
    constraint shep::placement on (.project);                 # stored with its project (§9.6)
    required title: str { constraint min_len_value(1);
                          constraint max_len_value(500); }    # was 200 in issues@3
    body: str;
    required state: State { default := State.Open; }
    closed_at: datetime;
    email_cc: str;                                            # a field policy may hide it (§6)
    priority: int32 { annotation deprecated := 'hidden from issues@4 on'; }   # §4.1.4
    required project: Project;                                # exactly one; frozen by placement
    assignee: users::User;                                    # at most one, cross-schema (§5)
    multi labels: Label;
    constraint expression on (.state = State.Closed or not exists .closed_at);
  }

  type Comment {
    constraint shep::placement on (.issue);                   # so, with the issue's project
    required body: str;
    required issue: Issue;
    required author: users::User;
  }

  type Label {
    constraint shep::placement on (.project);
    required name: str;
    required project: Project;
    constraint exclusive on ((.project, .name));              # local: includes the placement group (§3)
  }

  extend type users::User {                                   # a foreign type: computed backlinks only (§5)
    multi assigned_issues := .<assignee[is Issue];
  }
}
```

- **Every node type has a key.** By default it's the built-in `id`, a
  UUID v7 that Shep generates with the placement group's routing hash in
  it (§9.6). Other keys use `constraint shep::key` (below).
- **Every node has system fields.** Every type implicitly extends
  `shep::Node { created_at; updated_at; revision; }`, as read-only
  computeds. Shep derives `updated_at` and `revision` from the stamps of
  the node's keys, so nothing writes them (§3).
- **Placement keeps a project's data together.** `Issue`, `Comment`, and
  `Label` are placed with their project, so a mutation like `close_issue`
  (§8) commits in one partition.
- **A schema holds only the data model.** Operations are each app's own
  (§7.1). Policies live in their own files, with their own dependency
  pins (§4.1.4, §6.6).
- **No indexes in the schema.** Shep builds the indexes that declared
  operations need, including search indexes (§7.1, §16.6). The schema
  declares only constraints, such as `exclusive on ((.project, .name))`.
- **The viewer type is part of the schema.** `global viewer: users::User`
  fixes the type of `global viewer` in the schema's policies and
  mutations (§6.3, §8). Changing it is a new version. A schema denies
  requests whose viewer is of another type; credentials, not schemas,
  decide whom a request is for (§6.3).
- **Adapters come from the CLI.** When the title limit grew from 200,
  `shep schema check` proposed `shep::truncate(__subject__, 200)` for
  `issues@3` readers, and wrote the answer to `shep/adapters/` (§4.3).
- **Required links the checker proves visible are One.** A `required`
  single link normally reads as optional (§4.0 item 3). If every allow on
  the source's `select` implies `shep::visible(.link)`, or the link is
  `shep::contained_in`, anyone who can read the node can read its
  target, so the checker types the link One: `i.project` in `close_issue`
  (§8) needs no `assert_exists`. This applies in the operations read
  context only; in policies, a required hop to a `shep::Placeholders`
  type stays AtMostOne (§4.0 item 3). `shep schema check` reports which links
  qualify, and a policy deploy that would break the inference is checked
  like a field becoming hideable (§6.6 item 7).

How the forms map to the model:

| Need | SDL | Notes |
|---|---|---|
| Generated key | built-in `id` | UUID v7 with the routing hash (§9.6) |
| App or composite key | `constraint shep::key on ((.repo, .sha));` | Key fields are `readonly`. A type with `shep::key` has no `id` |
| Content-hashed key | `required hash: shep::Sha256 { readonly := true; rewrite insert using (shep::content_hash(__subject__.path, __subject__.bytes)); }` with `constraint shep::key on (.hash)` | |
| Placement | `constraint shep::placement on (.project)` | A path to a required single link or a key field |
| Frozen | `readonly := true` | Settable at insert, never updated. On a `multi` link, the whole set is fixed |
| Permanent, placeholders | `type Commit extending shep::Permanent, shep::Placeholders` | Mixins with no fields |
| Size cap | `constraint shep::max_size('1MiB')` | Canonical-encoding size of the whole value (§16.1) |
| Files | `multi files: shep::File { constraint shep::file(max_count := 10, max_size := '1GiB'); }` | §16.2 |
| Deprecated | `annotation deprecated := '…'` | Hidden from this version on. A required one also gets `default := …` |
| Redactable | `constraint shep::redactable on (.title)` | A required field a field policy may hide (§6.6 item 7). Schema-only: the generator never reads policies |
| One, exactly one, many | `link x: T`, `required x: T`, `multi x: T` | |
| Inverse edges | computed backlinks `:= .<e[is T]` | |
| Dependencies | `using schema users version '5';` | One pin per schema |
| Embedded structs | `type Address extending shep::Embed { … }`, built as `<Address>(street := …, city := …)` | A value, not an object: no `id`, stored whole under its field, equal field by field |
| Maps, sets | `map<str, T>`, `multi tags: str` | |
| Untyped documents | `shep::Document` | Diffed into facts (§10.2) |

Other key and placement examples:

```
type Country {
  required code: str { readonly := true; }
  required name: str;
  constraint shep::key on (.code);                 # Country::get_by_code(&code)
}

type Commit extending shep::Permanent {           # never deleted or restored (§7.6)
  constraint shep::placement on (.repo);           # commits of one repository stay together
  required repo: str { readonly := true; }
  required sha: shep::Sha1 { readonly := true; }   # computed by git: content-addressed
  required message: str { readonly := true; }
  multi parents: Commit { readonly := true; }      # fixed when the commit is created; walked for history (§7.1)
  constraint shep::key on ((.repo, .sha));         # Commit::get_by_repo_and_sha(&repo, &sha)
}

type Snippet {
  required hash: shep::Sha256 {
    readonly := true;
    rewrite insert using (shep::content_hash(__subject__.path, __subject__.bytes));   # Shep hashes these fields
  }
  required path: str { readonly := true; }
  required bytes: bytes { readonly := true; }
  constraint shep::key on (.hash);
}

type Message {
  constraint shep::placement on (.channel);        # stored with its channel
  required channel: Channel;
}
```

- **Keys are identity only.** Where a node is stored is the separate
  `shep::placement` constraint, on a path to a link (store a message with its
  channel) or a key field (store commits with equal `repo` together).
  Nodes written in one mutation should be placed together, or the
  mutation needs two-phase commit (§8, §9.6).
- **The key still routes.** Placement is computed from the key alone,
  with no lookup (§9.6). So placement by a link is allowed only on types
  keyed by the generated `id`, which carries the group's routing hash, and
  placement by a field must name a key field.
- **Placement freezes what it names.** It must name a required single link
  in the same schema, or a key field. Once the node exists the link can't
  be re-linked; the checker rejects placement by a link an in-service
  version can change. Moving a message to another channel means creating
  a new message. Naming only the schema's own links keeps one team's
  placement from depending on another team's keys.
- **Placement is fixed once a type has nodes.** Changing it on a type with
  nodes is rejected, because existing keys carry their route. Placement
  isn't part of a version's hash.
- **Other natural keys** are `exclusive` constraints.
- **Edges are links, not foreign-key columns,** so there's no
  `project_id` field. Reading a link goes through visibility (§6.5): a
  single link reads as optional, whose absence means absent, hidden, or
  deleted, which the actor can't tell apart, and a `multi` link reads as
  a filtered set.
- **Inverse edges are computed backlinks,** `multi issues :=
  .<project[is Issue]`, so pairing is explicit. Multi-hop read-only
  relations are computed links too (`multi commenters :=
  .<issue[is Comment].author`); they don't create an edge type.

#### 4.1.2 Constraints, disciplines, and generated Rust types

**Constraints.** Checks are SDL constraints (§4.5): built-ins such as
`max_len_value(500)` on a property, and `constraint expression on (…)` on
a type.

**Disciplines.** A discipline is a contract on how data may change,
checked against every writer: every active operation set of every app,
every primitive, and system writers such as sweepers. The planner uses
them to decide which reads need no coordination (§7.6).
- `readonly := true` on a property or a link (Shep's **frozen**): written
  (`+` or `−`) only by the entry that creates the node or fills its
  placeholder, never after. A later entry can't set a frozen field that
  was unset at creation either (`weakFrozen_unsound`); with that rule, a
  frozen fact's value at any version is its value at creation
  (`frozen_eq_creation`). On a single link, such as a message's `author`,
  the target never changes. On a `multi` link, the whole set is fixed at
  creation, like a content-addressed tree's children or a commit's
  parents. Key fields, hashed fields, and placement links are already
  frozen.
- `extending shep::Permanent` on a type: no `@deleted_at` fact at all,
  so neither delete nor restore applies, and its lifecycle never retracts
  (`present_mono`).
- **`@type` is written once.** Only a create or a fill writes a node's
  `@type`, and nothing retracts it, so at most one dot writes it
  (`ty_unique`). A future purge is outside this guarantee.
- Only these two. Disciplines for edges that are only ever added, and for
  values that only move up an order, are open question 22.
- **What counts as a writer.** Every mutation and primitive, system
  writers (sweepers), backfills, write adapters, and a future purge. The
  checker rejects a backfill or write adapter that would change a frozen
  value. A purge is forbidden on permanent types and on the targets of
  frozen links. Read adapters (§4.3) are checked per version: the planner
  judges each version through its own lens, and a `hide` predicate counts
  as a negative input.
- **Enforcement.** A mutation or primitive that would break a declared
  discipline is rejected when its operation set is deployed. At run time,
  front ends check every delta they build: no fact, `+` or `−`, on a
  frozen field or link outside the entry that creates or fills its node,
  no `@deleted_at` fact on a permanent node, and no `@type` fact outside a
  create or fill (`Disciplines.lean`). The leader never reads deltas
  (§10.3). A front end with stale disciplines can't slip past: its
  proposal's `S` must contain the partition's latest registry dot, or the
  registry barrier rejects it (§10.3).
- **Weakening is fenced.** Allowing deletes of a permanent type, or
  updates of a frozen field, is a checked registry change.
  `shep schema check` shows every operation whose plan it changes. It
  rolls out in two steps: first the re-planned operation sets activate
  and a barrier reaches every partition where an affected operation
  writes or is the home; only then is the weaker rule enforced (§7.6,
  §10.3).
- **Suggestions, not inference.** `shep ops check` and
  `shep schema disciplines` suggest disciplines that every current writer
  already respects, with what each would save. The planner never assumes
  one that isn't declared.

**Defaults must be computable by the server.** Defaults are what lenses
use to fill in fields for older writers (§4.3), so a `default` must be a
Pure ShepQL expression: a constant, or one over the node's own fields.

**`revision`** says which version of a node was read: the newest stamp
among its fields and lifecycle (§3). Updates are plain patches by default;
an update can opt in to compare-and-swap with `.if_revision(r)` (§7.3).
It is unrelated to schema versions and version tokens.

**Embedded values.** A type extending `shep::Embed` is a value
stored whole under its top-level field (§3), with its own field IDs.
`map<str, T>` and `multi` properties nest to any depth and are logged
entry by entry. `array<T>` is an atomic value, logged whole. A
`shep::Document` field holds arbitrary BSON-like content under the same
rules (§10.2). The hoist/plunge lenses (§4.3) work on embedded values.
- **The size cap covers the whole value.** A field's cap, 64 KiB by
  default and up to 1 MiB with `constraint shep::max_size('…')`, measures
  everything nested under it in Shep's canonical encoding, so a map, set,
  array, or document can't grow without bound (§16.1). An insert or push
  past the cap fails with `too_large`. `shep schema check` warns about a
  collection or document field with no `shep::max_size` that an operation
  appends to. Collections that grow without bound belong in links.

**Deprecated fields** keep their declaration with
`annotation deprecated`, so older versions still read and write them while
this version hides them: its operations can't name them, and its generated
API drops them (§4.1.4). A required one also
names the default that fills it in for nodes newer clients create.

**Generated Rust types.** `shep-build` generates the typed Rust API from
SDL, for the app's own schema and for every pinned dependency alike:

```rust
// build.rs
fn main() { shep_build::generate().unwrap(); }   // reads shep/schema/*.esdl and shep/deps/*.json

// lib.rs
pub mod issues { include!(concat!(env!("OUT_DIR"), "/shep/issues.rs")); }   // the app's own schema
pub mod users  { include!(concat!(env!("OUT_DIR"), "/shep/users.rs")); }    // users@5
```

- **What's generated, per type:** a struct with its fields, typed by
  visibility (single links as `Option` unless proven visible, `multi`
  links as filtered lists, `shep::redactable` fields as `Redactable<T>`),
  its key type
  (`issues::IssueKey`), the primitives (`Issue::get_by_id`,
  `get_by_<exclusive field>`, create, update, delete, restore, link,
  unlink, paged link loads), and field constants for options such as
  `.if_unchanged(&[Issue::TITLE])`.
- **Loading.** Reading a link the operation didn't load is an error
  (`NotLoaded`), never `None`. Loading one later is a primitive:
  `issue.assignee().exec(&alice)` or
  `project.issues().first(50).exec(&alice)` (§7.3).
- **Hermetic, and schema-only.** The build script reads only committed
  schema files and dependency IR, never policies or the network, so
  builds work offline and a policy deploy never changes a generated
  type. The registered schema is the same IR,
  whether it came from this repository or another team's.
- **Other apps' schemas are generated, never hand-written,** from the
  dependency's committed IR (§4.1.4).
- **Adapters.** `shep schema check` proposes an adapter for each change
  and writes it to `shep/adapters/` (§4.3).

#### 4.1.3 The `shep` CLI

- **Distribution.** The CLI is a Rust binary: `cargo install shep-cli`.
  JS teams need it only to check and deploy, and to run `shep dev`, and it
  has no npm package. Everything JS that runs in the browser comes from
  the service (§7.4).
- **Configuration.** `shep.toml` at the project root names:
  - the app
  - its schema, if it owns one: a directory of `.esdl` files
    (`shep/schema/`)
  - its operations: `shep/ops/` with `.edgeql` files, plus any inline
    `shep::query!` in a Rust crate's one `#[shep::ops]` module (whose
    file `shep.toml` names), and GraphQL
    documents if it has a browser front end (§7.1)
  - its policies (`shep/policies/`)
  - how to get a token from the auth service. A token names its
    universe, and with it the host, so nothing else picks the universe
    (§6.3, §14.3).
- **No build step to read the schema.** The CLI parses and checks `.esdl`
  and `.edgeql` itself (`shep-ql`, §17). For inline `shep::query!`
  operations it parses the one `#[shep::ops]` module that `shep.toml`
  names, the same parse the macro does, without compiling the crate
  (§7.1).
- **Commands.**
  - `shep deploy`: the one command for the common path. It checks and
    registers the schema if it changed, deploys the policies, and
    registers the operation set. They remain separate artifacts, each
    with its own checks (§4.4, §6.6, §7.1). It builds the indexes the
    operations need, shows their progress, and activates the operation set
    once they're ready (§7.1). A new policy still starts in shadow mode on
    its own schedule (§6.6 item 6), and the operation set doesn't wait for
    its promotion.
  - `shep schema check`: compatibility against the in-service versions
    (§4.4), with the report in SDL. For each change
    that needs an adapter, it proposes one and asks, as it does for
    renames, and writes the answer to `shep/adapters/` (§4.3).
  - `shep schema register`: run the check, assign the version number and
    field IDs, update the lockfile and snapshots, and register.
  - `shep schema status`: in-service versions, their adapter chains, every
    lossy adapter and `hide`/`redact`, deprecated fields and who still
    uses them, and who is blocking retirement.
  - `shep schema show <schema@N>`: any version, printed as SDL.
  - `shep schema disciplines`: each fact class's declared discipline, its
    writers, and suggested disciplines with what they'd save (§7.6).
  - `shep ops check | show | status`: plan and price the app's operations
    against current storage, list the indexes they need with their cost,
    show each read point's consistency class and why (§7.6),
    warn about consequences the author should see (§7.1), and show which
    operation sets are in service, how close each is to going dormant,
    and how long `at_least` reads wait (§4.6, §15).
  - `shep gen ts`: generate the JS client module and `client.d.ts` from
    the app's `.edgeql` files and committed IR, offline, for bundler and
    `tsc` setups. The service serves the same generator's output (§7.4).
  - `shep deps add | update <schema@N>`: pin a dependency. This fetches
    its IR into `shep/deps/` and records its hash in the lockfile (§4.1.4).
  - `shep dev`: creates a dev universe on the non-production fleet,
    keeps it alive, and redeploys the schema, policies, and operations on
    save (§7.3, §14.2). Nothing runs locally. It runs as the person, with
    a token from the auth service carrying `manage_universes` for the dev
    fleet, and the person's local processes get tokens valid only in that
    universe. A brand-new app or schema name must
    be registered in production first: until it has propagated, `shep dev`
    fails with an error that says how to register it (§14.1). Dev
    universes accept `localhost`
    origins automatically, so a browser app served locally can connect.
  - `shep policy show | matrix | explain | test`: what policies do, and
    their fixtures and property tests (§6.7).
  - `shep query`, `shep shell`, and `shep get`: ad-hoc access as
    yourself, with a token from the auth service (§7.2).
  - `shep app create <name>` and `shep schema reserve <name> --app <app>`:
    register an app or reserve a schema name. They run only against
    production's registry, with a production `deploy` token, and are
    typically run by CI. Every other universe gets the name from
    production's copy within 30 seconds (§14.1).
  - `shep top`: live load (§15).
  - `shep file get | reprocess`: download a file as yourself, and
    re-extract a field's files with a newer extractor (§16.4, §16.5).
  - `shep universe create | list | drop`, and
    `shep log tail | export | replay`: administration (§14, §10).
- **Deployment never happens from a browser.** It happens from developer
  machines (dev) and CI (production), with a token carrying `deploy`
  (§6.3). A browser bundle carries only the hash of the operation set its
  generated client was built from.

#### 4.1.4 Versions, field IDs, and dependencies

- **No field numbers in source.** The source describes only the *current*
  version. When a version is registered, the CLI assigns field IDs and
  records them in a committed lockfile (`shep/schema.lock`), alongside a
  snapshot of every registered version. This is the same pattern as
  Toasty's migration snapshots. Binaries and bundles built from older
  commits are the older clients.
- **Renames** look like one field dropped and another added.
  `shep schema check` finds dropped/added pairs of the same type, asks
  whether each is a rename, and records the answer in the lockfile. In
  non-interactive runs (CI), an unresolved pair is an error. You can
  answer it in source instead: `annotation shep::renamed_from := 'old'`
  on the field.
- **Removing a field takes two steps: deprecate, then delete.**
  - **Deprecate.** `annotation deprecated` on the field (§4.1.2). The
    field disappears from this version's view and from every API
    generated from it, so new code can't see or write it. Older versions keep it, and
    storage keeps its values.
  - **Its owner still controls it.** A deprecated field stays declared, so
    its policies keep applying to the older versions that use it. The owner
    can, for example, stop older clients from writing it (a field policy
    that denies `update write`), hide it from them (one that denies
    `select`), or keep both working (§6.1). The field's
    default fills it in for nodes that newer clients create.
  - **Who still uses it is exact.** Every app's operations are registered
    (§7.1), so `shep schema status` lists the in-service versions and
    operations that still read or write each deprecated field.
  - **Delete.** Once no in-service version uses the field, `shep schema
    check` suggests deleting it from source, and its storage can be
    reclaimed. Deleting a field that an in-service version still uses,
    without deprecating it first, is an error.
- **Shep numbers versions; a content hash identifies them.**
  - **No number in source.** The source describes the current data
    model. `shep schema register`, or `shep deploy`, assigns the next
    number when it registers new content, and records `(number, content
    hash)` in the lockfile.
  - **The hash is the identity.** A client names the schema content it was
    built from by hash, and the registry maps the hash to its number
    (§7.1). A client built from content that was never registered fails
    at connect, naming the hash and the schema, so "changed the schema but
    didn't deploy" is caught at once.
  - **Numbers are for people and pins.** Reports, `shep schema show`, and
    dependency pins (`using schema users version '5'`) use numbers.
  - **Immutable in production.** A registered `issues@4` never changes
    content.
  - **Two branches register in order.** If two branches both change the
    schema, the first to register in production gets the next number, say
    `issues@5`. The second is checked again against `issues@5`, which is
    now its predecessor: the CLI proposes adapters for that step, and the
    branch registers as `issues@6`. An adapter written for the old
    predecessor becomes stale, and the CLI asks again.
  - **Dev and test universes are looser.** Unpromoted content registers
    as a draft version, replaced on each change while iterating. Only
    production assigns permanent numbers.
  - **Pinning a draft.** In dev and test universes and overlays only,
    `using schema users version 'next'` pins the draft of `users` from the same overlay or deploy.
    `shep deploy` registers the schemas in dependency order and rewrites
    the pin to the number `users` was assigned.
  - **The lockfile catches up.** In production, `shep deploy` runs in CI,
    which doesn't commit. The numbers and field IDs it assigns are in the
    registry, and the next `shep schema check` anywhere reconciles the
    lockfile from it.
  - **Operations never bump a schema version,** and neither do indexes or
    policies. Operations belong to apps' operation sets, indexes are built
    from operations, and policies have their own artifacts (§6.6, §7.1). A
    schema version changes only when the data model does.
- **Clients never need field IDs.** Operations name fields, and the server
  resolves the names through the bound versions' IR when the operation set
  is registered. At runtime, requests are positional (§7.1). `shep-build`
  and `shep::query!` never read the lockfile's field IDs.
- **Dependencies are pinned versions, recorded in the lockfile.**
  - **Pinning.** `using schema users version '5'` names the pin.
    `shep deps add users@5` fetches the `users@5` IR into
    `shep/deps/users@5.json` and records its hash in the lockfile. Both
    files are committed, so the checker, `shep-build`, and
    `shep::query!` resolve `users::User` offline.
  - **Rust.** `shep_build::generate()` writes `users::User` and the rest
    of the typed API, at exactly that version, into `OUT_DIR` (§4.1.2).
    An app that owns no schema uses the same generated types in its
    operations (§7.1).
  - **Nothing to publish.** The owner doesn't publish anything. The
    registry is the source.
  - **JS/TS: one module per app.** A browser app imports
    `/js/<its app>/client.js` (§7.4). It's generated from the app's
    operation set, which already names the pinned versions of every schema
    the app uses, and it exposes only what the app is granted.
  - **Changing a schema pin is a new version.** It changes the
    dependent's IR hash, so it is a new version of the dependent schema
    and is checked as in §4.6. Schema pins cover only the types the data
    model references, such as `users::User` for `Issue.assignee`.
- **Policies are separate files, with their own pins.** They live in
  `shep/policies/*.esdl` and are separate artifacts from the schema. `shep deploy` deploys them in the same
  step as the schema.
  - **Why separate.** Putting policies in the schema's own `.esdl` files
    would suggest that a compiled client carries a particular
    policy version, and it doesn't (§6.2).
  - **Policy pins.** Policies can use the schema's own pins. A policy that
    reads a schema the data model doesn't reference, such as `agents` in
    a `global actor` declaration (§6.3), pins it in the policy file: `using
    schema agents version '2'`. The pin is recorded in the policy
    artifact, keeps `agents@2` in service (§4.6), and is checked as in
    §6.6 item 3. Moving it is a policy deploy, never a schema version.
  - **One pin per schema.** When both the schema and its policies pin the
    same schema, the pins must be equal. The checker rejects version `6` of
    `users` in a policy file next to the schema's version `5`.

### 4.2 Compatibility: the newest version defines storage

Usual compatibility rules (protobuf, Avro) assume old clients go away
eventually. Here they may never go away. A client built against `issues@1`
can run indefinitely, reading and writing the same rows as `issues@9`.
Waiting for old clients to upgrade is never part of the plan.

So the model is:

- **The head defines storage.** The newest *activated* version of a
  schema is its *head*. Stored data must always be valid for the head, and
  for no other version. A newer version becomes the head only at the end
  of the cutover below, once every stored value is valid for it. During
  DualWrite every write is valid for both heads, and backfill brings the
  remaining rows over; from Active on, every value is valid for the new
  head (`Inv_Shape` in `Cutover.tla`).
- **Every older version sees storage through adapters.** Each new version
  declares an adapter back to the version before it (§4.3):
  - a *read* adapter that maps the new shape into the old one
  - a *write* adapter that maps the old version's writes into the new
    shape

  Adapters chain. `issues@1` reads through v4→v3, v3→v2, and v2→v1,
  composed.
- **Registration is a proof.** A new version is accepted only if every
  adapter it introduces, and every chain it extends, satisfies the adapter
  laws (§4.4). Nothing is deferred and nothing waits for old clients.
- **Retirement is optional cleanup.** Retiring an unused version only
  stops serving it. Nothing depends on old versions going away.

What each kind of change requires:

| Change | Read adapter (old readers) | Write adapter (old writers) | Existing data |
|---|---|---|---|
| Add optional field | none (field not in old view) | none (field untouched) | none |
| Add required field | none | `put := __subject__ ?? d` fills it | backfilled with the default |
| Rename | none (field IDs) | none | none |
| String limit 200 → 500 | `shep::truncate(__subject__, 200)` (lossy) | none | none |
| `int32` → `int64` | `shep::clamp(__subject__)` / `shep::saturate(__subject__)` (lossy) | none | none |
| New enum variant `Blocked` | `shep::map(__subject__, {(new::State.Blocked, old::State.Open)})` (lossy) | none | none |
| Edge one → many | `shep::first(__subject__)` by sort key (lossy) | replace; nodes with 2+ edges reject old writes unless `overwrite` | none |
| Required → optional | `__subject__ ?? d` for missing values (lossy: rows where the value is missing reject old writes to it unless `overwrite`) | none | none |
| String limit 200 → 100 | none | `shep::truncate(__subject__, 100)` (lossy) or `shep::reject()` | backfilled with `shep::truncate(.f, 100)` |
| Split `name` → `first`, `last` | `.first ++ ' ' ++ .last if .last != '' else .first` | split at the first space with `find` and slicing; lossy (a single trailing space is dropped); representable only where `first` has no space | backfilled with the split |

- **Some changes need nothing.** Widening and additive changes need only
  read adapters, because old writers already produce valid head data.
- **Tightening changes stored data.** Any change that makes the head
  stricter than existing data needs a write adapter, and registration
  includes a **backfill**: the transform is applied to every existing value
  that doesn't fit, as the cutover below describes. Each rewritten value is
  an ordinary logged write (§10), so the backfill is visible, auditable,
  and replayable.
- **What "keeps working" means.** It's an interface guarantee, not a
  behavioral one. An old version's client still type-checks and runs
  against the same operations. But its reads can be lossy (adapters), rows
  and fields can be hidden from it (policies, `hide`), and its writes can
  be rejected (not representable, or a tightened check). Behavior an old
  client relied on beyond its interface can change.
- **The checker requires an adapter.** When a change needs one, the checker
  rejects the version until it is declared. It also reports which adapters
  are lossy (§4.3) and requires them to be acknowledged, in the same way
  §6.7 handles surprising policy outcomes.

**Protocol: schema cutover.** A new version becomes the head in four
states, each recorded by an entry in the meta partition, in the style of
F1's online schema change
([Rae et al., VLDB 2013](https://research.google/pubs/online-asynchronous-schema-change-in-f1/)).
The meta leader drives it. A schema has at most one cutover at a time; a
newer version registers only after the previous one is Active or
Abandoned, and is diffed against it.

| State | Stored shape | Readers of older versions | Readers of the new version | Writes |
|---|---|---|---|---|
| **Prepared** | old head | as before | `not_ready` | checked against the old head |
| **DualWrite** | old head, with every new write and backfilled row valid for both heads | as before (a shape-changing step: the new step's lens) | `not_ready` | patch must be valid for both heads; new set-level constraints enforced |
| **Active** | new head | through their adapter chains, now including the new step | the head | checked against the new head only; lossy write adapters apply |
| **Abandoned** | old head | as before | never served | as before |

1. **Prepared.** The version is registered and checked, its adapters
   proven, its indexes and backfill planned (§4.4). New indexes are built
   in every partition. A set-level constraint the new head adds (a local
   `exclusive`) is checked over the built index, at a snapshot holding every
   partition's index barrier. **From the index barrier on, every write must
   keep the new head's set-level constraints over its new-head values**
   (`put(x)`); a write that would break one is rejected with `invalid`,
   reason `cutover`. A one-time check alone isn't enough: a colliding write
   arriving after it would deadlock the cutover past its point of no
   return (`Cutover_dualunique`, `Cutover_dualunique_live`), and a check at
   activation's snapshot can't bind writes that commit after it
   (`Cutover_noenforce`, `Cutover_noactunique`). With write-time
   enforcement, activation's uniqueness check never fails
   (`Cutover_noactunique_idx`). Adding a *global* unique (§3) to a type that
   already has nodes is rejected.
2. **DualWrite**, entered by a registry barrier (§10.3) once every
   partition holding an affected type holds it, children split off after
   the barrier was sent included: a delivery to a split parent counts only
   when each child has the barrier (§9.6; `Cutover_noredeliver`).
   - **Writes are valid for both heads.** A write's patch must be valid for
     both heads, so the new step's `put` leaves it unchanged; a patch only
     one head accepts is rejected with `invalid`, reason `cutover`. Lossy
     write adapters apply only once the version is Active. Otherwise an
     abandoned cutover would leave writes truncated that their writers
     never saw truncated (`Cutover_putlossy`).
   - **Backfill.** Backfill rewrites each existing row that the new head
     doesn't accept, as ordinary validated mutations: a concurrent edit
     makes a rewrite fail validation and retry against the new value
     (`Cutover_blindbf`). It reads at a snapshot containing its partition's
     DualWrite barrier (in a split child, its first entry or the
     re-delivered barrier), and its decision to skip a row is validated
     too, so a stale write can't slip in behind the cursor
     (`Cutover_bfsnap`). Every write after the barrier is valid for both
     heads, so a row the cursor has passed stays valid. Backfill keeps a
     cursor per partition, remapped across splits (`Cutover_cursorlost`;
     resetting a child's cursor is also safe).
   - **Old-version readers switch lenses at activation.** Until then they
     read as before. Every read at V applies adapters under the registry at
     V, never a front end's cached registry, so the switch happens exactly
     at the activation dot for every reader. The checker proves the new
     step's lens is the identity on rows valid for both heads, and
     activation's guard requires every row to be, so activation changes no
     reader's view and abandoning changes none either (`Cutover_lensact`
     passes with no registry dot counted). The cost: backfill of a lossy
     step (`truncate(100)`) is visible to older readers as an ordinary
     stamped change, so their topics get a delta and freshness checks
     answer "changed" for each rewritten row (`Cutover_lensact_quiet`).
     A row read before and after its backfill can look different.
     - **A step that changes the stored shape** (a field split such as
       `name` → `first`, `last`) can't keep rows valid for both heads, so
       older readers switch to the new step's lens at the DualWrite
       registry dot instead, chosen by the registry at V
       (`Cutover_lenskey`). Then freshness checks and older-version topics
       count the DualWrite, abandon, and activation registry dots
       (`Cutover_abandoncount`). `Cutover.tla` models a tightening step
       only.
   - **Policies.** A policy deployed during DualWrite must type-check
     against both heads (§6.6), and is evaluated against the stored values,
     which are valid for both.
   - **Point of no return.** Before the first backfill write, the cutover
     can be abandoned; the barrier is withdrawn by another barrier, and the
     version is never served. Once backfill has written anything, it can't
     be undone: backfills can be lossy (`Cutover_abandonbf`). `shep deploy`
     says so when the cutover enters backfill. Writes before that point are
     valid for both heads, so abandoning loses none of them.
3. **Active**, entered by an activation barrier. Its guard is the check
   the old Validate state made: at a snapshot containing every barrier and
   every backfill dot, every stored value is valid for the new head, every
   index is built, and every set-level constraint holds. The activation
   entry's context is that guard snapshot, so no causally closed version
   holds the activation without the backfill (`Cutover_actctx`). Writes
   after it keep the set-level constraints because every write enforces
   them (item 1). Activation makes
   the version the head; policies type-check against it; writes are
   checked against it only; indexes no in-service operation uses are
   dropped.

- **Lifecycle (§4.6).** A version in Prepared or DualWrite is `pending`;
  Active is `active`.
- **`shep deploy` waits for activation,** reporting progress, and never
  abandons a cutover past its point of no return. A timeout leaves the
  cutover running and the old operation set active.
- **Dev and test universes** with few nodes run a cutover synchronously in
  one step, so a `shep dev` save doesn't wait on barriers. A production
  single-node deployment (§9.6) runs cutovers through the four states
  like any other: the shortcut is for dev and test only.
- **Crash recovery.** Each state is a log entry, so a crash resumes at the
  last recorded state. Backfill rewrites are conditional on the value they
  read, so restarting them is safe.

### 4.3 Adapters

An adapter is a pair of pure functions between two consecutive versions of
a node or edge type: `get` (read) and `put` (write).

- **Proposed by the CLI, answered by the author.** `shep schema check`
  diffs the new version against the head, proposes an adapter for each
  change that needs one, and asks, as it does for renames:
  - a string limit raised from 200 to 500: `shep::truncate(__subject__,
    200)` for older readers;
  - a new enum variant: which old variant older readers see, or whether
    those nodes are hidden from them (`hide`);
  - a field older writers can't represent: reject their writes (the
    default) or let them overwrite (`overwrite`).

  The answers are written as source, to `shep/adapters/*.esdl`, and
  registered with the version; the lockfile records only each adapter's
  hash. Non-interactive runs (CI) fail on an unanswered question.
- **Written in ShepQL, in source files.** An adapter block goes from the
  current version back to the one it names. It uses one vocabulary, that
  of SDL rewrites: `__subject__` is the field's value, `.` is the node,
  `__old__` is the stored value, and `__specified__` the fields a patch
  names. Enum literals are qualified by version: `new::` names the newer
  version's types, and `old::` the older one's.

  ```
  # issues: shep/adapters/issues@4.esdl
  adapter from version '3' {
    Issue.title    { get := shep::truncate(__subject__, 200); }
    Issue.state    { get := shep::map(__subject__, {(new::State.Blocked, old::State.Open)}); }
    Issue.estimate { get := __subject__ ?? 0;  put := __subject__; }      # a default
    Issue          { hide := (.state = new::State.Blocked); }
  }

  # users: shep/adapters/users@6.esdl, a cross-field split of name into first and last
  adapter from version '5' {
    User.name { fields := (.first, .last);
                get := (.first ++ ' ' ++ .last) if .last != '' else .first;
                put := (with i := find(__subject__, ' ') select
                         (first := __subject__[:i] if i >= 0 else __subject__,
                          last  := __subject__[i + 1:] if i >= 0 else '')); }
  }
  ```
- **Built-in library.** These adapters have their laws proven once, as
  part of Shep:
  - rename (free, because of field IDs)
  - a default, `__subject__ ?? e` (a constant or a Pure expression)
  - `shep::truncate`, `shep::clamp`, `shep::saturate`
  - `shep::map(variant → variant)`
  - `shep::first` (one edge out of many), with `replace` for edge writes. An
    `add` write would be unlawful: writing a second edge reads back as the
    first, and two adds in a row differ from their merge
    (`firstAdd_putGet_fails`).
  - wrap/unwrap between a scalar and a list (`shep::head`)
  - hoist/plunge for nested fields (the Cambria set)
  - `shep::reject` (for writes) and `shep::redact`
- **Custom adapters are ShepQL expressions** in the SMT subset (§4.5),
  Immutable: no graph reads. Their laws are proven when the version is
  registered. Expressions outside the subset and wasm are never allowed in
  adapters: an adapter that can't be proven can't be registered.
- **Writes stay patches.** A write from any version touches only the fields
  it changes, so fields an old writer doesn't know about are never
  affected.

**Lossy reads and the write guard.** Some read adapters lose information:
`shep::truncate(__subject__, 200)` hides the tail of a 400-character title. The hazard is an
old client that reads a lossy value, edits it, and writes it back, wiping
what it couldn't see.

- **Representable values.** A stored field value is *representable* for
  version `v` if reading it through `v`'s chain and writing it back
  unchanged leaves it unchanged (`put(get(x), x) = x`). The server can
  compute this per row and per field from the stored value alone. It needs
  nothing from the client.
- **The default is to reject.** A write from version `v` to a field that
  isn't representable for `v` on that row is rejected with a clear error:
  "this field holds data issues@1 can't represent". The author can opt a
  field into `overwrite` instead.
- **The guard means "the writer could undo this".** A write passes the
  guard exactly when its writer could restore storage by writing the old
  values back. So an accepted write can always be undone that way, and a
  version can always edit values it wrote itself (`guard_undo`,
  `no_silent_loss` in `formal/lean`).
- **Other fields are unaffected.** The same old client can still write
  every other field of that node, because writes are patches.

**When no mapping makes sense.** Sometimes there's no meaningful old-shape
value at all. Two fallbacks exist, and both reuse the rule that anything can
be hidden from a reader (§6.2):

- `hide`: the node is invisible to that version, as if the reader lacked
  permission. A version can't write a node it can't see, so writes to
  hidden nodes are rejected. That condition is also what makes the
  identity adapter a left identity for composition (`comp_id_left_iff`).
- `redact`: the field reads as hidden. This is only possible if the old
  version reads the field as optional, or declared it `Redactable` (§6.6
  item 7).

Neither is ever implicit. Each must be declared, and the CLI lists both
for acknowledgment. Silently dropping rows from old clients is exactly the
kind of surprise authors have to choose deliberately.

### 4.4 Adapter laws and the checker

**The setup.** A schema's versions form a chain `v1 < v2 < … < vn`, with
`vn` as the head.

- **Step adapters.** Each step `k → k−1` has an adapter
  `A_k = (get_k, put_k)`.
- **A version's lens.** Version `j` sees storage through the composed lens
  `L_j = A_{j+1} ; … ; A_n`, and the head's lens is the identity.
- **`S(v)`** is the set of states valid for `v`.

**Laws each step adapter must satisfy, proven at registration:**

1. **Total read.** For every `x ∈ S(v_k)`, `get_k(x)` is defined and lies
   in `S(v_{k−1})`, or is an explicitly declared `hide`. Old readers never
   see invalid data and never fail. Writes to rows the adapter hides are
   rejected.
2. **Total write.** For every `x ∈ S(v_k)` and every valid `v_{k−1}` patch
   `p`, `put_k(p, x)` is defined and lies in `S(v_k)`. The only exceptions
   are writes the author declared `reject`, and their rejection condition is
   explicit. Old writers never produce invalid head data.
3. **GetPut, on representable values.** `put_k(get_k(x), x) = x` for every
   `x` in the representable subset `R_k`. `R_k` must be decidable, because
   it is exactly what the write guard evaluates per row. For lossless
   adapters, `R_k` is all of `S(v_k)`.
4. **PutGet.** `get_k(put_k(p, x)) = apply(p, get_k(x))`: an old writer
   reads back what it wrote. There is one exception, a write adapter that
   is declared lossy (for example `truncate(100)` on a tightening). There,
   the law becomes `get_k(put_k(p, x)) = apply(norm_k(p), get_k(x))`.
   - **`norm_k` is per field.** It normalizes the values `p` writes, field
     by field, so fields `p` doesn't name read back unchanged. A whole-view
     normalization could silently change fields the write never touched.
   - **Acknowledgment.** The exception is listed for acknowledgment.
5. **PutPut.** `put_k(p2, put_k(p1, x)) = put_k(p1 ⊕ p2, x)`: applying two
   patches in sequence is the same as applying their merge. Batches,
   transactions, and retries can therefore group writes freely.
6. **Locality.** An adapter declared on field `f` reads and writes only `f`,
   or only the field set it declares (cross-field adapters such as a
   split). A patch that doesn't touch those fields leaves them unchanged.
   This is what makes field-level reasoning, and the per-field write guard,
   sound.
7. **Purity.** Adapters are Immutable ShepQL, with no clock, no graph
   reads, and no randomness (§4.0). Proofs, replay, and every server then agree on
   what a version sees.

**Composition must behave.** Chains are what keep `issues@1`'s interface alive forever,
so composition itself is held to rules:

- **Identity.** An unchanged field has the identity adapter, and
  `id ; A = A = A ; id`.
- **Associativity.** `(A ; B) ; C = A ; (B ; C)`. Lens composition is
  associative by construction. The requirement is that every *compiled*
  form honors it: the service caches and fuses chains in whatever grouping
  is convenient, so any grouping must produce the same lens.
- **Which laws survive composition.** These results are proven in
  `formal/lean` (`comp_laws`, `chain_weakLaws`, `fieldLens_comp_laws`).
  - **Always.** Laws 1, 2, and 6 hold for every composed lens whenever they
    hold for its steps. Locality composes as the union of the steps' field
    sets.
  - **When every step below writes exactly.** Laws 4 and 5 also survive,
    for every step that has no lossy write below it.
  - **Chains of single-field built-ins on one field** stay lawful even with
    lossy writes. Their composition is again a single-field adapter, with a
    computed `norm`.
- **When chains must be re-proven.** A lossy write (a declared `norm`)
  breaks laws 4 and 5 for any chain above it that contains a cross-field
  adapter whose write reads the normalized fields.
  - **Example.** `total = sub + tax` above a clamp on `sub`. The same v1
    write reads back differently depending on `tax`, a field v1 can't see,
    so no normalization of v1's view exists (`chain_putGet_no_norm`,
    `chain_putPut_fails`).
  - **What the checker does.** When a new head's step has a lossy write,
    it re-proves laws 4 and 5 for every in-service chain that crosses
    fields above it. Alternatively, the author uses `reject` instead of the
    lossy write.
  - **When nothing needs re-proving.** When the new step writes exactly, no
    chain needs re-proving: the step's laws and the backfill conditions are
    enough (`registration_sufficient`).
- **Representability is per field.** A composed lens represents fields `D`
  on a row when both of these hold:
  - `A` represents `D` on `get_B(x)`;
  - `B` represents, on `x`, the fields that `A`'s write-back touches.

  This is exact when `B` writes exactly, and a safe under-approximation
  when `B` normalizes (`rep_comp_iff`, `rep_comp_strict`). The whole-row
  formula `R_B ∩ get_B⁻¹(R_A)` is too coarse: it would reject a write to
  one field because some other field isn't representable.
- **Fused forms are proven equal to the chain.** For example,
  `truncate(200) ∘ truncate(500)` fuses to `truncate(200)`. The service
  evaluates fused, simplified expressions, and every fusion is checked
  equivalent to the naive composition: by SMT for custom adapters, and by
  pre-proven rewrite rules for built-ins.
- **Queries use the same lens.** A filter or sort on a `v_j` field
  compiles to a predicate over storage through `get_j`
  (`truncate(title, 200) == "x"`). The compiled scan must agree with the
  evaluator, under the same differential testing as policies (§6.4).

**What the checker does for each new version:**

1. **Structural diff** against the current head: field IDs, types, edges,
   and cardinalities.
2. **Required adapters.** Determine which adapters the diff needs (the
   table in §4.2). Fail if any are missing.
3. **Prove the laws.**
   1. Prove laws 1–7 for each new step adapter.
   2. If the new step's write is lossy, re-prove laws 4 and 5 for each
      composed chain `L_j` that contains a cross-field adapter.
   3. Prove each fused form equivalent to its chain.
   - Built-ins use their pre-proven laws and rewrite rules.
   - Custom adapters go to the SMT solver (Z3 or cvc5).
4. **Plan the backfill.** If the head became stricter than existing data,
   compute the backfill and how many rows it touches. Then prove two things
   about it:
   - every backfilled row is valid for the new head;
   - through the new step, every backfilled row reads back as the old row,
     or as its declared lossy image.

   With that, every older version reads what it read before on lossless
   rows, across any number of head changes (`head_forward_many`).
5. **Re-plan every in-service operation.** Every active operation set
   that uses an in-service version of this schema is planned again against
   the new head, through its adapters (§7.1). A dormant set is re-planned
   when it wakes (§4.6).
   - **Indexes.** A filter on an adapted field compiles to an expression
     (`shep::truncate(.title, 200) = 'x'`), so a plain index no longer serves it.
     Registration builds indexes over the adapted values, from each
     partition's state at an index barrier (§10.3), while the new version
     is Prepared and before dual checks rely on them (§4.2, schema
     cutover).
   - **Indexes follow operations.** An index that any in-service operation
     uses is itself in service, and isn't dropped. One that no operation
     uses any more is dropped.
   - **Exact blast radius.** An operation that can no longer be planned
     within its bounds fails registration, and the report names the app
     and operation.
6. **Report.** List every lossy read, lossy write, `hide`/`redact`,
   backfill, and re-planned operation for acknowledgment. When a law fails,
   show a counterexample state.

### 4.5 Checks

**Checks are SDL constraints** (§4.0), so there's no separate validation
language.

- **Built-in constraints** on a property cover the common checks, and
  are compared structurally, with no solver: changing
  `max_len_value(200)` to `max_len_value(500)` is a parameter diff on a
  named constraint. Leader-resolved field operations read the same forms
  for their bounds (§8).

  | Check | Constraint |
  |---|---|
  | string length at most 200 | `max_len_value(200)` |
  | non-empty string | `min_len_value(1)` |
  | `>= 0`, `<= N` | `min_value(0)`, `max_value(N)` |
  | enum membership | `one_of(…)`, or an `enum` scalar |
  | pattern | `regexp('…')` |
  | encoded size | `shep::max_size('1MiB')` |

- **Type-level checks** use `constraint expression on (…)`, such as
  `.state = State.Closed or not exists .closed_at`. EdgeQL's own
  restrictions on them (Immutable, single pointers of the object itself,
  no link traversal) are exactly "node-local", so the checker rejects a
  graph check and points to policies and guards.
- **Empty means satisfied.** A check holds when its expression is
  `¬defined ∨ value`: one that is `{}` passes, as in SQL `CHECK`. That's the useful default for `.closed_at >
  .created_at`, but it can surprise: a check over an optional field says
  nothing when the field is unset. So `shep schema check` reports every
  check whose inferred cardinality can be empty as "vacuous when
  `closed_at` is absent". The author silences it by writing `?? true` (the
  intended reading) or `?? false` (the field is effectively required).
- **The SMT subset.** Adapter laws (§4.4), policy diffs (§6.6), and check
  changes are proven over this part of ShepQL:
  - **Encodable:** single scalar properties (`bool`; integers with linear
    arithmetic; `str` with `len`, `++`, `find`, slicing with linear
    bounds, `like` against constant patterns, and `contains`; enums;
    `datetime` as an integer), comparisons, `and`,
    `or`, `not`, `if … else`, `??`, `?=`, `?!=`, `exists` on single
    pointers, and `in` a set of literals.
  - **Encoding.** Each optional term becomes a pair `(defined, value)`.
    An element-wise operator is defined when all its inputs are, and
    `if c then a else b` is defined when `c` is and the branch it chooses
    is; requiring both branches would disagree with the evaluator
    (`ite_naive_disagrees`, `enc_agrees` in `Checks.lean`). `??`,
    `?=`, and `exists` are total. A policy guard or filter holds when it's
    `defined ∧ value` (totality makes `defined` true for guards); a check
    holds when it's `¬defined ∨ value`. Empty propagates uniformly, so
    there's no error-absorbing logic to model.
  - **Graph terms.** Single-hop link equality (`.author ?= global
    viewer`), membership (`global viewer in .members`), and
    `shep::visible(.x)` become uninterpreted relations. Absent principals
    are a case split: each formula is checked with the viewer and the
    actor present and absent, under the per-atom rule (§4.0 item 1).
  - **Opaque:** `sum` and `avg`, `count` beyond `count(s) op k` for small
    `k`, regular expressions beyond simple patterns, JSON, array indexing,
    casts from `str`, and functions that can't be inlined. The decidable
    core is quantifier-free linear integer arithmetic, plus strings with
    length, plus uninterpreted functions. Quantified graph properties
    stay bounded enumeration (§6.7).
- **Opaque checks are compared by AST.** An unchanged opaque check (same
  normalized AST) passes. Changing one can't be proven against older
  versions' writes (§4.4), so it's rejected. Rewrite it in the subset
  first.
- **Common checks don't wait for the solver.** The built-ins above are
  compared structurally before the SMT backend exists (milestone 14).

**Wasm is the escape hatch** for validators ShepQL can't express:
`constraint expression on (shep::wasm('name', .field))`. They run in
wasmtime with fuel limits and a WIT interface, are opaque to the prover,
and are compared by hash, as above.

**Node-local invariants and graph invariants are kept separate.** "Assignee
must be a member of the project" depends on other nodes. Concurrent
mutations elsewhere (removing the member) can break it after the fact unless
every such mutation re-checks every dependent rule. Proposal:
graph-dependent rules are **write policies** and mutation `assert`s (§8). They are
checked when the write happens and explicitly not guaranteed afterward.
Only node-local checks are invariants that the system guarantees.

### 4.6 Lifecycle and dependencies

`draft → pending → active → deprecated → retired`. A version is
`pending` while its cutover is Prepared or DualWrite, and `active` once
it's Active (§4.2).

- **Retirement is optional cleanup.** It only stops serving a version that
  nothing uses any more. Compatibility never depends on it (§4.2).

- **Usage is exact.** Every registered operation set names the schema
  versions it uses (§7.1), and Shep tracks which operation sets still have
  live sessions, with last-seen times and request counts.
- **Unused operation sets go dormant, not away.** An operation set unused
  for a configured period becomes dormant. What lapses:
  - it isn't re-planned when a schema gets a new head (§4.4)
  - its indexes and text extraction are released, unless another active
    set uses them (§7.1, §16.5)
  - it no longer blocks deleting deprecated fields (§4.1.4)

  What doesn't: **versions it pins are not retired** unless their owner
  forces it, and the report names the sets that would then fail.
  - **A dormant set wakes on connect,** if the versions it pins are still
    registered. Waking re-plans it against the head and rebuilds what it
    needs, and the set is `not_ready` until it's active again. A monthly
    job built from an old commit keeps working, after a wait.
  - **Warned in advance.** `shep ops status` shows each set's countdown to
    dormancy, and the app's owners are told before a set goes dormant.
- **Dependencies block retirement.** `users@5` can't retire while `issues@3`
  is in service. The registry answers "who is blocking me?" with an owner
  and a last-seen time.
- **Live policies are dependents too.** A policy's own pins (§4.1.4) keep
  those versions in service (§6.6).
- **Changing a pinned dependency is a change to the dependent schema.** If
  `issues@4` moves from `users@5` to `users@6`, the checker verifies it the
  same way, from `issues`' point of view: the `users::User` fields that issues
  exposes must still be present and compatible.

## 5. Cross-schema links

- **A schema may declare edges whose source or destination is a foreign
  type** (`Issue.assignee -> users::User`), including inverse edges that hang
  off the foreign node (`users::User.assigned_issues`). These edges exist only
  in the declaring schema's view. The users schema never sees them. The
  inverse is a computed backlink in an `extend type users::User { … }` block,
  which may hold only computed backlinks (§4.1.1). `shep-build` generates
  an extension trait for it on the foreign type (§4.1.2).
- **A schema may *not* add fields to foreign node types.** If Slack needs
  `User.status`, it creates its own `SlackProfile` node with a 1:1 edge.
  Ownership stays unambiguous, and the users team's compatibility proofs
  never depend on code they don't control. The generated API makes the
  extra hop read naturally (`user.slack_profile()`).
- **Foreign nodes are always read through the pinned version's adapter
  chain (§4.3).** That is how `issues@3` keeps working while `users` moves
  on. The pin is `using schema users version '5'` (§4.1.4).
- **Deletion needs no cross-schema coordination.** Deletes are soft (§6.5),
  so links are never broken. When a `User` is deleted, `issues` keeps its
  `assignee` edge, and the user simply reads as not visible, exactly as if
  permission had been revoked. Dependent schemas don't declare `on_delete`
  behavior, and they have no way to block the owner's deletes.

## 6. Privacy

### 6.1 Policy semantics

Policies are ShepQL access policies (§4.0), combined as in Gel and Cedar:

- **Each node and edge type has policies** for the operations `select`
  (read), `insert` (create), `update read` (the pre-state of an update),
  `update write` (its post-state), `delete`, and `restore`. A policy names
  one or more of them:
  `access policy <name> allow|deny <ops> using (<guard>);`
- **Union minus deny, with default deny.** An operation is allowed when
  some `allow` guard holds and no `deny` guard does:
  `(A_1 ∨ … ∨ A_n) ∧ ¬(D_1 ∨ … ∨ D_m)`. Order doesn't matter. With no
  matching allow, the answer is deny, so failing closed is structural.
  (entgo treats all-Skip as allow, which is why its examples end with
  `AlwaysDenyRule`.)
- **Decisions are total.** A guard must have inferred cardinality exactly
  One, and may not call partial functions, so it's always true or false
  and never an error (§4.0 item 1). The checker rejects a guard that can
  be empty and suggests a fix by polarity: `(…) ?? false` in an allow,
  `(…) ?? true` in a deny, or `exists`. There's no error case to fail
  closed on, and nothing for a stale read to turn from one answer into
  another except the facts the guard reads.
- **Absent principals, per atom.** Where a request has no viewer, or no
  actor, every comparison, `in`, or `is` atom that reads
  `global viewer` or `global actor` is false; `not`, `and`, and `or` then
  apply as usual (§4.0 item 1). So `allow … using (global viewer in
  .members)` doesn't admit the app acting as itself, and `deny … using
  (global viewer in .author.blocked)` doesn't deny it. `exists global
  viewer` is an ordinary atom, and is how a guard says "only with a
  viewer" or "only without one". This is the rule `Principals.lean`'s
  `UMD` model states. It's also a typing rule: such an atom is One, so
  these guards need no `??` (§4.0 item 1, `viewerIn_one`).
- **Deterministic in their inputs.** Policies and functions are Pure
  (§4.0): they read only the graph, `global shep::app`, `global viewer`,
  and (where declared) `global actor`. They can't read the clock
  (`datetime_current()`, `shep::now()`), randomness, or any other input
  from outside the graph, and the checker rejects any that try. Wasm
  functions get no clock or randomness from their host either. So a
  policy's answer changes only when a fact it reads changes, which live
  queries (§13.2) and freshness checks (§11.7) rely on.
  - **Time-dependent access is a graph fact.** A grant that should lapse
    gets an `expires_at` field and a job that writes `expired := true`
    when the time comes. The policy reads `.expired`, and the write is an
    ordinary logged change.
- **Policies see raw data; visibility is explicit.** As in Gel since
  3.0, a guard reads other nodes without applying their policies (§4.0).
  `shep::visible(.channel)` is how a policy defers to another type: it's
  true when the node at the other end of the link is readable for the
  same request, including the deletion and placeholder checks, and false
  when the link is empty, so a deleted or missing target never admits.
  Its argument is an optional node; over a `multi` link, write it per
  element, as `any(shep::visible(.quotes))`. `shep::can(shep::Op.update,
  .project)` does the same for other operations. The target's schema
  sees the request's viewer, checked against its own viewer type (§6.3).
  Example: a `Message` is readable if its `Channel` is. Most real
  policies are a short chain of `shep::visible` ending at a membership
  check.
- **Functions** name an expression once, for any policy to use:

  ```
  # users: shep/policies/defs.esdl
  module users {
    function member(v: User, g: Group) -> bool
      using (v in g.members or v in g.parent.members);
  }

  # issues: shep/policies/issue.esdl
  module issues {
    extend type Issue {
      access policy members allow select
        using (users::member(global viewer, .project.group) ?? false);
    }
  }
  ```

  - **Pure, typed, policy-only, and not recursive.** A function reads the
    graph and its parameters, and nothing else, with policies' raw-data
    reads; operations can't call it (§4.0). It's inlined element-wise:
    each ordinary parameter is bound as `for v in {arg} union (body)`, so
    an empty argument gives an empty result, which the caller's `??`
    handles. A `set of` parameter is bound whole.
  - **Inlined when policies compile** (Gel doesn't inline). After
    compilation, a policy that uses functions is the same IR as one
    written out by hand. Pushdown, compilation to relations (§6.8), the
    diff report (§6.6), and live permission routing (§13.2) all work
    unchanged.
  - **Resolved live from the exporter.** `users::member` resolves through
    the `using schema users version '5'` in issues' policy files for its
    types, but its body comes from `users`' current policy artifact, as
    any policy does (§6.2).
  - **A change is checked for every importer.** When the users team
    deploys a new `member`, Shep recompiles every importing policy, runs
    the policy checks for each (§6.6), and rolls the change out in
    shadow mode for them too. `users`' deploy names the importing
    schemas it affects.
  - **Shared conditions are functions.** There's no policy inheritance:
    two types that share a rule each declare an access policy that calls
    the same function. Each type's policies stay readable in one place.

```
# slack: shep/policies/message.esdl
using schema users version '5';

module slack {
  extend type Message {
    constraint shep::contained_in on (.channel);       # §6.7
    access policy blocked deny  select using (global viewer in .author.blocked);
    access policy author  allow select using (.author ?= global viewer) {
      shep::ack(shep::Event.ViewerRemoved, .channel);  # reviewed: §6.7
    };
    access policy channel allow select using (shep::visible(.channel));
  }

  extend type Channel {
    access policy members allow select using (global viewer in .members);
    access policy public  allow select
      using (.visibility = Vis.Public and shep::visible(.workspace));
  }
}
```

- **A deny beats every allow.** A blocked author's messages are hidden
  from the viewer even in a channel they share. There's no way to write
  "this allow wins over a deny"; the exception goes into the deny:
  `deny select using (global viewer in .author.blocked and not
  (.author ?= global viewer))`. The access-path matrix shows every allow
  a deny overrides, and suggests that text (§6.7).
- **App-only access is written out.** No rule treats the app acting as
  itself specially. An allow meant only for it says so:
  `allow select using (global shep::app = <shep::App>'slack' and not
  exists global viewer)`. Without the second atom, it also admits every
  request from that app that has a viewer.
- **Field and edge policies.** An access policy inside a property or
  link, in an `extend type` block, governs that field or edge: `select`
  and `update write` for fields; `select`, `insert` (link), `delete`
  (unlink), and `update write` for links. In it, `.` is the node and
  `__subject__` is the field's value or the link's target.

  ```
  extend type Issue {
    email_cc {
      access policy members allow select using (global viewer in .project.members);
    }
  }
  ```

  - **How they combine.** A field operation is allowed when the type
    allows the operation on the node, and either the field has no allows
    or one of them holds, and none of its denies does:
    `type_allowed ∧ (no field allows ∨ ∨A_f) ∧ ¬∨D_f`. It's default deny
    and never exceeds the type (`field_default_deny`, `field_le_type` in
    `PolicyCombine.lean`). A field with only denies narrows the type's
    policy; a field with allows further requires one of them, on top of
    the type's allows, rather than replacing them (`field_not_replace`).
    So adding a field's first allow narrows access, and later allows
    widen it again (`first_field_allow_narrows`).
  - A field policy can't read fields that can be hidden (§6.6 item 7).
- **Path constraints** declare what isn't a guard, in the `extend type`
  block: `shep::contained_in` and `shep::independent_of` (§6.7), and
  `shep::strict_revocation` on a file field (§16.4). `shep::ack` in a
  policy's body records a reviewed report cell (§6.7).
- **Policy files hold only policies.** A file in `shep/policies/` may
  add, in `extend type` blocks, access policies, field and edge policy
  blocks, and those path constraints, and may declare functions and the
  `global actor` (§6.3). It can't add fields.

**Porting Ent-style policies.** Ordered first-match rules have an exact
union-minus-deny translation, which authors porting from Ent can apply by
hand (`port_exact` in `PolicyCombine.lean`). Write the rules in order;
each is an allow `A_i` or a deny `D_j`. Keep every allow, and rewrite
each deny as `D'_j = D_j ∧ ¬(∨_{i<j} A_i)`: a deny carries the negation
of every allow that came before it. A policy whose denies all come
first, as above, needs no change. Ent treats "every rule skips" as
allow, so a port adds a final `allow … using (true)` unless the Ent
policy ends in `AlwaysDenyRule` (`port_entDefault`). The port keeps the
policy's stability (`port_safe`, §7.6). A rewritten deny
that negates a recursive `shep::visible` isn't allowed (§6.8); restate
the exception instead.

Guards are ShepQL expressions rather than arbitrary code. Keeping them
declarative makes three things possible: pushdown into storage scans
(§7.5), compilation to relations for audience queries and their changes
(§6.8), and static analysis of policy changes. Wasm functions are allowed
but get none of these.

### 6.2 Policies belong to data, not to views

This is the most important privacy decision. **Read policies are not
versioned with the schema view.** The owning schema's *current* policy
always applies, whatever version the reader is pinned to. If `users@6` hides
`email` from non-admins, `issues@3`, which reads through `users@5`, must not
see email either.

As a result, **visibility is never part of the compatibility contract.** Any
node, edge, or field can come back *not visible* to any reader at any time.
Ent works the same way: edge loads can return fewer nodes than exist. The
generated API has to model this explicitly. Edge lists are filtered, single
edges can be `null`, optional fields hidden by a field-level policy read as
absent, and required fields must be declared `Redactable` (§6.6 item 7).
Policies have their own history, separate from schema versions.
That doesn't mean they skip checking. One policy governs data that every
in-service version reads and writes, so a policy update has to be verified
against all of those versions, just as a new schema version is (§6.6).

### 6.3 Authentication, actors, and viewers

A request carries three principals: the **calling app**, the **actor**,
and the **viewer**. This is OAuth 2.0 token exchange's model (RFC 8693),
where `act` names who is acting and `sub` whom the request is for. All
three come from a bearer token issued by an external auth service (below).

- **The actor** is who is doing the work, as authenticated: any node,
  such as a `users::User` who signed in through SSO or an `agents::Agent`
  an app runs, or the app acting as itself. The app acting as
  itself is `actor = app` with no viewer.
- **Per actor and per viewer.** Read-your-writes and audit are per
  actor. Everything access-related (policies, audiences, live views,
  links, per-viewer aggregates) is per viewer.
- **The viewer** is whom the request is for. **It defaults to the
  actor:** a person signed in through SSO is both. An agent working for a
  user has `actor = agent g1` and `viewer = user u1`.
- **Tokens establish act-for, never schemas.** Which viewer an actor
  may act for is the auth service's decision, carried in the token
  (below), so no node type declares it and no schema maps it.

**Policies see the viewer.** Each schema declares its viewer type
(`global viewer: users::User`, §4.1.1), and policies read it:

```
extend type Issue {
  access policy members allow update read, update write
    using (global viewer in .project.members);
}
```

- **A viewer of another type is denied.** If a request's viewer isn't of
  the schema's viewer type, every decision of that schema is a deny,
  before any policy runs. It never reaches allows on `global shep::app`,
  so a stray agent viewer can't reach grants to the app.
- **No viewer is an ordinary case.** The app acting as itself
  (`db.as_app()`) has no viewer. Atoms on `global viewer` are then false,
  per atom (§6.1), and an allow meant only for the app says `not exists
  global viewer` explicitly.
- **`shep::visible` keeps the request's viewer.** The target schema sees
  the same viewer, checked against its own viewer type. The checker warns
  when `shep::visible` or a function crosses into a schema with a
  different viewer type, since the target then denies.

**Actors are opt-in per schema.**

- **Any actor is permitted unless a schema says otherwise.** Most schemas
  never mention actors: what matters is whom the request is for.
- **A schema that cares declares its permitted actors** in its policy
  files, as the type of `global actor`, and only then may its policies
  read it:

  ```
  # issues: shep/policies/actors.esdl
  using schema agents version '2';

  module issues {
    global actor: users::User | agents::Agent;

    extend type Issue {
      access policy no_agents deny delete using (global actor is agents::Agent);   # agents edit, never delete
      access policy members  allow delete using (global viewer in .project.members);
    }
  }
  ```

  - **Undeclared means unreferenced.** The checker rejects
    `global actor` in the policies of a schema that declares no actors.
  - **The app can be declared too.** `global actor: users::User |
    shep::App` lets policies tell the app acting as itself apart from
    people.
  - **Allows on the actor are flagged.** `allow … using (global actor is
    agents::Agent and …)` grants by who is acting rather than whom it's
    for, so the checker flags it as possibly amplifying access, and the
    deploy report names it.
  - **Agents reading their own state** declare `global actor:
    agents::Agent` and write allows on it (`using (.agent ?= global
    actor)`), with the agent as its own viewer or a credential naming its
    user (§6.3 below).
  - **Declared means enforced.** A schema that declares actors denies a
    request whose actor isn't one of the declared types (`denied`), before
    its policies run.
  - **Policy, not contract.** The declaration lives with the policies and
    deploys like them, because no generated code depends on it: it only
    changes what's admitted, which is what policies do. Adding or
    narrowing it is checked like any policy tightening (§6.6 item 8).
    The viewer type stays part of the schema version, because mutations'
    `global viewer` and generated types depend on it.
- **Seeing who arrives.** "Any actor is permitted" is the default, so
  tooling shows who actually reaches each schema: `shep policy matrix`
  lists, per schema, the actor types that reach it through in-service
  operation sets and the tokens seen, and a schema owner's deploy report lists
  newly arriving actor types.

**Deleted actors and viewers.**

- **A deleted actor is rejected** (`denied`) before any policy runs,
  whatever its token says. This check is load-bearing: soft delete keeps
  edges, so policies alone would still admit a deleted actor. `can_op` is
  false for a deleted actor on every node, under every policy and app,
  only because of it (`canOp_deleted_actor`, and `precheck_needed` for
  the counterexample without it, in `formal/lean`). The theorem holds at
  the snapshot a check evaluates; the horizon below says how old that
  snapshot can be.
- **A deleted viewer is rejected too.** A request whose token names a
  deleted viewer is `denied` before any policy runs, the same as a
  deleted actor. An agent acting for an offboarded user keeps nothing the
  user had, even though the user's membership edges are still in place,
  and even while its token hasn't expired (`canOp_deleted_viewer`, proven
  in `Principals.lean`, §18).

**Credentials belong to an external auth service.** Shep manages no
credentials and signs nothing to authenticate anyone. A separate auth
service, outside Shep, is responsible for:
- **identity:** signing people in (company SSO), and mapping identities
  to actor nodes such as `users::User`;
- **consent for act-for:** deciding when an agent may act for a user,
  and which agents an app runs;
- **credential lifecycle:** the keys and secrets apps and CI hold,
  rotation, and expiry;
- **allowlists:** which apps may obtain tokens for arbitrary users, for
  work no user is present for, such as a queue worker;
- **revocation:** refusing to issue new tokens to a revoked app, key, or
  person.

It hands out short-lived **bearer tokens** that say what the bearer may
do in Shep.

**Shep verifies bearer tokens.**
- **Format.** A token is an OAuth 2.0 access token in the JWT profile of
  RFC 9068: a JWT (RFC 7519) in JWS compact form, with header `typ`
  `at+jwt`, signed with an asymmetric key: EdDSA over Ed25519, with ES256
  also accepted. Shep holds only the auth service's public keys, so it
  can verify tokens and can never mint one; a shared HMAC secret would
  let any Shep node do that, so none is accepted.
- **Keys.** Shep fetches the public keys from the auth service's JWKS
  endpoint, configured per fleet, or takes them from fleet
  configuration, and picks one by the token's `kid`. Several keys can be
  active at once, so the auth service rotates by publishing a new key
  before signing with it.
  - **Freshness.** Shep refreshes the JWKS every `R` (60 s by default),
    and also when a token names an unknown `kid`, rate-limited. A key
    missing from the latest successful fetch is dropped at once. If
    fetches keep failing, cached keys expire `M` (1 hour) after the last
    successful one, and Shep then fails closed. A fleet-configuration
    denylist of `kid`s is the break-glass for a leaked key.
  - **The bound.** A key the auth service withdraws stops being accepted
    within `R`; a token signed with it before then works for at most `R`
    plus its own lifetime.
- **Claims:**

  | Claim | Meaning |
  |---|---|
  | `iss` | The auth service. It must equal the fleet's configured issuer |
  | `aud` | The universe's URL, such as `https://staging.shep.internal`. It must match the host the request reached (§14.3). Tokens for fleet-level operations name the fleet, `https://<fleet>.shep.internal`, and tokens for ephemeral universes name the universe's immutable ID (§14.3) |
  | `client_id` | The calling app, a name in the universe's registry (§14.1), as RFC 9068 names the client |
  | `act` | The actor, as RFC 8693's actor claim: `{"sub": "agents::Agent:<key>"}`, or `{"sub": "app"}` for the app acting as itself. A nested `act` (a chain of actors) is refused |
  | `sub` | The viewer: a node, or absent for the app acting as itself. Defaults to the actor. This is RFC 8693's `act`/`sub` split |
  | `scope` | A space-delimited string of what else the bearer may do: `as_app`, `deploy`, `manage_universes`, `seed` |
  | `exp`, `nbf`, `iat` | Expiry, not-before, and issue time |
  | `jti` | The token's ID, recorded with every write (§10.1) |

- **What Shep checks:** `typ` is `at+jwt`; the header's `alg` equals the
  JWK's `alg`, and is never `none` or an HMAC; the signature, against an
  active key; `iss`; `exp` and `nbf`, with a small allowance for clock
  skew; that `aud` names the universe of the host, so a test token sent
  to `shep.internal` fails and never falls back to production; that the
  app is registered in the universe and admitted by the schemas it uses;
  and that the actor and the viewer exist and aren't deleted (above).
- **Reused universe names.** A universe dropped and recreated under the
  same name has the same URL, so a token for the old one would reach the
  new one within its lifetime. Tokens for long-lived universes
  (production, staging) name the URL; tokens for ephemeral test and dev
  universes name the immutable ID instead, which is never reused.
- **Everything else comes from the claims.** The `(app, actor, viewer)`
  triple and the scopes are the token's; Shep trusts the auth service
  for act-for and checks nothing about who may act for whom. The Rust
  client's handles are ways to present tokens (§7.3), and
  `db.as_user(u).by(g)` means "a token whose claims say actor `g`,
  viewer `u`". Running as the app itself needs a token with the `as_app`
  scope and no `sub`.
- **Revocation is bounded by token lifetime.** Shep keeps no revocation
  list in v1: a token is honored until it expires, even after the auth
  service revokes the key or person it was issued to. Deleting the actor
  or viewer node in Shep is the exception, and takes effect within the
  horizon below. Tokens should live minutes, not hours; whether Shep
  should also accept an optional revocation list is open question 26.
- **Expiry on long-lived connections.** Shep sends a subscription a
  `reauth` update 60 seconds before its token's `exp`. At `exp` the
  stream pauses: no pushes, with its state kept, until a new token
  arrives or a grace period ends, when it closes (§13.4).
  - **Same claims.** A new token is accepted for the stream if its
    `aud`, `client_id`, `act`, and `sub` equal the old one's and its
    scopes are no wider; anything else is a new subscription.
  - **Messages.** On the native protocol, `reauth` is a stream frame and
    the client answers with a `reauthenticate` request carrying the
    token on the same connection. Over `graphql-transport-ws`, `reauth`
    is a `next` message with an `extensions.shep.reauth` field, and the
    client answers with a `ping` whose payload carries the token.
  - **Links follow.** On `reauth`, the subscription's download handles
    are extended to the new token's `exp` (§16.4). Download links never
    outlive the token that minted them.

**The authorization horizon: one rule.** The revocation bound `B` is 5
seconds by default, and absorbs clock drift between leaders and front
ends. It bounds what Shep itself decides: registry changes (policy
deploys and promotions, app grants) and deleting an actor or a viewer.
Token revocation isn't Shep's, and is bounded by token lifetime (above).

- **Tails.** Each front end tails the committed entries of the
  universe's meta partition and the partitions of the actors and viewers
  it serves, with long-poll fetches from those partitions' leaders
  (§10.4). The evidence for a partition is the front end's position in
  that log, plus the leader's `(commit index, leader-lease stamp)`,
  which every fetch response carries, and followers relay unchanged.
  - **Long-polls are short enough.** A fetch's `max_wait` is at most
    `(B − 2ε)/4`, so an idle partition still refreshes its evidence well
    within the bound.
  - **Across a split.** Evidence from a partition whose committed log
    contains a split entry doesn't count for ranges past that entry. The
    front end switches its tail to the child holding the range, and fails
    closed for it until it has evidence from the child. So the front
  end knows each partition's commit index as of a recent moment, without
  a read per request.
- **The rule.** Every principal-sensitive request, mutation proposal,
  two-phase commit decision, live-topic step, and download-link check
  runs at a version `S` that contains the **horizon**: for each of those
  partitions, an index the front end saw committed no more than `B` ago.
  So policy deploys, app grants, and deleting an actor or a viewer take
  effect within `B` everywhere.
  - **When evidence counts.** A heartbeat counts while the front end's
    clock minus the heartbeat's leader-lease time is at most `B − 2ε`,
    where `ε` bounds any node's clock error. That keeps the real age of
    accepted evidence at or below `B` (`AuthHorizon_nodrift`).
  - **Leases bound stamps.** A leader stamps heartbeats only while it
    holds its lease, and a new leader commits nothing until its own clock
    passes the previous lease's end. No other margin is needed at a
    handoff: the `B − 2ε` test already absorbs the skew between the two
    leaders (`AuthHorizon_deposed`).
- **Two parts.** The principal part of the horizon (the registry, and
  the actor's and viewer's lifecycles) always applies, and comes from
  those tails at no read cost per request. The data-partition part applies
  only to reads under a revocation-sensitive decision (§7.6). That is
  judged over the whole decision: `visible()` of the type, field
  policies, `contained_in`, and the version's lens, not just the policy's
  guards.
- **Materializers refuse stale versions.** A materializer refuses an
  principal-sensitive, revocation-sensitive read at a version whose component
  for its partition is older than its leader's commit index of `B` ago,
  judged by the same evidence and the same `B − 2ε` test. Being caught up
  isn't enough: a topic reads at the published frontier, and a closed
  multi-partition read at its closure, and either can be older than what
  the materializer has applied (`AuthHorizon_topicchan`). So other
  permission changes it applies, such as removing someone from a channel,
  are bounded the same way, and a topic whose frontier lags on such a
  partition pauses (§13.2). A read decision that
  isn't revocation-sensitive can only under-grant when stale, so a
  lagging materializer still serves it. For example: `Build` is
  `shep::Permanent`, its `owner` link is `readonly`, and its read policy
  is just `allow select using (.owner ?= global viewer)`. Most real
  policies have a deny or `contained_in` over mutable facts, such as `Message.read`'s
  blocked list (§6.1), and get no benefit.
- **Write policies are validated.** A write policy's reads are validated
  at commit, through a read-only participant if they're in another
  placement group (§8). A stale write is permanent, so the horizon alone
  isn't enough there. `shep ops check` shows what each participant costs.
  A write policy is skipped only if every allow is Stable and
  every deny is Stable⁻ (§7.6).
- **A new viewer waits for evidence.** A front end serving a viewer whose
  partition it doesn't tail yet fails closed until it has evidence for
  that partition.
- **Fail closed.** A front end that can't establish the horizon, for
  example because it's partitioned from a leader, refuses with
  `unavailable` (§15.3). It never answers from state it can't show is
  fresh.
- **Writes in flight carry a deadline.** A proposal or two-phase commit
  decision carries a **horizon deadline**: the oldest leader-lease time in
  the evidence its `S` was checked against, plus `B − 2ε`. The leader that
  appends it rejects it with a retryable `too_old` once its own clock
  passes the deadline, and the front end re-executes at a fresh horizon.
  Checking when sending isn't enough on its own, because a message can wait
  in a queue, a retry, or a paused process (`AuthHorizon_inflight`). So a
  mutation admitted before a revocation can't be appended more than `B`
  after it. The deadline alone carries the bound; re-checking before
  sending only avoids certain rejections (`AuthHorizon_reusedeadline`).
- **Not validated at commit.** The actor's and viewer's lifecycles are
  checked against the horizon, not as validated reads (§10.3): validating
  them in every mutation would make nearly every mutation a two-phase
  commit with their partitions.
- **On a single node** every partition is local, so the evidence is
  trivially current (§9.6).

- **App identity matters on its own.** If an app can act as a user, that
  alone shouldn't let it read every schema that user can see.
- **Schemas grant access per app.** A schema grants which apps may use it,
  and which apps may depend on it (for example, `users` grants dependency
  to `issues`).
- **Policies can test the app** (`global shep::app =
  <shep::App>'slack'`), **and the actor** in schemas that declare their
  actors (`global actor is agents::Agent`, above).

**Ways in.** Every request presents a token from the auth service; the
paths differ only in how the bearer gets one.

- **End users, from browsers.** The app's page gets the user's token from
  the auth service, after the user signs in there, and passes it to
  `connect({ token })` (§7.4). The token's `client_id` must be the app whose
  origin the request comes from, among the origins it registered for
  that universe; production origins are never accepted by another
  universe. Shep's endpoints must be reachable from wherever users'
  browsers are, which for an internal system means the corporate network
  or VPN.
- **Backends, for a signed-in user.** A backend that serves a browser
  request forwards the user's token, or exchanges it at the auth service
  for one naming the backend's app: `db.with_token(&token)`.
- **Agents.** An app that runs an agent for a user gets a token from the
  auth service whose `act` is the agent and whose `sub` is the user, as
  the auth service's consent rules allow. Read-your-writes then follows
  the agent. An agent with no user gets a token whose `sub` is itself,
  or relies on `global actor` policies in the schemas it uses (open
  question 23).
- **Work no user is present for,** such as a queue worker, uses tokens
  the auth service issues to that app for arbitrary users; which apps may
  get them is the auth service's allowlist.
- **People, from the CLI.** The CLI gets a token from the auth service,
  through whatever flow it offers, and presents it. The app is the
  built-in `shep-cli` (§7.2). Shep has no login flow.
- **CI.** Deploys present a token with the `deploy` scope, and test runs
  one with `manage_universes`. A leaked deploy token can't read data,
  and expires within minutes.
- **Tests, dev, and CI.** No Shep tool signs anything.
  - **In-process tests.** `shep::test::db` uses the token verifier's test
    variant (the verifier is an enum, §17), which takes claims directly
    instead of a token, so tests can act as made-up users and agents
    with no keys at all.
  - **Shared-fleet test and dev universes** get tokens from the auth
    service like everything else, for example from a test issuer whose
    key only universes of kind test or dev trust.
  - **Fleet-level operations,** such as creating a universe, use tokens
    whose `aud` is the fleet, `https://<fleet>.shep.internal`.
  - **Seeding.** A token with the `seed` scope may create principal
    nodes (made-up users and agents) directly, and is accepted only in
    test and dev universes.

**Scopes.** Reading and writing data needs no scope: the claims name the
principals, and policies decide. The scopes cover the rest:

| Scope | Allows |
|---|---|
| `as_app` | Running operations as the app itself (`db.as_app()`), with no `sub` |
| `deploy` | Deploying the app's schemas, policies, and operation sets, and registering names in production (§14.1) |
| `manage_universes` | Creating and dropping dev and test universes (with a fleet `aud`) |
| `seed` | Creating principal nodes, such as made-up users and agents, directly; accepted only in test and dev universes |

- **Every write is attributable.** The mutation log records the app, the
  actor, the viewer, the token's `jti`, and its other claims for each
  transaction (§10.1), so any impersonation can be traced to the token,
  and through the auth service's records to whoever obtained it.

### 6.4 Evaluation strategy

- **Single check** ("can this actor read node N?"): evaluate guards with
  a per-request memo keyed by `(policy, node)`. `shep::visible` chains are shared
  across a request, which matters when loading 50 messages from the same
  channel.
- **Lists** (traversals and paginated queries): post-filtering alone, which
  is Ent's model, gives short pages and wasted reads. Instead, **compile
  pushdown-able guards into the scan**: the materializer checks
  `members(channel, viewer)` against the edge range as it scans, a semi-join
  per row or one batched range read per page, and post-filters only the
  remainder. A pushed-down scan returns the keys and ranges it read along
  with its rows, so a mutation's read set includes them (§10.3), and the
  read uses the policies in force at its version. When the anchor is fixed, partially evaluate the policy
  with respect to it (see §6.8).
- **Nested group membership** ("member of a team that's a member of an org")
  can need deep transitive checks. Zanzibar's Leopard index is the reference
  design if this gets slow. Don't build it until it's needed.
- **The evaluator and the pushdown compiler must agree exactly.** Missing
  fields are where they tend to differ. In ShepQL an element-wise operator
  over an unset field is empty, and only `??`, `?=`, `?!=`, and `exists`
  see absence; a compiled predicate that followed SQL's `NULL` rules, or
  compiled `?=` as `=`, would admit or deny differently. Totality keeps the
  cases few (§6.1), but the compiler must keep the evaluator's semantics.
  Differential tests (generate random policies and states, run both, compare)
  are the guard here.

### 6.5 Deletion is a visibility state

Deleting a node is soft: it sets `deleted_at`, and every edge stays. A
deleted node is conceptually the same as a node the viewer has no permission
to read. Because §6.2 already makes every reference possibly hidden, deletion
adds no new cases for readers, schema versions, or the compatibility
checker.

**Visibility check.** A node is visible to a viewer when:

```
visible(node) = not node.placeholder
                and policy_allows(select, node)
                and (not exists node.deleted_at or query roots at shep::with_deleted)
```

- **The deletion check sits outside the policy**, so policy authors can't
  forget it.
- **Placeholders are never visible** (§3), not even under
  `shep::with_deleted`. A placeholder has no data for a policy to read, so
  the check comes first, and hidden, deleted, placeholder, and absent all
  look the same.
- **The policy still runs on deleted nodes.** Their edges are intact, so
  membership checks evaluate normally.
- **Queries opt in.** `shep::with_deleted(Issue)` at a query's root
  returns deleted nodes as tombstones (key, `.deleted_at`, which is
  readable only there, and whatever fields the read policy allows),
  but only for actors the read policy already admits. The default is to
  treat deleted nodes exactly like hidden ones.
- **Hidden and deleted look the same by default.** Telling them apart would
  reveal that a node exists to viewers who can't read it. Only a query that
  opts in, from a viewer the policy admits, learns that a node is deleted
  rather than hidden.

**Deletion can cascade through `shep::visible`, with no cascade step.**
`shep::visible(.channel)` includes the deletion check. If `Message`'s
read policy is `allow select using (shep::visible(.channel))`, deleting the channel
hides all its messages in default queries, and no message rows are written.
Restoring the channel makes them visible again, and there is no cascade
bookkeeping to undo. The messages' own `deleted_at` stays null, so a message
that was deleted individually beforehand stays deleted after the restore.

This is a consequence of how the policy is written, not something the
system guarantees. Another allow (for example, `allow select using
(.author ?= global viewer)`) still admits after the parent is deleted.
If a deleted parent must always hide its children, the policy declares
`constraint shep::contained_in on (.channel)` (§6.7). Authors shouldn't have to work
out these interactions by hand; §6.7 covers the tooling that shows them.

**Restore** is a normal mutation with its own `restore` policy. It can fail
on an `exclusive` constraint if another live node has taken the value since
(for example, the same email). The error names the conflicting constraint.

**Physical purge** (storage reclamation) comes later and also fits. After a
purge, references point to a node that no longer exists, and readers
already handle that as hidden. Purge then means deleting every version of
the node's keys and its edges, leaving only a key ghost (§3), with no change to any schema's contract. The edges must go:
an app or content key is never reused (§3), but a later node with the same
key must never inherit a purged node's edges. Purged nodes stay in the
mutation log, by design: the log is the full history (§10). A purge
leaves a key ghost with the purge's dot, because keys are never reused
and a freshness check over that key must still see that it changed
(§11.7).

**Cost.** Edges pointing to deleted nodes still appear in edge lists and
have to be filtered out, which shortens pages the same way privacy filtering
does. Pushdown (checking the target's lifecycle key during the scan)
handles this. If an edge list with heavy deletion (for example, deleted
messages) becomes a hotspot, the fix is to copy the deleted flag onto the
edge.

### 6.6 Checking policy updates

**When checks run.** Policies are not versioned with views, so a policy
applies to every reader regardless of version. It evaluates stored data,
which is always valid for the head (§4.2). The checker runs whenever any of
its inputs changes:

- a policy update
- a new schema version
- a change to a dependency pin

**0. Only graph inputs, and total.** The checker rejects a policy or
function that isn't Pure (it reads the clock, randomness, or anything
else outside the graph), and a guard whose inferred cardinality isn't
One (§6.1).

**1. Type-check against the head's shape.**

- **Why the head is enough.** Storage always holds head-valid data. Older
  writers' writes pass through adapters, and tightening changes come with
  backfills, so the head's field types are exact.
- **Re-checking.** Registering a new head re-checks every policy.
- **Why a type error is a hard error.** An unhandled optional field
  wouldn't fail loudly at run time: a guard that could be empty would
  silently hide every row that lacks a value, or silently stop a deny
  from denying. Totality turns that into a registration error (§4.0).

**2. Write policies must not lock out in-service writers.** For each
in-service version `v` and each write operation, at least one post-state
that a `v` write can produce must be allowed (an SMT satisfiability check
of `(∨A) ∧ ¬(∨D)` over the subset, §4.5). Example: `v2` adds `visibility`
with no default and the create policy becomes `allow insert using (exists
.visibility)`. Every `v1`
create is now denied, which breaks `v1` as surely as tightening a
constraint. This check catches only structural lockouts, not every
unintended denial. Items 4 and 5 cover the rest.

**3. Cross-schema references are dependencies.** When a policy reads a
foreign node's fields, calls `shep::visible` on it, names its type in a
`global actor` declaration, or uses its functions, it does so through a
policy pin (`using schema agents version '2'` in the policy file,
§4.1.4). The pin is recorded in the policy artifact
(in the meta partition, §3) and keeps the version in service (§4.6) for as long
as the current policy references it. `users@5` can't retire while
`issues`' live policy still reads `users::User` through it. Moving a pin is
a policy update, checked like any other.

**4. Property tests.** A policy update runs every property test of its
type (§6.7). That's how owners pin down what a policy must keep doing
across policy updates, such as "authors can always read their messages". They
are the owner's own safety net, not part of any version's contract:
visibility stays entirely outside the cross-schema contract (§6.2).

**5. Diff report.** Every update is classified as *more restrictive*,
*more permissive*, *equivalent*, or *incomparable*, with a concrete
counterexample (viewer plus node state) for each direction that changed.
Union minus deny makes each side one formula, `(∨A) ∧ ¬(∨D)`, so the
comparison is two satisfiability checks, as in Cedar's analysis tooling.
- **The diff is of the effective policy,** not the source: the formula
  includes the deletion check, `shep::contained_in`, the viewer-type
  pre-check (§6.3), and the per-atom rule for absent principals (§6.1).
- **Principal presence is a case split.** Each comparison is run with the
  viewer present and absent, and the actor present and absent, since an
  atom on an absent principal is false and `not` flips it. A change that
  only matters for the app acting as itself is reported as such.

- **A field's first allow is labeled.** Adding the first allow to a
  field narrows access to it, unlike any other added allow
  (`first_field_allow_narrows`, §6.1), so the diff report labels it
  "first field allow: narrows".

More-permissive changes can require a second reviewer.

**6. Shadow rollout.** Static checks can't know intent. A new policy first
runs in shadow mode: it is evaluated alongside the live policy on real
traffic, and differing decisions are logged and summarized but not
enforced. Promotion is an atomic switch, and rollback is just another update
that goes through the same checks.

**7. Field-level policies only on fields that can be hidden.**

- **Why this is needed.** Policies aren't versioned, but generated client
  types are. Suppose a policy update could hide a field that a compiled
  `issues@3` client has typed as `String`. That client would then receive a
  value it has no way to represent.
- **The rule.** A field can be hidden when every in-service version reads
  it as optional. (A field policy is an `access policy` block inside the
  field, §6.1.) A hidden optional field reads as `None`, the same way a
  hidden edge does: absent and hidden look alike. A required field has to
  be declared *redactable* (`shep::Redactable<T>` in Rust, plus a
  `<field>Redacted` flag in GraphQL). The checker rejects a field-level
  policy on any field that some in-service version reads as required and
  not redactable.
- **Hideability is part of the contract; the policy isn't.** Making a
  required field redactable is a change readers can see. A new version
  declares it, and older versions need an explicit adapter fallback for
  hidden values, usually `hide` (§4.3).
- **Deprecated fields are the main use.** An owner hides a deprecated
  field from the older versions that still read it, or denies their writes
  (§4.1.4).
- **Hiding a whole node is always allowed**, because every reference was
  already typed as possibly absent.
- **A field policy must not read a field that can be hidden from the same
  actor.** Otherwise the visible result of one field's policy depends on
  another field's hidden content, and search ranking, which scores only
  visible fields, leaks it (`fieldPolicy_reading_hidden_leaks` in
  `formal/lean`, §16.6). The checker enforces this.

**8. Actor declarations must not strand apps.** A policy deploy that
adds or narrows a schema's `global actor` declaration (§6.3) denies every
request whose actor isn't listed. The checker compares it against the
principal shapes of in-service operation sets that reach the schema,
directly or through `shep::visible` and functions, including their use of
`as_app`, and treats any it would deny like a writer lockout (item 2):
the deploy report names the apps and operations that lose access, and the
change needs the same review as a more-permissive one.

### 6.7 Showing authors what their policies do

Policies are short, but their consequences are hard to see. Allows,
denies, `shep::visible`, and the deletion check interact, and the
author-still-sees-their-message case in §6.5 is typical of the mistakes
that result. Reading a
policy isn't enough to understand it. The tooling has to **show** authors
the consequences before anything ships, and **make them choose
explicitly** wherever the outcome is likely to surprise them.

**Containment.** Authors state intent directly, with an annotation in the
policy file (the `Message` policies of §6.1):

```
extend type Message {
  constraint shep::contained_in on (.channel);
  access policy blocked deny  select using (global viewer in .author.blocked);
  access policy author  allow select using (.author ?= global viewer);
  access policy channel allow select using (shep::visible(.channel));
}
```

- **Semantics.** `shep::contained_in` adds an implicit deny for every
  operation: deny if the container is deleted. Like the node's own
  deletion check, it respects `shep::with_deleted`. No allow can get
  around it.
- **Where it lives.** It belongs to the policy file, not to the link
  declaration, because it only affects visibility, and visibility isn't
  part of the versioned contract (§6.2).
- **Authors must choose.** If a policy uses `shep::visible` through a
  required single link (the usual shape of a parent link), the checker
  requires the author to annotate either `shep::contained_in` or
  `shep::independent_of` for that link. A
  silent default would be wrong for some apps no matter which way it went.
  Requiring a declaration means the question gets answered when the policy
  is written.

**Access-path matrix.** For every policy, the checker derives its *access
paths*. Each one is an allow, with `shep::visible` expanded, plus the
conditions it needs; every deny applies to every path. It then evaluates each path against a standard set of
events:

- the node is deleted
- each parent it reaches through `shep::visible` is deleted
- the viewer loses each edge the path depends on
- the viewer is blocked
- the viewer is deleted (denied), the request has no viewer (the app
  acting as itself), or its viewer is of another type (denied)
- for each allow, the denies that override it: the cells where the
  allow's conditions hold and a deny's do too

A schema whose decisions read `global actor`, directly or through
`shep::visible` and functions, gets one column group per actor type, so an author sees at
once what agents can do that people can't, and the reverse. Every schema's
matrix also lists the actor types that actually reach it (§6.3).

```
Message select     channel deleted   viewer removed    author blocks     allow overridden by
                                     from channel      viewer
  author           hidden¹           STILL VISIBLE²    hidden            deny blocked³
  channel          hidden            hidden            hidden            deny blocked

  ¹ constraint shep::contained_in on (.channel)
  ² acknowledged: shep::ack(shep::Event.ViewerRemoved, .channel)
  ³ to let authors keep seeing their own messages when blocked, write:
    deny select using (global viewer in .author.blocked and not (.author ?= global viewer))
```

- **Overrides come with the exception text.** The "allow overridden by"
  column names each deny that can cancel an allow, and the CLI prints the
  rewritten deny that would exempt that allow's case, so an author who
  wants Ent's "this allow wins" behavior can paste it.

- **How cells are computed.** Initially by bounded enumeration: generate
  small graph states and run the evaluator. Later, by the SMT backend
  (§4.5), which turns each cell into a proof. Cells involving guards
  outside the SMT subset are marked *not proven*.
- **Where it shows up.** The CLI prints the matrix on every check. Diff
  reports (§6.6 item 5) highlight the cells that changed.
- **Survival cells need an acknowledgement.** A *STILL VISIBLE* entry under
  a deletion or revocation event must be acknowledged once in the policy
  source, as a typed statement in the allow's body:
  `shep::ack(shep::Event.ViewerRemoved, .channel)` names the event
  (`Deleted`, `ParentDeleted`, `ViewerRemoved`, `ViewerBlocked`) and the
  path it's about, so the checker can match it to its cell. An update
  that introduces a new survival cell fails the check until it is
  acknowledged. Acknowledgements are the counterpart to property tests:
  property tests record what must stay true, and acknowledgements record
  surprising outcomes the author has deliberately accepted.

**Effective policy view.** `shep policy show Message select` prints the
policy as it is actually evaluated:

- inlined functions spliced in
- implicit denies made visible: the node's deletion check,
  `shep::contained_in`, and the viewer-type pre-check
- each `shep::visible` expandable to the policy it calls

What authors read should be exactly what runs.

**Explain.** `explain(viewer, node)` returns the allows that admitted
and the denies that matched, the full `shep::visible` chain, and, for a
denial with no matching deny, the nearest allow that would have admitted
and the condition that failed. It's available in dev tools and to
schema owners. It isn't exposed to end users, because it reveals both the
policy and facts about the graph.

**Policy tests.** Policy tests are ShepQL files in
`shep/policies/tests/`, written with `assert`. `shep policy test` runs
them, and so does every policy deploy and schema registration. Inside a
test, `shep::decide(op, node, viewer := v)` evaluates a decision for a
given viewer (and optionally `actor :=`); it exists only in tests. There
are two kinds.

- **Fixture tests** (`#! shep: fixture`). Inserts that build a small
  graph, then `assert`s on decisions. The matrix suggests one test per
  cell, so the cases most likely to surprise someone tend to be covered.
- **Property tests** (`#! shep: property`). Node-typed parameters are
  quantified over every generated graph and node, and each `assert`
  must hold for all of them:

  ```
  # slack: shep/policies/tests/blocked_never_see.edgeql
  #! shep: property
  with v := assert_exists(<users::User>$viewer), m := assert_exists(<Message>$message),
  select assert(not (v in m.author.blocked)
                or not shep::decide(shep::Op.select, m, viewer := v),
                message := 'blocked_sees_message');

  # users: shep/policies/tests/read_self.edgeql
  #! shep: property
  with v := assert_exists(<User>$viewer),
  select assert(shep::decide(shep::Op.select, v, viewer := v), message := 'cant_read_self');
  ```

  Under union minus deny, "authors can always read their messages"
  doesn't hold for a blocked author; a test that says so fails and names
  the `blocked` deny.

  - **How they're checked.**
    - **Now:** by bounded enumeration over small generated graphs, the same
      machinery that computes the access-path matrix.
    - **Later:** proven with the SMT backend once it exists (milestone 14).
      Until then, a property that holds within the bounds is reported as
      checked, not proven.
  - **Limits.** A property involving guards outside the SMT subset, or
    wasm, can only be checked by enumeration.
  - **Why they matter.** Fixture tests cover the cases someone thought to
    write down. Property tests cover all of them. They're the right tool
    for "must never" security properties on long-lived policies that many
    people edit.

### 6.8 Policies as relations

A declarative read policy defines a relation, `can_read(viewer, node)`,
and the same holds for each operation. Requests come from an app and an
actor for a viewer, so what Shep actually evaluates is
`can_op(app, actor, viewer, node)` (below). Compiling policies into relational
form enables three things that evaluating them one row at a time can't:
"who can see this?" queries, the changes to that answer over time, and
materialized permission indexes.

- **Compiling union minus deny.**
  - **One anti-join.** `can_op = (A_1 ∪ … ∪ A_n) ▷ (D_1 ∪ … ∪ D_m)`: the
    union of the allows' matches, minus the union of the denies'.
    Viewers no allow admits are absent, which is the default deny. There
    is no rule order to compile.
  - **Guards.**
    - Predicates over node fields become filters.
    - `global viewer in .members` becomes a join on the edges relation.
    - `shep::visible(.e)` becomes a join with the target type's
      `can_read`.
  - **Recursion.** Recursive `shep::visible`, such as nested groups,
    becomes a recursive relation: a least fixed point.
    - **Cost.** Its incremental cost depends on how many derived facts a
      change adds or removes, not on the change's size (§11.5).
    - **What's required.** A `can_read` that recurses must be materialized
      (§12) or declare a depth bound, and the checker enforces this.
      `audience` and `observe` over such a policy read the materialized
      relation.
    - **No cycle through negation.** Policies are stratified: the
      dependency graph of `can_op` relations, through `shep::visible`,
      `shep::can`, and functions, may have cycles, but none that passes
      through a negation, and a deny counts as one.
      - **What the checker rejects.** A cycle through a deny's guard, a
        `not`, or a `not exists` over a recursive relation.
      - **Why.** Such a cycle can have no consistent answer, as
        `X = {v} ∖ X` shows (`nonmonotone_no_fixpoint`).
      - **What's still allowed.** Denies, and negations, over relations
        that don't depend on the one being defined.
- **Audience queries.** `shep::audience(node, op)` is `can_op` restricted to one
  node: everyone who can perform `op` on it. Its delta between two versions
  is the permission change between them. This is what an `observe` clause
  asks for (§11.3).
- **Anchors.**
  - **The split.** For a `Message` read policy that allows
    `shep::visible(.channel)`, partial evaluation (§6.4) splits
    `audience(select, message)` into two parts: the channel's audience,
    which is shared by every message in the channel, and the message's
    residual: the viewers its own allows add, minus those its denies
    remove.
  - **Why it matters.** An app caching the channel's audience then needs
    only the residual per message.
- **Materialized permissions.** For hot types, `can_read` can be a
  materialized query (§12). That gives Zanzibar-Leopard-style fast checks
  and fast audience queries, maintained incrementally.
  - **Only within the horizon.** A materialized `can_read` lags the log, so
    a principal-sensitive decision may use it only when its version contains
    the authorization horizon (§6.3). Otherwise the evaluator falls back
    to depth-bounded evaluation over base data. Write policies never use
    it: they're validated at commit (§7.6), and a view's rows aren't in
    any partition's read set, so a write policy evaluates over base data.
- **Limits.** Guards outside the relational fragment (opaque functions)
  and wasm don't compile to relations. A policy that uses them can't
  serve `audience` queries. The checker says so
  when the policy is deployed, and the access-path matrix (§6.7) marks it.
- **Apps, actors, and viewers.** A request is `(app, actor, viewer)`, so
  the full relation is `can_op(app, actor, viewer, node)`:
  - **Viewer-only and actor-reading policies.** The checker classifies
    every policy as viewer-only or actor-reading, transitively through
    `shep::visible` and functions. A viewer-only policy is a relation over
    viewers, `can_op(viewer, node)`, and an agent acting for a user can do
    what the user can, through the user's membership edges, because the
    user is the viewer.
  - **Guards on `global actor` and `global shep::app`** add terms that
    don't go through the viewer, such as `deny delete using (global actor
    is agents::Agent)` (only in schemas that declare actors, §6.3) or
    `allow select using (global shep::app = <shep::App>'slack')`.
  - **Most policies never read `global shep::app`.** For those, `can_op` doesn't depend
    on the app, and results can be shared across apps (§13.2).
  - **Audiences are sets of viewers** (§11.3). For an actor-reading
    policy they are computed per actor class: the actor and app terms are
    evaluated for each actor type that reaches the schema.
  - **The app as itself.** `db.as_app()` is `actor = app` with no viewer.
    Its relation is `can_op(app, app, ⊥, node)`: each atom on
    `global viewer` is false, and the rest of each guard is evaluated as
    written (§6.1). The relation is compiled once for the no-viewer case.
    Subscribers and feeds that run as the app are keyed by the app, and
    permission changes reach them through the facts those guards read
    (§13.2). A request whose viewer is of another type is denied outright
    (§6.3); it never reaches this relation.

## 7. Client APIs and queries

Apps reach Shep through **declared operations**, which each app keeps in
its own operation set (§7.1). People reach Shep through ad-hoc sessions in
the CLI, as themselves (§7.2).

Operations, schemas, and policies are all written in ShepQL (§4.0). Two
client families call declared operations: the native Rust client, which
is the primary one (§7.3), and served JS modules with GraphQL for
browsers (§7.4). Every path compiles to the same query IR (§7.5) and
transaction IR (§8). Privacy, validation, and compatibility therefore
behave identically whichever path a request takes.

### 7.1 Operations belong to apps

**Schemas are shared; operations are each app's own.** A schema describes
data that many apps read and write. What one app does with that data is
that app's business. So an app declares its operations in its own
**operation set**, apart from every schema:

- **Queries.** ShepQL `select`s: a root (by key, exclusive field, or
  index), hops along links, bounded walks that repeat a hop (below),
  filters, ordering, and nested shapes, with typed parameters.
- **Live queries.** Queries marked `#! shep: live`. Clients subscribe to
  one, or poll the same changes as a feed (§13).
- **Mutations.** ShepQL `insert`, `update`, and `delete` statements with
  `assert`s (§8).
- **Materialization hints.** `#! shep: materialize` on a query asks Shep
  to maintain its result incrementally (§12).
- **Primitives, implicit for every node type the app uses.** Get by key
  (`Issue::get_by_id`) and by each unique field; create, update, delete, restore, link, and unlink;
  paged edge loads (`project.issues().first(50)`); and uploads for each
  file field (§16.3).

**An operation set is pinned and content-addressed.**

- **Pinned.** It names the schema versions it's written against: the app's
  own schema, if it has one, and every schema it uses. Every name in it
  resolves through those versions.
- **No version number.** Its identity is a hash of its contents, and
  changing it never changes a schema version (§4.1.4).
- **Principal shapes.** It names the actors and viewers the app runs
  as, in `shep.toml` (`[ops] actors = ["users::User", "agents::Agent"]`)
  or as `#[shep::ops(actors(users::User, agents::Agent),
  viewers(users::User))]`. By default both are the viewer types of the
  schemas it uses, usually just `users::User`. Handles are typed by these shapes (§7.3), and
  `shep ops check` warns when a shape can't satisfy a schema the set
  reaches: a viewer type the schema denies, or an actor its
  `global actor` declaration excludes (§6.3).
- **Checked when deployed.** `shep deploy` registers it (§4.1.3).
  Registration plans and prices every operation against current storage,
  through the adapters of the pinned versions (§4.4, §15). An operation
  that can't be planned within bounds, or (for a live query) doesn't
  incrementalize, is rejected by name.
- **Old sets keep working.** An operation set stays registered while
  clients built from it still connect, and it keeps the schema versions it
  names in service. An unused set goes dormant and wakes on connect
  (§4.6). A deploy never breaks a running binary.
- **Every app has one.** An app that owns no schema still declares
  operations over the schemas it's granted (§6.3).

**Shep builds the indexes operations need.** Schemas declare no indexes
(§4.1.1). Every index exists because some declared operation uses it.

- **What gets built.** Filters and orderings become index key ranges in
  the partitions that hold the type (§3), over a field's value, or over
  its adapted value when a filter goes through an adapter (§7.5).
  Searches become indexed fields in their type's search index (§16.6).
  `exclusive` constraints, which the schema declares, have their own
  indexes.
- **Filters on edges.** A filter on a single link, such as
  `.project = <Project>$project`, is an inverse lookup: the `i` keys when the
  two ends are always placed together, as an issue is with its project,
  and otherwise the inverse view (§3). Which one is decided at deploy,
  from placement. A composite constraint over a link and fields, such as
  `exclusive on ((.project, .name))`, is an index keyed by the link's
  target and the fields, in the group's partition.
- **Shared by identity.** An index is identified by its type, fields, and
  expression. In search, the shared unit is the indexed field: a field,
  an analyzer, and an adapted form (§16.6). Two apps whose operations need
  the same one share it.
- **Different analyzers mean different indexed fields.** One app
  searching `Message.text` in English and another with no stemming get
  two indexed fields. `shep ops check` points out when an operation could
  use an analyzer that's already built instead.
- **Each app pays the full cost.** Every app whose operations use an index
  is charged its full storage and write cost, so one app's usage never
  depends on another's (§15). A build is charged to the app whose deploy
  needs it. Rebuilds a new schema head triggers, such as expression
  indexes for adapters, and `exclusive` indexes are charged to the
  schema's owner.
- **Shown before deploy.** `shep ops check` lists the indexes the set needs,
  which already exist, and the cost of building the rest.
- **The set activates as a whole.** `shep deploy` builds the missing
  indexes and text extraction, shows progress, and activates the operation
  set once all of them are ready. Until then, a client that connects with
  the new set gets `not_ready` with the build's progress. `shep deploy`
  returns only after activation, so CI deploys before it rolls out
  binaries.
  - **Builds have a timeout,** configurable per universe. On timeout,
    `shep deploy` fails with the progress so far, and the old set stays
    active.
  - **Policies don't hold it up.** A new policy starts in shadow mode on
    its own schedule (§6.6 item 6). The operation set doesn't wait for its
    promotion.
  - **In dev and test universes,** connecting with a pending set waits
    until it activates, instead of failing with `not_ready`.
- **Unused indexes are dropped** once no active or pending operation set
  uses them. A pending set counts as a user, so an index it needs is never
  dropped while it builds (§4.4).

**Why apps declare everything:**

- **The checker sees every access path.** An operation that can't be
  served well is rejected at deploy, not discovered under load. A new
  schema head re-plans every in-service operation set that uses the
  schema (§4.4), so its owner learns exactly which apps' operations a
  change affects.
- **Cost is known before a request runs.** Admission control prices each
  request from its operation's cost (§15).
- **Policy tooling is exact.** The access-path matrix and policy tests
  (§6.7) enumerate the operations apps actually run, not every possible
  query.
- **Deprecation is exact.** `shep schema status` lists the operations that
  still use each deprecated field (§4.1.4).
- **Metrics and quotas have names.** An operation is identified by app and
  name, across operation sets (§15).
- **Browsers need it anyway.** Generated JS clients and GraphQL expose a
  fixed set of operations (§7.4).

**What `shep ops check` warns about.** Consequences that are easy to miss
in code are reported when an operation set is checked:

- **No viewer.** `global viewer` is AtMostOne in a set whose principal
  shapes include one without a viewer (`db.as_app()`), so writing it to a
  required link is a type error until the operation asserts it; using it
  at all in a set with no viewer shape is an error (§4.0 item 11).
- **Optional comparisons in filters.** `filter .assignee =
  <users::User>$user` with an optional argument matches nothing when the
  argument is absent; the lint suggests `?=`, `assert_exists`, or the
  optional-filter idiom below.
- **Two-phase commit.** An operation that writes two placement groups, or
  must validate reads in another group (§10.3), so it commits with
  two-phase commit (§8), with its cost. A global `exclusive` constraint
  (§3) counts too.
- **Hidden existence.** An operation that returns a content-addressed
  create's result to a user, which can't say whether the node already
  existed (§3).
- **Per-app live results.** A live query whose policies read
  `global shep::app`, so its
  topic can't be shared with other apps (§13.2).
- **Lag.** A read of a materialized query or a search that waits on `at_least`,
  with the typical wait (§12, §16.6).
- **Walks that will be cut off.** A walk whose depth and measured fan-out
  reach more nodes than its budget allows, so results will often come back
  `truncated` (below).
- **Consistency classes.** Each read point's plan (skipped, validated,
  validated in another group, or horizon) and the rule behind it, the
  participants it costs, disciplines that would remove them, and queries
  in the same set whose cross-group results look like a mutation's
  arguments, where the read-only anomaly can show up (§7.6).

**Writing operations: one ShepQL file each.** An app's operations are
`.edgeql` files in `shep/ops/`, one operation per file, named by the file
stem, as in Gel's `queries` convention:

```
# shep/ops/open_in_project.edgeql: open issues in a project, newest first
select Issue { title, state, created_at, assignee: { name } }
filter .project = assert_exists(<Project>$project) and .state = State.Open
order by .created_at desc
limit 100                                   # a literal top-level limit is max_first
```

```
# shep/ops/assigned_to.edgeql: clients can subscribe, or poll it as a feed (§13)
#! shep: live
select Issue { title, project: { name } }
filter .assignee = assert_exists(<users::User>$user)
```

- **Options are front-matter.** `#! shep: live`,
  `#! shep: materialize(per_viewer)` (§12), and `#! shep: stale_ok`
  (§7.6) go in a leading comment, so the file stays plain EdgeQL to
  editors and other tools.
- **Parameters are typed.** `<Project>$project` is a key cast: on the
  wire it's the bare key, and a composite key travels as a tuple
  (`<Commit>(repo := <str>$repo, sha := <shep::Sha1>$sha)`). A cast is
  AtMostOne, and `{}` for a key that's missing, hidden, deleted, or a
  placeholder; `assert_exists(<Project>$project)` turns that into
  `not_found` (§4.0 item 4). A list of keys is `<array<Message>>$quotes`.
- **The operation's shape is fixed.** A ShepQL query is a value-free
  description, so its shape can't depend on runtime data; a different
  shape is a second operation, or an `if` on the server (§8).
- **Paging.** `offset` is rejected. The top-level `limit` must be a
  literal, and it's the operation's `max_first`: the runtime supplies the
  page size (up to it) and the cursor. `order by` must end in the type's
  key, so pages are stable: `.id` for types keyed by `id`, and the
  `shep::key` fields, in order, for the others. The checker appends the
  missing part. Nested shape `limit`s must be literals
  too, which bounds fan-out.
- **Optional filters.** `filter .state ?= <optional State>$state ??
  .state` applies only when the argument is present. A search screen with
  several optional filters is still one operation, provided every
  combination keeps an indexed filter that is always present, and fan-out
  stays bounded.
- **Aggregates.** `group … by` and `count(…)` are accepted only in the
  shapes of §12 (grouped by anchor, or per viewer).
- **Deleted nodes.** `shep::with_deleted(Issue)` at the root returns
  tombstones (§6.5).
- **Operations call operations.** Every file is callable from another as
  `ops::<name>(param := …)`, with named parameters only, which is how
  `shep::observe` and `shep::unchanged` name the query they mean (§11.3,
  §11.7).

**In Rust: one `#[shep::ops]` module.** A Rust app has exactly one
operation module, written inline in its crate. Every file in `shep/ops/`
is part of the set; `query_file!` gives one a Rust function, and `query!`
adds an operation written inline:

```rust
#[shep::ops]                   // the operation set: shep/ops/*.edgeql plus the query! blocks below
pub mod ops {
    shep::query_file!(pub open_in_project = "shep/ops/open_in_project.edgeql");
    shep::query_file!(pub close_issue = "shep/ops/close_issue.edgeql");     // §8
    shep::query!(#[live] pub assigned_to = "
        select Issue { title, project: { name } }
        filter .assignee = assert_exists(<users::User>$user)");
}
```

- **One way to collect.** `#[shep::ops]` reads `shep/ops/` and receives
  the module's tokens, so it sees every operation in the set and nothing
  outside it. There's no `query!` in expression position, and no source
  scanning. A file with no `query_file!` is still in the set, for
  browser clients; it just has no Rust function.
- **One hash.** The macro lowers every operation to IR and hashes it into
  `ops::set()`, the operation set as a value with the schema versions it
  pins. `shep ops check` writes the same IR's hash to `shep/ops.lock`, so
  `shep ops check --frozen` in CI fails when the crate and the lockfile
  disagree. The client connects with `ops::set()` (§7.3).
- **Checked offline, at compile time.** The macro reads only committed
  files: the app's SDL (`shep/schema/*.esdl`), the dependency IR in
  `shep/deps/`, the operation files, and the policy files for the
  `global actor` declaration. It parses and type-checks each query
  against the pinned versions with the same checker as the CLI
  (`shep-ql`, §17), and never touches the network. It emits
  `include_bytes!` of each file, as sqlx does, so editing one rebuilds.
  This is why schemas are SDL: a proc macro can read committed text, but
  not sibling `#[derive]` structs (§4.1).
- **Generated types.** Each operation gets a module of types and a
  function:

  ```rust
  pub mod open_in_project {
      pub struct Params { pub project: issues::ProjectKey }
      pub struct Row {
          pub id: issues::IssueKey,
          pub title: String,
          pub state: issues::State,
          pub created_at: shep::Timestamp,
          pub assignee: Option<RowAssignee>,   // a link: Option through visibility
      }
      pub struct RowAssignee { pub id: users::UserKey, pub name: String }
  }
  pub fn open_in_project() -> shep::QueryBuilder<open_in_project::Params, open_in_project::Row>;
  ```

  - **Arguments are named, never positional.** A call passes the
    generated `Params` struct, `ops::open_in_project().params(Params {
    project })`, or the named builder,
    `ops::open_in_project().project(&project_id)`. A missing required
    argument is a compile error with the builder's typestate. Reordering
    parameters in the `.edgeql` file never silently swaps arguments.
  - **Cardinality maps to Rust.** One is `T`, at most one is `Option<T>`,
    and many is `Vec<T>`. Links are `Option` or `Vec`, except required
    links proven visible, which are `T` (§4.1.1), and a
    `shep::redactable` field is `Redactable<T>` (§6.6 item 7).
  - **Mutations get an error enum** from their `assert` messages plus
    Shep's standard errors: `close_issue::Error { NotFound,
    AlreadyClosed, Denied, Conflict, Invalid(..), … }` (§7.3). Shep's
    error names (`not_found`, `denied`, `conflict`, `invalid`,
    `stale_revision`, and the rest of §7.3's table) are reserved: an
    `assert` message can't use one, so variants never collide.
  - **Live queries** add `.subscribe()` and `.feed()` (§13). Walks
    return `Walk<Row>`, with `truncated` (below).
  - `shep::query_as!(MyRow, "…")`, inside the module, maps rows into an
    app struct with `#[derive(shep::Row)]`.
- **Errors point into the query.** A proc macro can't point inside a
  string literal on stable Rust, so the message embeds a rendered snippet
  with the file and line:

  ```
  error: shep: condition can be empty
    --> shep/ops/close_issue.edgeql:3:17
     |
   3 |   ok := assert(i.closed_at < <datetime>$t, message := 'too_late'),
     |                ^^^^^^^^^^^ `i.closed_at` is optional, so this is `{}`
     |                            when it's unset, and conditions must be One
     = help: write `(i.closed_at < <datetime>$t) ?? false`
  ```

  Planning and pricing happen at deploy, not in the macro.
- **GraphQL documents** in `shep/ops/*.graphql` declare queries and
  subscriptions too, written against the app's authoring schema, for
  browser front ends that prefer them (§7.4).

**Walks: bounded recursive traversal.** History and dependency questions
follow one edge over and over: the ancestors of a commit, a build target's
dependency closure, everything a task is blocked on. A **walk** repeats a
hop up to a declared depth. `shep::walk` reads like a function call; its
step argument is a path from the walked node, the way `filter` rebinds
`.`:

```
# shep/ops/ancestors.edgeql: a commit's ancestors, nearest first, up to 1,000 generations back
select shep::walk(<Commit>(repo := <str>$repo, sha := <shep::Sha1>$sha), .parents,
                  max_depth := 1000)
  { sha, message, depth := @depth }         # @depth is a path property
limit 200
```

```
# shep/ops/dependency_closure.edgeql: stopping at targets from other repositories
select shep::walk(<Target>$target, .deps, max_depth := 64, stop_at := .external ?? false)
  { name, kind }
filter .kind = Kind.Library                 # drops nodes from the result; the walk still passes through
limit 500
```

- **The depth bound is required** and must be a literal. A walk without
  `max_depth` doesn't type-check.
- **`stop_at` is a decision,** total like a guard (§4.0 item 1): it must
  be One, hence `?? false` on an optional field.
- **The step.** One link (`.parents` walks back), its backlink
  (`.<parents[is Commit]` walks forward to descendants), or a short fixed
  path of hops repeated as a unit.
- **Each node once.** A walk keeps a visited set, so a merge commit's
  shared ancestors, diamonds in a dependency graph, and cycles never
  repeat a node. A node is returned at the smallest depth at which the
  walk reaches it.
- **Stopping and filtering are different.** `stop_at := pred` returns a
  matching node but doesn't walk past it. A `filter` on the walk only
  drops nodes from the result; the walk still goes through them.
- **Order and pages.** Results come in breadth-first order: by depth, then
  by the edge's sort key, then by key. Every row can read its `@depth`. A
  cursor holds the depth and position, and the next page walks again from
  the root to that point, so deep walks are cheaper when the root is
  narrowed (start from a later commit) than when paged far.
- **Privacy at every hop.** Each node is checked when the walk reaches it,
  as on any hop (§7.5).
  - **A hidden or deleted node is neither returned nor walked through.**
    Nodes reachable only through it don't appear at all.
  - **Depth doesn't leak either.** Depths count only visible paths, so a
    shortcut through a hidden node can't make a visible node look closer.
  - **Edges with their own policies** are checked the same way.
- **Cost is known at deploy.** The planner prices a walk from its depth
  bound, the edge's measured fan-out, and its `limit`, and rejects one it
  can't bound (§15). At run time the request's budget caps the nodes the
  walk visits.
- **Cut-off results say so.** A walk that reaches its depth bound, or
  runs out of budget, returns what it found with `truncated: true` and
  the depth it completed. It never claims a closure is complete when it
  isn't.
- **Budgets count only visible nodes.** `truncated`, the depth reported,
  and the budget that decides them are all computed from nodes the actor
  can see. A budget that counted hidden nodes would reveal how much lies
  behind a hidden node, through where the walk stops
  (`budget_counting_hidden_leaks`; the fix is `visibleWork_congr`, in
  `formal/lean`).
- **Not live, at first.** A walk can't be `live` or `materialize` yet
  (open question 16). Apps re-run it, or follow the edge type's
  changes with a feed.

**Calling them.** Every call runs through an actor handle (§7.3):

```rust
let alice = db.as_user(alice_id);

let open = ops::open_in_project().project(&project_id).first(50).exec(&alice).await?;
let mut assigned = ops::assigned_to().user(&alice_id).subscribe(&alice).await?;   // §13
let closed = ops::close_issue().issue(&issue_id).comment("fixed")
    .exec(&alice).await?;                                                  // closed.comment, closed.revision
let issue = Issue::get_by_id(&issue_id).exec(&alice).await?;               // a primitive
```

**Undeclared operations are rejected,** in every universe.

- **No friction in development.** `shep dev` and the test harness register
  the operation set on every change (§7.3, §14.2), so a new operation can
  be called as soon as the code compiles, or the `.edgeql` file is saved.
- **No schema change in production.** A new operation is a `shep deploy`
  of the app. No schema version changes.
- **Ad-hoc queries are for people (§7.2), not for app code.** Only
  tokens for the built-in `shep-cli` app can run one.

**The operation set is bound once, at connect.**

- **At connect.** The client names its operation set by hash. The server
  checks that it's registered, registering it first in dev and test
  universes, and binds the connection to it in a **session**. The session
  implies every schema version the set pins, so requests carry none.
- **Per request, only the operation and its arguments.**
  - The operation is an index into the set.
  - Arguments are encoded in parameter order, by the operation's declared
    types. Result rows follow its declared shape.
  - No schema names, versions, type names, or field names travel with a
    request, and an ID is a bare key (§3). A request never carries a query
    or transaction body.

  ```
  session 7f3a…  op 12 (open_in_project)  args [key]  page {first: 50}
  ```
- **The other inputs:**
  - the actor
  - a page: a size up to the declared maximum, and a cursor
  - the actor handle's causal position, sent as `at_least` (§7.3)
  - for a mutation, an idempotency key, which the client library generates
    (§8)
- **Any front end can serve a session.** The session ID is derived from
  the operation set's hash, and the registry holds every registered set. A
  front end that hasn't seen a session loads it, so a connection that
  moves to another front end needs no new handshake.
- **Debugging.** A JSON encoding with names exists for tools and tests, and
  `shep` decodes captured requests.
- **Sessions are internal.** Client libraries create them, and app code
  never handles one.

### 7.2 Ad-hoc access for people

People can query Shep directly, as themselves, with every policy applied.

- **Tokens, not logins.** The CLI gets a token from the auth service,
  through whatever flow the auth service offers, and presents it. The
  token names the person's node, as for browsers (§6.3). Shep has no
  login flow.
- **The principals.** The person is both actor and viewer, and each schema
  checks them against its viewer type as usual. The app is `shep-cli`, a
  built-in app.
  - **Policies can tell ad-hoc access apart** with
    ``global shep::app = <shep::App>'shep-cli' ``.
  - **Per-app grants apply.** Schemas grant `shep-cli` read access by
    default, and an owner can revoke it.
- **Sessions bind schema versions.** An ad-hoc session has no operation
  set, so it binds a set of schema versions at connect instead. Every name
  in a query (types, fields, edges) and every ID resolves through that set.
  - **By default,** the newest production version of every schema the
    person can use. The CLI prints the versions it bound, and a saved
    query records them, so rerunning it later means the same thing.
  - **Explicit:** `--schema issues@4 --schema users@5`.
  - **Keys in queries are typed by their casts,** as in operations
    (`<Issue>'…'`). Only an untyped lookup (`shep get Issue:<key>`)
    names the type (§3), qualified as `issues::Issue:<key>` only when the
    name is ambiguous in the session.
- **Queries are ShepQL.** `shep query` runs one query, and `shep shell`
  is an interactive session, with `\schema` to change the bound versions.
  Ad-hoc queries are type-checked like operations, and hold to the same
  rules: a literal `limit`, no `offset`.

  ```
  $ shep query --universe staging \
      'select Issue { title, assignee: { name } } filter .state = State.Open limit 10'
  ```

  Each universe's host also serves GraphiQL, which gets the person's
  token from the auth service as any browser app does, over a
  generated GraphQL schema for ad-hoc sessions with a root field per node
  type (`issues(where:, orderBy:, first:)`) and a version picker
  (`https://staging.shep.internal/graphiql`). Only `shep-cli` sessions
  can use it.

  Commands run in the universe named by the token's `aud`; `--universe`
  asks the auth service for a token for another universe, for one
  command.
- **Read-only by default.** Ad-hoc mutations are off unless a schema
  enables them for `shep-cli`. They then run as the person, under write
  policies, and the log records the person and `shep-cli` (§10.1).
- **Planned and priced before running.** An ad-hoc query is arbitrary, so
  the planner checks it at submission: no unindexed scans of large
  partitions, bounded hops and fan-out, and a row limit. A query over
  budget is rejected with the plan's estimate, before it touches storage.
- **Walks, with lower bounds.** `shep::walk` works ad hoc, and in GraphQL
  every edge field of the ad-hoc schema, and of an app's authoring schema
  (§7.4), takes a `walk` argument:
  `{ commit(repo: "shep", sha: "…") { parents(walk: {maxDepth: 50}, first: 100) { nodes { sha depth } truncated } } }`.
  Ad-hoc walks get a smaller depth and node limit than declared ones, set
  per universe, since nothing prices them in advance.
- **Lowest priority.** Ad-hoc work runs in its own admission class, below
  app traffic, so it is shed first under load (§15).
- **No ad-hoc live queries, at first.** Subscriptions need declared live
  queries (§13). `shep query --watch` re-runs a query and prints what
  changed.

### 7.3 Rust client

```rust
let db = shep::Db::builder()
    .ops(tracker::ops::set())            // the app's operation set, bound at connect
    .token_provider(auth::provider())    // gets tokens from the auth service; they name the universe (§6.3)
    .connect()
    .await?;

// Every operation runs through an actor handle (§6.3).
let alice = db.as_user(alice_id);

let project = Project::get_by_id(&project_id).exec(&alice).await?;    // a primitive
let open = ops::open_in_project().project(&project.id).first(50).exec(&alice).await?;

for issue in &open {
    match &issue.assignee {
        Some(user) => println!("{} → {}", issue.title, user.name),
        None => println!("{} (unassigned)", issue.title),
    }
}

let issue = Issue::get_by_id(&issue_id).exec(&alice).await?;
shep::update!(issue { state: State::Closed, closed_at: shep::now() })
    .exec(&alice)
    .await?;                             // a patch: only these fields change
```

The API is Toasty's: `get_by_*`, `create!`/`update!`, and `.exec()` to
run a request. That's the only place Shep follows Toasty: schemas are SDL
(§4.1), and queries and mutations are ShepQL operations (§7.1), whose
generated functions take the same `.exec(&handle)`. The differences come
from running against a shared service:

- **Connecting.** `Db::builder().token_provider(p)` takes a token
  provider: a callback the app implements against the auth service, which
  returns a token for given claims and refreshes it before it expires.
  The universe, and so the host, comes from the tokens; `SHEP_URL`
  overrides it (§6.3).
- **Actor handles.** Every request takes one, so no call runs without an
  actor. They're cheap. A handle is a way to present a token: Shep reads
  the actor and viewer from its claims, and trusts nothing else.
  - **`db.with_token(&token)`:** a token the app already holds, such as
    the signed-in user's, forwarded from a browser request. This is the
    usual way a backend acts for a user.
  - **`db.as_user(id)`, `db.as_agent(id)`, and `.by(actor)`:** ask the
    token provider for a token whose `sub` is that node and whose `act`
    is the `.by()` actor, or the same node. `db.as_user(u1).by(g1)` is
    agent `g1` working for user `u1`. Whether the auth service issues
    such a token is its business (§6.3). The constructors are typed by
    the operation set's principal shapes (§7.1).
  - **`db.as_app()`:** a token with the `as_app` scope and no viewer.
  - One `Db` is one connection, and serves every actor of a backend.
- **Read-your-writes per actor.** Each handle tracks the causal position
  of everything it has read or written, and sends it as `at_least`
  (§9.4).
  - Alice's reads wait for Alice's writes, never for Bob's.
  - Handles for the same actor from the same `Db` share their position.
    An agent working for a user and the user are different actors, and
    don't.
  - **Writes and reads are tracked apart.** Ordinary reads, served by
    materializers, wait for both. Reads of a materialized query or a
    search wait only for the handle's own writes, so an ordinary read
    never makes the next search wait for an index to catch up (§12,
    §16.6).
  - **A subscription's first result** follows the same rule: at least as
    new as everything the handle read or wrote for an ordinary live query,
    and as its own writes for one over a materialized query.
  - **At the wait bound,** such a read fails with `lagging` by default.
    The bound is set per universe, a few seconds by default. An operation
    declared `stale_ok` (`#! shep: stale_ok`, or `#[stale_ok]` on its
    `query!`) returns the view's current result instead, with its version. A
    subscription to it starts from the view's current version and catches
    up through deltas.
- **Bookmarks cross process boundaries.** `alice.bookmark()` serializes the
  handle's position. A backend that writes and then hands off to another
  service passes it along, and `db.as_user(id).after(&bookmark)` resumes
  from it. It's the only version token app code handles. App-owned fan-out
  uses `shep::fanout` (§13.6).
- **Updates are patches.** `update!` sets the named fields, or applies one
  of a closed set of field operations evaluated on the server:
  `shep::now()`, and Toasty's `increment()`, `decrement()`, `add(v)`,
  `subtract(v)`, `push(x)`, and `extend(xs)`, plus `max(v)`. The leader
  resolves the field operations, so they never conflict with each other
  (§8). In ShepQL the same operations are `x := .x + e`, `tags += v`, and
  the rest (§8). Fields it doesn't name are
  left alone, so two updates to different fields never conflict.
- **Compare-and-swap is per field.** A loaded node remembers the version
  it was read at, internally. `.if_unchanged()` makes an update fail with
  `stale_revision` if any field it writes changed since then, using each
  field's dot (§3). `.if_unchanged(&[Issue::TITLE, Issue::STATE])` names
  the fields to check instead, including ones it doesn't write. Two people
  editing different fields of one issue then never conflict.
  - **Whole-node CAS** is the same call over every field:
    `.if_unchanged(Issue::ALL)`.
  - **`.if_revision(r)`** does that with an explicit `revision`, for
    clients that kept a revision rather than the loaded node, such as
    GraphQL's `updateIssue(id, patch, ifRevision)`. A revision is opaque:
    the newest stamp among the node's keys (§3).
- **Visibility shows up in the types.**
  - A single link reads as `Option<T>`. `None` means absent, hidden, or
    deleted, and the actor can't tell which (§6.5). That includes
    `required` links: `required` describes storage, and visibility is
    applied on top (§4.0).
  - A `multi` link reads as a list filtered by visibility.
  - An optional field that a field-level policy hides reads as `None`.
    Only a required field under a field-level policy is `Redactable<T>`
    (§6.6 item 7).
- **No interactive transactions.** Toasty's `db.transaction()` holds a
  transaction open across round trips, which Shep can't do. Conditional
  multi-step writes are declared mutations (§8). `shep::batch(...)` is
  shorthand for a mutation made only of primitives: the macro registers
  its shape in the operation set, like any mutation (§8).
- **Following changes: one stream, pushed or polled.** A live query
  is a stream of updates. A subscription pushes them; a feed hands them
  over when polled. They are the same stream, with the same updates:

  ```rust
  // Push: Shep sends each change as it commits (§13).
  let mut sub = ops::assigned_to().user(&alice_id).subscribe(&alice).await?;
  sub.more().await?;            // load the next page, growing the live range (§13.1)
  while let Some(update) = sub.next().await {
      match update? {
          shep::Update::Delta(delta) => apply(&mut issues, delta),   // added / removed / updated
          shep::Update::Resync(page) => issues = page,
      }
  }

  // Poll, on the app's own schedule: each poll returns what changed since the last.
  let mut feed = ops::assigned_to().user(&alice_id).feed(&alice).await?;
  let update = feed.poll().await?;
  ```

  - **No versions to handle.** A subscription or feed keeps the version
    of the result it holds, and resumes from it by itself (§13.1). Both
    hold a live range: the pages the client has loaded.
  - **Only live queries change.** Other queries have no `subscribe` or
    `feed`, so whether a query incrementalizes is decided once, by
    `#! shep: live`, and checked at deploy.
  - **App-owned fan-out is a separate module.** Apps that run their own
    real-time service use `shep::fanout`: result versions, `since`, and
    `observe` clauses on mutations (§11.3, §13.6). Most apps never need
    it.
- **Errors are business errors.** An operation's error type lists only what
  the app can act on:

  | Error | When | What the app does |
  |---|---|---|
  | an `assert` message | A mutation's `assert` failed (§8) | Whatever the business rule says |
  | `stale_revision` | An update with `.if_unchanged()` or `.if_revision(r)` found what it checks changed | Re-read and retry, or tell the user |
  | `conflict` | An `exclusive` value or app key is taken (§3) | Pick another value |
  | `deleted` | The node is deleted, and the actor may know (§3, §6.5) | Offer `restore`, or stop |
  | `denied` | A policy said no, or the actor can't act (§6.3) | Stop |
  | `invalid` | A check failed (§4.5), or, with the reason `cutover`, a write that a schema cutover in progress rejects: a patch only one of its two heads accepts, or one that breaks the new head's set-level constraints (§4.2) | Fix the input; for `cutover`, write a value valid for both, or wait for the cutover |
  | `lease_lost` | A write under a lease whose generation is no longer current: the lease expired and the node was claimed again (§8) | Stop working on the task; another worker has it |
  | `too_large` | A value would exceed its field's size cap (§16.1) | Shrink it, raise the field's `shep::max_size`, or move the collection to edges |
  | `not_representable` | An old version wrote a value it can't see whole (§4.3) | Upgrade, or write through the newer version |
  | `cursor_expired` | A search page came after its searcher closed (§16.6) | Start the search again |
  | `budget_exceeded` | The request ran over its budget (§15.3) | Narrow it |
  | `lagging` | A view or search didn't catch up within the bound | Retry, or declare the operation `stale_ok` |
  | `not_ready` | The operation set's indexes are still building (§7.1), the operation set is waking from dormancy (§4.6), the version read is still in its cutover (§4.2), or a copied file is still `Uploading` (§16.3) | Wait, then retry |
  | file errors | An upload's size, hash, or media type didn't match, or it expired (§16.3) | Upload again |

  The client library retries admission errors (§15.3) within its retry
  budget, so they reach the app only when the budget runs out, and
  handles `reauth` on subscriptions (§13.4). Storage-level retries, such
  as `contended` and `too_old` (§10.3), happen inside Shep and never reach
  the client; if they run past a request's deadline, the client gets
  `overloaded` with `retry_after`.
- **Native wire protocol, not GraphQL.** Operations and live-query streams
  travel over one multiplexed HTTP/2 connection, bound to a session (§7.1).
  When both ends are generated Rust, GraphQL's text format, selection sets,
  and introspection add cost and no benefit.
- **Dev loop.**
  - `shep dev` creates a dev universe on the non-production fleet, at
    `<name>.shep.internal`, keeps it alive, and redeploys on save (§14.2).
    Nothing runs locally.
  - In dev and test universes, connecting with an unregistered schema or
    operation set registers it, provided its checks pass. This is the
    equivalent of Toasty's `push_schema`.
  - Each test gets its own universe (§14.2).
  - Production deployment happens only through `shep deploy` in CI
    (§4.1.3).

### 7.4 GraphQL for browsers and TypeScript

- **One GraphQL schema per operation set.** It is generated from the
  operation set and the schema versions it pins, with `async-graphql`'s
  `dynamic` module, and served at `/g/<app>/graphql` on each universe's
  host (§14.3). A connection binds
  the operation set the same way as the Rust client (§7.1).
  - **The app's operations are the root fields.** Each query and get
    primitive is a `Query` field, each mutation and write primitive a
    `Mutation` field, and each live query a `Subscription` field.
  - **Documents are operations.** The generated JS client's documents come
    from the operation set itself, so there's nothing else to register.
  - **Teams that prefer Apollo, urql, or graphql-codegen** write documents
    in `shep/ops/*.graphql`, against the app's **authoring schema**: the
    pinned schema versions with a root field per node type, the same shape
    ad-hoc sessions use (§7.2), served at `/g/<app>/authoring.graphql`.
    `shep deploy` plans each document like any other operation and adds it
    to the operation set. A browser then sends a document by hash, with
    variables, against `/g/<app>/graphql`. Arbitrary documents are
    accepted only in ad-hoc sessions.
  - The served GraphQL schema is also available as a file
    (`/g/<app>/schema.graphql`).
- **TypeScript types come from the same files, offline.** `shep gen ts`
  reads the app's `.edgeql` files and the committed IR, with no server,
  and emits `client.d.ts`: `Params` and `Result` per operation, with
  cardinality mapped as `T`, `T | null`, and `T[]`, links nullable, and
  `…Redacted` flags. The served module below comes from the same
  generator run over the registered set, so the two can't diverge.
- **The service serves generated JS clients,** one per app, from its
  operation set:

  ```
  /js/<app>/client.js              the app's current operation set (ES module)
  /js/<app>/client.d.ts            its TypeScript types
  /js/<app>/<set>/g<gen>/client.js pinned to one operation set and generator
  /js/runtime/<ver>/shep.js        shared runtime the clients import
  ```

  ```html
  <script type="importmap">
    { "imports": { "shep:client": "https://shep.internal/js/tracker/client.js" } }
  </script>
  ```

  ```js
  import { connect } from "shep:client";        // the import map picks the universe

  const db = connect({ token: await getUserToken() });       // the actor is the token's user
  const project = await db.Project.getById(projectId);        // primitive
  const open = await db.openInProject({ project: projectId }).first(50);   // named arguments, as in Rust
  const sub = db.assignedTo({ user: me }).subscribe((update) => render(update));   // §13
  await db.closeIssue({ issue: id, comment: "fixed" });       // mutation
  ```

  - **What's in a module.**
    - A typed function for every operation in the app's set, and the
      primitives under each node type (`db.Project.getById`).
    - Every schema the app uses is in the same module, at the version the
      operation set pins, so there are no separate dependency imports.
    - The shape follows the Rust client (§7.3), adapted to JS idioms. The
      runtime tracks the user's causal position, as an actor handle does.
    - Baked in: the operation set's hash, the host it was served from,
      and the app's identity. A module from `staging.shep.internal` only
      ever talks to staging.
  - **Why scope by app.** App identity is part of every request (§6.3), and
    each app has its own operations and grants.
  - **Generated on demand, then cached.** A registered operation set never
    changes, but the generator and runtime do.
    - The plain URL serves the app's current operation set, with the
      current generator's output and an ETag.
    - A pinned form, `/js/<app>/<set>/g<gen>/client.js`, is
      `Cache-Control: immutable`, and the CLI prints its SRI hash for
      `<script integrity>`.
    - Teams that want reproducible builds pin; everyone else gets
      generator fixes automatically.
  - **One name, one value per environment.** App code always imports
    `shep:client`. The import map, which the app's HTML or build fills
    per environment, is the one place that names the universe's host.
    A vendored module from `shep gen ts` takes the host the same way,
    from the import map, or from `connect({ url })` when the app passes
    one; it has no host of its own.
  - **One TypeScript path, two ways to get it.** The served module and
    `shep gen ts` are the same generator over the same operations, so
    they're interchangeable:
    - import the served module through the import map, with no build step
    - `shep gen ts` to vendor `client.js` and `client.d.ts` for bundlers
      and `tsc`, which don't resolve types from URLs; it reads only
      committed files, so it works offline and in CI
    - or skip the module and use `schema.graphql` with a GraphQL client,
      with documents in `shep/ops/`
    - skip the module and use `schema.graphql` with a GraphQL client, with
      documents in `shep/ops/`
  - **Origin rules.** Modules are served with CORS for the origins the
    app registered for that universe. They contain no secrets: the user's
    token is supplied at runtime. Serving the code adds little trust beyond what
    the app already places in the service, which holds its data.
  - **In development,** the dev universe that `shep dev` keeps serves the
    same routes on its own host and regenerates on every change, so the
    browser app hot-reloads against the operations and schema being
    edited.
- **Same visibility rules as Rust.** Unique edges are nullable, lists are
  filtered connections, and hidden optional fields are `null`. A required
  field declared `Redactable` is nullable, with a generated
  `<field>Redacted` flag.
- **Primitives follow one naming rule.** Rust and JS group them under the
  type (`Issue::get_by_id`, `db.Issue.getById`, `db.Issue.update`).
  GraphQL root fields are flat, so they're named verb then type:
  `issue`, `createIssue`, `updateIssue(id, patch, ifRevision)`,
  `linkIssueLabels`/`unlinkIssueLabels`, `deleteIssue`, and
  `restoreIssue`. `ifRevision` is optional, as in Rust (§7.3).
- **Uploads.** Each file field has an upload primitive:
  `beginIssueSpecUpload(name, mediaType, size, sha256, from)` in GraphQL,
  where `from` names a file to copy, and `db.Issue.spec.upload(file)` in
  the generated client, which also sends the bytes, or copies when given a
  Shep file (§16.3). Files are a `File` type with the fields in §16.2.
- **Declared mutations are how browsers run server-side logic.** A
  mutation in the app's operation set becomes a typed function in the
  generated client and a GraphQL mutation with the app's own name,
  `closeIssue(issue: ID!, comment: String!)`. It runs as the caller's
  actor, so policies apply to every step.
- **Live queries are GraphQL subscriptions.** They travel over a WebSocket
  using the `graphql-transport-ws` protocol (§13). The generated client
  also offers `feed()` to poll the same stream.
- **Authentication.** Browsers send the user's token from the auth
  service directly, and Shep verifies it (§6.3).

### 7.5 Query execution

- **Query IR.** Typed ShepQL lowers to it (§4.0). A query starts from
  roots (by key, by an `exclusive` field, or by an index Shep built for
  it, §7.1), follows hops along edges, and applies filters, ordering, and
  connection pagination at each hop. A walk is a hop with a repeat
  count, a visited set, and an optional stop condition (§7.1). Cursors encode `(sort_key, id)` from
  the underlying storage order, so rows hidden by privacy or deletion don't
  break pagination.
- **Execution.** Start hop-by-hop with batched loads per hop (dataloader
  style), applying privacy at each hop. A walk runs one batched load per
  depth level, over that level's visible nodes. Only where the walked
  type's read policy pushes down completely can a materializer run several
  levels itself, with the depth bound and visited set, and return them in
  one response; otherwise a node that later fails a remaining guard would
  already have counted toward depth and budget, which would leak it
  (§15.2).
- **Walks across partitions.** Nodes placed with each other (§9.6), such
  as a repository's commits, keep a walk in one partition. Otherwise each
  level's batch splits by partition, and the walk's version spans
  partitions the same way any multi-partition read does (§9.7). This is
  simple, composes with privacy, and matches how TAO-backed Ent works,
  though TAO's performance comes from a deliberately narrow interface and
  doesn't validate Shep's broader queries. Add
  pushdown (hops plus compiled policy predicates run inside the
  materializer) for the patterns that need it.
- **One cut for materialized queries.** A plan that joins a materialized
  query with base data reads both at one version `C`, no newer than the
  view's frontier. `C` only has to cover the handle's own writes, as any
  view read does (§7.3), so these plans wait no longer than a plain view
  read. An inverse-edge view (§3) is read at its own version, as before,
  and the nodes it points to are read at `S` and checked against the
  forward edge, so a row whose edge was removed since doesn't appear.
  Paging is unchanged (§7.6).
- **Limits.** Declared operations are planned and priced when an
  operation set is deployed, and ad-hoc queries at submission (§7.1,
  §7.2). Execution enforces each request's budget for rows, hops, and
  time (§15).
- **Older versions query through their adapters.** A `v1` filter or sort
  on an adapted field runs against the adapted value
  (`truncate(title, 200) == "x"`, §4.4), so a plain index on `title`
  doesn't help. Registration builds indexes over the adapted values that
  in-service operations need (§4.4).

### 7.6 Consistency classes

Shep's planner decides, for every operation and every read point inside it,
how much consistency that read needs. It classifies each read once, at
deploy, from the operation's IR, the policies and checks it inlines, and the
schemas' declared disciplines (§4.1.2). It then builds a plan that validates
only what needs it. App code doesn't change. The classes are internal:
`shep ops check` reports each read as **skipped** (with the reason),
**validated**, **validated in group X** (a read-only two-phase commit
participant, §8), or **horizon** (§6.3).

**The theory behind it.** Three separate results, which are easy to mix up:

- **CALM** (Hellerstein; Ameloot, Neven, Van den Bussche, PODS 2011 / JACM
  2013) says which *outputs* are safe on stale data: monotone ones. It
  says nothing about invariants or how recent an answer is ("Keeping
  CALM", Hellerstein and Alvaro, CACM 2020).
- **Stability** (Chandy and Lamport; CISE, Gotsman et al., POPL 2016;
  Hamsaz, POPL 2019) says which *decisions* are safe on stale data: a
  condition that, once true, stays true at every later state.
- **Invariant confluence** (Bailis et al., VLDB 2015) says which
  *invariants* survive decisions made on stale data.

Monotone in the log isn't monotone in what's visible. Dots, stamps, and
ghosts only grow, but field overwrites, unlinks, soft deletes, and policy
deploys all retract what a reader sees. So the planner judges every read on
the visible state, through the reading version's lens, using declared
disciplines, never the log.

**What the planner computes at deploy.**

1. **Fact classes.** Every read scans keys of known kinds (§3): a field of a
   type, a type's lifecycle, an edge type, an edge set of a source, an
   index, or a registry head.
2. **Unchanging facts.** A fact is *unchanging* if it can't change once its
   node exists and is filled: keys, hashed fields, placement links, and `readonly` fields and links (§4.1.2). The
   lifecycle of a `shep::Permanent` type only grows: absent, then present,
   never deleted.
   Everything else is mutable.
3. **Implicit reads.** Every field read also reads its node's lifecycle, and
   every hop also reads `visible(target)`: the target's lifecycle and its
   read decision.
4. **Polarity of every occurrence**, through inlined policies and
   functions (§6.1), checks (§4.5), and the version's lens (§4.3). Order
   doesn't matter under union minus deny, so neither does a guard's place
   in the policy:
   - **Positive:** a hop, `exists`, the right side of `in`, `count(…) >=
     k`, `shep::visible` inside an allow.
   - **Negative:** `not`, `not exists`, `not in`, `count(…) <= k`, every
     deny guard, `shep::contained_in`, a `hide` predicate, and `not exists
     .deleted_at` inside `visible()`.
   - **`shep::visible` is classified by its effective polarity,**
     tracked through `not`. Inside a deny it's negative, so its target's
     lifecycle is a negative input, and lifecycles grow through create
     and fill: the decision is RevocationSensitive
     (`denyDelegate_overgrants`). `not shep::visible(…)` inside a deny is
     positive, and Stable⁻ when the `visible` is Stable⁺
     (`port_visible_safe` in `Polarity.lean`), so an Ent-ported policy,
     whose rewritten denies negate earlier allows, keeps its stability
     (`port_safe`).
   - **`??` defaults read a lifecycle.** `(x.f = 1) ?? d` reads the
     lifecycle of the node on the left, `x`, with the default's
     polarity: `?? true` makes "`x` doesn't exist" admit. So in a
     Stable⁺ position the default must be `false`, and in a Stable⁻
     position `true`; otherwise the lifecycle read counts as negative
     and the guard isn't Stable. A policy hop to a placeholder that is
     later filled reads its target's lifecycle the same way
     (`trueDefault_unsound` in `Planner.lean`).
   - **Both:** `=`, `?=`, `count(…) = k`, `if` conditions, exact values
     returned or written, ordering with a limit.
5. **The consumers of every read,** followed through data flow. A read's
   class is the strictest any consumer it reaches needs. That is the taint
   rule (MixT, ConSysT): a read that skipped validation can't flow into
   anything that needs it.
6. **The concurrent writers of every fact:** every active operation set of
   every app, every primitive, and system writers such as the lease
   sweeper, backfill, and (later) purge.

From these it derives three predicates:

- **Stable(p):** every positive input is unchanging or a
  `shep::Permanent` lifecycle, and every negative input is unchanging. If
  `p` is true at a snapshot, it's true at every later state, whoever
  writes. Its dual, **Stable⁻(p)**, holds when a false `p` stays false;
  it's what a deny guard needs. A negative unchanging input is sound only
  because a field read implies its node's lifecycle (the implicit
  lifecycle read). `not exists` is never Stable, because no lifecycle is unchanging
  (`stab_upClosed`, `gstab_sound`).
- **Unchanging(p):** every input is unchanging, so `p` never changes.
- **RevocationSensitive(decision):** judged over the whole read decision:
  `visible(T)`, the field policies, `contained_in`, and the version's lens,
  not just the policy's guards. A decision is sensitive unless `T` is
  `shep::Permanent`, every allow guard is Stable, and every deny guard,
  `contained_in`, and `hide` predicate is Stable⁻. (Asking only that the
  allow that admits each row be Stable would be finer, but needs a
  per-row argument; that refinement is open question 25.) A
  `contained_in` link counts only if it's `readonly` and its container
  type is `shep::Permanent`; otherwise the decision is sensitive. If it
  is sensitive, staleness could over-grant. Guards are total (§6.1), so
  there's no error case to classify. Proven in `Polarity.lean`
  (`vis_mono`, `history_admit_mono`), with counterexamples for a deny
  over mutable facts, a non-permanent type, and `shep::visible` inside a
  deny, restated for union minus deny (§18).

The classifier is syntactic, sound, and incomplete: when it can't prove a
read safe, it validates.

**The decision per read point.**

| Read point | Plan | Why |
|---|---|---|
| A query's result | `at_least` own writes by default, any snapshot with `stale_ok`; plus the data-partition horizon when its read decision is RevocationSensitive | Read-your-writes per actor is a product choice, and it's cheap |
| A read decision that isn't RevocationSensitive | No data-partition horizon; the principal part still applies (§6.3) | Staleness can only under-grant |
| A RevocationSensitive read decision | Horizon, on the partitions holding its sensitive facts | The new-enemy problem (Zanzibar, ATC 2019) |
| `assert(p)`, Stable(p) | Skipped, even in another group. A false `p` re-executes the mutation once at a fresher `S′` | True at `S` means true at commit, so the mutation is equivalent to one that read `p` at commit (CISE stability; Hamsaz) |
| `assert(p)`, otherwise | Validated, in group X if it's elsewhere | |
| `if p`, Unchanging(p) | Skipped | Both branches are final |
| `if p`, otherwise | Validated | A false branch taken at `S` could be wrong at commit |
| A value written, or a write's target | Skipped if every input is unchanging; otherwise validated | Lost updates and write skew (Fekete et al.) |
| A field operation (`x := .x + e`, `tags += v`, `max({.x, e})`, …) | Leader-resolved; no read of the target (§8) | Commutative updates need no coordination (RedBlue; FoundationDB atomic ops) |
| `shep::claim` | Leader-resolved (§8) | A reservation |
| A write policy | Validated, in group X if it reads elsewhere; skipped only if every allow is Stable and every deny is Stable⁻ | A stale write is permanent |
| A policy condition that encodes an invariant (`count(.members) < .limit`) | Validated, as a guard | It's an invariant, not authorization |
| A check on the written node | Validated | Its keys are in the write set already |
| A target-lifecycle check on `link` or `create` (the target exists, isn't deleted) | Skipped, as a best-effort error at `S`; a link that would create a placeholder is validated | Under soft delete, a link to a deleted node is a state link-then-delete reaches anyway (Bailis, the cascading-delete case) |
| A check that reads other, mutable nodes | Validated | Not invariant-confluent in general |
| A local `exclusive` | Validated | The range is in the written partition |
| A global `exclusive` on a value the client chose | Validated in the unique keyspace's partition, which the mutation writes (§3) | Uniqueness of chosen values isn't invariant-confluent (Bailis) |
| Generated `id` keys | Nothing | Generated unique IDs are invariant-confluent |
| A freshness check (`shep::unchanged`) | Validated; unchanging facts of nodes present and filled at `S` are dropped (§11.7) | "No new stamp" is negation over the future |
| `shep::observe` | Validated | Exactness at the commit point is its purpose |
| A live query | Phase 1 exact between frontiers; phase 2 needs the data-partition horizon only for RevocationSensitive decisions (§13.2) | |
| A materialized query | Exact at its frontier, with own writes, or any snapshot with `stale_ok` (§12) | |
| Search | Stale; never feeds a decision (§16.6) | Ranked over an index version |
| The actor's and viewer's lifecycles, and the keys | The principal part of the horizon, always (§6.3) | The horizon is a fresh time |

**How classification changes the plan.**

- **Pruned read sets.** A partition validates only reads classified
  validated. `shep ops check` reports the reads and read-only participants
  it saved.
- **Leader-resolved operations.** Field operations and `shep::claim` are resolved
  by the leader into ordinary facts with old values (§8, §10.3), so the
  log, replay, and every log consumer are unchanged.
- **Source routing.** A read whose decision isn't RevocationSensitive can
  use any materializer, even one more than the bound behind. A
  RevocationSensitive read needs the horizon on the partitions holding its
  sensitive facts.
- **A stale false is retried, not reported.** A Stable `assert` that is
  false at `S` may only be stale. The front end re-executes the whole
  mutation once, at an `S′` that contains each relevant leader's commit
  index (a read-index call), and `S′` joins the mutation's context. Only a
  false at `S′` returns the `assert`'s error.
- **One cut for materialized queries.** A plan that joins a materialized
  query (§12) with base data reads both at one version, which covers the
  handle's own writes as any view read does. Inverse-edge views (§3) and
  pages are read as before.

**`shep ops check` shows every read point.**

```
accept_build.edgeql
  r1  t := assert_exists(<Target>$target)                     line 2
        Target is shep::Permanent; its select isn't revocation-sensitive  skipped (stable)
  r2  assert(shep::unchanged(ops::inputs(target := t), since := …))       line 3
        Bookmark.head (group repo:42)                                   validated in group repo:42
        Tree.children (readonly link, filled at S)                      skipped (unchanging)
        Blob fields (readonly)                                          skipped (unchanging)
  w1  update t set { builds := .builds + 1 }                            leader-resolved
  policy BuildResult insert: allow using (global viewer in .target.owners)
        owners: mutable link, group repo:42                             validated in group repo:42
```

It also suggests disciplines that would help: "making `Commit` extend
`shep::Permanent` removes 2 read-only participants from 4 operations".

**Other tools.**

- **`shep schema disciplines`** shows each fact class, its declared
  discipline, and every writer (operation, app, primitive, system) with the
  kind of change it makes. It lists disciplines that every current writer
  already respects but nobody declared, as suggestions.
- **The polarity report** extends `shep policy matrix` (§6.7): for each
  policy, its inputs, their polarity and stability, whether reads under it
  need the data-partition horizon, and warnings for denies over mutable
  facts, `shep::visible` inside a deny, and `contained_in` on mutable
  parents.
- **The read-only anomaly heuristic.** Plain reads across partitions are
  causal, not serializable, so the read-only anomaly (Fekete, O'Neil, and
  O'Neil, 2004) is reachable. When the same operation set has a query whose
  cross-group results look like the arguments of one of its mutations,
  `shep ops check` notes it and suggests `shep::unchanged` or
  `.if_unchanged()`. App code itself isn't visible to Shep, so this is a
  heuristic.

**Changing what plans rely on.** A plan relies on disciplines, policies, and
schema heads, so changing any of them can change its classes. A discipline
weakening, a schema head, or a policy deploy that changes any in-service read
point's class rolls out in two steps (§4.1.2):

1. The affected operation sets are re-planned and activated, and a barrier
   goes to every partition where an affected operation writes or is the
   home. Proposals carrying an older registry dot are rejected (§10.3).
2. Only after every one of those partitions has the barrier is the change
   enforced: for example, the barrier that lets deletes of a formerly
   `shep::Permanent` type through.

A plan may rely on a discipline for facts in partition `Q` only if its `S`
contains `Q`'s enforcement barrier. A policy deploy re-plans and re-prices
the affected operation sets, and its report lists the cost changes for each
app.

**Testing.** Classification-differential tests run each operation set twice
under randomized concurrency, once with the planner's skipping and once with
"validate everything". Both must commit histories that are
outcome-serializable: there is a serial order, consistent with each
partition's log, in which every committed mutation's writes and `assert`
outcomes equal its outcomes at that position. Every invariant must hold in
both.

**Limits.**

- **Undecidable in general.** Monotonicity, coordination-freeness, and
  invariant confluence are undecidable for arbitrary programs, so the
  classifier is conservative. Wasm and opaque functions always validate,
  or need the horizon in a read decision.
- **Stability is global.** A discipline must hold for every writer: other
  apps, primitives, sweepers, backfills, and future deploys. That's why
  disciplines are declared, checked in every front end under the
  registry barrier (§4.1.2), and never inferred.
- **Caching across cuts is unsafe.** Composing sub-decisions or audiences
  cached at different cuts is unsafe even for monotone policies (Lee and
  Winslett's incremental consistency). Every request and topic step
  evaluates at one cut.
- **Z-set deltas are invertible,** so nothing in delta space is ever final.
  Finality comes from declared disciplines and the tombstone storage model.
- **Most real policies don't benefit on reads.** A policy with a deny
  or `contained_in` over mutable facts is RevocationSensitive, so reads
  under it still need the horizon. The availability win is for permanent
  types with frozen ownership.

## 8. Mutations

Clients can't hold a transaction open, so each transaction is a **single
declared mutation** that runs atomically on the server. Every transaction is a
declared mutation in an app's operation set (§7.1), written in ShepQL.
This is `close_issue` from §7.1 in full:

```
# shep/ops/close_issue.edgeql
with
  i  := assert_exists(<Issue>$issue),                     # not_found if missing or hidden
  p  := assert_exists(i.project),                         # a required link still reads as optional
  ok := assert(i.state = State.Open, message := 'already_closed'),
  c  := (insert Comment { body := <str>$comment, issue := i,
                          author := global viewer }),     # One: tracker's shapes all have a viewer
  u  := (update i set {
          state := State.Closed,
          closed_at := shep::now(),
          labels += (if not exists i.labels then (
                       insert Label { project := p, name := 'triage' }
                       unless conflict on (.project, .name) else (Label))
                     else <Label>{}),
        }),
select (comment := c.id, revision := u.revision);
```

```rust
// App code calls the generated function (§7.1).
match ops::close_issue().issue(&issue_id).comment("fixed").exec(&alice).await {
    Ok(r) => println!("comment {} at revision {}", r.comment, r.revision),
    Err(close_issue::Error::AlreadyClosed) => println!("already closed"),
    Err(e) => return Err(e.into()),
}
```

- **One placement group, one partition.** `Issue`, `Comment`, and `Label`
  are placed with their project (§4.1.1), so everything `close_issue`
  reads and writes is in one group, the `unless conflict` checks an
  `exclusive` index that includes the project (§3), and the mutation
  commits in one partition without two-phase commit.
- **Every `assert` runs, in order, before any write.** Unlike Gel, which
  skips `with` bindings nothing references, Shep evaluates every `assert`
  binding (§4.0). Each condition must be One, so `assert({})` can't pass
  silently. Its `message` must be an identifier, not one of Shep's
  reserved error names; it becomes a variant of the operation's
  generated error enum (§7.1). `assert_exists` with no message raises
  Shep's own `not_found`.
- **The shape is fixed.** A ShepQL statement is a description, not a
  program: there are no values to branch on with Rust's `if`. A condition
  is an `assert`, which aborts, or an `if … then … else`, which the server
  evaluates (its condition must be One too). "No interactive
  transactions" is a property of the language rather than a rule in the
  documentation.
- **Lowered once, at registration.** The checker lowers the typed
  statement to the transaction IR, which is registered with the operation
  set. A request names the mutation and passes its arguments. Server-side
  retries replay the IR.
- **Every read sees `S`.** Paths, key casts, and `with` bindings read the
  mutation's snapshot, through visibility (§4.0). Two things see the
  merged post-state instead: the bindings of DML results (`c`, `u`
  above), and `shep::observe` (§11.3). So `i.state` in the `assert` is the
  state before the update, wherever the `assert` is written.
- **Patches merge, then policies and checks run once.** All updates to
  one node merge in statement order into one patch. `update write`
  policies, field policies, and checks then run once, on the merged
  post-state, and `update read` on the pre-state.
  - **The merged patch is the definition.** It equals applying the
    updates in sequence whenever every intermediate view is valid
    (`merge_eq_seq` in `PatchMerge.lean`, §4.4 law 5); when an
    intermediate view wouldn't be, the merge is still what runs, and
    only the merged result is checked (`merge_accepts_invalid_prefix`).
  - **Assigning a field twice is an error.** At compile time when the
    targets are statically the same node (Gel silently keeps one); at run
    time, `invalid`, when two updates may alias, such as two key
    parameters naming one node, and do.
  - **In a `for`,** field operations on one field apply in statement
    order (§8, below), and a repeated plain assignment is the same error.
  - Fields no statement names are left alone.
- **Single-target writes fail loudly.** An `update` or `delete` of one
  node, through a binding or a key cast, raises `denied` when its policy
  denies it, or `not_found` when the actor can't read the target. A
  set-valued `update T filter …` or `delete T filter …` filters instead:
  it writes the nodes the actor may write and skips the rest, and
  returns them, so its result is a set (§4.0 item 8).
- **Return values are the final `select`.** Values of `insert` and
  `update` results it returns are post-commit values (`u.revision`).
- **Optional arguments don't guess.** Assigning an optional parameter to
  a field needs `?? .f` to keep the field when the argument is absent, or
  `shep::clear()` to unset it (§4.0 item 12).
- **Upserts.** `insert … unless conflict on (…) else (select T)`, or
  `else (update T set {…})`. The conflict target must be one `exclusive`
  constraint or the key. Two inserts in one mutation that may hit the
  same key are rejected unless they go through `unless conflict`.
- **`global viewer` is typed by the schema** (§4.1.1), and its
  cardinality by the operation set's principal shapes (§4.0 item 11). It's
  the request's viewer. Written into `issues::Comment.author`, the checker
  requires `issues`' viewer type to match the link's target. When an
  agent runs `close_issue` for a user, the comment's author is that user.
  In a set that can run through `db.as_app()`, it's AtMostOne, and a
  mutation that writes it must `assert_exists(global viewer)` first. A
  viewer of another type is denied before the mutation runs (§6.3).
  - **`global actor`** is the actor itself, in mutations of an app whose
    principal shapes include actors (§7.1). A mutation can compare it,
    and narrow it with `[is …]`: `if global actor is agents::Agent then
    (update t set { agent := global actor[is agents::Agent] }) else (…)`.
  - **The log records both** (§10.1), so which agent acted is never lost.
    Schemas that need it in the data, not just the log, add a link, such
    as `written_by_agent: agents::Agent`, and set it from `global actor`.
- **One name in every context.** `Issue::get_by_id(&issue)` in Rust and
  `<Issue>$issue` in ShepQL are the same primitive read; inside a
  mutation it's a snapshot read at `S` that can feed decisions. Both
  return nothing for a missing, hidden, deleted, or placeholder key.
- **One IR, every client.** Browsers call the same mutation through the
  generated client or GraphQL (§7.4).
- **`shep::batch` is shorthand, not a second mechanism.** A batch is a
  generated mutation made only of primitives, applied atomically, and
  registered in the operation set like any mutation. A batch whose length
  varies at runtime is a bounded `for` over an array argument:
  `for f in shep::bounded(array_unpack(<array<shep::Upload>>$files), 10)
  union (…)`. An argument longer than the bound is an error, not a
  truncation.
- **Invariants don't live in mutations.** Operations belong to apps, so
  another app can reach the same data through its own operations or the
  primitives. Anything that must hold for every writer is a check (§4.5)
  or a write policy (§6).

Properties:

- **Reads are explicit.** Every read is a path in the statement, usually a
  `with` binding. Every read a decision depends on (an `assert`, an `if`
  condition, a write policy, a freshness guard, `shep::observe`, a value
  it writes) is validated at commit (§10.3), except stable guards,
  unchanging facts, best-effort target-lifecycle checks, and the targets
  of leader-resolved field operations, which the planner proves can't
  change the outcome (§7.6). None needs a lock or a `for update`. A read
  that only shapes the mutation's return value is a snapshot read at `S`.
- **Guards are Pure ShepQL** over bound values, and total (§4.0):
  `assert` aborts with a named error, and `if` branches. Mutations may
  also read `shep::now()`, the commit time.
- **Guards on freshness.** `assert(shep::unchanged(ops::inputs(target :=
  t), since := <shep::Version>$inputs_at), message := 'stale_inputs')`
  aborts unless a declared query's inputs are unchanged since a version
  (§11.7).
- **Bounded.** No unbounded loops. `for` takes a `shep::bounded` source,
  or a query with a literal `limit`.
- **Concurrency primitives.** Opt-in compare-and-swap per field or on a
  node's `revision` (§7.3), freshness guards (§11.7), upserts by
  `exclusive` constraint or key, and field operations resolved at the
  leader (below), so common read-modify-write patterns don't need a round
  trip.
- **Field operations are resolved at the leader.** The planner recognizes
  these update forms, and only these:

  | Form | Op | Meaning at the leader, with `b` the base value |
  |---|---|---|
  | `x := .x + e`, `x := .x - e` (`x` required) | `add n`, `sub n` | `b + n`, `b - n` |
  | `x := (.x ?? 0) + e`, `- e` (`x` optional) | `add n`, `sub n` | `(b ?? 0) + n`, `(b ?? 0) - n` |
  | `x := max({.x, e})` | `max v` | `max({b, v})`: `v` when `b` is unset, as EdgeQL's `max` over a set |
  | `tags += v`, `tags -= v` (`multi` property) | `insert v`, `remove v` | set insert and remove; idempotent |
  | `arr := .arr ++ [v]`, `arr := .arr ++ e` (`arr` required) | `push x`, `extend xs` | `b ++ [x]`, `b ++ xs` |
  | `arr := (.arr ?? []) ++ …` (`arr` optional) | `push x`, `extend xs` | `(b ?? []) ++ …` |

  - **Exactly the evaluator's semantics.** Each op's result is what the
    ShepQL evaluator would compute from the form at the base value,
    including `{}`: `.x + e` on an unset optional `x` would be `{}` and
    unset the field, which is why the optional forms need `?? 0` or
    `?? []` to qualify. Anything else, `.x + e` on an optional field
    included, compiles to a validated read and write.
  - **When it qualifies.** The target is read only by its own update, `e`
    reads no graph state and has cardinality One (an optional `e` would
    unset the field when empty: `optionalArg_unsets`,
    `max_optionalArg_unset` in `LeaderOps.lean`), and the field reaches storage through the
    identity lens: a write from an older version whose adapter isn't the
    identity on this field is never leader-resolved (§4.3).
  - **In a `for`,** operations on one field apply in statement order, as
    the leader resolves them: `add` and `sub` add up, `insert`s and
    `remove`s apply in order, pushes concatenate. Ops of different kinds
    on one field, such as an `add` and a `max`, don't commute
    (`add_max_not_comm`), so their order is the statement order, not a
    sum.
  - `shep ops check` prints "leader-resolved", or the reason it isn't.
    They're the same operations as Toasty's `increment()`, `add(v)`,
    `push(x)`, and the rest in `update!` (§7.3). The operation exists
    only in the proposal.
  - **What the proposal carries.** Alongside its opaque delta, an **op
    record** per field operation, `{key, op, base_value, base_stamp,
    bound?}`: `op` is `add n`, `sub n`, `max v`, `insert v`, `remove v`,
    `push x`, or `extend xs`; `base_value` and `base_stamp` are the field's value and
    stamp at `S`; and `bound` is the field's recognized single-field
    check, copied from the schema. It also carries a **value record**,
    `{key, new_value}`, for every plain write to a key that field
    operations may target, and `{key, absent}` for each such key the
    mutation unsets or clears by deleting its node, so the leader never
    resolves against a stale value. The front end knows which keys are
    op-eligible from the schema.
  - **The leader's last-value map.** The leader keeps `key → (value,
    stamp)` for op-eligible keys written within the validation window,
    filled from resolved operations and value records. A new leader
    rebuilds it from the log tail, starting at the window floor and
    including uncommitted entries, as it does the validation table
    (§10.3).
  - **The leader resolves it.** If `S`'s prefix for the partition is below
    the window's floor, the proposal gets `too_old`, the ordinary
    validation check (§10.3). Otherwise the operation applies to the
    map's newest write to the key whose stamp is not in `S`, or, if there
    is none, to `base_value`, however old its stamp. Stamps are never
    compared with each other: with extras, a stamp and a prefix aren't
    comparable, but membership in `S` always is. So the result is the
    field's value at the entry's position in the log, counting every
    earlier appended entry, committed or not.
  - **No conflicts between operations.** Op keys aren't in the read set,
    so two increments of one counter never conflict, and a counter every
    worker increments never serializes through retries.
  - **Deletes still conflict.** An operation on a node that could be
    deleted concurrently puts the node's lifecycle key in its read set,
    so a concurrent delete gets `contended`. The op keys themselves stay
    out of the read set.
  - **Readers see ordinary facts.** The leader writes the results into the
    entry before appending it, in a **resolved writes** section,
    `{key, old, new}`, which the materializer merges with the opaque
    delta (§10.1). Replay, change queries, validation, idempotency
    outcomes, and return values need no leader logic.
  - **Bounds come from the schema.** A built-in constraint on the field,
    `min_value(0)`, `max_value(N)`, or `max_len_value(cap)` (and the size
    cap, §16.1), travels in the op record, and the leader checks it on the
    resolved value. Crossing it is a failure outcome, `invalid`, naming
    the constraint (§10.3). So `stock := .stock - 1` on a field with
    `min_value(0)` can't go negative, however the proposals interleave.
  - **Otherwise it's an ordinary write.** If another check, a write
    policy, an `exclusive` constraint, or another read in the same
    mutation reads the field, the operation compiles to a validated read
    and write instead.
  - **Ordering.** The leader puts the key's previous writer `prev` and
    `prev`'s context in the entry's context, so writes to one key stay
    causally ordered (§9.5). `prev` is the map's newest write to the key
    whose stamp is not in `S`, or else the base's stamp. If `prev`'s
    index isn't known when the context is fixed, the context carries a
    **self-prefix flag** instead, which readers expand to the entry's own
    partition up to the entry before it (§9.5).
  - **Crate boundary.** Values are encoded by a small shared codec,
    `shep-value` (scalars, lists, and byte sizes for size caps),
    used by both the WAL and `shep-log`. The WAL never depends on the
    delta format (§17).
  - **Not in two-phase commit**, in v1, like `shep::claim`.
- **`shep::claim` for work queues.** Agent orchestration means many workers
  asking for "the next unclaimed task". With compare-and-swap, every worker
  races for the same few nodes and retries.
  - **The operation.** `shep::claim(query, n)` atomically takes up to
    `n` matching nodes that no one else holds, the graph form of
    Postgres's `FOR UPDATE SKIP LOCKED`, and returns a **lease** for
    each, a `shep::Lease { node, generation }`:

    ```
    select shep::claim((select Task filter .state = TaskState.Ready
                        order by .priority desc), 5)
      { node: { id }, generation }
    ```
  - **Contended workers don't conflict.** They get different tasks. The
    front end proposes more candidates than `n`, and the partition's
    leader drops any that another claim took, or that hold two-phase
    commit intents, and keeps up to `n` of the rest (§10.3).
  - **Its shape is restricted.** One partition's index range, chosen when
    it runs; a candidate predicate that reads only the candidate's own
    keys; and one independent claiming update per node, made by Shep. No
    `shep::observe`, and no part of a two-phase commit. The checker
    rejects anything else. A retried `claim` after an unknown outcome returns the
    keys it claimed the first time, through its idempotency key, checked
    at the leader against appended entries. That check is the only thing
    that stops a duplicate: claims skip contended candidates, so
    validation never sees one (`WalValidate_claim_idem`, §10.3).
  - **An empty claim is a client-visible outcome.** Like any failure the
    client sees, a claim that finds nothing on a re-execution is reported
    through the anchor, so it never hides an earlier attempt's leases
    (§10.3, `WalValidate_fail`).
  - **A lease, not exactly-once execution.** Lease state is system-owned
    (`@lease`: holder, expiry, and an **attempt generation** that goes up
    with every claim), and never appears in a schema. The generation is a
    fencing token in the style of Chubby's sequencers
    ([Burrows, OSDI 2006](https://research.google.com/archive/chubby-osdi06.pdf)).
    - **Fenced writes.** `.under(&lease)` on any mutation conditions every
      write it makes on the lease's generation still being current. A
      stale holder's writes fail with `lease_lost` and change nothing.
    - **Renew and complete.** `shep::renew(<shep::Lease>$lease)` extends
      the expiry, and `shep::complete(<shep::Lease>$lease)` releases the
      node, both fenced the same way; `lease.renew()` and
      `lease.complete()` in Rust.
    - **Expiry is a fact.** A sweeper on the partition's leader writes
      `@lease_expired` once a lease's expiry passes by the leader's clock,
      plus a skew allowance. Until then the lease holds. So expiry is a
      stamped change that live queries and freshness checks see, like any
      other (§6.1), and the next claim takes the node with a higher
      generation.
    - **Processing is at least once.** A worker can die after doing work
      and before completing, so the work can run again. Effects that leave
      Shep, such as a webhook or a build, must be idempotent, keyed by the
      node and the attempt generation.
- **Bulk ingestion is batched.** One transaction can carry thousands of
  creates, upserts, and links. It counts once against the per-universe
  per-transaction overhead and appears as one dot and one log entry. SCM
  ingestion and other high-volume writers are expected to use this.
- **Linking ahead of creation.** On a placeholder-enabled type, linking
  to a key with no node creates a placeholder, and a later create fills
  it: a no-op if the data is equal, `conflict` if it differs (§3).
- **Deletion ops.** `delete x` and `shep::restore(x)` set and clear
  `deleted_at`, and each is governed by its own policy. By default, reads
  treat deleted nodes as not found (`shep::with_deleted` opts in, §6.5). Updating a deleted node
  fails unless the transaction opts in explicitly. Linking to a deleted
  node fails too, but as a best-effort check at `S`: a concurrent delete
  can land just before the link commits, which leaves the same state as
  link-then-delete (§7.6).
  Unlinking an edge is a normal mutation. The edge disappears from every
  read, but its keys keep a ghost version with the unlink's stamp, so
  freshness checks can see it left (§3).
- **Deterministic**, so the server can retry it. A front end executes it
  against a snapshot and proposes its delta with its read set, and the
  partition's leader appends it only if nothing it read has changed
  (§10.3). On a conflict, the front end re-executes it at a newer version.
  Range reads are in the read set, so guards like `count(...) < N` see
  phantoms too.
- **Every read behind a decision is validated, unless it can't matter.**
  Guards, write policies, freshness guards, `observe`, and values flowing
  into writes are validated at commit even when they read another
  placement group: that group's partition joins the commit as a read-only
  participant (§10.3). The planner skips the reads it proves safe from
  declared disciplines: a stable `assert`, an `if` over unchanging
  facts, a target-lifecycle check on `link`, the unchanging parts of a
  freshness check, and the targets of field operations the leader
  resolves (§7.6). Write policies are always validated. Which
  operations need two-phase commit is decided at deploy, by placement
  group, and `shep ops check` shows each one's cost.
- **Checks run against the final state of the transaction**, like deferred
  constraints. "Issue requires a project" must hold at commit, not after the
  `create` that comes before the `link`. Create/update/delete policies run
  per operation, with both the pre-state and the post-state available.
- **Idempotency keys are required.** A client that loses the connection
  during commit can't otherwise tell whether the commit happened. Client
  libraries generate one per call and reuse it on retries, so app code
  never sees them.
- **Escape hatch: registered wasm mutation functions**, which are basically
  stored procedures. They're shipped with an operation set, run in the
  front end as part of the mutation, through a host API that enforces
  privacy and records reads, and are fuel-limited.
- **Observing changes, for app-owned fan-out.** A mutation can declare
  `observe` clauses, from `shep::fanout`. Their reads are validated like
  any other, so the reported changes end exactly at the commit (§11.3,
  §13.6).
- **The commit advances the actor's causal position**: the mutation's
  context plus its own dot (§9). The client library keeps it in the
  actor handle (§7.3).
- **Every commit is logged.** The log entry is the commit: its effects and
  intent metadata are what the WAL replicates (§10).
- **Mutations across placement groups use two-phase commit.** Placement
  keeps data that's written together in one group, so most mutations touch
  one partition (§9.6). When one writes two groups, or reads another
  group in a way that must be validated (§10.3), the front end that
  executed it coordinates a two-phase commit. The **participants** are the
  partitions of every group it writes or validates, and the transaction is
  **one dot**: its decision's.

  **Protocol: two-phase commit.**
  1. **Prepare.** The front end gives this attempt a fresh **transaction
     ID**, which carries a deadline. The **home** is the mutation's
     **anchor partition** (below). It sends each participant, in parallel,
     its part: the delta for that partition, its read set there, the read
     version `S`, the transaction ID, the home, the participant list, and
     the registry dot the part was computed under. Each participant's
     leader validates the part like any proposal (§10.3) and appends a
     **prepare** entry, which holds **intents**: write intents on the
     part's write set, and read intents on its read set. Until the
     transaction is decided, a proposal that writes a write or read
     intent's keys or ranges, or reads a write intent's, is rejected with
     a retryable `contended`; reads never block reads. Nothing waits on a
     lock, so nothing deadlocks. A prepare stamps nothing: its part isn't
     visible. Its context is the participant's read version `S`.
  2. **Decide.** Once every participant has prepared, the front end
     appends a **commit decision** to the home's log. Its context is the
     merge of every participant's read version and every participant's
     prepare dot. Its position is the transaction's **dot**. The home's
     leader accepts the first decision for a transaction ID and rejects
     any later one. It also:
     - **checks the idempotency key**, at the anchor: if the key already
       has a commit, the decision is an abort and the client gets the
       original outcome. At most one attempt per key ever commits.
     - **aborts on a newer registry**: if any participant's prepare was
       computed under an older registry dot than that participant's
       latest barrier, the decision is an abort (`TwoPhase_noregcheck`).
       This check alone can't order a release after a barrier: a barrier
       can land between the decision and a release. So a barrier also
       waits for every prepare at its partition to be released before it
       is appended (§10.3, `TwoPhase_barrierlog`, `TwoPhase_barrierdrain`).
     - **aborts past the deadline**: a decision for a transaction whose
       deadline has passed is an abort.
     - **aborts past the horizon deadline**: a decision whose horizon
       deadline (§6.3) has passed by the home leader's clock is an abort
       (`AuthHorizon_reuse`).
  3. **Reply.** Once the decision commits, the front end replies to the
     client, with the results and the version token: the decision's
     context plus the decision dot.
  4. **Release.** The front end tells each participant the outcome. Each
     participant's leader appends an **intent release** naming the
     transaction, its decision dot, and the commit time, and drops its
     intents. The release's context is its decision's context plus the
     decision dot, so every causally closed version that contains the
     release contains its decision, and anything after the release in this
     partition's log, including a split entry, has the decision in its
     causal past (`Lineage.lean`, `FrontierMaterialize_relctx`). The materializer applies the prepared part only at the
     release, **stamped with the decision dot**, with the release's
     physical version (§3). It becomes visible exactly in the versions
     that contain the decision. On abort, the release drops the part.

  - **The anchor partition.** Every mutation has one, derived from its
    request arguments alone: the partition of a placement group its
    arguments name. The checker requires every mutation to have one. The
    anchor records and checks the mutation's idempotency key, whether the
    mutation commits in one partition or by two-phase commit, and it is
    always the home of a two-phase commit (a participant even if the
    mutation neither reads nor writes there). So a retry that executes in
    a different shape still meets the same key.
  - **Each attempt is fresh.** A retry, internal or from the client,
    gets a new transaction ID and may reach a different set of
    participants. Attempts that abort (contention, a two-phase commit
    abort) don't consume the idempotency key; a failure the client sees
    is recorded as the key's one outcome (§10.3).
  - **Timeouts.** A participant whose prepare has waited longer than the
    prepare timeout (two seconds by default) for its release asks the home
    for the decision. If the home has none, the participant appends an
    **abort decision** to the home's log itself. Prepares name the home,
    and only the home records decisions: a participant never decides at
    its own partition (`TwoPhase_twohomes`). The home accepts whichever
    decision arrives first, so the outcome is single even when the
    coordinator and a participant race. A coordinator that can't prepare
    every participant before that timeout appends the abort itself.
  - **Recovery.** If the coordinating front end dies, every participant
    reaches the timeout and learns the outcome from the home.
  - **Three bounds, kept apart.**
    - **The request deadline** bounds how long the client waits. A client
      whose deadline passes gets `overloaded` with an unknown outcome,
      and its retry reuses the idempotency key.
    - **Recovery starts** at the prepare timeout: a participant then asks
      the home, or appends the abort.
    - **Completion** is eventual, assuming the home's quorum is available.
      While it isn't, intents stay held. The prepare timeout is when
      recovery begins, not a bound on how long intents are held.
  - **The transaction's state, per participant:** `prepared` (intents
    held, part durable), then `released-commit` or `released-abort`. At the
    home it's: no decision, then `committed` or `aborted`, then
    `forgotten`. Every transition is a log entry.
  - **Durable identity.** Every prepare names the transaction ID, the
    home, and the full participant list, so any participant can find the
    decision without the coordinator.
  - **Finding a retired home.** If the home has split, the transaction ID's
    home is routed through the split lineage, from the split entry in
    the parent's log or the controller's partition table, to the child
    that holds the decision. A split hands its unforgotten decision
    records, with a snapshot of its idempotency table, to the child
    holding the home's lowest placement group, in the child's first
    entry, which the child commits before accepting any proposal (§9.6,
    `TwoPhase_splitlost`, `TwoPhase_split`).
  - **Safe forgetting.** The home keeps a decision until every
    participant that prepared has released **and** the transaction's
    deadline has passed. Forgetting earlier lets a delayed prepare reach
    a participant that never prepared, and a slow coordinator then
    commits a transaction the home already aborted
    (`TwoPhase_forgetearly`, `TwoPhase_nodeadline_atomic`). Before the
    deadline the home holds every decision it made. After it, a lookup
    for an ID the home doesn't hold is answered `expired`, and the first
    such answer for an undecided ID appends an abort decision, so expiry
    is monotone across home leader changes. Deadline checks use the
    leader's lease-bounded clock.
  - **Materializers look ahead in their own log first.** A materializer
    that holds a prepare in `V` but hasn't applied its release first reads
    its own partition's log to the commit index. If the release is there,
    it uses the release's outcome and dot and never asks the home. It asks
    the home only when no release is committed, and then the home still
    holds the decision. Without the lookahead, a lagging replica resolves
    a forgotten commit as not committed (`TwoPhase_nolookahead`).
  - **Late messages.** Participants remember released transaction IDs
    until their deadlines. A prepare or release that arrives for a
    released ID, or after its deadline, is rejected by ID, and so is a
    decision past the deadline. It never starts the transaction again or
    applies a part twice (`TwoPhase_dup`). `TwoPhase.tla` checks these
    cases (§18).
  - **What's durable where.** Prepares, decisions, and releases are log
    entries. A participant's prepared parts and intents are in its Raft
    state machine and its snapshots, so a new leader holds them. The
    prepared part is applied from the prepare entry, and log truncation
    never passes the oldest prepare that isn't released (§10.4).
  - **Atomic visibility, by construction.** Every write of the transaction
    is stamped with the decision dot, and the decision's context contains
    every participant's prepare. A causally closed version contains that
    dot or doesn't, so a read sees all of the transaction or none of it
    (§9.5, `TwoPhase_localstamp`, `TwoPhase_noprepdot`). A frontier can
    contain a decision whose participants haven't applied, or even
    appended, their releases. Readers and derived consumers resolve such
    a prepare as in doubt (§9.7, `FrontierMaterialize_noindoubt`). A
    point read whose version holds a decision before its release has
    applied waits for the release; that wait ends only when a release is
    appended, so liveness rests on the participant timeout appending it
    (`FrontierMaterialize_livenorel`).
  - **Splits.** A partition with prepared transactions finishes them
    before its split entry: between `start_split` and the split entry its
    leader drains prepares (§9.6), so a transaction never spans a split.
  - **Cost.** Three appends in sequence: prepare on every participant,
    the decision at the home, and the release, which the client doesn't
    wait for. Admission control prices that (§15), and `shep ops check`
    names every operation that can need it (§7.1).

## 9. Versions: dots and dotted version vectors

Shep doesn't order all mutations in a single sequence. Each mutation gets a
**dot**, and every fact it writes is **stamped** with that dot. **Versions**
are dotted version vectors: they say exactly which dots a read, a write, or
a materialized query reflects, and a fact is visible at a version exactly
when its stamp is in it. There is no global ordering point, so adding
partitions adds write capacity.

**The consistency contract.** Each guarantee, separately, and where it's
specified:

| Guarantee | For app developers | What Shep promises | Section |
|---|---|---|---|
| Mutation isolation | Your mutations behave as if run one at a time | Committed mutations are serializable, in a serial order consistent with each partition's log order: each one's writes and `assert` outcomes are what they would be at its position. Every read behind a decision (guards, write policies, freshness guards, `observe`, values flowing into writes) is validated at commit, including reads in other placement groups, through read-only participants, except reads the planner proves can't change the outcome: stable guards, unchanging facts, and leader-resolved field operations. A value a mutation only returns is a snapshot read at its `S` | §10.3, §8, §7.6 |
| Read freshness | Reads are consistent snapshots that include your own writes | Every read has an exact, causally closed version. Plain reads are causally consistent snapshots, not serializable. `at_least` gives read-your-writes per actor. Views and search wait to a bound, then fail with `lagging`, or return their own version if `stale_ok` | §9.4, §7.3 |
| Atomic visibility | You see all of a mutation or none of it | A transaction is one dot, so every causally closed version holds all of it or none of it | §9.5, §8 |
| Authorization staleness | Shep's revocations take up to 5 seconds; credential revocation takes up to a token's lifetime | Every principal-sensitive request, mutation, link check, and topic step runs at a version that contains the authorization horizon: the meta partition and the actor's and viewer's partitions, each as committed no more than the revocation bound ago. A front end or materializer that can't show that fails closed. Bounded this way: policy deploys and promotions, app grants, and deleting an actor or a viewer. A revoked credential is the auth service's: Shep keeps no revocation list, and honors a token until it expires (open question 26). Other permission changes, such as removing a member from a channel, are bounded too, because a materializer refuses revocation-sensitive reads at versions more than the bound behind its leader's commit index, and proposals and decisions carry a deadline the appending leader enforces. Write policies are validated at commit, not bounded (§6.3, §7.6) | §6.3, §7.6, §13.2, §16.4 |
| Leftover access after revocation | A download link works as you for an hour; its redirect works for anyone for a minute | A link is a bearer capability for up to an hour, rechecked on every use. Its redirect's presigned GET is a residual capability for up to one minute after the check, unless the field's policy sets `strict_revocation` | §16.4 |
| Derived-data lag | Views, search, and live updates trail writes slightly | Views, search indexes, and live topics step between causally closed versions, and lag the log by their consumers' delay | §9.7, §12, §13 |
| Search | Search results can be a moment behind, and are ranked, not exact | Ranked over one index version. Not exact at a version, and not freshness-checkable | §16.6, §11.7 |

### 9.1 Dots

- **A dot is `(partition, index)`,** where `index` is a Raft log index in
  that partition's WAL (§3). Every entry in a partition's log has one,
  whatever its kind (§10.1).
- **A mutation's dot stamps its writes.**
  - **A single-partition mutation's dot** is its own entry's position,
    assigned when its partition's leader appends it (§10.3).
  - **A two-phase commit's dot** is its decision entry's position in the
    home partition (§8). Every participant's writes are stamped with that
    one dot, so a transaction is one dot, however many partitions it
    writes.
  - **Other entries stamp nothing.** Prepares, intent releases, registry
    barriers, splits, failure records, void entries, and Raft's own
    entries (§10.1) take positions in the log, but write no visible facts.
    They still carry contexts: a prepare's is the participant's read
    version `S`, and a release's is its decision's context plus the
    decision dot (§8). Without the release's, a causally closed version
    can hold a release without its decision, and compaction can drop a
    version that version still needs (`FrontierMaterialize_relctx`).
- **Void entries.** Each proposal records the term its leader validated it
  in (§10.3). An entry that Raft appends under any other term is **void**:
  it stamps nothing and decides nothing (no outcome, no idempotency
  record), and readers see its kind as Void and skip it.
  - **Why.** A proposer learns "I'm leader in term `T`" from Raft's
    metrics, and Raft can still append the entry in term `T+2` after a
    leadership change the proposer never saw. Validation in `T` says
    nothing about the log in `T+2`, so the entry can't count.
  - **It keeps its index.** A void entry is a dot with no facts, like a
    barrier. Committed indexes stay contiguous, versions are unaffected,
    and a version that contains a void dot contains nothing more for it.
  - **It stays for now.** With `shep-raft`, the leader validates and
    appends in one step on its own core, which may make the rule
    unnecessary; a re-run of `WalValidate.tla` is checking that (§18),
    and until it does the rule applies.
- **Committed indexes are final and contiguous.** Raft commits a
  partition's entries in index order, and a committed entry never changes.
  Uncommitted entries that a failed leader appended can be overwritten by
  a new leader, but nothing reads them: materializers, readers, and
  responses only ever see committed entries.
- **Partition IDs increase.** The WAL controller assigns each universe's
  partition IDs in increasing order, from its `next_partition`, and never
  reuses one. A split retires a partition and creates new ones with
  larger IDs (§9.6).
- **A partition's ID is its generation.** IDs only increase, and a
  split's children get larger IDs than their parent, so the ID orders
  lineage. Keys order their physical versions by partition ID, then
  position (§3).
- **Dots stay inside Shep.** Clients never see or construct them. They see
  versions, as opaque tokens (§9.4).

### 9.2 Versions are dotted version vectors

A version is a **dotted version vector (DVV)**: for each partition, a
contiguous prefix `1..n` of its log, plus a set of **extra dots** beyond
the prefix.

- **Visibility is by stamp.** The state at version `V` is every fact whose
  stamp is in `V`, and, for each key, the newest such version (§3). Reads,
  ghosts, freshness checks, change queries, and views all define "the state
  at `V`" this way, never by log position.
- **Operations.**
  - **Membership:** `(p, i) ∈ V` iff `i ≤ prefix[p]` or `(p, i)` is one of
    `V`'s extras for `p`.
  - **Merge:** the union. Prefixes take the pointwise maximum, and extras
    the prefix now covers are dropped.
  - **Comparison:** set inclusion.
- **Why extras.** A version must be **causally closed**: it contains the
  context of every dot in it (§9.5). Extras let a version include a dot
  without the whole prefix before it. An actor handle's own write is the
  common case: its token is its context plus its own dot, and the entries
  between the context's prefix and that dot aren't in its causal past.
  Validation (§10.3) means the write didn't depend on them, so the handle
  doesn't wait for them.
- **Causal closure, not a prefix, is what makes a version valid.** Every
  causally closed version is a valid state (§9.5), with or without extras.
  Prefixes are the compact common case: a read at a partition's applied
  index, with nothing else, is a prefix.
- **Retired partitions.** A version may name a partition that a split has
  retired. Its entries are still in the log and the archive, and §9.6 says
  how versions translate across a split.

### 9.3 What this removes

The Postgres-based revision of this design used dotted version vectors too,
but needed machinery that a replicated log makes unnecessary:

- **Holes and voids.** A sequence value taken by a transaction that then
  aborted left a gap, which a void entry had to fill before a prefix could
  move past it. Raft never commits a gap, so a partition's committed log
  is a prefix, and extras only ever come from causality, never from gaps.
- **The void fence and the frontier advancer.** These resolved gaps and
  advanced a per-node prefix past them. A partition's commit index already
  is that prefix. (The frontier §9.7 publishes is a different thing: a
  causally closed version across partitions.)
- **Epochs on failover.** Promoting an asynchronously replicated Postgres
  replica could lose committed log rows and reissue counter values. A Raft
  leader change never loses a committed entry, and terms never appear in
  dots.
- **Taking the dot before or after the writes.** The leader assigns the
  index at append, after validation (§10.3), and the materializer stamps
  the delta with it.

The TLA+ models `DotFrontier`, `SerialLog`, and `MultiNode` in
`formal/tla` describe that Postgres-era protocol. They're kept as
history. The WAL, validation, two-phase commit, and splits get new specs
(§18). The Lean model of causal closure (`wf_stateAt_closed` in
`formal/lean`) carries over unchanged: it's stated over abstract dots, each
with a delta and a context (§9.5).

### 9.4 Snapshot versions and version tokens

- **Every read has an exact version.** It is causally closed, and every
  fact the read returns has its stamp in it.
  - **A read of one partition** at applied index `n` has version `{p: n}`
    closed under contexts: materializers keep, for each partition, the
    **causal frontier** `cf(p, n)`, the union of the contexts of entries
    `1..n`, keyed per position (§3). Every context is causally closed
    (§9.5), so `{p: 1..n} ∪ cf(p, n)` is closed and the closure is one
    lookup at exactly `n` (`FrontierMaterialize_onceclosed`). With open
    contexts one lookup would miss dots (`FrontierMaterialize_once`), and
    only the §9.7 fixpoint would close them (`FrontierMaterialize_ctx3`).
  - **A read across partitions** picks its version as §9.7 describes.
- **Every response carries a version token.** Every read and every write
  returns one. A read's token is its version. A write's token is its
  context plus its own dot, usually as an extra (§9.2).
- **Tokens are opaque. There are two uses, and they must not be mixed.**
  - **Causal tokens, for `at_least`.** Client libraries merge tokens from
    several responses and send the merge as `at_least`. A merge of
    causally closed versions is causally closed (`causallyClosed_union`
    in `formal/lean`).
  - **Result versions, for `since`.** A `since` must be the version of the
    specific result the caller holds: the token that came with that result,
    or with the last delta applied to it.
    - **Never a merged token.** With a merged token, a change committed
      between the result and the merge is never reported
      (`ChangeQueries_handle`).
    - **Kept apart by the client libraries.** Causal tokens live in actor
      handles, and result versions live inside subscriptions and feeds
      (§7.3). App code never handles a result version, so it can't merge
      one. On the common path, the only token apps see is a `Bookmark`, a
      serialized causal token. Only `shep::fanout` exposes result
      versions, for `since` (§11.3, §13.6).
  - **Size.** A token is a few bytes per partition it names, plus its
    extras. Tokens name only the partitions their reads and writes
    touched and their contexts' closure. A retired partition's entry
    folds into its children's only in causal tokens (`at_least`), or once
    its prefix reaches the split entry, and never in a result version used
    as `since` (§9.6). Extras fold into the prefix as soon
    as the prefix reaches them.
- **Causal reads.** A request with `at_least: V` is served from a
  materializer, a materialized query (§12), or a search index (§16.6),
  whose version contains `V`.
  - **Materializers wait, briefly.** A materializer that hasn't applied
    every dot of `V` in its partitions waits until it has. The entries are
    committed, so the wait is the apply lag, normally well under a
    millisecond to a few milliseconds, bounded by the request's deadline
    (§15.2).
  - **One slow case: your own two-phase commit.** A two-phase commit's
    parts are applied at their releases (§8). A read at the client's own
    token after a coordinator crash waits for those releases, which come
    after the prepare timeout plus the home's availability. The request
    deadline still bounds the wait, which ends in `overloaded` with a
    retry.
  - **Views and search indexes** wait, with a bound, for their version to
    cover `V`. Past the bound, the read fails with `lagging`, or returns
    what it has, with its version, if the operation is declared
    `stale_ok` (§7.3).
  - Each actor handle tracks two merges: every token it has received, and
    the tokens of its own writes. Materializer reads send the first.
    Views and search indexes are sent only the second, so a handle's reads
    never make its next view read wait for that view's lag. This gives
    read-your-writes per actor everywhere, and monotonic reads per actor
    on materializer reads (§7.3).

### 9.5 Context: a mutation's causal past

- **What a context is.** Every mutation records its context: the version
  it read at, merged with the client's `at_least` and `since`, and the
  registry dot its partition requires (§10.3). `since` is merged so that
  `changes` can rely on `V1 ⊆ V2` (§11.1), even when `since` names other
  partitions.
  - **A two-phase commit's context** is its decision entry's context: the
    merge of every participant's read version, plus every participant's
    prepare dot (§8).
  - **A field operation adds its key's previous writer.** For an entry `e`
    with field operations, `ctx(e) = S ∪ ⋃ₖ ({prevₖ} ∪ ctx(prevₖ))`,
    where `prevₖ` is the leader's newest in-window write to key `k` whose
    stamp isn't in `S`, or else the base's stamp (§8). When written keys
    can be strictly prefix-related (a map entry and the map, a struct
    field and the struct), the rule also adds each conflicting key's last
    writer. With scalar leaves the per-key rule is enough. Proven in
    `LeaderOps.lean` (`leader_wf`, `ctx_closed`, `selfPrefix_closed`,
    `ctx_pathDisjoint`; counterexample `ctxOmitPrev_not_set`).
  - **On the wire, a context is a DVV plus an optional self-prefix
    flag.** The flag was needed because openraft assigned indexes inside
    its core and interleaved blank entries, so when the leader fixed
    `e`'s context, `prevₖ` might not have an index yet. `shep-raft`
    assigns an index in the same step that validates and appends, so the
    leader can usually name `prevₖ`'s dot; the flag stays in the format,
    and stays sound, until the void-entry re-check (§18) settles what the
    atomic step removes. Then the context carries the flag instead of
    the dot, and readers expand it to `{p: i−1}`, where `p` is `e`'s
    partition and `i` its index. That over-approximates (every earlier
    entry of `p`), which costs nothing because `p` is applied in order:
    closing the version just folds in `cf(p, i−1)` (§9.7). After
    expansion a context is a plain DVV, still causally closed.
- **What it captures.** Causality across partitions and across app round
  trips. If an app reads from partition B and then writes to partition A,
  the write's context includes the B index it read.
- **A context is causally closed.** It's built from `S`, which is closed
  (§9.7), merged with `at_least` and the registry dot, both of which are
  closed versions too. So every dot an entry's context names has its own
  context inside it. That's the hypothesis `wf_stateAt_closed` and the
  causal frontier `cf` both rely on (`Stamps.lean`, `Closure.lean`). It
  didn't hold in the Postgres-era design, where a context named only
  what a mutation read (`MultiNode_closure`).
- **Why it's sound.** A mutation's delta depends only on what it read, and
  validation (§10.3) guarantees that every read it made in a partition it
  validates was still current at its commit. So its context
  over-approximates its causal past.
- **Every causally closed version is a valid state.** A state is valid when
  it is a set, holds at most one value per field, and is tree-shaped. Three
  conditions make every causally closed version's state valid. This is
  proven in `formal/lean` (`wf_stateAt_closed`), over abstract dots, each
  with a delta and a context, so it applies unchanged with one dot per
  two-phase commit:
  1. **Each dot's delta is a valid transition from the state at its
     context.** Validation provides this for a single-partition
     mutation. For a two-phase commit, the delta is the union of its
     participants' parts, and each participant validated its part against
     its own read version, which the decision's context contains (§8).
  2. **Every fact a dot writes lives in a partition that validated it:** the
     mutation's partition, or one of a two-phase commit's participants.
     Facts change partitions only by a split, whose entries order the
     parent's writes before the children's (§9.6).
  3. **Concurrent dots never write the same fact key, or one key and a key
     under it.** Validation provides this: a mutation reads every fact it
     overwrites, since its delta carries the old value, so its read set
     covers its write set, and two concurrent writes to one key can't both
     validate (§10.3). A field operation resolved at the leader doesn't
     read its target; instead the leader puts the key's previous writer in
     the entry's context (§8), which orders the two just as well. A prepared two-phase commit holds intents on its
     keys until it's decided (§8), so it can't race either. So writes to
     one key, whether a field, an index entry, or an edge, are causally
     ordered. Freshness checks rely on this too: a key's newest version is
     the last write to it (§11.7).

  Without condition 3, two concurrent `false → true` changes to one field
  leave a weight of −1 on `false` (`concurrent_same_field_not_set`).
- **Atomicity of two-phase commits follows.** A transaction is one dot. A
  causally closed version holds that dot or doesn't, so it holds all of a
  transaction's writes or none of them. No separate rule is needed.

### 9.6 Partitions, placement, and deployment

- **Partitions are ranges of a hash space of placement groups.** Which
  partition holds a node is placement's business (§3), and placement is
  computed from the node's key, with no lookup.
  - **Placement groups.** By default, a node is its own group, routed by
    a hash of its type and key. `constraint shep::placement on
    (.channel)` puts a message in its channel's group, and
    `constraint shep::placement on (.repo)` puts commits with equal `repo` in one group (§4.1.1). Data that is written
    together should share a group, so most mutations touch one partition.
  - **A group is the root of its placement chain.** A message placed with
    its channel, whose channel is placed with its workspace, is in the
    workspace's group.
  - **The key carries the route.** Every node's partition is computed from
    its key alone:
    - **Placed by its own key** (the default): the group hash is a hash of
      the type and key.
    - **Placed by a key field** (`'repo'` on a type with `shep::key on
      ((.repo, .sha))`): the hash of that field. Placement may name a
      field only if it's part of the key, so placeholders and app keys
      route without anything else.
    - **Placed with another node** (`'channel'`): only types keyed by
      the generated `id` may do this. Shep generates the key
      with the group's routing hash in its leading random bits, so the key
      still looks and behaves like any UUID v7, and still says nothing an
      app should read. A message's key then routes to its workspace's
      group.
  - **Placement is fixed.** The link or field it names is frozen once the
    node exists, it names only the schema's own links, so one team's
    placement never depends on another team's keys, and changing
    `shep::placement` on a type that has nodes is rejected (§4.1.1).
  - **The partition map** maps ranges of the group hash space to
    partitions. It lives in the WAL controller's state (below), and front
    ends cache it. A WAL leader rejects a proposal for a key range it
    doesn't hold with an internal `moved`, and the front end refreshes its
    map and retries. Client libraries never see `moved`.
  - **One partition until it's needed.** A small universe has one data
    partition, and every mutation is local.
  - **At most 1024 data partitions per universe,** a constant for now.
    The controller's leader checks the cap before planning a split or a
    create, and the apply step checks it again, deterministically, on
    every node, so a command that would exceed it is refused with a clear
    error (`partition_limit`) rather than half-applied. A split that
    would exceed the cap is refused the same way. The apply step also
    rejects empty or duplicate replica sets.
  - **Partition IDs come only from the controller.** On a
    controller-managed node (one configured with seeds), every partition
    ID comes from the controller's `next_partition`, and creating or
    initializing a partition by hand is refused with
    `ControllerManaged`. Otherwise a hand-made partition could take an ID
    the controller later allocates, and a new universe would inherit its
    data. Only a **standalone WAL node**, configured with no seeds and no
    controller, creates partitions by hand; it exists for WAL-only tests
    and tools that exercise one partition's log without the rest of
    Shep.
- **Splitting, by size and load.** A partition splits when it grows large
  or hot, as CockroachDB's load-based splitting does.
  - **Only at group boundaries.** A split point is a group hash, so a group
    is always in exactly one partition. A giant group, such as `#general`
    or a monorepo, stays in one partition: its write rate is capped at one
    Raft group's, which is a documented limit (§19).
  - **No merges, and no splits inside a group, in v1.** Both are deferred
    (open question 20). Without them, a key's partition changes only by
    a split at a group boundary, and versions need only the lineage rule
    below.
  - **Versions across a split.** The controller records each split's
    lineage in its partition table.
    A version naming a retired parent `p` at prefix `n` means the same as
    naming it in its children: everything up to `min(n, i)` of the parent,
    where `i` is the split entry, which the children's state includes from
    their first entry. So `at_least {p: n}` is satisfied by either child's
    materializer. A causal token's retired entries fold into its
    children's when it's compacted; a result version's fold only once its
    prefix reaches `i`, since folding earlier would grow it past what the
    result reflects (`fold_exact`, `fold_grows` in `Lineage.lean`). Each
    partition has exactly one parent. A dot `(p, j)` of a retired parent is in `V` iff
    `j ≤ V[p]`, `(p, j)` is one of `V`'s extras, or `V` names any
    descendant of `p` (a child, or a child's child, at any prefix,
    including 0) and `j ≤ i`. Naming a child at prefix 0 therefore means
    `{p: i}`: the child's state starts there.

  **Protocol: a split.** Redpanda never splits partitions, so this
  protocol is Shep's own, written as controller commands around the
  parent's log (the WAL controller is below). The **partition balancer**
  runs on the controller's leader and decides splits from the load and
  size that materializers report. Like Redpanda's
  `partition_balancer_backend` (its `per_term_state`), it keeps its
  planning state per controller term and discards it when the term
  changes, and it submits commands fenced by that term, so two leaders
  never plan from each other's state.
  - **`p`'s log decides.** The controller plans and cleans up, but
    whether the split happens is decided by `p`'s own log: whichever of
    a `split_aborted` entry and the split entry comes first there wins.
    Every request between the controller and `p` names the split's
    revision, so a stale request can't act on a different split.
  - **Routing comes from the split entry,** not from the controller, so
    writes route to the children as soon as the split entry commits.

  1. **Start.** The balancer appends a `start_split` command to the
     controller log: the parent `p`, the split point, two new partition
     IDs, and their replica sets. Every node applies it to its partition
     table, and the reconcilers on the children's replica nodes create
     their Raft groups, with no entries yet. The command's controller
     offset is the split's revision.
  2. **Drain.** The controller's leader asks `p`'s leader to split, naming
     the revision. The leader rejects new prepares with a retryable
     `splitting`, and waits until every prepared two-phase commit it holds
     is released. Two-phase commits that touch `p` wait and retry;
     single-partition proposals keep committing. The drain has no time
     bound, because a transaction's completion depends on its home's
     availability (§8).
  3. **Split entry, or abort.** Either:
     - `p`'s leader appends the **split entry** at index `i`, in `p`'s own
       log, naming the children, their ranges, and the split's revision.
       Its causal past contains everything `p` applied, including the
       decision dots of released two-phase commit parts, because each
       release's context carries its decision dot (§8), so a child's
       version `{p: i}` includes them. From then on `p` answers every
       proposal with `moved`, and the `moved` reply carries the split
       entry itself (the children, their ranges, and the revision), so a
       front end routes to the right child at once, without asking the
       controller. Proposals in flight that didn't make it in before `i`
       are rejected and re-executed against a child.
     - or, if the balancer wants to cancel, `p`'s leader appends a
       **`split_aborted`** entry naming the revision, and reports it. Only
       then does the balancer append `cancel_split` to the controller log.
       If `p` reports that its split entry came first, the balancer must
       finish the split instead.
  4. **Handoff.** `p`'s materializer writes a SlateDB checkpoint at `i`.
     Each child's materializer opens a clone of it, which shares the
     SSTs, ignores keys outside its range, and drops them at compaction.
     The clone's keys keep the parent's generation, and the child writes
     at its own, larger generation, so its writes sort as newer (§3).
     - **Who sends it.** `p`'s leader sends each child its **split
       handoff**, and so does any later leader of `p` that finds a
       committed split entry in its log, so a crash right after the
       split entry doesn't strand the children.
     - **Idempotent.** The handoff becomes the child's first entry, with
       context `{p: i}`. It's keyed by the split's revision, and a child
       that already has it ignores a second copy.
     - **What it holds:**
       - a reference to an object-store snapshot of `p`'s idempotency
         table at `i` (§10.3), which the child installs before accepting
         proposals; it may be filtered to the keys whose anchor groups
         the child holds
       - the parent's unforgotten two-phase commit decisions, each handed
         to the child holding its home's lowest placement group (§8)
       - the released transaction IDs, until their deadlines (§8)
       - the latest registry dot, and the retention floor (§10.4)
       - an empty validation table: a child answers `too_old` to every
         proposal whose read version doesn't contain `(p, i)`, and to any
         older than its own window

     A child's leader accepts proposals once that entry commits.
  5. **Finish.** Once both children's first entries commit, the balancer
     appends `finish_split`. It only retires and cleans up: every node's
     partition table records the lineage `p → (children, i)` for readers
     (§9.7) and marks `p` retired, and the reconcilers then remove `p`'s
     replicas, once its log is archived (§10.4). Front ends that refresh
     their maps learn the children from it, but routing never waited for
     it. A registry barrier sent to `p` after its split entry is delivered
     again to each child.

  A crash at any step leaves the split's state in the controller log and
  `p`'s log; a new controller leader finds the split in progress in its
  partition table and resumes it, starting a fresh balancer state for its
  term, and asks `p` which entry came first.
- **Moves.** Moving a partition's WAL replicas is two controller commands
  around a Raft membership change, as in Redpanda: `move_partition_replicas`
  names the new replica set, the reconcilers on the new nodes create
  replicas, the partition's leader runs joint consensus in the
  partition's own Raft group, and `finish_moving_partition_replicas`
  records the result. Only the finish command lets the reconcilers remove
  the old replicas. Until the finish, a move can be cancelled
  (`cancel_moving_partition_replicas` in Redpanda), which reverts the
  replica set through the same joint consensus. A replica's identity
  includes the revision of the command that created it, so a node
  removed and re-added is a new member, never a stale one (Redpanda's
  `log_revision`). A partition's replication factor changes the same
  way, by a move to a larger or smaller replica set. Moving a leader is a
  leadership transfer. Neither changes dots or versions. Moving or
  adding a materializer means opening the partition's SlateDB from its
  manifest in the object store and tailing the log, with no copying
  between nodes.
- **A mutation that writes several partitions** uses two-phase commit
  (§8). Placement keeps it rare, admission prices it higher (§15), and
  `shep ops check` warns about any operation that needs it, with its cost
  (§7.1). Whether an operation needs it is decided by placement groups at
  deploy, never by which partitions hold them at the moment (§10.3).
- **Deployment.** Two things vary independently: the **topology** (one
  process on one node, or roles spread over many processes and nodes)
  and the **implementation variants** of the enums that have more than
  one (the transport: in-process or shep-rpc; the object store: memory,
  local directory, or S3; the token verifier: real or test, §6.3) (§17).
  - **One node, one binary.** `shep-node` can be built with every
    service in it (WAL and controller, materializers, search indexers,
    view maintainers, front ends, files, and extraction), and run on a
    single server with replication factor 1. This is a supported,
    production-shaped deployment, not a test mode: "it should be possible
    to run a real Shep service using a single server and binary, just
    losing replication and fault tolerance." It has two uses:
    - **Production on one server,** for small installations: the
      in-process transport between components, the real token verifier,
      and a real object store, a local directory or S3.
    - **Tests and local runs,** with the in-memory object store and the
      test token verifier. The test harness gives each placement group
      its own partition, assigns groups to partitions at random, and
      injects a little apply lag, so two-phase commit, cross-partition
      reads, and lagging views all run in ordinary tests (§14.3).
  - **What stays the same on one node:** the protocols (every partition
    is still a Raft group with its own log, validation, two-phase commit
    across partitions, the controller and its commands, splits,
    barriers), the APIs, and every consistency guarantee. Code paths
    don't branch on node count.
  - **What it loses:** fault tolerance and availability. A restart or a
    crash is downtime.
  - **Its recovery point.** Losing the server's disk loses every
    committed entry since the last archived segment (§10.4), and
    everything, if the object store is on the same disk. So on one node
    the WAL seals and archives segments by age as well as size, every
    second by default (configurable), which bounds the loss to about
    that much with an object store elsewhere (§19).
  - **Files on one node.** With a local-directory object store there are
    no presigned URLs, so uploads and downloads use proxy mode through
    Shep (§16.3, §16.4).
  - **Dev and test universes.** A single-node install can host its own
    dev and test universes on the same node, next to production. Their
    bases come from the production universe's registry, so they read
    app and schema names from it, as on the shared non-production fleet
    (§14.1).
  - **Growing.** A single node grows by joining nodes to the controller.
    Going from one voter to three directly keeps the controller
    available; a second node alone should join as a learner until the
    third arrives. A partition's replication factor then rises through
    `move_partition_replicas` to a larger replica set (above).
  - **What assumes more than one node, and how it degrades at RF 1.**
    Each degrades correctly: the quorum checks become trivially true,
    and splits on one node keep every guarantee.
    - **Quorums of one.** Every Raft group, the controller's included,
      has one voter, so an append commits on the leader's own fsync, and
      a leadership check (`not_leader`, the anchor's fresh quorum check
      in §10.3) is trivially true.
    - **The controller is a group of one,** founded from a one-entry seed
      list. Joins still work.
    - **Moves need a second node.** With one node, the balancer has
      nowhere to move replicas, and only moves replicas between cores
      (§9.9).
    - **Splits still happen,** with both children on the same node.
    - **Election timing** is moot with one voter: there are no elections
      after startup, and the timing defaults below matter only with more
      nodes.
    - **The authorization horizon** is trivially current, since every
      partition is local (§6.3). With no front ends outside the node, the
      leader-lease wait for evidence is skipped. Front ends on other
      machines keep the full rule, and drop their evidence when the WAL
      node restarts, which they detect from the incarnation ID in every
      fetch response (§10.4).
    - **Materializer replicas** for a hot partition still help with read
      load, but share the server's disk and CPU.
  - **Distributed:**
    - a **WAL cluster**: `shep-raft` nodes, thread-per-core (§3), each
      hosting a replica of the WAL controller (below) on core 0 and many
      partitions' Raft groups, three replicas per partition, reached over
      Shep's RPC (§17). Readers tail committed entries with long-poll
      fetches from any replica.
      - **Two levels of placement,** as in Redpanda: the controller
        assigns each replica to a node, and only to a node; each node
        assigns its replicas to cores, as Redpanda's node-local core assignment does
        with its `shard_balancer` and `shard_placement_table`, whose
        entries move through `receiving`, `hosted`, and `obsolete`.
        Moving a replica between cores is local to the node and never
        goes through the controller log. A request that lands on the
        wrong core hops once (§3).
      - **Timing.** `shep-raft` batches heartbeats per node pair, apart
        from appends, so a slow fsync (macOS's `F_FULLFSYNC`) delays
        appends' acknowledgements but not heartbeats. Defaults, as in
        etcd: a 100 ms heartbeat and 1–2 s election timeouts. The other timers sit above these:
        the proposal timeout (one second) and the revocation bound `B`
        (5 s) exceed a heartbeat by far, and `B` covers an election plus
        clock drift, so one leader change doesn't break the bound.
    - **materializers**, any number, each serving a set of partitions.
      A hot partition can have several materializer replicas: each is an
      independent SlateDB database applying the same log.
    - **search indexers** (§16.6) and **view maintainers** (§12)
    - **front ends**: sessions, admission control, planning, privacy,
      mutation execution, and live-query topics (§13)

  **What each component holds.** The WAL holds the only state that can't
  be rebuilt. Materializers, view maintainers, and search indexers hold
  derived state, rebuildable from the log and their copies in the object
  store. Front ends hold soft state only: sessions, live-query topics, and
  caches of the registry and of the controller's state, all rebuilt on
  restart or reconnect (§13.3). Front ends scale horizontally behind a
  load balancer.

  **The WAL controller.** The WAL service has one Raft group of its own,
  the **controller**, with its own log, in the model of Redpanda's
  controller (`raft0`, the `redpanda/controller/0` partition). It's the
  core of Shep, so it's isolated from every universe's data.
  - **Every WAL node is a voting member,** and every node holds the
    controller's whole state, as every Redpanda broker does.
  - **Its log holds typed commands:** node join and decommission,
    universe create and drop, partition create, `start_split`,
    `cancel_split`, `finish_split`, `move_partition_replicas`,
    `cancel_moving_partition_replicas`,
    `finish_moving_partition_replicas`, and feature activations. Redpanda's
    equivalents are `add_node_cmd`, `decommission_node_cmd`,
    `create_topic_cmd`, `create_partition_cmd`,
    `move_partition_replicas_cmd`, and
    `finish_moving_partition_replicas_cmd` (`cluster/commands.h`).
  - **Every node rebuilds the desired state from the log:** a members
    table (WAL nodes and their state) and a partition table (each
    universe's partitions, ranges, generations, replica sets, split
    lineage, and in-progress splits and moves), as Redpanda's
    `members_table` and `topic_table` do.
  - **Controller offsets are revisions.** Each command's offset in the
    controller log is its revision, the epoch every derived decision
    carries, as Redpanda's `topic_table` tracks
    `_last_applied_revision_id`. A front end's cached map is at some
    revision, and a replica is created at one.
  - **A reconciler on every node** compares the desired state with the
    replicas it hosts. It creates and removes local Raft groups, and
    drives Raft membership changes for the partitions whose leaders it
    hosts, as Redpanda's `controller_backend` reconciles each partition.
    Commands come in start and finish pairs, and only a finish command
    removes old replicas, so a crash between them leaves the old replicas
    serving. §9.9 describes reconciliation in more detail.
  - **A feature table,** as in Redpanda: the controller records which
    versions every member supports, and a new command or log-entry
    variant is emitted only once every member supports it, so a rolling
    upgrade never sends a node something it can't read.
  - **Cluster identity.** Founding writes a cluster UUID, persisted on
    every node and checked in a hello exchange at the service layer
    whenever a shep-rpc connection opens (the framing itself has no
    handshake), so a production
    node and a non-production node can never join each other's
    clusters.
  - **Front ends cache it.** They follow the controller's state from any
    WAL node and keep a read-only copy, as Redpanda's `metadata_cache`
    does on every shard: the universe directory, each universe's
    partition map, and replica locations. A WAL leader that gets a
    proposal for a range it no longer holds answers `moved`: after a
    split, with the split entry, which routes the retry directly (above);
    otherwise with the current revision, and the front end catches up to
    it and retries.
  - **When the controller is unavailable,** for example without a
    quorum of its members, the data path doesn't stop: data partitions,
    reads, live queries, and two-phase commits continue, and front ends
    route with their cached maps. What waits for the controller:
    splits, moves, universe create and drop, joins, re-delivering
    barriers to new children, and resolving `moved` from a partition
    whose split has finished and been cleaned up. On restart, a node's
    reconciler opens the replicas its locally applied controller state
    assigns it, and removes a replica only on a finish command committed
    in its local log.

- **Universes are a fixed kind of the controller,** like a Redpanda topic
  whose layout isn't configurable. Each universe is a cell: a meta
  partition group (its registry, policies, registry barriers, and
  published frontier) and its data partition groups, with nothing shared
  with other universes in the core. The controller holds which universes
  exist, with their immutable IDs and names, and where their partitions
  are; the meta partition holds only the universe's registry and what
  goes with it. Implementation starts with a single universe, and
  production runs one; the non-production fleet hosts many (§14.3).

  **Protocol: bootstrap.** Every WAL node hosts a controller replica, so
  there's no separate system partition on seed nodes, as earlier
  revisions had.
  - **Founding.** A static seed list in each node's configuration names
    the founding nodes. A cluster is founded only when every seed is
    reachable and empty: then the seeds agree on the list and form the
    controller group with it as the initial Raft configuration, as
    Redpanda's `cluster_discovery` does with its founding brokers
    (`founding_brokers`, `is_cluster_founder`). An empty node that any
    seed reports as already part of a cluster joins it instead of
    founding. The seed list is used for nothing else.
  - **Joining.** A new node contacts any node it can reach, seeds first,
    and asks the controller's leader to admit it. The leader appends a
    node-join command and adds the node to the controller group through
    Raft joint consensus, in the controller's own log, as Redpanda's
    `members_manager::handle_join_request` and `members_backend` do with
    `add_node_cmd`. Decommission works the same
    way in reverse. Replacing every seed needs no configuration rollout:
    later membership lives in the controller log, and seeds matter only
    to founding and as a first contact.
  - **No circularity.** The `Bootstrap_selfmember` counterexample was a
    cold start that needed the system partition to find the system
    partition. Here, a restarting member reads the controller's
    configuration and state from its own local log, with no lookup. A
    node that isn't a member yet needs only one reachable member address
    to join, and the static seed list supplies it. Initial membership
    comes from static configuration, and every later change goes through
    the controller's own consensus, as in Redpanda.
  - **Then the universes.** A node that starts replays its controller
    log, rebuilds the members and partition tables, and its reconciler
    opens the replicas it should host: each universe's meta partition and
    data partitions.

### 9.7 Causally closed versions and the frontier

Everything that reads more than one partition reads at a causally closed
version. There is one rule, and one way to compute it.

- **Closing a version.** Start from the indexes to read, at least the
  request's `at_least`, and repeat until nothing changes: add the causal
  frontier `cf(p, V[p])` of every partition in `V` (§9.4) and the context
  of each of `V`'s extras, and wait for each named partition's
  materializer to apply what's added. Adding `cf` alone is exact only from
  a start that's already covered, such as a prefix or a closed token
  (`stepDVV_eq`, `iterate_stepPre`, and `stepPre_unsound` in
  `Closure.lean`).
  - **It terminates.** Every dot a context names is committed, and the
    loop only adds dots, up to what's committed when it runs.
  - **Point reads** by front ends close their versions this way.
- **In-doubt two-phase commits.** A prepared part becomes visible only
  at its release entry (§8). A materializer serving `V` that has applied
  a prepare, but not its release, first applies its own partition's log up
  to the commit index. If the release is there, the part is applied at it.
  If not, it asks the home whether the transaction was decided at a dot in
  `V`. If it was, the read waits for the release; if not, the
  transaction isn't in `V`: a decision appended later has a dot no
  existing version contains.
- **The frontier, a convenience.** Derived consumers (view maintainers,
  search indexers, live-query topics, `changes`, and the baselines of
  freshness checks) step from one causally closed version to the next.
  Each could close versions itself. To spare them the work, the meta
  partition's leader publishes a recent causally closed version, the
  **frontier**, into the meta partition every tick (10 ms by default,
  only when something advanced).
  - **How it's computed.** Each materializer reports, for its partitions,
    its applied index and the causal frontier's changes since its last
    report. Per partition, the leader uses the highest applied index any
    replica has reported. It takes every such entry and drops,
    repeatedly, any whose context isn't covered by what remains. What's
    left is the largest causally closed set of applied entries, as a DVV:
    usually a prefix per partition, with extras where one partition's
    entries got ahead. A per-partition maximum without this step isn't
    closed, and a view stepping to it gets weight −1
    (`FrontierMaterialize_maxfrontier`, `_maxview`).
  - **It never goes backwards.** The leader publishes the union of the
    previous frontier and that closed set. Applied indexes can move back:
    a replica lags, or a crash loses unflushed batches it had reported.
    Without the union, the frontier moves backwards, and a view stepping
    to it keeps dots its version lacks (`FrontierMaterialize_nokeep`,
    `_nokeepview`). A consumer at a frontier ahead of a restarted
    materializer waits for it to re-apply.
  - **A frontier can hold a decision before its releases.** A frontier
    can contain a two-phase commit's decision whose participants haven't
    applied, or even appended, their releases. Consumers resolve those
    prepares as in doubt (above, `FrontierMaterialize_noindoubt`); that
    terminates because the participant timeout appends the release (§8,
    `FrontierMaterialize_livenorel`).
  - **Consumers step between frontiers.** A consumer at version `F₁`
    moving to `F₂` takes the entries whose dots are in `F₂` but not in
    `F₁`, filtered by footprint (§10.1), and reads the materializers at
    `F₂`. It reads the logs of only the partitions its footprint touches;
    it needn't tail the others, because the frontier already accounts for
    them. That removes the old rule that every maintainer tails every
    partition (`ViewMaintainer_untailed`).
  - **A stalled partition holds the frontier back** only where its
    entries are named by others' contexts. Frontier lag is reported per
    partition (§15.1).
  - **It's not authoritative.** Losing the meta leader loses no data: a
    new leader resumes from the last published frontier, and any
    consumer can close versions without it.

### 9.8 What this replaces

The first implementation (milestone 1) runs on Postgres. It orders a
universe's commits with an end-of-transaction advisory lock and a global
`seq`, which caps each universe's write throughput. A later revision of
this design kept Postgres and replaced the lock with per-node counters and
dotted version vectors. This revision replaces Postgres itself with the
WAL and materializers, and keeps the dotted version vectors. Milestone 2
makes the move (§18).

### 9.9 WAL internals

The WAL's internal protocols are copied from Redpanda's, adapted where
Shep differs. This is their design; the specs that check them are listed
in §18.

**The `shep-raft` runtime.** `shep-raft` is openraft 0.10's engine and
runtime, forked with targeted changes (§3). Each Raft group runs on its
owning core's `current_thread` runtime. What follows marks which parts
are openraft's as they are, and which the fork changes to Redpanda's
behavior.
- **Pipelined appends (openraft's).** A leader keeps a queue of
  AppendEntries requests in flight to each follower (openraft's
  `InflightAppendQueue`), each identified so that replies match
  requests. A follower rejects a request that arrives out of sequence,
  and the leader rejects a reply that does, so reordering on the wire
  never moves a follower's log backwards.
- **Replies carry two indexes:** the follower's dirty index (appended,
  not yet fsynced) and its flushed index (fsynced). openraft already
  tracks submitted and flushed I/O; the fork carries both in replies.
- **Commit (changed).** The leader commits the highest index a quorum has durably,
  taking each follower's `min(flushed, match)` and its own flushed index,
  and clamping to entries of its current term, as Raft requires. A
  follower's commit index is `min(leader_commit, own flushed)`, so it
  never applies what it hasn't made durable.
- **Group flush (changed).** A follower appends a burst of entries, then fsyncs
  once for all of them; the leader does the same for its own appends.
- **Heartbeats (changed).** openraft runs heartbeats as their own
  command, with a worker per follower. The fork batches them per node
  pair in Shep's network layer: one message carries the lightweight
  heartbeats of every group the two nodes share. A group whose follower
  needs more than a heartbeat (a commit index to advance, a term to
  check, a gap) falls back to a full heartbeat.
- **Elections (openraft's).** Prevote, so a node that was partitioned away doesn't
  disrupt a working leader when it returns, and leader stickiness: a
  follower that has heard from a live leader recently refuses to vote,
  except for a leadership transfer the leader asked for.
- **Follower-admitted recovery (changed).** A follower far behind is caught up in
  bulk, but only when it admits it: each node has a few recovery slots
  (`may_recover`), so a restarted node isn't flooded by every group
  recovering at once.

**Moving a replica between cores.** Each node assigns its replicas to
cores, and moves them between cores without the controller (§9.6), as
Redpanda's `shard_placement_table` does:
1. **Receiving.** The destination core records the replica as
   `receiving`, a persisted marker in its kvstore.
2. **Copy.** The source core stops the group, and its kvstore state (the
   vote, the Raft configuration, and the start offset) is copied to the
   destination core's kvstore. The log segments are files, so they stay
   where they are.
3. **Hosted.** The destination marks the replica `hosted` and starts the
   group; the source marks its copy `obsolete`.
4. **Obsolete** state is deleted from the source core's kvstore.

Each step's marker is persisted, so a crash between steps resumes from
the marker. If the core count changes across a restart, each replica's
markers are merged on its newly assigned core before any group starts.

**Controller reconciliation.** The desired state is a fold of the
controller log: every node applies the same commands in the same order,
and each command's offset is its revision (§9.6).
- **Moves are two-phase:** `move_partition_replicas`, then the
  partition's own joint consensus, then
  `finish_moving_partition_replicas`. A move can be cancelled until the
  finish. Only the finish deletes old replicas.
- **Vnode revisions.** A replica is identified to Raft by `(node,
  revision)`, the revision of the command that created it, so a group
  deleted and recreated on the same node is a new member, and a stale
  replica from before can't vote in the new group.
- **Each node reconciles** its local replicas against the fold:
  creating, starting, and deleting, and, where it hosts the leader,
  driving the membership change.

**Storage.** Each Raft group has its own log on its core's disk:
- **Segments.** A group's log is a sequence of segment files, each named
  `<base offset>-<term>`, rolled on size, on age (§10.4), and on a term
  change, so a segment never spans terms.
- **Batches** carry two CRCs, one over the header and one over the
  payload, so a torn write is detected at either end.
- **Three offsets:** appended ≥ written ≥ flushed. Appended is in memory,
  written has reached the file, and flushed has been fsynced. Only
  flushed counts toward commit.
- **A per-core kvstore** holds each group's vote, its start offset, and a
  clean-shutdown marker. The marker is cleared before any truncation and
  on startup; recovery rescans the last segment, checking CRCs and
  dropping a torn tail, unless the marker is set.
- **Suffix truncation** (a new leader overwriting uncommitted entries):
  delete later segments, truncate the segment that holds the point, then
  fsync.
  - **Readers.** The truncation sets a `truncating` flag in the same step
    that bumps the log's epoch, and clears it once the segment metadata
    matches the files again, on success, error, or cancellation alike. A
    reader that sees the flag waits on the log lock, then restarts; a
    read that began at an older epoch restarts when it sees the epoch
    change.
- **Prefix truncation** (after archiving or a snapshot, §10.4): persist
  the new start offset in the kvstore first, then delete segments below
  it, so a crash never leaves a start offset pointing at deleted data.
  It's atomic for readers: the metadata changes in one step, before any
  whole segment is deleted.
- **The reader invariant.** A read never combines file contents and
  metadata from different sides of a truncation.
- **Snapshots** are written to a temporary file, fsynced, renamed into
  place, and the directory is fsynced, so a crash leaves either the old
  snapshot or the new one.

## 10. The mutation log

Every committed mutation, to data, the registry, or policies, is one entry
in its partition's write-ahead log, at its dot; a two-phase commit is its
decision entry in its home partition, with its parts in its participants'
prepare entries (§8). The log is the source of truth: materializers build stored state from it (§3), and replaying it
rebuilds a universe exactly. The same entries drive change queries (§11),
materialized queries (§12), and search (§16.6).

### 10.1 What an entry records

- **Identity and causality.** The dot, the context (§9.5), and the commit
  time.
- **Kinds of entry.** Most entries are mutations. The others take a
  position in the log but stamp no visible fact (§9.1):
  - two-phase commit records: prepares, which carry a participant's part
    of the delta, decisions, which are the transaction's dot, and intent
    releases (§8)
  - registry barriers (§10.3) and split entries (§9.6)
  - in the meta partition, registry changes and published frontiers
    (§9.7)
  - Raft's own entries, such as a new leader's no-op and membership
    changes, which readers see as Internal
  - void entries, appended under a term other than the one they were
    validated in (§9.1)
  - failure records, which record a failure the client saw as its
    idempotency key's outcome (§10.3)

  Readers of the log (materializers, replay, tailers, search indexers,
  and view maintainers) skip Void, Internal, and Failure entries.
- **Intent metadata, for audit.** App, actor, viewer, the token's `jti`
  and its other claims, schema version,
  the operation set and operation (§7.1), and the idempotency key. Replay
  ignores it.
- **The delta: a Z-set of facts.** Shep's base data is a set of **facts**,
  not wide rows:
  - **Node facts.** `(node, path, value)`, one per leaf value. A top-level
    field's path is just its field ID; nested data has longer paths
    (§10.2).
  - **Lifecycle facts.** `(node, @type)` while a node exists,
    `(node, @deleted_at, t)` while it's deleted, and `(node,
    @placeholder)` while it's a placeholder (§3).
  - **Edge facts.** `(edge type, src, dst, sort_key)`, with at most one
    live fact per `(edge type, src, dst)`. That triple is the link's `l`
    key (§3), which validation protects like any other key, so concurrent
    links of one pair can't both commit (condition 3 in §9.5, §10.3).
    Changing a sort key is `−old`, `+new`.
  - **Registry and policy facts**: schema versions, operation sets, policy
    updates, and shadow promotions.

  A delta is a set of facts with weights `+1` (added) and `−1` (removed):

  | Change | Facts |
  |---|---|
  | create a node | `+(n, @type)`, plus `+1` for each field set |
  | change one field | `−(n, done, false)`, `+(n, done, true)` |
  | delete or restore | `+` or `−` `(n, @deleted_at, t)` |
  | link to a missing key (placeholder) | `+(n, @placeholder)`, plus the edge |
  | fill a placeholder | `−(n, @placeholder)`, `+(n, @type)`, plus `+1` for each field set |
  | link or unlink | `+` or `−` `(Todo.owner, t1, ada, k)` |
- **A fill looks like a creation.** A placeholder has no `@type` fact,
  and readers never saw it, so change queries and live queries see its
  fill as the node being created.
- **Size follows the change, not the node.** Changing one field logs two
  facts, however wide the node is. Queries that don't read that field skip
  the entry by footprint (below) instead of processing an update that
  cancels out.
- **Old values are included.** Every `−1` fact carries the value it
  removes. That lets change queries and views subtract old state (§11.5,
  §12), while replay just sums.
- **Effects, not intents.** Every server-computed value (IDs, timestamps,
  adapter output) is in the delta, so applying it needs no schema, policy,
  or code. That includes field operations such as `increment` (§8): the
  proposal carries the operation, but the leader resolves it before it
  appends, and the entry holds the result in a **resolved writes**
  section, `{key, old, new}`, that the materializer merges with the rest
  of the delta. Replay, change queries, and validation need no leader
  logic.
- **Storage IDs only.** Entries use type, edge, and field IDs, never names.
  The entry format is versioned, and readers support every format ever
  written.
- **A field-level footprint.** Each entry stores its footprint: which node
  types, field paths (as prefixes), edge types, and node IDs it touched.
  Change queries and view maintainers use it to skip entries without
  decoding them.

### 10.2 Nested documents

A node's data can be a full document: nested objects, maps, sets, and lists.
Changes inside it are still Z-set deltas of facts. Maps and sets are
structural, so a change logs only what changed. Ordered lists are atomic
values.

**Two kinds of document fields.**

- **Typed embedded values (preferred).** These are `shep::Embed` types
  and enums, `map<str, T>`, `multi` properties (sets), and `array<T>`,
  nested arbitrarily (§4.1.2). Every struct field has a field ID, so the
  compatibility checker, adapters, filters, and policies work at any depth.
- **Untyped documents (an escape hatch).** A field of type `Document` holds
  arbitrary BSON-like content: objects, arrays, and BSON's scalar types
  (ObjectId, datetime, binary, decimal128, int32, int64, double).
  - **What Shep does with it.** It stores the document and diffs it with
    the same rules as typed values, so deltas and change queries still work
    inside it.
  - **What Shep doesn't do with it.** Shep only checks that the value is a
    document. There are no compatibility guarantees or adapters inside it,
    and filters on paths inside it are dynamically typed.
  - **Key order.** Object key order isn't preserved, as with JSON. An app
    that needs exact BSON bytes stores a `Bytes` field instead.

**Paths and facts.** Each leaf is one fact, `(node, path, value)`.

- **Path segments.** A path is a sequence of segments, and each segment is
  either:
  - **a struct field ID**, for typed embedded structs, or
  - **a map key**, for typed maps and for object keys inside a `Document`.
    Adding or removing an entry is `±1` on the facts under that key.
- **Sets.** A set path holds one fact per member, `(node, path, member)`,
  where the member is the fact's value. Adding or removing a member is a
  single `+1` or `−1`.
- **Containers record that they exist.** Every map, set, struct value, and
  `Document` object that exists has a presence fact,
  `(node, path ++ [@], ())`.
  - **Why.** Without these, `Some({})`, an empty set, and an absent field
    all encode to no facts, and replay couldn't tell them apart
    (`encodeDoc_empty_containers` in `formal/lean`).
  - **Lifecycle.** Creating a container adds its presence fact, and
    removing the subtree removes it.
  - **No prefix clash.** The reserved `@` segment keeps a presence fact
    from being a prefix of the container's leaves.

**Ordered lists are atomic leaf values.** A `Vec<T>`, or an array inside a
`Document`, is a single leaf value at its path. Changing it in any way is
`−(node, path, old list)`, `+(node, path, new list)`. There are no element
IDs and no order keys. That is correct and simple, and cheap enough because
node documents are size-capped.

- **Large or frequently edited ordered collections are edges.** Edges
  already have sort keys, pagination, per-node privacy, and independent
  writes, and they're the design's answer for collections that grow
  without bound anyway.
  - **If sort keys are ever generated between neighbors** (for example, to
    reorder), string keys must never end in the minimum digit. Otherwise
    some pairs have no key between them, such as `"1"` and `"10"`. Equal keys
    leave no room at all, so one of them has to be re-keyed first.
- **Fine-grained sequences, if they're ever needed.** They would be a
  separate type with stable element IDs.
  - **No CRDT merge needed.** Writes to one node are already serialized in
    its partition's log, so the type wouldn't need merge semantics.
  - **One idea worth borrowing.** From RGA, operations anchored on element
    IDs ("insert after e42"), so a client working from a stale read still
    means what it said.

**Operations.**

- **What clients send.** Clients send `set(path, value)`, map
  `insert`/`remove`, set `add`/`remove`, and whole-value replacement for
  lists.
- **Every operation is diffed against the snapshot.**
  - **What gets logged.** Replacing a whole map or struct logs only what
    changed. Adding a set member that's already there, or removing one that
    isn't, logs nothing. Replacing a value with one of a different shape
    logs `−1` for the old leaves and `+1` for the new ones.
  - **What this guarantees.** Every state keeps three properties: weights
    of 0 or 1, at most one value per scalar or list path, and no live leaf
    under another live leaf. Each operation's precondition is proven in
    `formal/lean` (`wf_*`).
- **Removing a subtree.** This is `−1` for every leaf under the path, so its
  cost is proportional to the subtree's size.

**Storage.** A materializer stores each top-level field's value whole, under
its own key, encoded by field ID at every struct level (§3). Map and set
changes apply as path updates to that value. Lists are replaced whole.

**Limits.**

- **Size.** Every top-level field's whole value, including everything
  nested under it, is capped in Shep's canonical encoding: 64 KiB by
  default, up to 1 MiB with `@max_size`. A node's fields together are
  capped at 4 MiB (§16.1). An `insert` or `push` that would push a map,
  set, list, or `Document` past its cap fails with `too_large`, like any
  other write; for a `push` resolved at the leader, the leader checks it
  (§8). Collections that grow without bound belong in separate
  nodes connected by edges, and large content in files (§16).
- **Concurrency is per top-level field.** Two mutations that change
  different top-level fields of one node don't conflict, because each
  reads and writes only its own field's key (§10.3). Two that change
  different parts of one map or struct field do conflict, and one retries.
  Facts make the logged deltas minimal; they don't make validation finer
  than a field.

### 10.3 Writing

A mutation is executed by a front end and committed by its partition's
WAL leader. The leader runs no application code: it checks key ranges,
resolves field operations and their single-field bounds (§8), and
appends. It never reads the delta's format: the delta is opaque to it. Execution, privacy, and adapters all run in front
ends, so a slow mutation never holds up a partition's log.

**Which reads are validated.** Every read that feeds a decision is
validated at commit, wherever it is, except the reads the planner proves
can't change the outcome (below):

- **Decided by placement group, at deploy.** For each operation, the
  planner knows which placement groups it writes and which it reads
  (§9.6). Reads in a written group are validated by that group's
  partition. Reads in other groups that feed a guard (`assert`, `if`),
  a write policy, a freshness check (§11.7), an `observe` clause
  (§11.3), or a value the mutation writes or a write's target make those
  groups' partitions **read-only participants** of a two-phase commit
  (§8). `shep ops check` names every operation that needs one, with its
  cost.
- **Pruned by consistency class.** The planner leaves out reads it proves
  can't change the outcome (§7.6): an `assert` whose condition is stable,
  an `if` over unchanging facts, data flow from unchanging facts,
  target-lifecycle checks on `link` and `create` that can't create a
  placeholder, the unchanging facts of a freshness check, and the targets
  of field operations resolved at the leader. Write policies are
  validated like any other decision (§6.3).
- **Classes follow data flow.** A read's class is the strictest that any
  consumer it reaches through data flow needs, so a read that skipped
  validation can't flow into anything that needs it (§7.6).
- **Never by current partitions.** Two groups in one partition today may
  be in two after a split, so the plan depends only on groups. That's
  also why tests and production behave alike (§9.6).
- **What isn't validated.** Reads that only shape what a mutation returns
  are snapshot reads at the mutation's read version. The actor's own
  lifecycle and key are checked against the authorization horizon instead
  (§6.3). A `claim`'s candidate range isn't validated either (below).

**Protocol: proposing, validating, committing.**

1. **Read.** The front end reads what the mutation needs from
   materializers, at a causally closed version `S` (§9.7) that contains its
   actor handle's `at_least`. It evaluates the operations' reads and
   guards, including freshness guards (§11.7), and stages their writes:
   validation, policies, and adapters run here, against the staged state,
   under the registry and policies at `S`.
2. **Build the proposal.** For each partition it writes or validates, the
   front end produces:
   - the **delta**: `±1` facts with old values (§10.1), opaque to the
     leader. Front ends check it against declared disciplines (§4.1.2)
     under the registry at `S`.
   - for field operations (§8), **op records** and **value records**:
     - an op record per field operation: `{key, op, base_value,
       base_stamp, bound?}`, where `op` is `add n`, `sub n`, `max v`,
       `push x`, or `extend xs`, `base_value` and `base_stamp` are the
       field's value and stamp at `S`, and `bound` is the field's
       recognized single-field check, copied from the schema
     - a value record `{key, new_value}` for every plain write to a key
       that field operations may target, and `{key, absent}` for each
       such key the mutation unsets or clears by deleting its node. The
       front end knows which keys those are from the schema. Index entries and ghosts aren't in it; the materializer
     derives them (§3).
   - the **read set**: every key it read in that partition, and every
     range it scanned there (index ranges, edge ranges, including ghosts),
     including the keys and ranges a materializer read on its behalf
     inside a pushed-down scan (§6.4). Every fact the delta overwrites is
     in it, because the delta carries its old value; the targets of field
     operations are the exception, since the leader resolves them, and
     op keys are never in a read set.
     **Read and write sets are key ranges** in an abstract conflict
     keyspace, not predicates over facts. The front end encodes facts so
     that every range it read is a key range, and every write lands on
     the keys those ranges cover. For an index range, a write's write set
     includes keys for both `(field, old value)` and `(field, new value)`,
     so a row moving into or out of a scanned range conflicts with it. The
     leader compares key ranges and knows nothing of indexes. A freshness guard's read set is
     everything its check visited, read at `S` and never from a cache
     (§11.7).
   - `S`, its context (§9.5), and intent metadata, including the
     idempotency key. **Every proposal carries one**, including internal
     and system proposals, so re-sending a proposal is always safe. `S`
     never names an entry of this partition at or
     after the position the proposal will get. A two-phase commit part
     whose decision is in `S` but whose release comes later in the log is
     covered by its prepared intents (§8). That's the hypothesis behind
     `serializable` in `Validation.lean`.
3. **Validate and append.** The front end sends the proposal to the
   partition's leader. The leader keeps a **validation table**: the write
   sets of the entries in its log for a recent window, each with its stamp,
   and the intents of prepared two-phase commits (§8). It checks, in this
   order:
   - **Leadership.** A leader that hasn't heard from a quorum for an
     election timeout rejects proposals with `not_leader`. Accepting would
     be safe, since nothing commits without a current-term quorum, but the
     client would learn of it only by timing out. The front end retries
     against the new leader.
   - **Registry.** The partition's latest **registry barrier** names a
     registry dot (below). If it isn't in `S`, the leader rejects with an
     internal `stale_registry`. The front end refreshes its registry to at
     least that dot and re-executes, without the client seeing it.
   - **Idempotency.** If the idempotency key is in the table, the leader
     returns that entry's outcome instead (below). The table holds keys
     from appended entries, committed or not, so a retry racing its
     original is caught before either commits; checking only committed
     entries lets one key commit twice (`WalValidate_idemcommit`). A hit
     on an entry that's appended but not committed waits for it to
     commit, then returns its outcome; if the entry is overwritten or
     made void instead, the waiter gets `not_leader` and the front end
     re-sends. The failure-record path below waits the same way.
   - **Too old.** If `S`'s prefix for this partition is older than the
     table's window, the leader can't check, and rejects with a retryable
     `too_old`. Without that rejection, a proposal from before the window
     commits over a conflict the table no longer holds
     (`WalValidate_notooold`).
   - **Conflict.** If any entry in the window whose stamp is not in `S`
     wrote a key inside one of the read set's key ranges, the leader
     rejects with a retryable `contended`. Ranges matter: checking only
     the keys read lets a phantom commit (`WalValidate_norange`). The same
     goes for an intent of a prepared two-phase commit on any of those
     keys or ranges. With `S` a prefix, "stamp not in `S`" means "after
     `S`"; with extras it's the exact test.
   - **Otherwise** it appends the entry. Its index is the mutation's dot,
     and the materializer stamps the delta with it. Before appending, the
     leader resolves the proposal's field operations into the entry's
     resolved writes (§8). The entry records the term it was validated
     in; if Raft appends it under any other term, it is void (§9.1), and
     the proposer, finding no outcome, re-sends. Its write set enters the
     validation table **at append time**, tagged with that term, so the
     next proposal is checked against it even before it commits; entering
     it at commit lets a phantom commit (`WalValidate_c1`,
     `WalValidate_claim_c1`). The term tag isn't needed for safety,
     because the table is rebuilt on every leader change (below); it only
     makes a void entry's write set easy to drop. The leader validates
     and appends in one step of its core's loop (§3), one proposal at a
     time per partition, in log order; replication to followers is
     pipelined and batched, and validation is a lookup per key and range,
     so it's cheap.
4. **Commit.** The entry commits once a quorum of the partition's replicas
   has it, and the leader replies with the outcome.
5. **Retry.** On `contended` or `too_old`, the front end re-executes the
   mutation from step 1 at a newer version. Mutations are deterministic and
   bounded (§8), so re-executing is safe. A request has a deadline; when
   internal retries run past it, the client gets `overloaded` with
   `retry_after` (§15.3).

- **Timeouts and unknown outcomes.** A front end waits for a reply up to
  the proposal timeout (one second by default). If the leader changed or
  the reply was lost, the outcome is unknown. The front end re-sends the
  same proposal, with the same idempotency key, to the current leader,
  which either finds the key (committed) or validates it afresh. A
  deposed leader can't commit anything: Raft only commits through a
  current-term quorum.
  - **Re-sending isn't re-executing.** A proposal re-sent with the same
    `S` across a slow leader change can legitimately get `too_old`, once
    the new leader's window has moved past `S`. The front end then
    re-executes from step 1 at a newer version; it never keeps re-sending
    the same proposal.
  - **A re-sent proposal keeps its horizon deadline** (§6.3). Past it, the
    leader rejects it with `too_old` and the front end re-executes.
- **Leader changes.** A new leader rebuilds both tables before it
  accepts proposals; until then, proposals get `too_old`. Skipping the
  rebuild lets a conflict commit (`WalValidate_norebuild`).
  - **The validation table** is rebuilt from every entry in its log since
    the window's start, committed or not, and from its prepared
    transactions' intents.
  - **The idempotency table** is rebuilt from the state machine's
    snapshot of it, plus every log entry after the snapshot, committed or
    not. The log alone isn't enough: the idempotency window (24 hours)
    outlives the log, which snapshots and archiving truncate.
  - **Uncommitted entries.** Entries a previous leader appended but never
    committed are either committed by the new leader, as Raft does with
    its own term's no-op, or overwritten, and overwritten entries' write
    sets and keys leave the tables with them.
  - **The window starts no earlier than the log.** After a truncation or
    a snapshot install, a leader's validation window starts no earlier
    than its first log entry, so the rebuilt table never claims coverage
    it doesn't have: a proposal whose `S` has a prefix below that gets
    `too_old`. A leader that rebuilt from a truncated log without this
    rule let a conflict commit (`WalValidate_trunc`; the fix passes,
    `WalValidate_truncfix`).
  - **The last-value map** for field operations (§8) is rebuilt like the
    validation table, from the window floor, including uncommitted
    entries.
- **What's durable where.** The log (Raft replicas, then the archive) is
  durable. The idempotency table is part of the WAL's Raft state machine,
  on disk in the state machine's own store and in its snapshots, for the
  idempotency window. It stores only `key → (dot, failure code?)`: a
  committed key's outcome is re-derived from the entry at that dot, so
  the table stays small over a 24-hour window. The validation table and
  the last-value map are
  memory only, rebuilt from the log.
- **The isolation guarantee.** Committed mutations are serializable, in a
  serial order consistent with each partition's log order: every
  validated read of a mutation was still current at its commit point, and
  every read that feeds a decision is validated unless the planner proves
  it can't change the outcome (above). So each committed mutation's writes
  and `assert` outcomes are what they would be at its position in that
  order. Values a mutation
  only returns are snapshot reads at `S`, and plain reads are causally
  consistent snapshots, not serializable (§9). This is optimistic
  concurrency control with a single validator per partition, as in
  FoundationDB's resolvers, and two-phase commit across partitions (§8).
  - **Phantoms are covered.** Range reads are in the read set, so an
    insert into a range a guard counted (`count(...) < N`) conflicts.
- **The validation table.** It holds write sets for a window of recent
  entries, by time and memory, a few seconds by default. Front ends read
  from materializers that are milliseconds behind, and mutations are
  bounded, so nearly every proposal falls inside the window. A leader
  with a materializer beside it could check older reads against per-key
  stamps instead of rejecting them (open question 21).
- **Idempotency keys.** Every mutation has an **anchor partition**,
  derived from its request arguments (§8), and its idempotency key is
  recorded and checked there, whatever shape the mutation commits in: a
  single-partition entry at the anchor, or a two-phase commit whose home is
  the anchor. The anchor's Raft state machine keeps recent keys, with the
  committed entry's dot or the failure's code, for the idempotency window
  (24 hours by default, longer than any client retries), and includes
  them in its snapshots. A retried mutation whose key is there gets the original
  outcome, even if re-executing it now would touch different partitions.
  A retry that reaches a different front end while the original is still
  in flight is caught the same way.
  - **One outcome per key.** A key's single outcome is a commit or a
    recorded failure.
    - **Retryable rejections don't consume the key.** Attempts rejected
      with `contended`, `too_old`, `stale_registry`, `not_leader`, or
      `splitting`, and two-phase commits that abort, record nothing.
    - **Client-visible failures go through the anchor when the key may
      already have an outcome.** A failure the client would see (a failed
      `assert`, `lease_lost`, an empty claim, `invalid`, `denied`, a
      bound crossed) goes through the anchor when the key may already
      have one: the client marked the request as a retry, or this front
      end sent a proposal for the key whose outcome is unknown. Then the
      front end sends the key and the error to the anchor's leader. If
      the key is in its idempotency table, the leader answers from it,
      after confirming it's still leader with a fresh quorum check, as a
      read-index call does, so a deposed leader can't answer; the
      outcome may be a commit from an earlier attempt. Otherwise it
      appends a **failure record**, which becomes the key's one outcome;
      the append itself needs a current-term quorum, so it needs no
      separate check.
    - **The fast path.** A first attempt that the client didn't mark as a
      retry, and that this front end never proposed, can't have an
      earlier outcome, and its failure is reported directly. Admission
      prices failure records like writes (§15), since each is an append.
      The fast path is pending a re-check in `WalValidate.tla` (§18).
    - **Why.** A front end that reported a re-execution's failure itself
      could tell the client "failed" for a call an earlier attempt had
      committed, through another front end or before a timeout
      (`WalValidate_fail`, `WalValidate_probe`).
    - **Clients accept a failure only from the anchor.** After a retry,
      the client libraries treat a failure as final only when it carries
      the anchor's confirmation; anything else is an unknown outcome, and
      they retry with the same key.
- **Registry barriers.** Every registry change (a new schema head, an
  index definition, a policy deploy or promotion, an operation set's
  activation) is an entry in the meta partition, with a dot.
  - **Protocol.** Once a change commits, the meta partition's leader
    appends a **barrier** entry, naming the change's dot, to every data
    partition of the universe that holds an affected type, including
    partitions split off later (§9.6). A partition's leader rejects
    proposals whose `S` lacks the latest barrier's dot (above), so every
    entry after the barrier was computed under the new registry.
  - **Barriers drain prepares.** A barrier at partition `p` is appended
    only once no prepare at `p` is unreleased. While it waits, `p`
    rejects new prepares with `stale_registry`. So every part a release
    applies after a barrier was validated under it. The decide-time check
    (§8) aborts parts decided after a barrier, but on its own it can't
    stop a barrier landing between a decision and a release
    (`TwoPhase_barrierlog`; `TwoPhase_barrierdrain` passes).
  - **What it fixes.** A front end with a stale registry can't commit a
    write that skips a new head's adapters, a new write policy, or a new
    check. The materializer derives index entries under the definitions
    in force at each entry, so replay reproduces them.
  - **Reads use the registry at their version.** Every read at V, not only
    a mutation's, applies adapters and policies under the registry at V,
    never under a front end's cached registry. So a cutover's lens switch
    happens for every reader at exactly its registry dot (§4.2,
    `Cutover_lenskey`).
  - **Backfill at a cutover.** A new index or search field is built from a
    partition's state at its barrier: the materializer, or the search
    indexer, writes entries for every existing node, stamped with the
    stamp of the field values it read, and maintains it from the barrier
    on. A new head's indexes are built the same way, at an index barrier
    while it's Prepared, and its data backfill is ordinary validated
    mutations during DualWrite (§4.2, schema cutover). The
    operation set that needs the index activates once every partition has
    built it (§7.1).
- **`claim` is resolved at the leader.** Its shape is restricted, so the
  leader can resolve it without application code:
  - **What a claim is.** `claim(query, n)` over one partition's index
    range, chosen when it runs. Its candidate predicate reads only each
    candidate's own keys, Shep makes one independent claiming update per
    node (writing its `@lease`, §8), and the result is the leases. It has
    no `observe`, no other writes, and isn't part of a two-phase commit.
    The checker rejects any other shape.
  - **How it runs.** The proposal names more candidates than `n`, read at
    `S`. The candidate range is left out of the read set. The leader drops
    candidates that any entry whose stamp isn't in `S` wrote, and any held
    by a two-phase commit intent, takes up to `n` of the rest, and appends
    the claiming updates for those. Contended workers then get different
    tasks instead of conflicting (§8).
  - **Retries.** A claim carries an idempotency key like any mutation, so
    one retried after an unknown outcome returns the leases it took the
    first time. For claims this check is the only protection: claims skip
    contended candidates instead of conflicting, so validation never
    catches a duplicate, and without the key check at append a retried
    claim takes a second node (`WalValidate_claim_idem`).
  - **The fence is necessary.** Without the generation check on writes
    under a lease, a stale holder's writes land after the node is claimed
    again (`WalValidate_claim_nofence`).
  - **Lease expiry.** The leader's sweeper appends `@lease_expired` for
    leases whose expiry has passed by its clock plus a skew allowance
    (§8).

The response carries the per-op results, the version token (the context
plus the new dot), and the observed changes when the mutation declares
`observe` (§11.3).

### 10.4 Archive, retention, snapshots, replay

- **The log is archived.** Sealed log segments go to the object store,
  per partition, by index range, checksummed.
  - **Protocol.** A segment is sealed at a size or age boundary (on a
    single node, every second by default, §9.6). The WAL leader uploads
    it, verifies it, and records the archived range in its state
    machine. Only then may the replicas truncate it, and only below the
    lowest index that every materializer and view maintainer of the
    partition has reported applying, or below a Raft snapshot that covers
    it. It never passes the oldest prepare whose release hasn't committed
    (§8).
  - **Progress leases.** Each materializer and view maintainer holds a
    progress lease on the partition, renewed with its reports; an expired
    lease is ignored, so a consumer that went away doesn't hold
    truncation back forever. Other tailers (front ends, search indexers)
    don't hold truncation back at all, and read the archive if they fall
    behind.
  - **Falling behind.** A replica that falls below the truncation point
    installs a Raft snapshot. A materializer or tailer that falls below it
    reads the archived segments, then the replicas.
  - **Truncation and snapshots shrink the validation window.** A leader
    whose log starts at index `f`, after a truncation or a snapshot
    install, starts its window no earlier than `f` and rejects older
    proposals with `too_old` (§10.3, `WalValidate_trunc`,
    `WalValidate_truncfix`). The idempotency table is in the snapshot, so
    it survives truncation.
- **Materialized state has checkpoints.** A SlateDB checkpoint pins a
  partition's state at an applied index, in the object store. Checkpoints
  let a materializer start without replaying the whole log, give
  point-in-time restore its starting points, and seed a split's children
  (§9.6).
- **Fetch.** Every tail is a long-poll fetch,
  `fetch(from_index, max_bytes, max_wait)` (§17).
  - **Each response carries** the partition's `(commit index,
    leader-lease stamp)` from its leader, which followers relay unchanged,
    and the serving node's incarnation ID, which changes when the node
    restarts, so a tailer can tell when evidence from before a restart no
    longer holds (§6.3, §9.6).
  - **Where to fetch.** Tailers on the latency path (materializers, and
    front ends' horizon tails) fetch from the partition's leader, or the
    leader pushes its commit index to them eagerly; fetching from a
    follower is for catching up and bulk reads.
  - `max_wait` is at most `(B − 2ε)/4`, so an idle partition still
    refreshes horizon evidence within the bound (§6.3).
- **Protocol: a materializer.**
  - **Tailing.** It reads committed entries with long-poll fetches,
    `fetch(from_index, max_bytes, max_wait)` as in Kafka's fetch, from any
    replica, not as a Raft learner, so it never slows the quorum. Shep's
    RPC has no server streams (§17), so every tail is a sequence of
    fetches.
    It skips Void, Internal, and Failure entries (§10.1).
  - **Applying.** It applies each entry, stamped, with its derived index
    entries and ghosts and the entry's commit time, in one SlateDB write
    batch, together with `m/applied` and the causal frontier at that
    position. A crash loses at most the batches SlateDB hadn't flushed,
    and it resumes from the last `m/applied` it finds, re-reading the log.
    `m/applied` is never durable ahead of the batch it describes, or a
    crash loses entries for good (`FrontierMaterialize_restart`,
    `_restartread`). Re-applying after a crash is harmless for keys, which
    carry their position.
  - **Reads wait for applies.** A read at `V` waits until every dot of `V`
    in its partitions is applied, and resolves in-doubt two-phase commits
    as §9.7 describes. Every wait has the request's deadline.
  - **One writer per database.** Each materializer replica is its own
    SlateDB database applying the same log, so there's no shared writer
    to fence. A replica that stops is replaced from the latest checkpoint.
- **Retention.** A materializer keeps every version of every key down to a
  retention floor, so it can serve reads at any version above the floor.
  Below it, compaction keeps only each key's newest version, live or ghost
  (§3). Freshness checks need only the newest version's stamp, so they
  work at any age (§11.7).
  - **The floor moves only past readers.** It advances to the oldest
    version that a read, a mutation's `S`, a topic, or a view still uses
    at this materializer, and never past the archived log the hot window
    needs (§11.4).
  - **The floor is a log position.** Per partition, it's the contiguous
    prefix of the oldest version in use, never that version's highest
    dot. A read pins its version when the materializer admits it. A read
    whose version's prefix is below the floor gets `too_old` and retries
    at a newer version (`FrontierMaterialize_floor`).
- **Readers track progress as versions.** Every reader of the log keeps
  one applied index per partition: materializers, view maintainers,
  search indexers, change queries, and replay. Committed indexes are
  contiguous, so a cursor per partition is exact.
- **Replay** applies entries so that the applied set stays causally
  closed: an entry's delta becomes visible once every dot in its context
  is applied, and a two-phase commit's parts become visible with its
  decision dot.
  - **Order doesn't change the result.** Z-set addition commutes, so the
    final state is the same whatever the order.
  - **Causal order keeps intermediate states valid.** A version that
    isn't causally closed can hold a removal without its matching
    addition, which gives a weight of −1. A causally closed version is
    always a valid state (§9.5).
- **Rebuilding and restoring.** A universe's materialized state is
  rebuilt from a checkpoint plus the archived log after it. Point-in-time
  restore within a universe replays to a chosen causally closed version.
- **The replay check from milestone 1 still applies.** Replaying a
  universe's log into an empty materializer for the same universe, with
  its blobs intact, must reproduce it exactly, index entries included.
  That check guards against write paths that bypass the log.
- **Blobs are outside the log.** The log records a file's blob by hash and
  size, never its bytes (§16.7). Blobs are never deleted (§16.2), so
  restoring a universe to any point in time finds every blob its files
  name.
- **Replaying into a different universe restores metadata only.** Each
  universe has its own blobs (§16.2), so a log replayed elsewhere, such as
  into a fresh dev universe to debug a failed test (§14.2), brings back
  nodes, edges, and files, but files come back `Failed` with
  `content_unavailable`.

## 11. Change queries

Change queries answer, precisely and consistently: **what changed in this
query between version V1 and version V2?** Live queries (§13) are built on
them, and that's how most apps use them. Apps that run their own fan-out
reach them directly through `shep::fanout`: `since` (§11.2) and `observe`
(§11.3).

### 11.1 The primitive

`changes(Q, V1, V2)` is `Q(V2) − Q(V1)`, as a Z-set:

- rows that entered the result
- rows that left it
- rows that changed, as `−old` and `+new`

`Q`'s result is a set, so every row in the delta has weight `+1` or `−1`.

It requires `V1 ⊆ V2`, but only to compute `ΔI`. Without containment, the
input delta is `Σ(V2∖V1) − Σ(V1∖V2)`, proven as `state_diff_general`. That
would let a lagging materializer answer instead of waiting. `Q` can be any query the planner can incrementalize
(§11.5): finds, edge traversals, joins along edges, counts, and the
policy-derived audience queries (§6.8).

### 11.2 Standalone: `changes(Q, since)`

`changes(Q, since: V)` returns the delta from `V` to the current snapshot,
plus a version token. That token is the snapshot's version merged with `V`
(§9.5). Apps reach it through a live query's `feed()` (§7.3), which keeps
`V` for them. `shep::fanout` adds `.since(&v)` to start a feed from a
version the app kept, and `shep::fanout::version(&feed)` to read one
(§13.6).

- **Pull-based.** Apps poll on their own schedule.
- **Cheap when nothing changed.** Entries are filtered by footprint before
  any query work happens.
- **`V` must be the caller's result version.** It is the token returned
  with the result, or with the last delta applied to it, never a merged
  token (§9.4).

### 11.3 Observing changes atomically with a mutation

This is for apps that run their own fan-out (§13.6), through
`shep::fanout`. Most apps use live queries instead.

A declared mutation can return `shep::observe(…)` and
`shep::audience(…)` values. Each one is evaluated by the front end as
part of the mutation, after its writes, and its reads join the
mutation's read set, so validation guarantees the result still holds at
the commit (§10.3):

```
# shep/ops/post_observed.edgeql
with
  ch := assert_exists(<Channel>$channel),
  m  := (insert Message { text := <str>$text, channel := ch,
                          author := assert_exists(<users::User>$author) }),
select {
  id       := m.id,
  messages := shep::observe(ops::in_channel(channel := ch)),    # another operation in the set
  audience := shep::audience(m, shep::Op.select, anchor := ch),
};
```

```rust
// The fan-out service runs as the app. It keeps the channel's messages and
// audience at one version: the version of its feed.
let app = db.as_app();
let mut feed = ops::in_channel().channel(&channel_id).feed(&app).await?;
// ...
let out = ops::post_observed().channel(&channel_id).author(&poster_id).text(text)
    .since(&shep::fanout::version(&feed))
    .exec(&app)
    .await?;                                   // a shep::fanout::ObservedResult<post_observed::Row>
apply(&mut messages, &out.value.messages);     // every message change since that version
apply(&mut audience, &out.value.audience);     // audience delta since then, plus the
                                               // message's own residual (§6.8)
shep::fanout::advance(&mut feed, &out.version); // the next poll starts at the commit
out.value.id;                                  // the new message's key
```

- **`observe` changes the return type.** A mutation that uses
  `shep::observe` or `shep::audience` returns
  `shep::fanout::ObservedResult<Row>`, with the row, whose observed
  fields are deltas, and the commit's version, and it requires `.since`.
  Mutations without them return their row directly.
- **One version for everything observed.** Every observed value in a
  mutation reports its change since the same `since`, so an app keeps
  all the state it maintains, results and audiences alike, at one
  version. After applying the observed changes, it moves its feed to the
  commit with `shep::fanout::advance`, or starts a new one with
  `.since(&out.version)`. Otherwise the next poll would deliver the same
  changes again.
- **The author is a parameter here.** The fan-out service runs as the app,
  which has no viewer, so `global viewer` isn't available (§8).
- **Declared, like everything else.** The observed queries are part of the
  mutation's IR. The only per-request input is `since`: the version of the
  result the caller maintains. `shep::fanout` is the one place app code
  holds a result version, and it must never be a merged one (§9.4).
- **The delta and the commit describe the same point.** Nothing can
  commit between them.
- **What can be observed.** Any declared query, called as
  `ops::<name>(…)`, plus `shep::audience(node, op)`, which asks who can
  perform `op` on `node`. An audience comes from the read
  policy compiled to a relation (§6.8). It is a set of viewers: the
  people (or other viewer-type nodes) the node is readable for. For a
  viewer-only policy, who acts for them doesn't change it. For an
  actor-reading one, the audience is computed per actor class (§6.8).
- **`observe` reads are validated wherever they are.** An `observe` that
  reads another placement group makes that group's partition a read-only
  participant of the commit (§10.3), and `shep ops check` shows the cost.
  Placing what an owner observes together, such as a channel with its
  members and messages, keeps it in one partition.
- **Anchored audiences.**
  - **What `anchor :=` returns.** It asks for the delta of the anchor's
    audience since `V`, plus the node's **residual**: the viewers the node's
    own policies add to or remove from the anchor's audience, as of the
    commit.
  - **The residual's two sets are disjoint.** `remove` is the viewers the
    node's own denies (and `contained_in`) match; `add` is the viewers its
    own allows admit, minus `remove`. With no rule order, that's all the
    bookkeeping there is. The node's audience is then exactly
    `(A ∪ add) − remove`.
  - **Why use it.** An app that caches a channel's audience can then
    compute the exact audience of each new message without fetching
    thousands of user IDs every time.
  - **How the split is found.** Shep derives it from the policy's
    `shep::visible` chain, by partial evaluation (§6.4). It rejects an
    anchor the policy doesn't reach through `shep::visible`.

### 11.4 Old tokens

- **Within the hot window, per partition.** A `since` token is within
  the hot window when, on every partition it names, its index is at least
  the retention floor of that partition's materializer (§10.4), so both
  the state at `since` and the entries after it are still at hand.
  - **Inside the window,** the token is answered with a delta.
- **Outside it, the response is a resync**: the full current result of `Q`
  plus a fresh version token. Apps handle resync the same way they handle a
  cold start.
- **Size the window to app needs.** The hot window should cover the
  longest reconnect gap that apps want to answer with a delta instead of a
  reload.
- **"Did it change?" needs no window.** Dots and ghosts (§3) answer
  whether a query's inputs changed since any old `V`, without the hot window
  (§11.7). They can't produce the delta, so beyond the window a feed still
  resyncs, but when nothing changed it skips the resync and keeps its
  result.

### 11.5 How changes are computed (DBSP)

Change queries use DBSP's incremental view maintenance
([Budiu, McSherry, Ryzhyk, Tannen, VLDB 2023](https://arxiv.org/abs/2203.16684)).
Incremental doesn't mean cheap: the work follows the size of the change in
the output, and one membership change can add or remove a large part of a
result.

- **Input delta.** `ΔI` is the sum of the deltas of the log entries whose
  dots are in `V2 \ V1`, filtered by footprint to the facts `Q` reads.
- **Facts are widened per query.** A query reads a projection of each node
  type it touches: the path prefixes it uses, at any depth.
  - **What a column is.** The sub-document under its prefix: a scalar, a
    list (an atomic value), a set's members, or a map's entries.
  - **What's always projected.**
    - `@type` and `@deleted_at`.
    - Every path the query's read policies read, including on types
      reached through `shep::visible`.
    - The lifecycle facts of actors and viewers.
    - For a query from an older version, every storage field its adapter
      chain reads, such as `first` and `last` behind a `name` view. This
      requirement follows from adapter locality, but isn't machine-checked.

    Leave any of these out and deletes, policy changes, or adapter inputs
    would be skipped.
  - **One row pair per node.** First, `ΔI` is summed over the whole window.
    Then, for each node it touches on a projected path, Shep emits `−old
    row` and `+new row`.
    - The old row is read at `V1 = V2 − ΔI`.
    - The new row is the old row with `ΔI`'s facts applied, column by
      column.
  - **Why it's exact.** This is exact for any `ΔI` (`widen_correct` in
    `formal/lean`). If columns are Option-valued ("take the `+` value"),
    exactness relies on V1 and V2 both being valid states, which every
    causally closed version is (§9.5). Widening entry by entry instead of
    once per window would expose intermediate rows.
  - **Skipping.** Facts outside the projected prefixes never reach the
    query. An entry can be skipped by footprint only when none of its
    footprint prefixes is comparable with a projected prefix: equal to it,
    above it, or below it. A footprint *above* a projected prefix, such as
    replacing a whole struct, does affect the query.
- **`Q` is compiled to a DBSP circuit.** Its incremental form `Q^Δ` maps
  `ΔI` to `ΔQ`:
  - **Linear operators** apply directly to `ΔI`: filter, projection that
    keeps duplicates, and `UNION ALL`.
  - **Collapsing operators end in `distinct`.** Change results are sets,
    so a projection that can collapse rows, or `UNION`, must end in
    `distinct`. Otherwise a weight can reach 2.
    - **`distinct` isn't linear.** Its incremental form needs the input's
      weight at V1 for each row `ΔI` touches. That weight is read from
      `V2 − ΔI` at the same snapshot, so the cost stays proportional to
      `|ΔI|`.
  - **Joins are bilinear.** `Δ(A ⋈ B) = ΔA ⋈ B₁ + A₁ ⋈ ΔB + ΔA ⋈ ΔB`,
    where `A₁ = A₂ − ΔA`. Equivalently, `ΔA ⋈ B₂ + A₂ ⋈ ΔB − ΔA ⋈ ΔB`,
    which joins the delta rows directly against the snapshot's base tables.
  - **Grouped aggregates.**
    - **`COUNT` and `SUM` are linear** when kept as per-group values: the
      new value is the old one plus the delta's.
    - **As result rows they aren't.** Each group `ΔI` touches emits
      `−(g, old)` and `+(g, new)`.
      - A group that appears emits only `+`, and one that disappears emits
        only `−`.
      - The old values are read at `V1 = V2 − ΔI`, only for the touched
        groups, so the cost stays proportional to `|ΔI|`.
    - **Presence comes from `COUNT`.** A group exists while its count is
      non-zero, not while its sum is. So `SUM` and `AVG` are maintained as
      a `(COUNT, SUM)` pair.
    - **Privacy.** Every aggregate, in any operation (a query, a live
      query, a change query, or an `observe`), must have one of the two
      shapes in §12, and the checker rejects others.
  - **`MIN` and `MAX`.** These follow from the old value on insertions, but
    not on deletions: deleting the current maximum needs the next-largest
    value (`max_not_incremental`).
    - **What Shep does for deletions.** For each group that loses a row, it
      reads the group's rows at `V1` through an index on (group, value).
    - **Rejected otherwise.** A query whose `MIN`/`MAX` groups can't be
      served from an index is rejected for live queries,
      `shep::observe`, and `changes`.
  - **Verification.** These formulas are proven in `formal/lean`
    (`linear_delta`, `join_delta`, `bilinear_delta_new`, `distinct_delta`).
  - **Recursive queries** (nested group membership) are least fixed points.
    They're computed by semi-naive iteration, and their incremental cost is
    **not** proportional to `|ΔI|`.
    - **Insertions** continue the iteration from the old result. The cost
      is the number of new facts derived, which can be large: one
      membership edge adds every (member, ancestor) pair.
    - **Deletions** use delete-and-rederive. Shep removes everything with a
      derivation through a deleted fact, then rederives what's still
      reachable. Removing only what was derived through the deleted fact is
      wrong, because a fact can have another derivation
      (`naive_delete_wrong`). The cost is the size of the over-deleted
      region, even when the result doesn't change.
    - **Sets, not bags.** The recursive relation is a set, with `distinct`
      inside the loop. Cyclic memberships have infinitely many derivations,
      so weights can't count them.
    - **Where recursion runs.** Recursive relations are maintained only as
      materialized queries (§12), off the commit path. `changes`,
      `observe`, and `audience` over a recursive relation read the view's
      recorded output deltas.
    - **Depth-bounded recursion.** It unrolls into a non-recursive union of
      joins, which incrementalizes at one snapshot like any other query.
- **Everything is computed at one snapshot.** The state at `V2` is a
  materializer snapshot at exactly `V2`, and the state at `V1` is
  `V2 − ΔI`. The incremental plan runs as materializer reads at `V2`, plus
  the delta rows from the log.
- **Cost.** It is proportional to `|ΔI|` and join fan-out, not to the size
  of `Q`'s result.
  - **Recursive queries are the exception.** Their cost depends on how much
    the result changes (see above), which is why they run only as
    materialized queries.
  - **Rejection.** Queries the planner can't incrementalize are rejected
    for live queries, `shep::observe`, and `changes`, and the checker
    reports which.

### 11.6 Guarantees

- **Exactness.** A delta is exactly `Q(V2) − Q(V1)`. No change is missed or
  duplicated.
- **Consistency.** `V2` is a real snapshot version. With `observe`, it is
  exactly the commit point of the mutation.
- **Chaining.** Deltas compose. An app that applies `changes(Q, V0, V1)`
  and then `changes(Q, V1, V2)` holds exactly `Q(V2)`, provided each
  `since` is the version returned with the previous delta (§9.4). This is
  checked in `formal/tla` (`ChangeQueries`) through polls, `observe`,
  archiving, and resync.
- **Dots stay inside Shep.** Apps only handle opaque tokens.

### 11.7 Freshness checks

A build cache wants to accept a result only if its inputs haven't changed
since it read them, and to reuse a result that another worker produced. An
agent wants to re-run a task when its inputs change. Both ask one
question: has this query's answer possibly changed since `V`? A
**freshness check** answers it from dots (§3), with no delta and no hot
log.

```
# shep/ops/inputs.edgeql: everything a build target reads: its sources,
# settings, and the accepted results of its dependencies
select Input { … } filter .target = assert_exists(<Target>$target) limit 1000
```

```
# shep/ops/accept.edgeql: accept a result only if its inputs are what the worker read
with
  t  := assert_exists(<Target>$target),
  ok := assert(shep::unchanged(ops::inputs(target := t),
                               since := <shep::Version>$inputs_at),
               message := 'stale_inputs'),
  r  := (insert BuildResult { target := t, output := <shep::Upload>$output }),
select r.id;
```

```rust
// A worker reads the inputs with their version, builds, and accepts.
let (inputs, v) = shep::fanout::versioned(ops::inputs().target(&target)).exec(&worker).await?;
let output = build(&inputs).await?;
ops::accept().target(&target).inputs_at(&v).output(output).exec(&worker).await?;   // fails with accept::Error::StaleInputs

// Another worker decides which saved results it can reuse, in one call.
let fresh: Vec<bool> = shep::fanout::unchanged(&saved).exec(&worker).await?;   // saved: [(op, args, V)]
```

- **The rule.** A declared query, run again as the same actor at a
  snapshot `S` that contains `V`, is unchanged since `V` if every index
  entry, link, and field it visits has a stamp in `V`. That covers:
  - live entries and ghosts: a row that entered the range has a new stamp,
    and one that left it left a ghost (§3). A ghost stores the full index
    key, sort columns included, so it sits where the row used to sort.
  - every fact in a live index entry's key and in its index's predicate,
    including the node's lifecycle dot: a delete, a restore, or a
    placeholder's fill counts (`Freshness_fielddot`)
  - data and policy inputs: membership links and fields that guards read
  - rows that residual filters or privacy drop later in the plan, since
    they could have passed at `V`
  - the deploy heads, which are registry dots (§10.3), of the queried
    type's policy, of every policy it reaches through `shep::visible`, of
    every schema whose functions are inlined, and the schema heads of
    every type it reads (`Freshness_ownpolicy`). The `global actor`
    declaration is part of a schema's policy head, so its deploys count
    too
- **The planner records a sound read set.** For every scanned range, it
  records the range itself, so its ghosts count, and for every candidate,
  including candidates later dropped by filters or privacy, the inputs of
  its filters, its projection, and its policies. The check's soundness
  rests on exactly that (`Scan.eval_readSetSound` and `freshness_sound` in
  `formal/lean`), and leaving out either part breaks it: checking only
  returned rows, or ignoring ghosts, can answer "unchanged" falsely
  (`returnedOnly_unsound`, `noGhost_unsound`).
- **How far the walk goes.** The check walks the same candidates as the
  query's plan at `S`, live and ghost, until that plan has produced the
  operation's `max_first` (its literal `limit`) rows the actor can see; a query without enough
  rows is walked to the end of its range (`Freshness_maxfirst`). It tests
  each stamp against `V` with the DVV membership rule, extras included
  (§9.2).
  - **Out of budget means "changed".** A check that can't finish its walk
    within its budget or deadline answers "changed", never "unchanged".
- **At a snapshot that contains `V`.** A materializer that hasn't applied
  everything `V` names waits, as `at_least` does. If it can't reach `V`
  within the deadline, the answer is "changed" or `lagging`, never
  "unchanged" (`Freshness_stale`).
- **Indexes newer than `V` answer "changed".** Each index records its
  barrier position in every partition (§3), and its ghosts there exist
  only from that barrier on. A check trusts an index's ghosts in a
  partition `p` only if `V` covers `p`'s log up to that barrier: since a
  barrier's context includes its partition's prefix, the test is
  `(p, barrier) ∈ V`. Otherwise it answers "changed", or plans without the
  index (`backfilled_check_sound`, and `ghosts_incomplete` for an earlier
  `V`, in `Backfill.lean`).
- **Inside a mutation, validated like any guard.** `shep::unchanged`
  covers `(V, S]` through stamps, and validation covers everything after
  `S` up to the commit (§10.3). Its validated read set is everything the
  check visited: every index range with its ghost range, every fact whose
  stamp it tested, and the registry dots it tested, all read at `S`, never
  from a cache (`Freshness_norange`, `Freshness_policycache`).
  - **Pruned by discipline.** Frozen fields and frozen edge sets of nodes
    that were present and filled at `S`, and the lifecycles of
    `shep::Permanent` nodes present at `S`, can't change, so they're left
    out of the validated read set and add no participant (§7.6). Keys
    that were absent or placeholders at `S` stay in it: creating or
    filling them changes the answer. A build cache over content-addressed
    commits, trees, and blobs then validates only its mutable heads, such
    as a branch's bookmark.
  - **Across partitions.** When the check's reads span placement groups,
    each group's partition validates its part, atomically with the commit:
    the partitions the mutation doesn't write join as read-only
    participants (§10.3, `Freshness_parts_local`). Policy and schema heads
    need no participant of their own: a change to one appends a registry
    barrier to the partitions holding the types it affects, so a proposal
    whose `S` lacks it is rejected (§10.3).
  - **At the commit point.** Validation's serial order is commit order, so
    "unchanged" holds at the moment the mutation commits.
  - **The standalone check** (`shep::fanout::unchanged`) needs none of
    this: it answers at `S`, which is what it promises.
- **One version, no fingerprints.** An app stores `(op, args, V)` next to
  the result it keeps, and nothing else. `shep::fanout::versioned` returns
  a result with its version, and `shep::fanout::unchanged` checks many in
  one batched read. `V` is the causally closed version of that read.
- **False "changed", never false "unchanged".** Coarse dots on nested
  data (§3), indexes newer than `V`, and inputs that changed back all make
  a check answer "changed" when a fresh run would return the same result.
  The cost is a re-run. The reverse never happens.
- **What it reveals.** A check never returns data. Like re-running the
  query, it runs as the actor. But it walks rows the actor can't see, so
  an answer of "changed" can mean that something the actor can't read
  changed in the range the query covers. It never says what. Apps that
  can't accept that use a re-run instead (open question 18).
- **Placeholders are skipped,** since no read sees them (§3). A fill moves
  the node's lifecycle dot, so it counts as a change.
- **What isn't eligible.** Deploy rejects a freshness check on:
  - a search: Tantivy ranks over the whole index (§16.6)
  - a materialized query or a `stale_ok` read: they lag the log (§12)
  - a query over a type whose policy uses opaque functions or wasm, whose
    inputs can't be enumerated (§6.8)
- **Across old versions.** Stamps and ghosts outlive the hot window, so a
  check works for any `V` above every index's creation (§11.4).
- **Verified.** `Freshness.tla` in `formal/tla` checks the check and its
  validation under optimistic concurrency with per-partition logs. Its
  configurations named above are the counterexamples each rule prevents.
  An edge set's own dot isn't needed: ghost links are enough.

## 12. Materialized queries

A **materialized query** is maintained incrementally from the mutation log
by a DBSP circuit. It lags the log slightly, and it always knows the exact
version it reflects.

- **Requested by an app.** `#! shep: materialize` on a query in an app's
  operation set asks Shep to maintain it (§7.1). The app calls it like any
  other query, and gets the view's rows. The definition uses the same query
  IR as reads, and identical definitions from different apps share one
  view.
- **Requested by Shep itself.** Shep materializes relations it needs, such
  as a recursive `can_read` (§6.8).
- **The maintainer.** Each materialized query has one maintainer, a kind
  of materializer that follows the WAL (§3).
  - **It steps between causally closed versions.** Usually those are the
    published frontiers (§9.7). From its version `F₁` to the next `F₂`, it
    takes the entries whose dots are in `F₂` but not `F₁`, filtered by
    footprint, and applies their deltas, a two-phase commit's parts all at
    once under its decision dot. Its version is always causally closed, so
    every state the view exposes is a valid one (§9.5).
  - **It resolves in-doubt prepares.** A maintainer that tails only its
    footprint learns which decision a prepare in its log belongs to from
    the release, or, if the release isn't in `F₂` yet, from the home, as a
    materializer resolves an in-doubt prepare (§9.7). It applies the part
    when the decision dot enters its version.
  - **It reads only the partitions its query reads.** The frontier already
    accounts for every other partition, so the maintainer needn't tail
    them. (Applying entries causally one by one would need every partition
    tailed, or `at_least` reads on the view could wait forever:
    `ViewMaintainer_untailed`.)
  - **Progress is its current version,** a DVV.
  - **One maintainer at a time.** It holds a lease in the meta partition;
    a replacement takes the lease, and resumes from the view's last
    checkpointed version. Two maintainers never write one view.
- **Initialization.** A new view evaluates `Q` once at a causally closed
  snapshot `V0`, then steps from `V0`. `V0` is fixed before the evaluation
  starts, so nothing committed during it is missed or counted twice.
- **Reads.**
  - **The result.** A read returns the view's rows plus its version.
  - **Freshness.** An actor handle sends the view only its own writes as
    `at_least` (§9.4), and the read waits, with a bound, until the
    version covers them. Past the bound it fails with `lagging`, unless
    the operation is declared `stale_ok`, which returns what the view has,
    with its version (§7.3).
  - **One cut.** A plan that joins a view with base data or another view
    reads them all at one cut, no newer than any view's frontier (§7.5).
  - **Authorization views.** A materialized `can_read` serves an
    principal-sensitive decision only when its version contains the
    authorization horizon; otherwise evaluation falls back to base data
    (§6.8).
  - **Privacy: rows.** Read policies apply to the nodes a view exposes, as
    they do for any read.
  - **Privacy: aggregates.** A count, sum, or summary exposes no nodes, so
    filtering its output can't hide anything. Every aggregate, in any
    operation, materialized or not, is accepted only in one of two shapes:
    - **Grouped by an anchor.** The rows it aggregates are readable
      exactly when the group's anchor is: a message count per channel, for
      a message type whose read policy only allows `shep::visible(.channel)`. One
      result per group then serves everyone who can read the anchor.
    - **Per viewer.** `#! shep: materialize(per_viewer)` maintains the
      aggregate for each viewer, over only the rows that viewer can read.
      An unread count per channel is per viewer, since what's unread
      differs by person. If any policy it reads reads `global actor`,
      directly or through `shep::visible`, it's keyed by `(actor class,
      viewer)` instead.
      - **Only for viewers who use it.** It's maintained for viewers who
        read or subscribe to it, and dropped for a viewer after a period
        without use.
      - **Priced per viewer** at deploy, so its quota grows with the
        viewers it serves (§15).
      - **Not for the app itself.** An operation run through `db.as_app()`
        can't use it, because the app acting as itself has no viewer.
      - **Deleted viewers drop out.** An aggregate whose viewer is deleted
        stops being maintained (§6.3).

    Anything else is rejected at deploy, and `shep ops check` names the
    policy that would let the aggregate count rows its reader can't see.
- **Change queries over views.** `changes(view, since)` works too.
  - **What the maintainer keeps.** The view's recent output deltas, indexed
    by the view's own versions, which form a chain.
  - **What `since` must be.** One of those versions, which the client
    library keeps from an earlier read of this view: the feed or
    subscription of a `live`, `materialize` query.
  - **Anything else gets a resync,** even a version the view covers. For a
    non-linear query, summing the recorded deltas of the dots that `since`
    lacks gives the wrong answer (`ViewMaintainer_changes`).
- **Uses.**
  - unread counts and badges
  - feeds
  - materialized permission relations (`can_read` for hot types, and
    required whenever a policy recurses, §6.8)
  - agent-orchestration summaries
  - search indexes, which follow the log the same way on Tantivy (§16.6)
- **Lag** is, per partition, how far the view's version trails the
  partition's commit index. It is reported per view.
- **Runtime.** Feldera maintains an open-source Rust implementation of
  DBSP. Use it if its storage and API fit view maintenance here; otherwise
  implement the operator subset Shep needs. Either way, the query
  compilation lives in `shep-delta` and is shared with change queries.

## 13. Live queries

Clients can subscribe to **live queries**: `#! shep: live` queries in an app's
operation set (§7.1), whose results Shep keeps current. A subscription is
a stream of updates. Each update is a delta or a resync, and updates arrive
in version order, with no gaps. Browsers subscribe directly with the
user's token, and backends subscribe through the Rust client. A feed is
the same stream, polled instead of pushed (§7.3).

Live queries are built on change queries (§11). For each **topic**, Shep
computes the change that doesn't depend on who's watching once. Then it
applies each subscriber's privacy rules. Apps that need their own logic at
the commit point can still build real-time themselves with
`shep::fanout` (§13.6).

### 13.1 The model

- **Declared and checked at deploy.** A live query is declared with
  `#! shep: live`, or `#[live]` on its `query!` (§7.1). The checker
  requires that:
  - its query incrementalizes (§11.5), so no opaque functions or wasm in
    the query itself, and its filters are Pure (§4.0)
  - the read policies of every type it returns or includes compile to
    relations (§6.8), so permission changes can be tracked
  - it reads through indexes, and the number of nodes each result row
    references is bounded
- **Subscribing.** `subscribe(op, args, since?)`. Client libraries fill in
  `since` themselves when they resume. A new subscription joins its topic
  through the topic's step loop: its first update is computed at a version
  the topic steps to, so it never misses or repeats a change the topic's
  other subscribers get.
  - **A join can wait for the topic.** If the subscription's `since`, or
    its actor handle's position, is ahead of the topic, for example a
    client reconnecting to a front end whose topic is behind, it waits in
    the loop until the topic steps to a frontier that contains that
    version, up to the wait bound. A first update is never older than
    the `since` or the handle's position (`LiveFrontier_lagjoin`).
  - **Without `since`,** the first update is a resync: the first page of
    the result, at a version as new as the actor handle requires (§7.3):
    everything the handle read or wrote, or only its writes for a live
    query over a materialized query. Past the wait bound, the subscription
    fails with `lagging`, or for a `stale_ok` operation starts from the
    view's current version.
  - **With `since`,** within the hot window, the first update is the delta
    since that version (§11.4).
- **Feeds.** `feed()` is the polling form of the same stream. Each poll
  returns the change since the previous poll, computed with `changes`
  (§11.2). A feed holds a live range the same way a subscription does.
- **Updates.** `{delta | resync, version}`.
  - **Versions only increase** along a stream. Consecutive updates chain:
    applying them in order gives exactly the subscriber's view at the
    latest version, as with `changes` (§11.6).
  - **A resync replaces the client's copy.** Shep sends one when a delta
    would be larger than the result, when a client falls too far behind
    (§13.4), or when the hot window can't answer `since`.
- **A subscription holds a live range.** The range starts as the first
  page and grows as the client pages (`sub.more()`). Deltas cover the rows
  inside the range and the rows entering it: a new message at the top of
  a channel enters the range, and an edit to a message the client hasn't
  loaded is not sent. So a subscriber's copy is always exactly its range
  of the result, and a topic's state is bounded by the ranges its
  subscribers hold, not by the whole result.
- **Resume.** Every update carries the version of the subscriber's view.
  A client that reconnects subscribes again with `since` set to the last
  version it applied, never a merged token (§9.4). The client library does
  this itself.
- **Graph data only.** Typing indicators, presence, and other ephemeral
  signals aren't graph writes. Apps send them over their own channels.

### 13.2 Two phases: the change once, privacy per subscriber

A subscriber is an app, an actor, and a viewer (§6.8). Its view is the
join of a result that doesn't depend on the subscriber with the read
policy:

```
view(app, a, v) = core ⋈ can_op(read, app, a, v, ·)
```

- **`core`** is the query's result with no privacy applied, within the
  union of the subscribers' live ranges: the root rows, plus every node
  they reference through includes, such as a message's author or a
  message it quotes. The referenced nodes form the topic's **reference
  set** `R`.
- **`can_op`** is the compiled read policy, over the subscribers'
  viewers (§6.8). For a reference set whose policies read `actor`, phase 2
  evaluates the actor and app terms per actor class.
- **Masking is the same as for reads (§7.3).** A hidden row leaves a list,
  a hidden unique edge reads as `None`, and a hidden field reads as `None`
  or redacted.

**Topics.** Subscriptions to the same query with the same arguments share
one topic, whichever app's operation set declares it, as long as the
query IR and the pinned schema versions are the same.

- **Unless a policy reads the app.** If any read policy in the topic's
  reference set tests `global shep::app`, or the schemas grant the apps differently,
  the topic is keyed by app too, and each app gets its own. `shep ops
  check` reports which live queries this applies to (§7.1).
- **Or reads the actor.** If any read policy in the reference set reads
  `global actor`, transitively, the topic is keyed by actor class too.
- Phase 1 runs once per topic. Phase 2 runs per subscriber.

**Phase 1: the change, once per topic.** When new log entries touch the
topic's footprint, Shep computes `Δcore` with the change-query machinery
(§11.5), along with the change in `R`.

**Phase 2: privacy, per subscriber.** The delta rule for a join
(`join_delta` in `formal/lean`) says what phase 2 must handle:

```
Δview(a) = Δcore ⋈ can_read(a)  +  core ⋈ Δcan_read(a)  +  Δcore ⋈ Δcan_read(a)
```

So phase 2 has two triggers:

- **A data change.** A new message quotes a message from another channel.
  Phase 2 checks which subscribers can read the quoted message, and masks
  the quote for the rest.
- **A permission change with no data change.** Alice joins channel C2.
  Nothing in the topic changed, so `Δcore` is empty. But every quote of a
  C2 message in the topic is now visible to her, so her view changes. The
  reverse holds when she leaves: quotes she could see must be masked again.
  - **Routing.** The policy-relevant facts in each log entry (membership
    edges and fields that guards read) give `Δcan_read`. It is routed to
    the topics whose reference set contains the affected nodes.
  - **Subscribers' own nodes are routed too.** Each subscriber's actor and
    viewer nodes are registered with the topic, so their `@deleted_at`
    reaches it. A subscription's actor and viewer are fixed by its
    token, so there are no mapping facts to follow and nothing to
    re-register.
  - **Tokens end subscriptions.** A subscription whose token expires
    without a `reauth` closes (§6.3). Revoking a credential at the auth
    service takes effect when its tokens expire.
  - **Deleting the subscriber's actor closes its stream** with `denied`,
    before any later push, so an offboarded user's subscriptions stop at
    once (§6.3).
  - **Deleting the subscriber's viewer** (for example, the user an agent
    works for) closes its stream with `denied` too, as §6.3 rejects a
    deleted viewer, whatever the subscriber's token says.
  - **Without this routing,** revoking access wouldn't hide quotes a
    subscriber already has.

**Lookups scale with nodes, not subscribers.** Phase 2 doesn't check privacy
once per subscriber per change. For each node in the delta, it runs one
batched lookup against the topic's subscribers' viewers. An agent
subscriber working for a user is a member through that user, its viewer.

```
-- one batched read, on the partition that holds channel C2
members = edges(members, src = C2, dst ∈ $viewers)  -- one range read of C2's members
return the subscribers whose viewer is in `members`
```

- **One post, one quote, 10,000 subscribers:** two batched lookups. That
  counts round trips, not work. The lookups still intersect 10,000
  subscribers' viewers with the channel's members,
  and produce and deliver one update per subscriber who can see the post.
  Work grows with the subscribers a change reaches. Reusable audience
  representations keep the per-change part small: an anchor's cached
  audience, kept as a set (a bitmap of subscriber IDs) that membership
  changes update incrementally.
- **Anchors make it cheaper.** Suppose the query's root is anchored:
  messages in a channel, with a message policy that allows
  `shep::visible(.channel)` (§6.8). Phase 2 then keeps the anchor's audience intersected with
  the topic's subscribers. A new message needs only its residual (§11.3).
  Full checks are needed only for referenced nodes outside the anchor, such
  as a quoted message.
  - **The cached audience follows the same routed facts** as `Δcan_read`:
    membership edges, `@deleted_at`, and policy deploys.
    Soft delete leaves edges in place, so a deleted subscriber stays in the
    anchor's audience until its deletion is routed, which closes its
    stream first.
  - **Revocation reaches streams by the same horizon rule.** Every topic
    step runs at a version that contains the authorization horizon
    (§6.3): the principal part always, and the data partitions' part only for
    revocation-sensitive read decisions (§7.6). A topic that can't reach one, because its frontier or its
    tails lag, doesn't step: it pauses its subscribers, and if it
    doesn't catch up, closes their streams so they resume later. It never
    pushes from a view of permissions older than the bound
    (`AuthHorizon_streamlag`).
  - **The step's `S` also passes the materializers' version rule** (§6.3)
    for every partition its routing reads. So a topic whose frontier lags
    on the channel's partition pauses too, even when every materializer
    has applied the change (`AuthHorizon_topicchan`).
- **Large changes become resyncs.** A subscriber who loses access to the
  anchor loses their whole view. Shep sends a resync to the empty view
  instead of deleting every row.

**Both phases run at one version.** Each step of a topic runs at one
causally closed snapshot `S`, always a published frontier and usually the
latest (§9.7), with its exact version (§9.4). It is never the
per-partition maxima of the entries the front end has tailed, because an
entry can be applied before its cause in another partition
(`LiveFrontier_maxima`):

1. Phase 1 computes `Δcore` as `changes(core, since: V_topic)` at `S`.
2. Phase 2 evaluates every visibility check against the same snapshot.
3. Every update the step produces carries `S`'s version, including a
   joining subscriber's first update, and `V_topic` becomes that version.
   A snapshot taken outside the loop could be newer than the topic's next
   step, so a joiner is never served from one (`LiveFrontier_snapshot`).

The log tail only triggers steps and filters entries by footprint. It
never supplies the state a check reads. Each update is therefore exactly
the change in the subscriber's view between two snapshot versions.

**Registry and policy changes.** A new schema head, a policy deploy, and a
shadow promotion are entries in the log too (§10.1). A topic steps across
one atomically:

- **A new head.** The topic's core is re-planned at that entry. A backfill
  arrives as an ordinary delta.
- **`hide` adapters apply in phase 1.** They depend on the schema version,
  not on the subscriber.
- **A policy change** has no data delta, so the topic recomputes phase 2
  for its subscribers at that entry. Where that changes a large part of a
  view, the subscriber gets a resync.

**Reading across partitions.** A quoted message can live in another
partition, so a topic's snapshot can span partitions. A causally closed
version covers that (§9.7). Placement can't co-locate everything a topic
references, because quotes cross channels.

### 13.3 Where it runs

- **MVP: every front end runs both phases for its own subscribers.**
  - A front end runs phase 1 once for each topic its subscribers use, then
    phase 2 for those subscribers.
  - A topic whose subscribers are spread over 20 front ends runs phase 1 20
    times per change. That's acceptable to start with. It needs no internal
    bus and no topic ownership, and its cost is bounded by the number of
    front ends, not the number of subscribers.
- **Stepping.** Each front end follows the frontiers published in the
  meta partition (§9.7). It keeps long-poll fetches (§17) open on the
  partitions its topics' footprints touch, and buffers their entries as
  they commit, so nothing busy-polls. For each new frontier, a topic
  takes the buffered entries between its version and the frontier, and
  reads only materializers, at the frontier, so what it reads and the
  entries it applies always describe the same version.
  - **Splits.** A front end learns of a split from the split entry in
    the parent's log, and starts following the children at the split's
    version. As with the horizon (§6.3), the parent's entries and
    evidence don't count past the split entry, and the topic pauses for
    a range until it has the child's.
  - **Registry changes** are crossed atomically, as above: the frontier
    that first includes a registry dot is where the topic re-plans.
- **Soft state.** Topics, reference sets, and subscriber lists live in
  memory and can all be rebuilt.
  - If a front end dies, its clients reconnect to any other and resume
    from their last version (§13.1).
  - Topics on different front ends step independently, so a new front
    end's topic may be behind a resumed client's version. The resume waits
    for the topic to reach it, up to the wait bound (§13.1,
    `LiveFrontier_lagjoin`).
  - Front ends keep no durable state, and any front end can take over any
    client.
- **Sticky connections.** A subscription lives on one connection, so load
  balancers must keep each streaming connection on one front end.
- **Later: topology from declarations.** Live queries are declared, so
  Shep knows every topic's shape in advance. It can later:
  - route a topic's subscribers to the same front ends, so phase 1 runs
    once
  - run phase 1 on topic owners, and fan `Δcore` out to the front ends
    holding connections, which fetch it with long-polls like any other
    internal feed (§17)
  - split tailing by partition, so a front end reads only the logs its
    topics need

  None of these changes the API.

### 13.4 Delivery and flow control

- **One ordered stream per subscription.** Shep never sends two updates
  for one subscription concurrently. The race the TLA+ model found in
  app-built real-time (`SlackRT_poll_push`, §13.6) can't occur.
- **Slow consumers.**
  - Deltas are Z-sets, and Z-sets add, so pending updates for a slow
    subscriber merge into one.
  - If the merged backlog grows past a bound, Shep replaces it with a
    resync.
  - If the subscriber still can't keep up, Shep closes the stream. The
    client resumes later from its last version.
- **Transport.**
  - Browsers use GraphQL subscriptions over a WebSocket
    (`graphql-transport-ws`, §7.4).
  - The Rust client uses streams on its HTTP/2 connection, next to its
    queries and mutations.
- **Limits.** Subscriptions count against admission control: per
  connection, per app, per front end, and per topic fan-out (§15).
- **Expiring tokens.** Every subscription runs under a token, and gets a
  `reauth` update 60 seconds before it expires. The client runtime asks
  its token provider, or the page, for a fresh token with the same
  claims; Shep checks it as on connect. At expiry the stream pauses, with
  its state kept, and if no token comes within a short grace period, it
  closes (§6.3). A feed needs no `reauth`: each poll is a request, and
  carries a current token.

### 13.5 Worked example: the Slack clone

Browsers talk to Shep directly. There is no app-owned real-time service.

- **Declarations,** in the slack app's operation set:

  ```
  # shep/ops/in_channel.edgeql
  #! shep: live
  select Message {
    text, created_at,
    author: { name },
    quotes: { text, author: { name } },     # may be in other channels
  }
  filter .channel = assert_exists(<Channel>$channel)
  order by .created_at desc
  limit 100
  ```

  ```
  # shep/ops/post.edgeql
  with
    ch := assert_exists(<Channel>$channel),
    ok := assert(global viewer in ch.members,                     # One; false with no viewer (§4.0)
                 message := 'not_a_member'),
    m  := (insert Message {
            text := <str>$text,
            channel := ch,
            author := assert_exists(global viewer),
            quotes := array_unpack(<array<Message>>$quotes),          # hidden keys drop out
            files := shep::bounded(array_unpack(<array<shep::Upload>>$files), 10),   # §16.3
          }),
  select m.id;
  ```
- **Opening a channel.** The browser subscribes to `in_channel(c)` with
  the user's token from the auth service. The first update is a resync with the newest
  messages. Later updates are deltas, and scrolling up grows the live
  range.
- **Posting.** The browser calls `post`, an ordinary mutation. The
  poster receives the message on the stream like everyone else, a few
  milliseconds after the commit.
- **Quotes.** A message in C1 quotes a message from C2.
  - Phase 1 sees one added message, and adds the quoted message to `R`.
  - Phase 2 finds, with one batched lookup, which subscribers can read the
    quoted message. They see it in the message's `quotes`. For the rest,
    it is absent from that list.
- **Membership changes.**
  - **Removed from C1.** The anchor's audience changes. Phase 2 sends that
    subscriber a resync to the empty view.
  - **Added to C2.** `Δcan_read` touches C2 messages in C1's reference
    set. Phase 2 sends the new member an update that adds each such quote
    to the messages that quote it.
  - **Blocked authors.** A deny such as `deny select using (global viewer
    in .author.blocked)` is part of the message's residual. Changes to
    `blocked` edges are routed like any other policy fact.
- **Writes from anywhere.** Edits and deletes from any app, such as a
  moderation tool or a bot, arrive on the stream too. Shep sees every
  write, so nothing depends on all writes going through one path.
- **Typing indicators and presence** stay in the app (§13.1).
- **Cost per post.**
  - Phase 1 runs once per front end holding subscribers of the channel.
  - Phase 2 runs the residual, plus one batched lookup per referenced node
    outside the anchor.
  - Pushes grow with the number of subscribers. Queries don't.

### 13.6 App-owned real-time

Live queries cover the common case. Some apps need their own logic at the
commit point, their own fan-out, or non-graph signals in the same ordered
stream. They build real-time with `shep::fanout`, a separate module so
that none of this appears on the common path:

- `shep::fanout::version(&feed)`: the version of the result a feed holds
- `.since(&v)`: start a feed, or run an observing mutation, from a version
  the app kept
- `observe` clauses on mutations, which return `ObservedResult<T>` (§11.3)
- `shep::fanout::advance(&mut feed, &v)`: move a feed to the version of
  an observing mutation's commit, after applying what it observed (§11.3)
- `shep::fanout::versioned(op)` and `shep::fanout::unchanged(...)`: a
  result with its version, and freshness checks over saved results
  (§11.7)

The rules for using them are verified in `formal/tla` (`SlackRT`):

- **One owner per stream, with transitions serialized.** The app's
  service owns its own streams, such as channels. Writes, polls, and
  listener joins for a stream run one at a time in its owner.
- **A response is applied only if it is still current.** Each request
  carries `since: <stream version>`. A response is applied only if the
  stream's version hasn't changed since the request was sent. Otherwise, a
  poll and a post applied with the same `since` can deliver a message to a
  user who was removed before it was posted (`SlackRT_poll_push`).
- **Listeners join through the owner,** at a fresh snapshot. Otherwise a
  listener can receive a push older than its own load, and its token goes
  backwards (`SlackRT_connect`).
- **Clients resume with the token of the last delta they applied,** never
  a merge of everything they've seen (`SlackRT_token`).
- **`observe` gives the exact audience at the commit,** with anchored
  audiences and residuals (§11.3).

### 13.7 Other patterns

- **Agent orchestration.**
  - Workers take work with `claim` (§8), which returns a lease. Every
    write the worker makes for the task runs `.under(&lease)`, so a worker
    whose lease expired changes nothing (`lease_lost`); it calls
    `lease.renew()` while it works and `lease.complete()` when done.
    Agents' side effects outside Shep, such as commits, messages, or API
    calls, must be idempotent per task and attempt generation, since a
    task can run again after a worker dies.
  - Dashboards subscribe to live queries over materialized summaries
    (§12). For such a topic, phase 1 reads the view's recorded output
    deltas instead of computing them, and the topic's versions are the
    view's.
- **Backends** subscribe the same way browsers do, through any actor
  handle, or as the app. A subscription as the app sees what the app-only
  rules allow (`can_op(app, app, ⊥, node)`, §6.8), and shares a topic only
  with other subscriptions of the same app.

## 14. Universes and apps

A **universe** is a fully separate instance of the graph. It has its own:

- data
- registry: schema versions, field IDs, dependencies, operation sets, and
  in-service tracking
- policies
- mutation log

Production is one universe, named `prod`. Staging, each developer's dev
universe, and each test run get their own. Test and dev universes start as a copy
of production's registry, with the candidates under test applied on top
(§14.2).

### 14.1 What's global and what's per universe

- **Apps are global.** Apps are registered once, in one global namespace,
  independent of universes. An app's name means the same thing everywhere.
  That is what keeps these consistent across universes:
  - JS module paths (`/js/<app>/...`), the same on every universe's host
  - policies such as `global shep::app = <shep::App>'slack'`
  - usage tracking
- **Schema names are global too.** Each name is reserved to its owning
  app, so no universe can register `users` under a different owner.
- **Credentials aren't Shep's.** The external auth service issues the
  tokens apps and people present, each naming one universe (§6.3). A
  token for a test universe can't be used against production: the
  universe claim must match the host.
- **Everything else is per universe.**
  - Registered versions, type and field ID assignments, policies,
    operation sets, dependencies, in-service tracking, data, and the
    mutation log.
  - Registering a schema in a dev universe has no effect on production's
    compatibility state.
- **Version numbers mean the same thing in every universe.**
  - Production assigns them (§4.1.4), and other universes start from
    production's registry (§14.2), so `issues@4` is `issues@4` everywhere.
  - A dev or test universe adds unpromoted drafts on top, whose content
    still changes. They never get production numbers.
  - The hash check at connect catches a binary or bundle built from
    different content than the universe holds.
- **Where fleet state lives.** Each fleet, production and
  non-production, is its own WAL cluster with its own WAL controller
  (§9.6). So no test, CI run, or `shep dev` session writes to anything
  production depends on.
  - **The WAL controller** holds the fleet's WAL nodes and its universe
    directory: which universes exist, with their immutable IDs and names,
    and their partitions and replicas.
  - **Production's registry** holds the app and schema-name records.
    Every universe's registry holds the origins each app registered for
    it, and everything in the per-universe list above.
  - **Not in Shep:** credentials, identities, and act-for consent, which
    belong to the external auth service (§6.3); and quotas, which are
    open (open question 27).

  **Apps and schema names have one authority:** production's registry.
  A test or dev universe reads them from its base: the base at version
  S is production's registry as of S (§14.3), so it already contains
  production's app and name records as of S. There's no separate copy to
  follow, and an app's name and schema reservations mean the same thing
  in every universe.
- **Names are registered in production first.**
  - **Only production registers names.** App names and schema-name
    reservations are created only in production's registry, by an
    operator or a CI job holding a production token with the `deploy`
    scope:
    `shep app create <name>` and `shep schema reserve <name> --app <app>`
    (§4.1.3).
  - **Non-production only reads.** It never forwards a registration to
    production, and never records a name of its own next to its base's.
    Either would let two fleets disagree about who owns a name
    (`Bootstrap_forward`, `Bootstrap_local` in `formal/tla`).
  - **A new name reaches new bases.** A name registered in production is
    in every base built from a version after it. Bases are built from
    production's registry as it changes, so a new name reaches the next
    base within a short bound (30 seconds by default).
  - **A brand-new app waits for its name.** Its first `shep dev` session or
    test run checks its base and, until the name is there, fails with:
    "name `foo` isn't registered in production yet; register it with
    `shep app create foo` (needs a production token) and wait for it to
    reach the test bases (up to 30 seconds)." A new schema name works the
    same way with `shep schema reserve`.

### 14.2 Kinds and lifecycle

| Kind | Typical use | Lifetime | Mutation log |
|---|---|---|---|
| `production` | the real thing: the universe named `prod` | permanent | hot + archive + snapshots |
| `staging` | pre-release | long-lived | hot + archive |
| `dev` | a developer's `shep dev` | while `shep dev` runs, plus its TTL | hot only |
| `test` | one test or test run | minutes (TTL) | hot only, dumped on failure |

- **Names.** Universes are named by DNS names (§14.3), so the rule is DNS
  validity: non-empty, at most 253 characters in all, made of labels of
  1–63 characters each, using letters, digits, and hyphens, with no label
  starting or ending with a hyphen. Names are
  case-insensitive, and stored lowercased. These limits are constants.
  `prod` is the production universe's name, and it and names like it
  are reserved.

- **Ephemeral universes expire.** Each one has a TTL, refreshed by a
  heartbeat from its owner (the test harness or `shep dev`). The TTL and
  its heartbeats are entries in the universe's own meta partition, so
  they never load the controller. The controller's reaper reads them and
  appends only `drop_universe` for an expired one, so crashed test runs
  don't leak universes.

**Test and dev universes are production plus candidates.** A test, a CI
run, or a `shep dev` session is about *candidates*, usually the ones in the
current commit: schema versions, policies, and operation sets. The
universe is built from two parts:

- **Base: production's registry as of a pinned version S.**
  - **What it includes.** Every app's schemas, policies, operation sets,
    and in-service state, copied with no data.
  - **Other apps.** Every app not under test appears exactly as in
    production, with its current operation set.
  - **Older versions come too.** Production's older in-service versions
    and their adapter chains are copied as well. That way the overlay is
    checked against exactly the chains production would check (§4.4).
  - **How the base is pinned.** Registry changes are events in
    production's log (§10.1), so the base is "production's registry as of
    version S". Rerunning a CI job against the same S reproduces the same
    universe. S is only ever a version where no schema version is Prepared
    or in DualWrite (§4.2), including when CI names one with
    `--base-version`: the newest such version, by default, so a base never
    holds a version that's half way to becoming the head. Otherwise a test
    universe runs the cutover synchronously and CI tests against a head
    production may still abandon (`Cutover_base`).
- **Overlay: the candidates.** Schema versions, the policies in the
  working tree, and operation sets. They're applied only in this universe.
  - **Several apps at once.** An overlay can contain versions of several
    apps' schemas, to test a coordinated change across them (for example,
    a new `users` version together with an `issues` version that pins it
    as `users@next`, §4.1.4).
  - **Same rules as production.** Applying an overlay runs the same
    checks as a production deploy (§4.4, §6.6, §7.1), against the base's
    in-service versions and operation sets. A candidate that production
    would reject can't be used to create a test universe, so a green CI
    run means the candidate is deployable as of S.
  - **Re-check at promotion.** Production may have moved past S by the
    time `shep deploy` runs. Deployment re-runs the checks against current
    production, for the exact hashes that were tested.

**Data never crosses universes.**

- **What gets copied.** A test or dev universe copies production's
  *registry*, never its data. Each universe's data comes only from writes
  made in that universe.
- **Production data never leaves production.** Snapshots and log replay
  (§10.4) restore a universe into itself (recovery, point-in-time restore),
  never into another universe.
- **The one exception is debugging a failed test.** That test universe's
  own log can be replayed into a fresh dev universe. That data was
  produced by the test, not by production. Its files come back without
  content, since blobs belong to their universe (§10.4).
- **Empty universes** (no schemas, no data) exist for testing Shep itself.

**The test harness and the CLI.**

- **Rust tests.** The operation set passed to the harness, with the
  schema it uses, is the overlay. Dependencies that are candidates too are
  included, and so are the policies in the working tree (`shep/policies`,
  found through `shep.toml`), so tests run the policies being changed.
- **Seeding.** `shep::test::seed(&db)` returns an actor handle that
  bypasses policies, valid only in test universes, for setting up
  fixtures in a few lines.
- **CI for any language.** CI uses
  `shep universe create --overlay <project dirs> [--base-version S]`, where
  each project's `shep.toml` names its schema, policies, and operations.
  It needs a token with `manage_universes`, and prints the universe's
  name, which tokens for it then carry as their audience.
- **Dev.** `shep dev` is the same thing, long-lived: it creates a dev
  universe on the non-production fleet, keeps it alive with a heartbeat,
  and re-applies the overlay whenever a source file is saved. Nothing runs
  on the developer's machine but the CLI.
- **New names come from production.** A test universe or dev session for
  an app or schema whose name isn't yet in the non-production copy fails
  until it is: names are registered in production first and propagate
  within 30 seconds (§14.1).
- **Failed tests keep their log.** On failure, the harness dumps the
  universe's log, and `shep log replay` rebuilds that state in a new dev
  universe, without file contents.

```rust
#[tokio::test]
async fn closing_an_issue() -> Result<(), shep::Error> {
    // production registry + this crate's operations, schema, and policies
    // as the overlay, all components in this process (§14.3)
    let db = shep::test::db(tracker::ops::set()).await;
    let seed = shep::test::seed(&db);                 // bypasses policies; tests only
    let alice = db.as_user(shep::test::user(&seed).await);
    let issue = shep::create!(Issue { title: "Crash", /* ... */ }).exec(&seed).await?;

    ops::close_issue().issue(&issue.id).comment("fixed").exec(&alice).await?;
    Ok(())
}   // universe dropped on drop (TTL as backstop); log kept on panic
```

### 14.3 Mapping universes onto storage

- **A universe is its own partitions: a cell.** Each universe has its own
  meta partition (§3), its own data partitions and their Raft groups, and
  its own prefix in the object store, keyed by its immutable ID. The WAL
  controller creates and drops them with universe commands (§9.6), and
  holds the universe directory. Nothing else is shared below the WAL
  cluster's machines.
  - **Production** runs on its own WAL cluster, materializers, and front
    ends, and starts with one universe, `prod`.
  - **Non-production universes** share a separate fleet that runs as its
    own deployment and hosts many universes. A single-node install hosts
    its dev and test universes beside production instead (§9.6).
  - **Result.** Nothing a test does (load, registry churn, a runaway
    query) can reach production, and nothing in one test universe can
    reach another.
- **Not a `universe_id` in every key.**
  - **Isolation.** With a shared keyspace, isolation would depend on every
    read filtering correctly.
  - **Teardown.** Dropping a universe would become a mass delete.
  - **With partitions per universe**, isolation is structural, and
    dropping a universe is one controller command: the reconcilers remove
    its Raft groups, and its object-store prefix is deleted.
- **Creating a test universe is cheap.**
  - **The base.** For each base version S, the service builds a base once:
    a meta partition holding production's registry as of S, with no data,
    and its published frontiers reset, since they name production's
    partitions. It's checkpointed in the object store. The partition map
    isn't in it: it's controller state, and a new universe's partitions
    come from its create command.
  - **Per test.** A universe's meta partition starts from that
    checkpoint as a Raft snapshot at the base's index, so its log
    continues from there: every key it copied has a stamp below the new
    universe's first entry, and the copied manifest shares the base's SSTs
    without copying data. Partition IDs are per universe, so the copy's
    IDs collide with nothing. The overlay is applied on top. Nothing is
    built per type, because indexes are only key ranges (§3).
  - **Rust tests run in one process.** `shep::test::db` runs the
    single-node deployment in the test's own process (§9.6), with an in-memory object
    store, from the base registry fetched once per S and cached. Each
    placement group gets its own partition, home partitions are picked at
    random, and a little apply lag is injected, so two-phase commit,
    cross-partition reads, and lagging views run in ordinary tests. A test
    universe still costs about as much as opening a few in-memory
    databases.
  - **A universe is never reused,** so no test sees another's data.
- **Each universe has an immutable ID,** never reused, even when a name is
  reused after a drop. Object-store prefixes and log archives are keyed by
  the ID, not the name, so a recreated `staging` can't see the old one's
  blobs while they're being deleted.
  - **Base changes are rare.** Production's registry changes far less
    often than tests run, so one base serves many CI runs.

- **Addressing: one domain per universe.** Every connection belongs to
  one universe, and the host says which.
  - **Hosts.** Production is `shep.internal`. Every other universe is
    `<universe>.shep.internal`, such as `staging.shep.internal` or
    `test-8f2c.shep.internal`. Universe names follow DNS rules (§14.2).
  - **Paths are the same everywhere.** `/g/<app>/graphql`, `/js/<app>/...`,
    and the rest mean the same thing on every host.
  - **No DNS change per universe.** Wildcard DNS records and wildcard TLS
    certificates cover every universe: `shep.internal` and
    `*.shep.internal` for the service, and `shep-files.internal` and
    `*.shep-files.internal` for downloads (§16.4). Test universes created
    on the fly are reachable at once.
  - **Why domains, not paths.**
    - **Browser isolation.** Each universe is its own origin, so storage,
      service workers, and CORS grants can't leak between production and
      a test universe. `staging.shep.internal` and `shep.internal` are
      still the same *site* to a browser, so Shep never relies on cookies:
      it authenticates with bearer tokens, and sets only host-only cookies
      for GraphiQL. Downloads use a separate site (§16.4).
    - **Routing.** Load balancers, TLS, rate limits, and logs can be set
      per universe from the host alone, before a request is parsed.
  - **One value picks it.** A token names its universe (§6.3), and the
    client libraries derive the host from it, so most apps configure
    nothing. `SHEP_URL` overrides it. Browsers get the host from the app's
    import map (§7.4).

## 15. Capacity and admission control

Shep is shared. One app's traffic must never take down another app, and
Shep must never accept more work than its components can finish. Every connection, request, and subscription is admitted against
measured resources before it runs, and every request runs within a budget.

### 15.1 What is measured

Each piece of work is attributed to its universe, app, operation (§7.1),
actor class (the app itself, an end user, an agent, or a person in an
ad-hoc session), and partition, all taken from the token's claims.

| Resource | Measured as | Where |
|---|---|---|
| Front-end CPU | CPU time per request, timed around each poll of its tasks | front ends |
| Front-end memory | Bytes reserved per request and per subscription. A tracking allocator charges allocations to the request whose task is being polled | front ends |
| Reads | Keys and ranges read, bytes returned, SlateDB block-cache hits and object-store fetches, per request | materializers |
| Write volume | Facts and log bytes per commit; append rate and replication lag per partition | WAL |
| Validation | Proposals validated, read-set size, rejections by reason (`contended`, `too_old`), and the validation table's size per partition | WAL leaders |
| Two-phase commits | Participants, prepare-to-decision time, intents held | WAL leaders |
| Streams | Open connections, subscriptions, and topics; reference-set sizes; phase 1 and phase 2 time; bytes pushed; backlog per subscriber | front ends |
| Lag | Apply lag per materializer, frontier lag per partition (§9.7), view lag (§12) | materializers, meta partition, front ends |
| Files | Bytes uploaded, stored, and served per app; extraction time and memory (§16) | object store, extract workers |
| Indexes | Storage and write cost of each index Shep built, charged in full to every app whose operations use it. Builds go to the deploying app; rebuilds a new head triggers, and `exclusive` indexes, to the schema owner (§7.1) | materializers, search indexers |
| Search | Query time and memory per operation; candidates scored and loaded; index lag (§16.6) | search indexers |

- **Every operation has a cost model.** When an operation set is
  deployed, the planner estimates each operation's cost from its plan:
  - the index ranges it scans
  - hop fan-out, bounded by edge cardinality and page sizes
  - which policy checks push down and which are evaluated per row
  - for a mutation, the placement groups it writes and validates:
    two-phase commit costs three appends in sequence instead of one (§8)
  - for a live query, the phase 2 work per change

  An operation without a finite bound is rejected (§7.1).
- **Measured cost corrects the model.** Shep keeps a moving average of
  each operation's actual cost, per resource. Admission uses the measured
  cost once there is enough data, and the estimate before that.
  `shep ops status` reports operations whose actual cost is far from their
  estimate.

### 15.2 Admission

Admission works at four layers. Every decision is local to a front end, so
admission adds no coordination to the request path. Admission runs in
front ends; materializers and WAL leaders also cap their own queues and
shed with `overloaded`, so a misconfigured front end can't swamp them.

1. **Connections.**
   - Caps per front end, per app, and per person (ad-hoc sessions).
   - A front end above its memory high watermark accepts no new streaming
     connections, and the load balancer sends them to other front ends.
2. **Requests.**
   - **Per-app limits are open.** Admission keys work on the token's
     `client_id` claim, so per-app accounting is exact, but how quotas are
     configured and enforced is open question 27.
   - **Adaptive concurrency limits per partition.** Each front end limits
     its concurrent reads per materializer and its proposals per WAL
     leader. The limit grows while latency stays at
     its baseline and shrinks when queueing appears (a gradient or AIMD
     limiter, as in Netflix's concurrency-limits). Requests wait in a short
     queue with a deadline, and past the deadline they are rejected instead
     of running late.
   - **Priority classes.** Under load, Shep sheds the lowest class first:
     1. mutations and interactive reads
     2. live-query maintenance: phase 1 and phase 2 (§13)
     3. background work: backfills (§4.4), view maintenance (§12), search
        indexing (§16.6), file processing (§16.5), and archiving (§10.4)
     4. ad-hoc queries (§7.2)

     WAL replication and materializers applying committed entries are
     never shed. Every version depends on them, and shedding them would
     only add lag that every read then waits for.
   - **Reserved capacity for required maintenance.** Each of these has a
     reserved minimum share of its nodes' capacity, and is never shed
     below it:
     - archiving and log truncation, and compaction
     - two-phase commit decisions and releases
     - front ends' tails of the meta, actor, and viewer partitions, and the commit-index reads that go with them, which the
       authorization horizon depends on (§6.3)
     - frontier publication (§9.7)
     - registry barrier delivery (§10.3)
     - any view that authorization depends on (a materialized `can_read`
       for a recursive policy, §6.8)

     Without these, overload would make every principal-sensitive request fail
     closed and every topic stall, a self-inflicted outage.
   - **Admission watches downstream health.** Apply lag, frontier lag,
     view lag, retained-log size, and storage headroom all feed admission.
     As they grow, Shep sheds new foreground work, starting with the lowest
     class, before maintenance starves. Starved maintenance means longer
     logs, longer waits, more retries, and resyncs, which make the overload
     worse.
3. **Execution budgets.** Every request runs with a budget derived from
   its admitted cost, plus headroom: rows read, hops, CPU time, memory, and
   a deadline that every wait honors: materializer applies, closing a
   version (§9.7), a two-phase commit's in-doubt resolution, and internal
   retries. A request that exceeds its
   budget is aborted with `budget_exceeded`, and the operation's measured
   cost goes up.
   - **A budget the caller can observe counts only visible work.** Whether
     a request ran out of budget, or where a walk stopped, is visible to
     the caller, so it must not depend on rows the actor can't see.
     Budgets whose outcome shapes a result, such as where a walk stops and
     whether it's `truncated` (§7.1), count only visible rows
     (`visibleWork_congr` in `formal/lean`). Budgets that also count
     hidden work, such as rows read or search candidates loaded (§16.6),
     can reveal through `budget_exceeded` roughly how much hidden data a
     query's range holds, never what it is. That's accepted.
4. **Subscriptions.**
   - Caps per connection, per app, and per front end.
   - Memory is reserved for each subscription and topic when it is
     admitted.
   - **Topic fan-out budgets.** Phase 2 work per change is priced like a
     request. A topic over its budget coalesces changes over a short
     interval before each step, trading latency for throughput.
   - Slow subscribers are handled as §13.4 describes.

### 15.3 Overload

- **Signals.**
  - Front ends: CPU utilization and memory watermarks.
  - WAL leaders: append latency, validation rejections, replication lag,
    and intents held by two-phase commits.
  - Materializers: read latency, object-store fetches, and apply lag.
  - Versions: apply lag and frontier lag.
- **Goodput, not throughput.** Under overload, Shep rejects work early and
  cheaply, before it reads anything, instead of letting every request slow
  down. Completed work per second should stay flat past saturation, not
  collapse.
- **Errors tell client libraries what to do.** App code sees these only
  when retries run out (§7.3).

  | Error | Meaning | Client library behavior |
  |---|---|---|
  | `overloaded` | Shep is shedding this class right now, a component it needs is unavailable past the request's deadline, or its own retries (`contended`, `too_old`, §10.3) ran out. The response names which, for dashboards | Retry after `retry_after`, with backoff and jitter |
  | `unavailable` | The front end can't show it's within the authorization horizon (§6.3), so it fails closed rather than answer from state that may be stale | Retry with backoff; alert if it persists, since it means a front end is cut off from a leader |
  | `quota_exceeded` | Reserved for per-app quotas, once they exist (open question 27) | Retry after the quota window |
  | `budget_exceeded` | The request ran over its budget | For a declared operation, retry once at the raised cost estimate |
  | `too_expensive` | An ad-hoc query can never fit a budget | Don't retry; the person changes the query |

- **Retry budgets.** Client libraries cap retries at a fraction of recent
  successful requests, as gRPC's retry throttling does, so retries can't
  amplify an overload.
  Idempotency keys make mutation retries safe (§8).

### 15.4 Operating it

- **Quotas are open** (open question 27). Admission control already
  measures and sheds per app, from the token's `client_id` claim, and test and
  dev universes run on a separate fleet anyway (§14.3).
- **`shep top`** shows live cost by app and operation, per resource, and
  what is being shed.
- **Every rejection is counted** by app, operation, class, and reason.
  Apps can alert on their own rejections.
- **Load tests are part of the test suite.** They drive the service past
  saturation and assert three things:
  - goodput stays flat
  - classes are shed in order
  - one app's overload doesn't raise another app's latency

## 16. Files and text search

Two additions that share machinery:

- **Files.** Large binary content, such as an upload in the Slack clone, a
  PDF attached to an issue, or a build log, lives in an object store such
  as S3. A built-in `File` type holds the content's metadata and state, and
  gives readers download links and extracted text.
- **Text search.** Full-text search over text fields and over the text
  Shep extracts from files, on Rust search indexers built on Tantivy.
  There is no vector search.

### 16.1 Size limits for ordinary fields

Every field's value is capped, whatever its type. Anything larger is a
`File`, and a collection that keeps growing is edges.

| Limit | Default | Maximum |
|---|---|---|
| One field's whole value, everything nested in it included: a scalar, `String` or `Bytes`, an embedded struct, a map, set, or list, or a `Document` | 64 KiB | 1 MiB, with `constraint shep::max_size('…')` |
| A node's fields together (§10.2) | 4 MiB | 4 MiB |
| An encoded key (§3) | 256 bytes | 256 bytes |
| One `File` | 100 MiB per field | the object store's limit, with `constraint shep::file(max_size := …)` |

- **Measured as stored.** The size is the field's value in Shep's
  canonical encoding: the bytes stored under that field's key (§3).
- **Why values are small.** Each field is one key, so its value is handled
  whole at every step:
  - **Reads.** A read of a field loads its whole value, and a read of a
    node loads every field it includes, which the node cap bounds.
  - **Validation.** Concurrency is per field (§10.2), so the whole value
    is what a mutation reads and validates.
  - **The log carries old values.** Changing a field logs the old value
    and the new one (§10.1), and every log entry is replicated to three
    WAL replicas before it commits.
  - **Materializers keep versions.** Every change to a field writes a new
    version of its key, kept down to the retention floor (§10.4).
  - **Live deltas carry whole values.** A changed field is pushed in full
    to every subscriber whose range holds the node (§13).
- **Enforced on every write.** A write whose value would exceed its
  field's cap, or a map or set `insert` or a list `push` that would take
  it past, fails with `too_large` (§7.3).
- **Caps are constraints.** `shep::max_size` is a built-in constraint
  compared structurally, like `max_len_value` (§4.5), so changing one
  follows §4.3 and §4.4.
  Raising a cap is a change older readers see, and `shep schema check`
  proposes an adapter (`shep::truncate` for text, or `hide`), as it did when
  `Issue.title` grew from 200 to 500 (§4.1.1).
- **Shown before it bites.** `shep schema check` warns about a map, set,
  array, or `Document` field with no `shep::max_size` that an operation
  appends to, such as `tags += …` or `arr := .arr ++ […]`, since it will reach
  the default cap in production.
- **Moving a field to `File` is a new field.** A `String` field doesn't
  become a `File` through an adapter. Add a `File` field, copy the values
  with a backfill mutation, and deprecate the old field (§4.1.4).

### 16.2 The `File` type

A file field links a node to a **file**: a node of the built-in type
`shep::File`, whose fields Shep owns.

```
type Message {
  constraint shep::placement on (.channel);       # stored with its channel (§9.6)
  required text: str;
  multi files: shep::File {                       # up to 10 files of any type, each up to 1 GiB
    constraint shep::file(max_count := 10, max_size := '1GiB');
  }
  required channel: Channel;
  # ... author, quotes (§13.5)
}

type Issue {
  # ...
  spec: shep::File {
    constraint shep::file(media := {'application/pdf', 'text/markdown', 'text/plain'});
  }
}
```

**What a file holds.** `shep::File` has these fields. Apps read them and
never write them:

| Field | Type | Meaning |
|---|---|---|
| `id` | `Uuid` | The file's key |
| `name` | `String?` | The file name the uploader gave, up to 255 bytes |
| `media_type` | `String` | A MIME type, such as `application/pdf` or `text/markdown`. The uploader declares it, and Shep checks it against the content (§16.3) |
| `size` | `Int?` | Bytes. Set once the content is stored |
| `sha256` | `Sha256?` | The content's hash. Set once the content is stored |
| `state` | `FileState` | `Uploading`, `Processing`, `Ready`, or `Failed` (below) |
| `error` | `FileError?` | Why the file `Failed`, or why it has no text |
| `text_status` | `TextStatus?` | When some operation uses the field's text: `Pending`, `Extracted`, `Truncated`, `Empty`, or `Unsupported` (§16.5) |
| `url` | `Url?` | A download link for the actor reading it (§16.4). `None` until the content is stored |
| `text_url` | `Url?` | A link to the full extracted text. Comes with `text`, and is `None` without it (§16.5) |
| `created_at` | `Timestamp` | System field, as on every node |

**States.**

```
            content stored            processing done
 Uploading ───────────────▶ Processing ───────────────▶ Ready
     │
     │ upload expired, or content rejected
     ▼
  Failed
```

- **`Uploading`.** The file is attached, but its content hasn't arrived
  yet. This happens only when an app attaches an upload before it
  finishes (§16.3). The file has no content and no link.
- **`Processing`.** The content is stored and verified, and can be
  downloaded. Text that some operation needs isn't extracted yet.
- **`Ready`.** Processing is done. A file on a field whose text no
  operation uses goes straight from `Uploading` to `Ready`.
- **`Failed`.** The content never became usable: the upload expired, or
  the content didn't match its declared size, hash, or media type. `error`
  says which: `upload_expired`, `size_mismatch`, `checksum_mismatch`, or
  `media_mismatch`. One more error can turn any file `Failed`:
  `content_unavailable`, when the log was replayed into another universe,
  which has the file's metadata but not its blob (§10.4). A file whose text couldn't be extracted is still `Ready`,
  with `text_status` and `error` set, because its content is fine.
- **State changes are ordinary commits.** Shep writes each one as a
  mutation with its own dot (§9), so live queries see a file go from
  `Uploading` to `Ready` like any other change (§16.7).

**A file belongs to one field of one node.**

- **No policies of its own.** A file is visible exactly when the node that
  holds it is visible and the actor can read that field. Its owner's
  read policy, and any field-level policy on the file field (§6.6 item 7),
  decide.
- **Attaching and detaching** are updates to the owning node, under its
  update policy. Detaching a file deletes it.
- **A file lives with its owner.** Shep places every file with the node
  that holds it (§9.6), so attaching is a single-partition mutation. Its key
  is a generated `Uuid`, which carries its owner's group's route, like
  any node placed with another (§9.6).
- **Copying is cheap.** Forwarding a message with its attachments creates
  new files that point to the same blobs, and no bytes move. A copy is an
  upload whose source is a file the actor can read (§16.3), so it works
  across partitions like any upload.
- **No file is shared between owners; only bytes are.** Two nodes that
  hold the same content hold two files that point to one blob. So a file
  never needs its own policy, and deleting one owner never changes what
  another owner shows.

**Reading files.** A file field loads like a link: an operation's shape
includes it, or reading it is `NotLoaded` (§4.1.2). Including a file
includes its metadata and its download link. Extracted text costs more,
so it is the one thing an operation asks for by name:

```
# shep/ops/channel_messages.edgeql: recent messages, with each file's metadata and download link
#! shep: live
select Message { text, files: { name, media_type, size, state, url } }
filter .channel = assert_exists(<Channel>$channel)
order by .created_at desc
limit 50
```

```
# shep/ops/spec_text.edgeql: an issue's spec, with its extracted text
select <Issue>$issue { spec: { name, text, text_truncated, text_url } }
```

```rust
for file in &msg.files {
    println!("{} ({:?})", file.name.as_deref().unwrap_or("untitled"), file.state);
    if let Some(url) = &file.url {               // None until the content is stored
        render_link(url);
    }
}
```

- **`text`** is the extracted text, up to 64 KiB, with a flag,
  `text_truncated`, that says whether it was cut. The full text has its
  own link, `text_url`. Selecting `text` in a shape is also what tells
  Shep to extract the field's text (§16.5).
- **In GraphQL:** a `File` type with `name`, `mediaType`, `size`,
  `sha256`, `state`, `error`, `textStatus`, `url`, `text`, and `textUrl`.
  Selecting `text` in a document is the same as including it.

**Storage.**

- **Each file field gets its own storage type,** named after the field
  (`slack::Message.files`), as an edge type is (§3). Its files have their
  own key prefix, and the indexes on them are its own.
- **Content is stored once.** A file points to a **blob**: immutable
  content, identified by its SHA-256 (the one hash Shep uses for content,
  which is also the checksum S3 verifies). A universe never stores the same
  bytes twice, however many files, owners, or apps hold them.
- **Blobs live only in the object store,** at
  `<universe ID>/sha256/<hex>`, whatever their size. Neither the WAL nor
  materializers hold blob bytes, so the log and the object store together
  hold everything (§16.7).
  - **Small content still skips the upload step.** Up to 256 KiB can
    travel inside the attaching mutation, and Shep writes the blob before
    the transaction starts (§16.3).
  - **Small blobs can be cached.** Blobs never change, so front ends may
    cache small ones to serve downloads and extraction without a round
    trip to the object store.
- **Writing a blob is idempotent.** Shep writes a blob with a conditional
  write (`If-None-Match`), which does nothing if the blob exists. So a
  blob can be written outside the transaction that attaches it, and a
  transaction that aborts leaves at most an unreferenced blob, which
  stays (below).
- **Deduplication is per universe.** Universes share nothing (§14):
  production content is never copied into, or shared with, another
  universe. Sharing blobs across universes would let a test universe learn
  what production stores, and would tie a test universe's teardown to
  production's references.
- **Blobs are never deleted. GC is punted.** Once stored, a blob stays for
  the life of its universe, even when no file points to it any more:
  after a purge (§6.5), after an aborted attach, or after an upload that
  was never attached. That includes blobs of extracted text (§16.5). Only
  dropping the universe deletes its blobs (§14).
  - **Why.** With deduplication, an upload or a copy can end up pointing
    at a blob that already exists without writing it. Deleting blobs
    safely then needs a lease protocol between uploads, copies, and
    collection across partitions. Never deleting has no such race, and
    storage is cheap next to lost data.
  - **What a later GC can rely on.** Files record their blob's hash and
    the log records every attach (§16.7), so which blobs are referenced
    can always be computed. Open question 15 records the design to start
    from.
- **Knowing a hash grants nothing.** A file's `sha256` and `size` are
  readable fields, so they can't work as a capability. An upload always
  sends its bytes, unless it copies a file the actor can already read
  (§16.3), and Shep discards a duplicate inside the object store. So a
  client can't obtain bytes by naming their hash, and an upload looks the
  same to the client whether or not the bytes were already stored. Only
  timing differs slightly, which is accepted for now (open question 10).

### 16.3 Uploading

**The flow.** Uploads follow the pattern most upload APIs share (§16.8):
ask for a target, send the bytes straight to the object store, then
attach.

1. **Begin.** The client asks Shep for an upload against a file field:
   `begin_upload(field, name, media_type, size, sha256?, from?)`.
   - Shep checks the field's limits (§16.1).
     Policies run at attach, once the owning node is known.
   - Shep returns an **upload**: an ID, where to send the bytes, if
     anywhere, and an expiry.
     - **A copy: `from` names a file.** Shep checks that the actor can
       read that file, as for any read, and that its content is stored.
       Copying an `Uploading` file fails with `not_ready`, and a `Failed`
       one with its error. The upload is then complete at once and points
       at that file's blob, and no bytes move.
       Forwarding uses this, and the client libraries use it whenever the
       content came from a Shep file.
     - **Small content** (up to 256 KiB) has no target. The bytes travel
       in the attaching mutation itself, and the upload is never a
       separate step.
     - **Up to 64 MiB:** one presigned `PUT` to a staging key. The
       signature fixes the `Content-Length`, and the
       `x-amz-checksum-sha256` header when the client sent a hash, so S3
       rejects any other bytes.
     - **Larger:** a multipart upload to a staging key. Shep presigns part
       URLs in batches, as the client asks for them, and each part carries
       its own SHA-256 checksum.
   - **An upload isn't data.** It's bound to the universe, the app, the
     actor, and the field, and expires after 24 hours. Nothing in the
     graph points to it yet, and no one else can attach it. Its own state
     is `pending`, `complete`, or `expired`, kept in an **upload record**
     in the hashed keyspace (§3), keyed by the upload's ID.
2. **Send.** The client sends the bytes to the object store directly.
   Shep never carries them, unless the deployment runs in proxy mode
   (below).
3. **Complete.** The client tells Shep the bytes are there. Shep finishes
   the multipart upload if there is one, then verifies the content: its
   size, its `sha256`, and its media type (below). It then copies the
   staged object to the blob's key inside the object store, with
   `If-None-Match`, and deletes the staged copy. If the blob already
   existed, the copy does nothing. The client sees the same result either
   way. Objects over 5 GB are copied in parts (`UploadPartCopy`).
4. **Attach.** A mutation sets the file field to the upload, like any
   other value: `files: vec![upload]`. Attaching creates the file.

**Attach before or after completing.** An app may attach an upload before
it completes. The file then starts as `Uploading`, so other actors can
show "uploading…", and a worker moves it on once the upload completes
(below). If the upload expires first, the file becomes `Failed` with
`upload_expired`.
Attaching after completion starts the file at `Processing` or `Ready`.

**Protocol: an upload's state.**

- **Where it lives.** An upload's own state is an **upload record** in the
  uploading app's universe, in the hashed keyspace (§3), written through
  the WAL like any data. Begin writes it `pending`.
- **It moves once.** A record leaves `pending` exactly once, to
  `complete` (with the blob's hash and size), `expired`, or `failed` (with
  the verification error), and never changes again.
- **Attaching an incomplete upload queues the file.** The attaching
  mutation creates the file `Uploading`, with the upload's ID, and in the
  same mutation queues it in the file's own partition.
- **A worker finishes it.** A worker takes the queued file with `claim`
  (§8), a lease with an attempt generation, waits for the upload record to
  leave `pending`, and then moves the file to `Processing` or `Ready`, or
  to `Failed` (`upload_expired`, or the verification error), under its
  lease (`.under(&lease)`, §8), so a worker whose lease expired changes
  nothing. Since the
  record's state never changes after it leaves `pending`, a worker that
  looks late still sees the final state, so no file is missed
  (`Files_push` in `formal/tla`).
- **Completing an upload only wakes those workers.** Complete verifies
  the bytes, places the blob, and writes the record `complete`; it never
  writes the files themselves.
- **Clients never write blob keys.** Every presigned `PUT`, and every
  multipart part, targets the upload's own staging key. Only Shep copies a
  verified staged object to the blob's key. If a client could write a
  blob's key directly, a `PUT` that found the key already present would
  complete an upload without its bytes, and knowing a hash would be enough
  to get a file of that content (`Files_direct`).
- **Copies are checked when attached.** Attaching a copy re-checks, by
  the same rule as a link (§16.4), that neither the actor nor the viewer
  is deleted and that `can_op(app, actor, viewer, source)` still allows
  reading the source; otherwise the attach fails with `denied`. The source's facts are
  validated reads of the attaching mutation (§10.3), so a revocation that
  commits first wins. Like a link (§16.4), a copy stops working when
  access is revoked (`Files_revoke`).

**The client libraries do all of this.** App code passes content, and the
library picks inline, upload, or copy:

```rust
// Rust: from a path, a buffer, a stream, or another file.
let report = shep::Upload::from_path("q3-report.pdf").await?;          // media type from the extension
let msg = ops::post().channel(&channel_id).text("Q3 numbers").files(vec![report])
    .exec(&alice)
    .await?;                                     // uploads first if needed, then runs the mutation
```

```js
// Browser: from an <input type=file>, with progress.
const upload = db.Message.files.upload(input.files[0], { onProgress });
await db.post({ channel, text: "Q3 numbers", quotes: [], files: [upload] });   // attaches while uploading
await upload.done;
```

In a declared mutation, a file argument has the type `shep::Upload`
(`<shep::Upload>$file`, or `<array<shep::Upload>>$files` with
`shep::bounded`), and attaching it is an ordinary field write inside the
statement (§8, §13.5).

**What Shep verifies.**

- **Size.** The stored object must have the declared size.
- **Hash.** For a single `PUT` with a client hash, S3 has already checked
  it. S3 checks a multipart upload only part by part, since it offers no
  whole-object SHA-256 for multipart uploads, so a worker hashes staged
  uploads that S3 hasn't checked whole. A client-sent hash that doesn't
  match fails the file with `checksum_mismatch`, and the staged copy is
  deleted.
- **Media type.** Shep sniffs the content's leading bytes. A file declared
  `application/pdf` must look like a PDF. A mismatch fails the file with
  `media_mismatch`. Text types must decode as UTF-8, or Shep detects the
  encoding and keeps a UTF-8 copy of the text alongside the original.
- **The field's rules.** The media type must be one the field allows, and
  the size within its maximum. These are checked when a file is attached,
  so tightening them never invalidates files already stored. Like any
  tightening, the checker reports which in-service operations it could
  now reject (§4.4).

**Cleanup.** An upload that's never attached expires, and its staged copy,
if any, is deleted. A blob it completed, with no file pointing to it,
stays, because blobs are never deleted (§16.2). An object-store lifecycle
rule also aborts multipart uploads left incomplete, so nothing is billed
forever.

**Proxy mode.** Some deployments can't let browsers reach the object store.
In proxy mode, `begin_upload` returns a Shep URL, and Shep streams the
bytes through to the store. The client API doesn't change.

### 16.4 Downloads

A link is minted whenever an operation includes a file, and checked again
when it is used.

- **The link** is a Shep URL on a separate files domain, one host per
  universe: `https://shep-files.internal/f/<front end>.<handle>` for
  production and `https://<universe>.shep-files.internal/f/<front
  end>.<handle>` for the rest. The
  files domain is a different site from `shep.internal`, so downloaded
  content never shares cookies or storage with Shep or with apps.
- **The handle is opaque, not signed, and soft state.** Shep signs
  nothing to authenticate anyone (§6.3), so a handle is 128 random bits,
  prefixed with the ID of the front end that minted it. That front end
  keeps what it stands for in memory: `handle → (app, actor, viewer,
  file, token jti, expiry)`. The expiry is one hour by default, and never
  later than the minting token's own expiry.
  - **Using one.** The files host forwards the request to the named
    front end over shep-rpc. If that front end is gone, the link 404s,
    and the client library mints a fresh one.
  - **Cost.** Minting is a memory write, and using a link is one RPC hop
    to its front end. There are no WAL writes for handles. A restarted
    front end loses its handles, which costs clients a re-mint.
- **Links never cross universes.** A handle is looked up only in its own
  universe, on its own files host.
- **Using it.** The front end looks the handle up, rejects it if it's
  missing or expired, checks that neither the actor nor the viewer is deleted, runs
  `can_op(app, actor, viewer, file)` (§6.8), and then redirects to a presigned `GET` whose expiry is the front end's clock plus one minute less `2ε`, so it works for at most one minute after the check (`AuthHorizon_presign`), or
  serves a small blob from its cache.
- **Revoking access takes effect on the next use of the link.** The
  check runs at a version that contains the authorization horizon (§6.3),
  so an actor removed from a channel, or a deleted actor or viewer,
  can't use a link given an hour earlier, within the revocation bound. A
  revoked credential's links stop working when the minting token would
  have expired, since a link never outlives it.
- **A link is a bearer capability for up to an hour.** Anyone holding it
  can use it, as its actor, for as long as that actor can still read the
  file. Its expiry limits how long a leaked link works.
- **The redirect is a residual capability.** Presigned S3 URLs are the
  object store's own credential mechanism, not Shep authentication, and
  Shep keeps using them. Once the check passes, the presigned `GET` it
  redirects to works without Shep for up to one minute, for anyone
  holding it. Revocation can't reach it in that minute. Proxy mode, and
  strict revocation below, avoid presigned URLs entirely.
- **Strict revocation, for sensitive fields.** It's an annotation in a
  policy file, not in the schema, so it deploys like any policy and
  applies to every version (§6.2):

  ```
  # slack: shep/policies/message.esdl
  extend type Message {
    constraint shep::strict_revocation on (.files);
  }
  ```

  Downloads of that field then stream through Shep, as proxy mode does
  (§16.3), with no presigned URL. Revocation takes effect on the next
  request, at the cost of carrying the bytes.
- **Content never runs on an app's origin.** Downloads come from the files
  origin or from the object store, with `Content-Disposition: attachment`
  for active types such as HTML and SVG.
- **Links expire; files don't.** The client libraries mint fresh links for
  files whose links are about to expire, so an app that holds a page of
  messages for hours keeps working links: for live queries, `reauth`
  extends a subscription's handles to the new token's expiry (§6.3); for
  other reads, `db.refresh_links(files)` re-mints them. A live query mints links for
  each subscriber in phase 2, because a link names its actor (§13.2).
- **People.** `shep file get Issue:<key>.spec > spec.pdf` downloads as
  yourself (§7.2).

### 16.5 Processing and text extraction

**Processing** turns stored content into derived data. In v1 that means
extracted text, for file fields whose text some operation uses.

- **Driven by operations.** Shep extracts a file field's text only when
  some active or pending operation set searches the field (§16.6) or includes its
  `text`. Deploying the first such operation processes the field's
  existing files in the background, and the operation set activates once
  that's done, like an index build (§7.1). Until then, and for fields no
  operation needs, files skip `Processing`.
- **Once per content, not per file.** Derived data is keyed by the blob's
  hash and the extractor's version, and stored in the object store. A
  second file with the same bytes finds it there and goes straight to
  `Ready`.
- **A queue per partition.** A file waiting for its upload, or for
  processing, is queued in its own partition, by the mutation that
  attached it or by the worker that moved it to `Processing` (§16.3).
  Each worker claims from one partition's queue range at a time, which is
  `claim`'s scope (§8, §10.3), so a split only changes which partition a
  file is claimed from. If a worker dies, its lease expires and another
  worker takes the file. Processing is at least once: extraction can run
  twice, which is harmless because it's keyed by the blob's hash, and the
  file's final write runs `.under(&lease)`, so only the current lease
  holder completes it.
  - **For each file,** a worker looks for the blob's derived text. If
    it's missing, the worker extracts it and writes it, which is
    idempotent: two workers racing on the same new blob write the same
    object.
  - **Then it commits,** in the file's partition: the file moves to `Ready`,
    with its `text_status`, in a transaction with its own dot. The commit
    is conditioned on the file's `revision` (§8), so a worker that
    finishes after the file was detached changes nothing.
  - **Search** includes a file's text once the search index covers that
    commit's version (§16.6).
- **Workers are sandboxed.** Parsing PDFs from arbitrary uploads is risky.
  Extraction runs in `shep-extract`, a separate worker process with CPU,
  memory, and time limits, and no network access except the object store.
- **Background priority.** Processing runs in the background admission
  class (§15.2), so a burst of uploads never slows interactive traffic.
- **What Shep extracts:**

  | Media type | Text |
  |---|---|
  | `text/plain`, and source code | The text as it is |
  | `text/markdown` | The text with the markup removed, keeping code blocks |
  | `text/html` | The visible text |
  | `application/pdf` | The PDF's text layer. No OCR, so a scanned PDF is `Empty` |
  | Office formats | Later |
  | Images, audio, video, archives | None: `Unsupported` |

- **Limits.** Shep indexes up to 4 MiB of extracted text per file, about
  a 600-page book. Past that, `text_status` is `Truncated`, and the full
  text is still available through `text_url`.
- **Extracted text is a blob too,** stored once by its own hash and never
  deleted, and the log records its hash (§16.7).
- **The checker shows what won't be searchable.** An operation that
  searches a file field allowing media types with no extractor, such as
  `image/png`, gets a warning in `shep ops check` that names them.
- **Extractors improve.** Each extraction records its extractor's version.
  `shep file reprocess` re-extracts a field's files with a newer
  extractor, as background work.

### 16.6 Text search

Search runs on **search indexers**: Rust processes built on Tantivy. They
keep their indexes current by tailing the WAL, as view maintainers do
(§12). Materializers do no text search.

**Operations decide what's searchable.** Schemas don't mark fields
searchable. A search operation names the fields it searches and how to
analyze each, and Shep builds the index it needs, as it does for any
index (§7.1).

- **Analyzers.**
  - **Words, stemmed:** `english` (the default) or another language.
    "deploy" matches "deployed".
  - **Words, exact:** `simple`, with no stemming.
  - **Substrings:** `substring`, an n-gram index, for identifiers, file
    paths, and code, where `contains(.message, 'ParseErr')` should match
    inside a word. `contains` implies it.
- **File fields** are searched through their extracted text, which
  searching them turns on (§16.5).
- **One index per node type and shard, one indexed field per (field,
  analyzer, adapted form).** The indexed field is the unit operations
  share, build, and pay for (§7.1): every operation and app that searches
  a field the same way uses one. Which fields a query searches, and how
  much each counts, belong to the operation.

**Searching in operations.** `shep::search` is a query root, like a
filter on an index, and orders by relevance. Gel's `fts::index` declares
searchability in the schema; in Shep, operations decide, so the fields,
weights, and analyzers are arguments:

```
# shep/ops/search_channel.edgeql: messages in a channel matching $q, most relevant
# first; matches in the text count twice as much as matches in its files
select shep::search(Message, <str>$q,
                    weights := {(.text, 2.0), (.files, 1.0)},
                    analyzers := {(.text, shep::Analyzer.english)})
  { text, snippet := shep::snippet(.text) }       # the matching passage, highlighted
filter .channel = assert_exists(<Channel>$channel)
limit 50
```

```
# shep/ops/grep_commits.edgeql: commits whose message contains $part, as grep would find it
select Commit { sha, message }
filter .repo = <str>$repo and contains(.message, <str>$part)
order by .created_at desc
limit 100
```

- **Path arguments.** `weights` and `analyzers` are sets of
  `(path, value)` pairs; each path is a single-hop path from the searched
  type, rebound like `filter`'s `.`, to a `str` field or a file field. A
  path named in `analyzers` but not `weights` gets weight 1.0; a field in
  neither isn't searched.
- **Naming an analyzer.** `shep::Analyzer.simple`,
  `shep::Analyzer.substring`, or a language such as
  `shep::Analyzer.german`. English is the default.
- **Query syntax.** `q` is what a person types: words, `"quoted
  phrases"`, `or`, and `-excluded`, as in a web search. It never fails to
  parse.
- **Ordering.** By BM25 relevance, with ties broken by key. An explicit
  `order by .created_at desc` orders the matches by that instead.
- **`shep::matches(.text, q)`** is the same match as a plain filter, with
  no ranking, and `contains(.field, s)` is the substring filter.
- **Snippets** (`shep::snippet`) are computed only for the rows on the page. A snippet is
  plain text with the offsets of its matches, never HTML, so an app
  can't render a stored script by accident.

**What an index holds.** One index per node type that some operation
searches, per shard, holding for each node:

- its key and `revision`
- its indexed fields: each searched field's text once per analyzer and
  adapted form that operations use, and each searched file field's
  extracted text, up to the limit in §16.5
- **filter fields:** the fields that in-service search operations filter or
  order on, such as `channel` and `created_at`, kept as columns the index
  can filter and sort by
- **policy fields:** the fields and edges the type's read policy checks
  in rules that push down, such as a message's channel and author (§6.4),
  so the index can apply the policy's allow paths itself

Deploying an operation that needs a field the index doesn't hold adds
that field, and the operation set activates once the index has it (§7.1).

**How a search runs.**

1. A front end plans the request and sends the indexer the query, the
   operation's filters, and a privacy filter: the OR of every allow path
   in the read policy that pushes down, evaluated for the actor. For the
   Slack clone that's "channel is one the actor's viewer can read, or the
   author is the actor's viewer". If some allow path doesn't push down,
   the privacy filter can't stand in for the policy, so the operation's
   own filter must narrow the index (below), and candidates are filtered
   after loading.
2. The indexer returns candidate keys with their scores and indexed
   `revision`s, a few more than the page needs, from one point-in-time
   searcher. It ranks and cuts by the same score the cursor holds.
3. Shep loads the candidates from materializers at one version and applies
   privacy as for any read (§6.4), which also covers rules that don't
   push down. A candidate whose `revision` changed since it was indexed is
   matched again, against its current text, with the same analyzer.
4. If filtering left the page short, Shep asks for more candidates, within
   the request's budget (§15.2).

**Versions and freshness.** An index is a maintained view of the log, with
a causally closed version, like a materialized query (§12).

- **Two versions, both exact.** A response says which matches it found as
  of the index's version, and returns rows and privacy as of the version
  it read from materializers, which is at least as new. A row edited after the
  index's version is checked against its current text, so stale matches
  never appear. A row that only started matching after the index's version
  may be missing until the index catches up.
- **Read-your-writes.** An actor handle sends a search only its own writes
  as `at_least` (§9.4), and the search waits, with a bound, until the
  index covers them. A client that just posted a message finds it. Past
  the bound the search fails with `lagging`, or, if the operation is
  declared `stale_ok`, returns what the index has.
- **Lag** is reported per index, like a view's (§12), and `shep ops
  status` shows how long searches wait. A file's text is searchable once
  the index covers the version that made the file `Ready`.

**Search follows the same rules as every other read.**

- **Privacy.** Rows the actor can't read are filtered, and the privacy
  filter in step 1 keeps pages full. Rows that only an allow rule which
  doesn't push down would admit are still returned when the operation's
  own filter brings them in as candidates.
- **Hidden fields neither match nor rank.** A match counts only in fields
  the actor can read.
  - **When the field-level policy pushes down,** it is part of the
    per-row filter, and the indexer scores each row using only the fields
    visible to the actor in that row. Ranking, the cut, and the cursor all
    use that score, so a hidden match can't move a row up or push another
    row off the page.
  - **When it doesn't push down,** deploy rejects relevance ordering over
    that field. The operation must order by a field instead, or leave the
    field out of its weights.
  - **Matches in a file** count only if the actor can read the file.
  - **Corpus statistics include hidden content.** BM25's term statistics
    are computed over the whole index, so how rare a word is can reflect
    text the actor can't see. The leak is real, not theoretical: changing
    only hidden text can reorder an actor's results (`idf_outside_model` in
    `formal/lean`). That's accepted for now (open question 12).
  - **Field policies can't read hideable fields** (§6.6 item 7), or the
    visible-only score would depend on hidden content after all.
- **Older versions search through their adapters.** A version that reads
  `title` through `truncate(200)` matches only the first 200 characters.
  The indexer keeps an indexed field per adapted form that in-service
  search operations need, as materializers keep indexes over adapted
  values for filters (§7.5). A version can't search a field it doesn't have, such as a
  deprecated one.
- **Indexes follow operations.** A search index or field exists while an
  in-service operation uses it (§4.4). A new one is backfilled from a
  materializer snapshot, then follows the log, and the operation set activates
  once it has caught up (§7.1).

**Planning and cost.**

- **A search must be filterable in the index.** A common word can match
  millions of rows. So a search operation needs a filter the index can
  apply, either from the operation (`channel == $channel`) or from a read
  policy that pushes down. A search whose matches could only be filtered
  after loading them is rejected at deploy, by name.
- **Budgets.** Execution budgets (§15.2) cap the candidates a search may
  score and load. A query over budget fails with `budget_exceeded`, and the
  client narrows it.
- **Pages.** A cursor holds a row's score, the same one the indexer
  ranks by, and its key, and names the searcher of each shard it read,
  which stays open for a few minutes. BM25 scores depend on the
  whole index, so a later searcher would score differently. A page asked
  for after the searcher closes fails with `cursor_expired`, and the client
  starts the search again.
- **Not live, at first.** Search operations can't be `live` (open
  question 11).

**Running the indexers.**

- **Maintained like views.** An indexer steps between causally closed
  versions, usually the published frontiers, and its version is its
  progress (§9.7, §12). Facts that touch an index's fields, filter fields,
  or policy fields become index updates. It reads the entries of only the
  partitions its types live in.
- **Durable, but rebuildable.** Tantivy writes immutable segments.
  Indexers copy each committed segment set to the object store with the
  version it reflects, so a new or replacement indexer starts from the
  latest copy and replays the log from there. Losing every copy means a
  rebuild from a materializer snapshot, not lost data.
- **Replicas and shards.** Each index has at least two replicas, and
  front ends spread searches across them. An index that outgrows one
  process is sharded by placement group (§9.6), so a channel's messages
  stay in one shard.
- **Separate from the service.** Indexers are their own processes,
  `shep-search`, so search load never competes with request handling on
  front ends, or with reads on materializers.
- **Admission.** Search requests are priced by the indexer's measured cost
  per operation, and indexing runs alongside view maintenance (§15.2).
- **The engine is Shep's business.** Operations name fields, analyzers,
  weights, and filters, never Tantivy queries, so the engine can change
  without changing any operation.

### 16.7 Files in the log, live queries, and universes

- **The log.** Creating a file, each state change, and its extracted text
  are facts on the file's node (§10.1). Content is logged as a reference:
  the blob's `sha256` and size, never its bytes. Blobs never change and are
  never deleted (§16.2), so the log plus the universe's blobs rebuild it,
  contents included, at any point in time.
- **Live queries.** A file is a node that the query includes, so its state
  changes reach subscribers through phase 1 like any included node's
  changes (§13.2). The Slack clone shows "uploading…", then the file, with
  no extra code.
- **Universes.** Each universe has its own object-store prefix, keyed by
  its immutable ID, and its own blobs. Dropping a universe deletes them
  (§14.3). Test and dev universes start empty, like everything else in
  them: production content is never copied.
- **Admission control (§15).** Uploads are measured per app in bytes
  uploaded and bytes stored, and downloads in bytes served, so per-app
  byte quotas can be added when quotas are (open question 27).
- **Quotas count what an app references.** An app's stored bytes are the
  sizes of the blobs its files point to, whether or not another app
  stored them first. Deduplication saves the universe storage, but never
  makes one app's usage depend on another's.

### 16.8 Prior art

Every mature upload API uses the same three steps: create a pending
record and a short-lived target, send the bytes straight to storage, then
complete or attach. They differ in how they authorize downloads, track
state, and clean up.

| System | What Shep takes | What Shep does differently |
|---|---|---|
| **Slack** (`files.getUploadURLExternal`, then `files.completeUploadExternal`) | A mandatory completion step, and files that are discarded if never completed. Typed link fields on the file (`url_private`, `url_private_download`, `permalink`) | Slack authorizes each download with the caller's bearer token. Browsers can't add that header to `<img src>`, so Shep's links carry a signed token and recheck policy on use |
| **Convex** (`generateUploadUrl`, `_storage`, `getUrl`) | A file is a reference to a system table that holds `sha256`, `size`, and `contentType`. The closest analogue | `getUrl` returns a URL that isn't checked against the app's rules, and only deleting the file revokes it. Shep's links are checked on use (§16.4). Shep also stores identical content once |
| **Rails Active Storage** | Immutable blobs, separate from the attachment that points to one. A permanent link that redirects to a fresh 5-minute storage URL. Direct uploads that send a checksum first | Abandoned direct uploads need a cron job that the guide warns is slow. Shep's uploads expire on their own, and lifecycle rules reap abandoned multipart uploads |
| **Shrine** | Temporary storage, then promotion. The promoting job checks that the attachment hasn't changed (`atomic_promote`). Shep's worker commits on `revision` for the same reason (§16.5). MIME types sniffed, not trusted | Shep stages uploads too, but completing one moves the bytes to their content-addressed key, so attaching only records it |
| **Mux, Cloudflare Stream** | The upload and the processed asset have separate states (upload: `waiting`, `asset_created`, `timed_out`; asset: `preparing`, `ready`, `errored`). "Ready to serve the original" is kept apart from "derived data done" | Shep tells subscribers about state changes through live queries, not webhooks |
| **S3** | Presigned `PUT` up to 5 GB with `x-amz-checksum-sha256`. Conditional writes (`If-None-Match`). Multipart up to 10,000 parts. `AbortIncompleteMultipartUpload` lifecycle rules | Multipart uploads get only per-part SHA-256 checks, so Shep hashes them itself before registering the blob |
| **Supabase Storage** | Object metadata in the same Postgres as the data, with access by row policies | Supabase's signed URLs can't be revoked before they expire, and deleting a metadata row in SQL orphans the object. Shep's links are checked on use, and Shep doesn't delete blobs yet (§16.2) |
| **Git LFS** | `(sha256, size)` as a content reference | LFS skips any upload the server already has, so knowing a hash is enough to claim its bytes. Shep skips bytes only for a copy of a file the actor can read (§16.2). Content addressing also makes garbage collection a reachability problem, which GitHub's LFS never solved; Shep punts on it too, for now |
| **Facebook Haystack, f4** | The graph holds an opaque, immutable handle, and a separate store maps it to a location and builds URLs. Deletion is a flag first, with space reclaimed later | f4 deletes by destroying a per-blob key. Shep can adopt that for purge later |
| **Tantivy** | The engine: BM25, stemming per language, n-gram tokenizers for substrings, fast fields for filtering and sorting, and point-in-time searchers over immutable segments | Shep feeds it from the mutation log, versions each index, and applies privacy on materializer reads, not in the engine |
| **Quickwit** | Tantivy segments kept in object storage, so a new node starts from a copy instead of reindexing | Quickwit separates indexers from searchers. Shep's indexes are smaller, so its indexers serve searches too, at first |
| **Postgres full-text search** | Considered when Shep was designed on Postgres, and dropped. It would have kept search inside the transaction, but search load would have competed with writes, a `tsvector` must be under 1 MB, and ranking has no IDF | Search runs on its own processes and lags slightly, with an exact version |
| **ParadeDB `pg_search`** | Tantivy inside Postgres: the same engine Shep uses | It runs inside the database, is AGPL, isn't offered on RDS, and was removed from Neon in September 2026 |
| **Convex search** | Search that is transactional, consistent, and reactive. It works because every search needs equality filters and scans at most 1024 results | Convex orders only by relevance. Shep also allows ordering by a field |
| **Elasticsearch attachment processor, Apache Tika** | Extract text in a separate, resource-limited worker, and cap it (Elasticsearch indexes 100,000 characters by default) | Shep records truncation and failure on the file (`text_status`) instead of dropping them silently |

Sources: [Slack upload URL](https://docs.slack.dev/reference/methods/files.getUploadURLExternal),
[Slack complete upload](https://docs.slack.dev/reference/methods/files.completeUploadExternal),
[Slack file object](https://docs.slack.dev/reference/objects/file-object),
[Convex file storage](https://docs.convex.dev/file-storage/upload-files),
[Convex serving files](https://docs.convex.dev/file-storage/serve-files),
[Active Storage guide](https://guides.rubyonrails.org/active_storage_overview.html),
[Shrine backgrounding](https://shrinerb.com/docs/plugins/backgrounding),
[Mux direct uploads](https://www.mux.com/docs/api-reference/video/direct-uploads),
[Cloudflare Stream direct uploads](https://developers.cloudflare.com/stream/uploading-videos/direct-creator-uploads/),
[S3 object integrity](https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html),
[S3 limits](https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html),
[Supabase downloads](https://supabase.com/docs/guides/storage/serving/downloads),
[Git LFS spec](https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md),
[Haystack](https://www.usenix.org/legacy/event/osdi10/tech/full_papers/Beaver.pdf),
[f4](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/muralidhar),
[Tantivy](https://github.com/quickwit-oss/tantivy),
[Quickwit architecture](https://quickwit.io/docs/overview/architecture),
[Postgres text search limits](https://www.postgresql.org/docs/current/textsearch-limitations.html),
[Postgres ranking](https://www.postgresql.org/docs/current/textsearch-controls.html),
[ParadeDB](https://github.com/paradedb/paradedb),
[Neon pg_search](https://neon.com/docs/extensions/pg_search),
[RDS extensions](https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-extensions.html),
[Convex text search](https://docs.convex.dev/search/text-search),
[Elasticsearch attachment processor](https://www.elastic.co/docs/reference/enrich-processor/attachment).

## 17. Rust architecture

```
crates/
  shep             Rust client: Db, token providers, actor handles,
                   bookmarks, primitives, subscriptions and feeds, batch,
                   shep::fanout (re-exports macros)
  shep-macros      #[shep::ops] (collects the operation set), query! and
                   query_as! (offline type-checking with shep-ql against
                   committed SDL and IR, include_bytes! of .edgeql files,
                   rendered error snippets), derive(Row), create!/update!
  shep-ir          schema IR, operation-set IR, node/edge/field identities,
                   key encoding, serde
  shep-proto       native protocol: session handshake, operation index,
                   positional arguments and results, streams
  shep-ql          ShepQL: tokenizer (borrowed from Gel's edgeql-parser,
                   Apache-2.0), parser for .esdl and .edgeql, name
                   resolution, type, cardinality, and volatility inference
                   (simple scoping, totality, Pure), lowering to the
                   schema, query, transaction, and policy IRs, and the
                   printer (IR → SDL, used for every diff and report).
                   Replaces the earlier shep-sdl
  shep-codegen     operation set → JS client modules + .d.ts and .graphql
                   (served by shep-server, and by `shep gen ts` offline);
                   IR → Rust typed APIs (used by shep-build and
                   shep-macros)
  shep-build       build-script helper: generates typed Rust APIs from the
                   app's SDL and from pinned dependency schemas' committed
                   IR
  shep-expr        ShepQL expression evaluator, shep:: functions, cost
                   limits, built-in constraint forms, the SMT encoding of
                   the subset (option-lifting)
  shep-compat      adapters (built-in library + ShepQL), adapter proposals,
                   law proofs, chain composition and fusion, write guard,
                   backfill planner, reports
  shep-version     dots, dotted version vectors, contexts, causal
                   frontiers and closing versions, token encoding, merge
                   and compaction, lineage across splits
  shep-raft        Shep's multi-raft, forked from openraft 0.10.0-alpha.37:
                   openraft's engine and runtime (pipelined appends,
                   heartbeat workers, submitted vs. flushed I/O), changed
                   only where Redpanda differs: heartbeats batched per
                   node pair, lightweight heartbeats, follower-admitted
                   recovery, and the flush rules; each group on its
                   owning core's runtime; one log per group
  shep-wal         the WAL: Raft groups per partition on shep-raft, log storage,
                   leader validation (validation table over key ranges,
                   idempotency keys with failure records, void entries,
                   registry barriers, claim), field-operation resolution
                   with the last-value map, two-phase commit participant
                   and decisions, splits, segment archiving to the object
                   store; thread-per-core, one current_thread runtime per
                   core, with node-local core assignment of replicas.
                   Built on openraft 0.9; moving to shep-raft
  shep-rpc         Shep's RPC, copied from Redpanda's (src/v/rpc): framing,
                   pipelining with out-of-order responses, envelopes;
                   an enum over the network and in-process transports
  shep-wal-api     the WAL's RPC methods and envelopes: proposals with
                   key-range read and write sets, op and value records,
                   outcomes, and long-poll fetch of committed entries
  shep-wal-client  the client front ends and readers use: proposing,
                   leader discovery and `not_leader` retries, re-sending
                   with idempotency keys, and fetch-based tailing
  shep-value       a small shared value codec (scalars, lists, byte sizes
                   for size bounds), used by the WAL and shep-log, so the
                   WAL never depends on the delta format
  shep-materialize SlateDB per partition: key layout with stamps, applying
                   entries, derived index entries and ghosts, causal
                   frontiers, snapshot reads at a DVV, at_least waits,
                   in-doubt resolution, freshness checks, pushdown scans
                   with footprints, checkpoints, retention and compaction
  shep-log         mutation log entries: Z-set delta encoding, footprints,
                   contexts, log tailing for every reader, causal replay
  shep-privacy     policy IR (union minus deny), actor declarations,
                   inlined functions and policy pins, evaluator,
                   memoization, pushdown, partial eval, compilation to
                   relations (can_op as one anti-join, audience,
                   anchors), field-policy combination
  shep-query       query IR, planner, consistency classification and
                   disciplines (§7.6), hop executor, pagination, search
                   planning (filters, privacy filters, re-matching and
                   re-ranking candidates); operation sets: deploy-time
                   planning, index derivation and builds, re-planning on
                   a new head, cost estimates, ops check warnings
  shep-delta       DBSP compilation of queries to incremental plans; change
                   queries (changes, observe, resync); shared with views
  shep-views       materialized queries: DBSP runtime, maintainers that
                   step between causally closed versions, leases, lag
                   reporting
  shep-live        live queries: topics, live ranges, phase 1 (core deltas,
                   reference sets), phase 2 (permission routing, batched
                   visibility), streams, feeds, coalescing, resume
  shep-admission   resource accounting (task CPU, tracking allocator,
                   materializer and WAL statistics), cost models, quotas,
                   adaptive concurrency limits, priority classes, budgets
  shep-mutate      transaction IR, executor in front ends, assert ordering,
                   patch merging, field-operation detection, read sets and
                   which reads to validate, retries, idempotency, claim
                   proposals, two-phase commit coordinator and read-only
                   participants
  shep-files       File type, blobs stored once per universe in the object
                   store (hashing, no garbage collection yet), uploads
                   (copy, inline, presigned, multipart, proxy),
                   verification, download links, processing queue
  shep-extract     sandboxed extraction worker binary: text from plain
                   text, Markdown, HTML, and PDF
  shep-search      search indexer binary on Tantivy: causal log tailing,
                   analyzers, filter and policy fields, BM25 search with
                   per-field scores, snippets, segment copies to the
                   object store, replicas
  shep-wasm        wasmtime host, WIT interfaces for validators/mutation fns
  shep-graphql     GraphQL schemas per operation set and for ad-hoc
                   sessions (async-graphql dynamic), documents as
                   operations, subscriptions for live queries
  shep-server      the front end: axum, native protocol and streams,
                   WebSockets, bearer-token verification (JWS against the
                   auth service's JWKS), registry API, /g and /js routes,
                   files host and download handles, routing by host
  shep-controller  the WAL controller: its Raft group on every WAL node,
                   typed commands (nodes, universes, partitions, splits,
                   moves), the members and partition tables rebuilt from
                   its log, the per-node reconciler, the partition
                   balancer, founding and joins from the seed list, and
                   the metadata feed front ends cache
  shep-control     each universe's meta partition: the registry (versions,
                   policies, operation sets; apps and schema names in
                   production's),
                   registry barriers, the published frontier, universe
                   creation from a base checkpoint, TTLs and heartbeats
  shep-node        the binary: composes any set of components, from all
                   of them in one binary on one node with replication
                   factor 1 (a supported production deployment, and the
                   test harness) to one role per process (WAL and
                   controller, materializer, search indexer, view
                   maintainer, front end, extraction)
  shep-cli         deploy; schema check / register / status / show; ops
                   check / show / status; gen ts; deps add /
                   update; dev (remote dev universes); policy show /
                   matrix / explain / test; query /
                   shell / get; file get / reprocess; top; universe create / list / drop; log tail /
                   export / replay; reading the named #[shep::ops]
                   module

js/runtime/        browser runtime (GraphQL transport, auth, causal
                   position, subscriptions and feeds): plain ES modules
                   plus a hand-written .d.ts, embedded in shep-server with
                   include_str!
```

Building Shep needs only Cargo. There is no Node toolchain and no npm
publishing. The JS runtime is small enough to write as plain modern JS
with no bundler or TypeScript compiler.

Shep doesn't depend on any Toasty crate. Toasty is the model for the
generated Rust client API (§7.3), and code is copied from it wherever that
helps, but the two projects evolve independently.

Shep doesn't depend on Gel either. `shep-ql` copies Gel's tokenizer and
uses its grammar as a reference, with attribution; Gel's type and
cardinality inference is Python and isn't reused (§4.0).

Stack: tokio, axum, shep-raft (forked from openraft), slatedb,
object_store, async-graphql (dynamic), tantivy, z3 (or easy-smt with an external solver), wasmtime,
DBSP (Feldera's crate, or an in-house subset; §12). No gRPC, tonic, or
prost: internal RPC is Shep's own.

**Internal RPC, copied from Redpanda.** Components talk over `shep-rpc`,
modeled on Redpanda's `src/v/rpc`, not gRPC:
- **Frame.** A 26-byte little-endian header: version (1 byte), a
  crc32c checksum of the rest of the header (4), compression (1),
  payload size (4), the method ID on requests or a status on responses
  (4), a correlation ID (4), and an xxhash64 checksum of the payload (8).
- **Pipelining.** A connection carries many requests at once; responses
  come back out of order, matched by correlation ID.
- **Envelopes.** Payloads are serde-style envelopes: a `u8` version, a
  `u8` compat_version (the oldest version a reader must understand), and
  a `u32` size, then the fields. Fields evolve append-only: a newer
  writer adds fields at the end, an older reader skips what it doesn't
  know using the size, and a reader rejects an envelope whose
  compat_version is newer than its own version. That's the whole
  wire-compatibility rule between Shep's own processes.
- **No streams.** There are no server streams. Anything that would be one
  is a long-poll fetch, `(from_index, max_bytes, max_wait)`, as in
  Kafka's fetch: log tailing by materializers, search indexers, view
  maintainers, and front ends, and the feed between internal components
  for live queries.
- **Client-facing APIs keep their own transports:** the native protocol
  over HTTP/2 for Rust clients, and GraphQL over HTTP and WebSockets for
  browsers (§7.3, §7.4).

**Enums, not traits.** Shep prefers enums and enum dispatch to traits,
including for real and test implementations: the transport, the object
store, the clock, the token verifier, and the like are each an enum with
real and test variants (the verifier's test variant takes claims
directly, §6.3). The topology, one process or many, is chosen
separately from the variants (§9.6): a production single node uses the
in-process transport with the real object store and verifier. Which
variants exist in which builds is decided per case.

## 18. Milestones

1. **Done.** IR, storage, the Rust client, single-version CRUD, universes,
   and a replayable log, as implemented in this repository, on Postgres.
   The log is ordered by an advisory lock and a global `seq`.
2. **The WAL and materialization, in one process.** This replaces milestone
   1's Postgres storage while the code is small, because everything after
   it builds on the log and on versions:
   - the WAL: Raft groups per partition, log storage, and leader
     validation with the validation table (write sets entered at append),
     key-range read and write sets, idempotency keys with failure
     records, void entries, `not_leader`, `too_old`, and leader changes
     (§10.3). **Built today** (`shep-wal`, `shep-wal-api`,
     `shep-wal-client`, `shep-version`): those protocols on openraft 0.9,
     a multi-threaded runtime, and gRPC with tonic
   - **to replace what's built:** tonic with `shep-rpc` (§17); the
     thread-per-core runtime with node-local core assignment (§3, §9.9);
     `shep-raft` in place of openraft (§9.9); and `shep-controller`, the
     WAL controller (§9.6)
   - materializers: SlateDB per partition, the key layout with stamps,
     applying entries, derived index entries and ghosts, causal
     frontiers, snapshot reads at a DVV, `at_least`, and restart from the
     applied index (§3, §10.4)
   - dots as log positions, stamps, dotted version vectors with extras,
     closing versions, tokens, and contexts (§9)
   - node keys: `(type, key)` identities with generated, app-declared,
     composite, and content-addressed keys (§3), replacing milestone 1's
     UUIDs
   - field-level fact deltas with old values, nested map and set paths,
     container presence facts, atomic list values, and field-level
     footprints (§10.1, §10.2)
   - per-field dots, links with their `l` keys, local index entries, ghost
     index entries, and ghost links, which come with the key layout, and
     derived `revision` and `updated_at` (§3)
   - the single-node deployment, with the in-process transport and
     in-memory object store variants, which every later milestone's tests
     use (§9.6)
   - causal replay; the replay check carries over

   Tests to write here:
   - concurrent proposals with overlapping and disjoint read sets, ranges
     included: exactly the overlapping ones are rejected, including
     proposals validated while an earlier entry is appended but not yet
     committed
   - a leader change: the new leader rebuilds its validation table from
     committed and uncommitted entries before accepting proposals, and no
     conflict slips through
   - reads at versions with extras return exactly the facts whose stamps
     are in them
   - a materializer crash mid-apply: it resumes from its applied index,
     and reads at every version match a fresh replay
   - N concurrent mutations on disjoint keys all commit without retries
   - fault injection in the single-node deployment: worker death
     mid-claim, object-store stalls, leader changes during appends, and an
     isolated front end that can't reach a meta partition's leader, which
     must fail closed (§6.3)

   **Exit criterion: one vertical slice.** Mutations, snapshot reads, one
   live query, and recovery after each injected fault, end to end, with
   the replay check passing. Also measured here: apply throughput, storage
   per materializer replica, and rebuild time (§19).

   The TLA+ specs of the WAL, validation, and materializer apply are
   written with this milestone, and their configurations are the
   specification for these tests. The existing `DotFrontier`, `SerialLog`,
   and `MultiNode` models describe the Postgres-era protocol and are kept
   as history (§9.3). They don't validate the Raft protocols.
3. **Versioning, adapters, the checker, and cross-schema dependencies.**
   - **Head-defines-storage.** Patch writes, and the built-in adapter
     library with its laws and fusion rules proven up front.
   - **Chain composition.** Composition with the law re-checks that
     built-ins allow, the per-row write guard, and backfills for
     tightening.
   - **Authoring.** Adapter proposals in `shep schema check`, deprecated
     fields, built-in constraint forms compared without SMT (§4.5), the
     lockfile, version numbers assigned at registration, snapshots,
     rename detection, `shep schema check` / `register` / `show`, the SDL
     parser in `shep-ql` (Gel's tokenizer, a Shep-owned grammar) and the
     SDL printer, `shep-build` generating Rust types from SDL, and
     adapter files (ShepQL phase 1, below). Only built-in adapters
     register in this milestone; custom ShepQL adapters wait for the SMT
     encoding (milestone 14), since they can't be proven before it.
   - **Starts after the ShepQL prototype.** The kill-criterion prototype
     (below) runs before this milestone, so the SDL parser isn't built on
     a language that might be dropped.
   - **Dependencies.** `shep deps` pinning, `shep-build` generating
     dependency APIs, foreign links and computed backlinks, the registry
     dependency graph, and retirement blocking. Privacy needs
     them next: viewers are nodes, usually
     `users::User`, and policies follow edges to them.
   - **Tracking.** The in-service tracker.
   - **Registry barriers.** Barrier entries, the registry dot in every
     proposal's version, and backfills at a cutover (§10.3).
   - **Disciplines (§4.1.2).** `readonly` on properties and links and
     `shep::Permanent` on node types, checked against every writer
     (including backfills, write adapters, and sweepers) and enforced by
     front ends under the registry barrier, with weakening as a fenced
     registry change.
   - **Test and dev universes** become production plus an overlay.

   Two items finish in milestone 4, once operation sets exist: reporting
   which operations still use a deprecated field, and overlays that carry
   policies and operation sets.
4. **Privacy, queries, and operation sets.**
   - **The ShepQL checker (§4.0).** Simple scoping; type, cardinality,
     and Pure-volatility inference; totality; lowering to the query and
     policy IRs. This is ShepQL phase 2, with its kill criterion (below).
   - **The evaluator.** Policy IR, union minus deny with default deny,
     `shep::visible`, inlined functions, per-request memo, filtered
     pagination, and type-checking against the head's shape.
   - **Author tooling (§6.7).** This includes fixture tests, and property
     tests checked by bounded enumeration.
   - **Compilation to relations (§6.8).** This is needed by the audience
     queries in milestone 6 and by live queries in milestone 9. Until
     materialized queries arrive in milestone 10, recursion in policies
     must be depth-bounded.
   - **Operation sets (§7.1).** `.edgeql` operations, `shep::query!` with
     `#[shep::ops]` collection and offline checking, generated row and
     parameter types, primitives, registration, indexes derived from operations and built
     before activation, re-planning on a new head, cost estimates, the
     `shep ops check` warnings, and dormant sets (§4.6).
   - **Consistency classes (§7.6).** Fact classes, implicit reads,
     polarity per version, classes that follow data flow, the three
     predicates, pruned read sets and participant lists, the split read
     horizon, the two-step rollout for changes that alter a class, and
     `shep schema disciplines`. Test: classification-differential runs,
     skipping versus validating everything, under randomized concurrency,
     with outcome-serializable histories and every invariant holding in
     both.
   - **Walks (§7.1).** `shep::walk`, depth bounds, visited sets, `stop_at`,
     breadth-first pages, `truncated` results, and ad-hoc limits. Test:
     a walk never returns, counts, or measures depth through a node the
     actor can't see.
   - **Sessions and the native protocol.** Binding at connect, operation
     indexes, positional arguments and results.
   - **Actors, viewers, and keys.** Viewer types, requests carrying (app,
     actor, viewer), per-schema `global actor` declarations, deleted
     actors and viewers, actor types in operation sets, policy functions,
     and policy pins (§6.1); bearer-token verification against the auth
     service's JWKS, with claims, scopes, and expiry, and actor handles
     with token providers (§6.3, §7.3); bookmarks with separate write
     positions; universes picked by token; and `shep deploy`.
   - **The test harness,** with `seed` and the working tree's policies.
5. **Transactions.** Transaction IR, ShepQL mutations (ShepQL phase 3:
   DML, `assert` ordering, `if`, `unless conflict`, patch merging, key
   casts), `shep::batch`, execution in front ends with read sets and
   re-execution on `contended`, idempotency at the anchor partition,
   deferred checks, field operations detected from `x := .x + e` and the
   other forms and resolved at the leader (with op records, the leader's
   last-value map, and schema bounds checked at the leader; §8),
   `shep::claim` resolved at the leader with leases (`.under`, `renew`,
   `complete`, `lease_lost`, swept expiry), batched bulk ingestion,
   and placeholders created with `shep::placeholder` (§3). Placeholder
   tests:
   - placeholders are invisible to every read path: gets, lists,
     single links, `exists`, `.__type__`, counts, walks, search, live
     queries, and audiences
   - filling with equal data is a no-op, and with different data a
     `conflict`
   - a fill appears as a creation in change queries
6. **Change queries.** DBSP compilation to incremental plans (§11.5),
   `changes(Q, since)`, `live` declarations and their feeds
   (subscriptions follow in milestone 9), `shep::observe` in mutations,
   audience and
   anchored audience queries, and resync. The `ChangeQueries`
   configurations in `formal/tla` specify the tests: chains of deltas
   through polls, `observe`, archiving, and resync.
   - **Freshness checks (§11.7).** `shep::unchanged`, batched
     `unchanged`, and skipped resyncs past the hot window. They come here
     because they need query plans and compiled policies (milestone 4) and
     guards (milestone 5). Test: a check never says unchanged when a fresh
     run's result differs, by differential tests against re-running the
     query at `V` from a snapshot, including rows that leave a range,
     privacy changes, policy deploys, and new schema heads.
7. **Capacity and admission control (§15).**
   - Resource accounting per request: task CPU time, the tracking
     allocator, and materializer and WAL statistics.
   - Cost models corrected by measurement, execution budgets, connection
     caps, adaptive concurrency limits per partition, and
     priority classes.
   - The overload errors, and retry budgets in the clients.
   - Load tests past saturation.

   This comes before live queries, which add long-lived connections and
   fan-out.
8. **Files and text search (§16).**
   - **Text search.** `shep-search` on Tantivy: indexes derived from
     search operations, stepping between causally closed versions, with
     `at_least`, `lagging`, and `stale_ok`; search
     roots and filters, analyzers, BM25, snippets, privacy filters from
     every pushdown-able allow path, re-matching changed candidates,
     ranking by readable fields, cursors, budgets, and segment copies to
     the object store.
     This builds the stepping consumer that view maintainers reuse in
     milestone 10.
   - **Files.** The `File` type and file fields; blobs stored once per
     universe in the object store, never deleted; uploads (copy, inline,
     presigned, multipart, and proxy mode), verification, download links
     naming app and actor and checked on use, and the processing queue,
     driven by operations, with extraction once per blob.
   - **Extraction.** `shep-extract` with plain text, Markdown, HTML, and
     PDF, sandboxed, in the background class.
   - **Tests.**
     - Searches never match, or rank by, text the actor can't read, and
       return every row a plain read would.
     - Links stop working when access is revoked.
     - An upload attached before it completes ends `Ready` or `Failed`,
       never stuck.
     - The same bytes uploaded twice, concurrently or not, are stored
       once.
     - An actor who knows a blob's `(sha256, size)` but can't read any
       file that points to it can't obtain its bytes.
9. **Live queries (§13).**
   - **Verification first.** A TLA+ spec of the stream: no gaps, causal
     order, increasing versions, revocation before any later push, live
     ranges, steps across registry and policy changes, resync on a
     hot-window miss, and streams of deleted actors closing before any
     later push.
   - **The machinery.** Topics, live ranges, phase 1 with reference sets,
     phase 2 with permission-change routing and batched visibility checks,
     streams for the Rust client, resume, coalescing, and subscription
     admission.
   - **Differential tests.** At every version, a subscriber's view must
     equal a fresh run of the query as that subscriber, over its range.
   - **Demo.** The Slack clone's backend logic, with Rust subscribers
     standing in for browsers: quotes across channels, membership changes,
     and writes from other apps.
10. **Materialized queries.**
   - **Maintenance.** The DBSP runtime decision (Feldera's crate or an
     in-house subset), and maintainers that step between causally closed
     versions, with leases.
   - **Reads.** `#! shep: materialize`, `at_least` reads, change queries over
     views, and live queries over views.
   - **Permissions.** Materialized `can_read`, and with it recursive
     policies.
11. **Archive, checkpoints, and recovery.** Log segments archived to the
    object store, SlateDB checkpoints, retention floors, materializers that
    start from a checkpoint and the archive, point-in-time restore within a
    universe, and `shep log export` / `replay`.
12. **GraphQL, TypeScript, and people.**
    - GraphQL per operation set, with documents as operations, served JS
      modules, offline TypeScript generation (`shep gen ts`) from
      `.edgeql` files (ShepQL phase 4), browser tokens from the auth
      service (verification itself lands in milestone 4), and GraphQL
      subscriptions for live queries.
    - Ad-hoc sessions: `shep query` and `shep shell` in ShepQL, `shep
      get`, and GraphiQL, in their own admission class.
    - The Slack clone's browser front end subscribes to live queries
      directly.
13. **Distribution and scale-out.**
    - **Components in their own processes.** The WAL cluster with many
      Raft groups per node, materializers with replicas, search indexers,
      view maintainers, and front ends, deployed separately, with the
      WAL controller on every WAL node, founded from the seed list (§9.6).
    - **Many partitions per universe.** Routing from keys, the partition
      map, splits at group boundaries by size and load with the placement
      balancer on the WAL controller, closing versions across partitions, the published
      frontier (§9.7), and live-query topology: topic routing and stepping
      by partition (§13.3).
    - **Two-phase commit (§8).** One dot per transaction, read-only
      participants, and the hashed keyspace for global uniques and upload
      records (§3). It lands here because until a universe has several data
      partitions, no mutation spans two. The test harness puts each
      placement group in its own partition, so it's exercised from the
      start of this milestone (§14.3).
    - **Benchmarks.** A hot channel (one giant group's write ceiling and
      its live fan-out), a large monorepo (SCM ingestion into one group),
      and broad revocation during a reconnect storm (many subscribers
      losing access while reconnecting).
    - **Verification first.** TLA+ specs of two-phase commit (one dot,
      atomic visibility through causal closure, in-doubt resolution,
      recovery after a coordinator crash, intents released), of splits (no
      lost writes, stale maps caught, versions across lineage), and of the
      frontier, before the code.
14. **ShepQL → SMT, wasm validators and mutation functions, pushdown for
    policies.**
    - the SMT encoding of the ShepQL subset by option-lifting (§4.5,
      ShepQL phase 5)
    - custom ShepQL adapters, with proofs
    - the solver-backed policy checks: writer lockout, diff reports as
      `(∨A) ∧ ¬(∨D)`, and proofs of property tests

**Testing.** Shep's tests follow one layout: no unit tests; each crate's
tests in `crates/<c>/tests`, against its public API; end-to-end tests in
`tests/test-<aspect>`, against `shep-node`. Where another project has
already written the tests a component needs, Shep imports or adapts them
rather than writing new ones. Deterministic simulation isn't used for
now: the TLA+ specs, imported suites, and fault-injection tests cover
that ground.
- **Raft (`crates/shep-raft/tests`).** openraft's whole test suite is
  vendored in the fork, engine and integration tests alike, and all 837
  pass. Redpanda's Raft tests (`src/v/raft` tests: leadership transfer,
  replication with failures, configuration changes, recovery, and
  heartbeat batching) are ported where they test what the fork changes:
  batched heartbeats, flush rules, and follower-admitted recovery.
- **RPC (`crates/shep-rpc/tests`).** Redpanda's `src/v/rpc` tests:
  framing and checksums, out-of-order responses, envelope versioning and
  compat_version, timeouts, and connection loss mid-request.
- **Controller (`crates/shep-controller/tests`).** Adapted from Redpanda's
  `src/v/cluster` tests for the topic table, members table, controller
  backend reconciliation, partition moves, and cluster discovery, plus
  Shep's own split commands.
- **WAL (`crates/shep-wal/tests`).** The configurations of
  `WalValidate.tla`, `TwoPhase.tla`, and `FrontierMaterialize.tla`, each
  as a test, and Jepsen-style histories checked with Elle for
  serializability under injected faults.
- **SlateDB and Tantivy.** Their own suites run in CI against the pinned
  versions (§19).
- **End to end (`tests/test-<aspect>`).** By aspect: `test-replay`,
  `test-privacy`, `test-live`, `test-files`, `test-search`,
  `test-cutover`, `test-faults`, and `test-single-node`.
- **The single-node deployment is tested as a deployment.**
  `tests/test-single-node` runs the full binary with replication factor
  1 as production would, over a local-directory object store: restart
  and recovery from the local log and the archive, a split with both
  children on the node, growing to three nodes by joins and moves, and
  the same consistency checks as the multi-node suites. Every other
  end-to-end suite also runs against it, since it's the harness's
  default.

**ShepQL phases.** The language (§4.0) lands across milestones:

- **Phase 0 (design, now).** The dialect spec (§4.0, §21), the policy
  semantics (§6.1), the Lean restatements below, and the design text.
- **The prototype, before milestone 3.** A bounded prototype of the
  checker (4–6 weeks): parsing, simple scoping, type and cardinality
  inference, and totality, over the examples in §6.1, §7.1, §8, §11.3,
  §11.7, §13.5, and §16.6.
  - **Kill criterion.** If it can't type those examples with good errors,
    and reject the negative tests below with the right message, fall
    back: EdgeQL for queries and mutations only, with a smaller
    Shep-designed syntax for schemas and policies.
  - **Negative tests,** each a mistake this design's own drafts made:
    `assert(i.assignee = global viewer)`, where the assignee can be
    absent (must say: not One; suggest `?? false`), and `?? false` on
    `global viewer in ch.members` (must flag it as redundant); writing `i.project` from a
    key-cast issue into a required link (optional, unless proven visible,
    §4.1.1); `<optional Issue>$issue` passed to a single-target update;
    `global actor` written to an `agents::Agent` link without
    `[is agents::Agent]`; a deny guard fixed with `?? false` (must
    suggest `?? true`); `.x + 1` on an optional field claimed as
    leader-resolved; the same field assigned twice; an `assert` message
    named `not_found`.
- **Phase 1 (milestone 3).** The SDL parser and IR, built-in constraints
  compared structurally, `shep-build` generating Rust types from SDL,
  built-in adapters in `shep/adapters/`, and `shep schema show` printing
  SDL.
- **Phase 2 (milestone 4).** The full ShepQL checker in Rust: simple
  scoping, type, cardinality, and Pure-volatility inference, totality,
  the two read contexts, and lowering to the query IR; `#[shep::ops]`
  with `query!` and `query_file!`; policy files, inlined functions,
  `shep::visible`, `shep::walk`, and `shep::search`.
- **Phase 3 (milestone 5).** Mutations: DML, `assert`, `if`,
  `unless conflict`, reads at `S` and patch merging, field-operation
  detection, `shep::claim`, `shep::observe`, `shep::unchanged`, key
  casts, and `shep::placeholder`.
- **Phase 4 (milestone 12).** Offline TypeScript generation and served
  modules from the same generator; `shep query` in ShepQL.
- **Phase 5 (milestone 14).** The SMT encoding of the subset, and custom
  adapters.

**Formal work.** Each distributed protocol gets a TLA+ spec before its
code, with the milestone that builds it: the WAL, validation, registry
barriers, and materializer apply in milestone 2, two-phase commit, splits,
and the frontier in milestone 13, and the live-query stream in milestone
9. `Freshness.tla` in `formal/tla` already checks freshness checks under
optimistic concurrency with per-partition logs, and replaces the
Postgres-era assumptions for that feature (§11.7). `Files.tla` checks
uploads, copies, and their workers (no file is left `Uploading`, and
knowing a hash grants nothing, §16.3), and `LiveRevocation.tla` checks
that revocations and deleted actors and viewers reach live streams
before any later push (§13.2).

**Specs in progress**, in `formal/tla`:
- `SegmentLog.tla`: one group's segment log and kvstore through crashes:
  the three offsets, suffix and prefix truncation, the clean-shutdown
  marker, and snapshot install, with reader configs for concurrent reads
  against the `truncating` flag and epoch, checking that no read mixes
  sides of a truncation (§9.9).
- `RaftDriver.tla`: `shep-raft`'s runtime with the fork's changes:
  pipelined appends, commit over flushed offsets clamped to the term,
  batched heartbeats, prevote and stickiness, and follower-admitted
  recovery (§9.9).
- `CoreMove.tla`: moving a replica between cores with persisted
  markers, crashes between steps, and a core-count change at restart
  (§9.9).
- `ControllerReconcile.tla` and `Split.tla`: the controller fold, moves
  with cancellation, vnode revisions, and the split protocol with `p`'s
  log deciding between the split entry and `split_aborted`, routing from
  the split entry, and handoff by any later leader (§9.6). `Split.tla`
  also covers lineage and the physical winner (SR-1); its earlier
  version modeled the placement controller in the meta partition.
- `RegistryBarrier.tla` (barriers and backfill).
- A re-run of `WalValidate.tla` with validation and append as one atomic
  step, as `shep-raft` does them, to check whether the void-entry rule
  (§9.1) is still needed, and the fast path for client-visible failures
  (§10.3), which is pending re-check against `_fail` and `_probe`.

`WalValidate.tla` (one partition's Raft group with elections,
truncation, and snapshot install, under §10.3's validation, idempotency,
`too_old`, retries, and claims with leases, §8, §10.3) is checked: seven
configs pass, up to 15.5M distinct states, and its failing configs are
the counterexamples behind §10.3's rules: write sets entered at append
(`_c1`, `_claim_c1`), the window after truncation (`_trunc`; the fix
`_truncfix` passes), failures reported through the anchor (`_fail`,
`_probe`), the idempotency check for claims (`_claim_idem`), and the
necessity of rebuilding on leader change, checking idempotency at
append, range predicates, `too_old`, and the lease fence (`_norebuild`,
`_idemcommit`, `_norange`, `_notooold`, `_claim_nofence`). Findings are
in `formal/tla/FINDINGS-wal.md`. It doesn't model two-phase commit,
barriers, splits, membership changes, pipelining, or real time.

`TwoPhase.tla` (two-phase commit with anchor homes, deadlines,
forgetting with lookahead, read intents, barriers, and splits, §8) is
checked: 12 configs pass, and its 14 failing configs are the
counterexamples behind §8's rules (`_forgetearly`, `_nodeadline`,
`_nolookahead`, `_firsthome`, `_barrierlog`, `_splitlost`, `_twohomes`,
`_localstamp`, `_noprepdot`, `_noreadonly`, `_wait`). Findings are in
`formal/tla/FINDINGS-2pc.md`.

`FrontierMaterialize.tla` (closure, the published frontier, materializer
restart, retention and compaction, in-doubt resolution, §9.4, §9.7,
§10.4) is checked: five configs pass, including liveness, and its
failing configs are the counterexamples behind the release-context rule
(`_relctx`), closed contexts (`_once`), the frontier union (`_nokeep`),
in-doubt resolution (`_noindoubt`), `m/applied` durability (`_restart`),
and pinned floors (`_floor`). Findings are in
`formal/tla/FINDINGS-frontier.md`.

**Checked**: `LiveFrontier.tla` (live topics stepping between published
frontiers: no gaps, increasing versions, revocation before any later
push, joins that wait for the topic; `LiveFrontier_lagjoin`,
`_maxima`, `_snapshot`) and `Bootstrap.tla` (seed lists outside the
system partition; `Bootstrap_selfmember`). Findings are in
`formal/tla/FINDINGS-live-bootstrap.md`, including how app and schema
names flow between fleets: registered in production first, and only
followed elsewhere (§14.1, `Bootstrap_forward`, `Bootstrap_local`).
`Bootstrap.tla` models the old system partition on seed nodes; it needs
updating for the controller group on every WAL node, founded from the
static seed list and changed only through its own joint consensus
(§9.6). The name-flow results carry over, with production's registry in
place of its system partition.

`AuthHorizon.tla` (the authorization horizon, §6.3) is also checked. With
the fixes above it passes. Its failing configs are the counterexamples
behind §6.3's freshness test (`_nodrift`, `_deposed`), the materializer
version rule (`_topicchan`, `_streamlag`, `_nomatcheck`), the horizon
deadline on proposals and decisions (`_inflight`, `_reuse`), and the
presigned expiry (`_presign`). Findings are in
`formal/tla/FINDINGS-auth-horizon.md`. The model predates the
credential-based actor model (§6.3): its `map` partition corresponds to
the viewer's partition. Its `sys` partition's key-revocation case is
superseded: credential revocation is the auth service's, bounded by
token expiry, and Shep keeps no key state to tail (§6.3). Its
`AuthHorizon_viewerapponly*` configs model
the old rule that an app-only allow matches only the app acting as
itself, which §6.1 drops; they're kept as history.

`Cutover.tla` (the four-state schema cutover, §4.2, with splits, barrier
re-delivery, backfill cursors, freshness and topics, and test bases) is
also checked: `Cutover.cfg` passes all 15 invariants (8.4M states), and
`Cutover_live` passes backfill liveness. Its failing configs are the
counterexamples behind §4.2's rules: enforcing new set-level constraints
from the index barrier (`_dualunique`, `_noenforce`, `_noactunique`),
patches valid for both heads (`_putlossy`), backfill reads and skips
(`_bfsnap`, `_blindbf`), barriers reaching split children
(`_noredeliver`, `_cursorlost`), the activation context (`_actctx`), the
point of no return (`_abandonbf`), lenses chosen by the registry at V
(`_lenskey`, `_abandoncount`), and test bases (`_base`).
`Cutover_lensact` shows switching lenses at activation is sound. Findings
are in `formal/tla/FINDINGS-cutover.md`. It models a tightening step only.

**Proven in Lean**, with no `sorry` and standard axioms only
(`formal/lean/FINDINGS.md`, S1–S6):

- `Stamps.lean`: a causally closed version's state is valid with
  stamp-based visibility (`wf_stateAtS_closed`), a two-phase commit's
  parts are all visible or none (`twoPC_atomic`), and local stamps would
  break that (`localStamp_not_atomic`) (§8, §9.5)
- `Validation.lean`: validation against every appended entry gives a
  serializable log (`serializable`), and validating only committed
  entries doesn't (`committedOnly_unsound`) (§10.3)
- `Closure.lean`: closing a version terminates at the least closed
  version above it, when each round adds `cf` and the extras' contexts
  (`stepDVV_eq`, `iterate_stepPre`, `stepPre_unsound`) (§9.7)
- `Backfill.lean`: backfill at a barrier plus maintenance equals the full
  index (`backfill_correct`), and ghosts are trusted only from each
  partition's barrier (`backfilled_check_sound`, `ghosts_incomplete`)
  (§11.7)
- `Lineage.lean`: the physical winner rule (`winner_correct`,
  `localPos_unsound`, §3), and folding tokens across splits
  (`fold_exact`, `fold_grows`, §9.6)
- `Planner.lean`: skipping a Stable guard preserves outcome-serializability
  (`outcome_serializable`, `readAt_le_serial`, `stab_upClosed`), and a
  guard over a mutable lifecycle doesn't (`mutableLifecycle_unsound`)
  (§7.6). Restated over `Bool` for ShepQL's two-valued guards
  (`gstab_sound`, `stab_neg_closed`; `trueDefault_unsound`).
- `LeaderOps.lean`: the context rule, with either context form, gives
  every condition of `wf_stateAt_closed` (`leader_wf`, `ctx_closed`,
  `selfPrefix_closed`, `ctx_pathDisjoint`); resolution at the log
  position never crosses a bound under pipelining (`bounded_leader`);
  counterexamples `ctxOmitPrev_not_set`, `unrecordedRemoval_not_set`,
  `committedResolve_crosses_bound` (§8, §9.5, §10.3)
- `Polarity.lean`, restated for union minus deny and two-valued guards,
  with every allow Stable: a decision that isn't RevocationSensitive
  never over-grants when stale, with or without a viewer (`vis_mono`,
  `history_admit_mono`, `fieldVis_mono`); counterexamples
  `denyMutable_overgrants`, `allowMutable_overgrants`,
  `ignoreVisible_overgrants`, `denyDelegate_overgrants`,
  `fieldDenyMutable_overgrants`; polarity through `not`
  (`port_visible_safe`, `port_safe`) (§6.1, §7.6). The CEL model is in
  `PolarityCEL.lean`, superseded.
- `Totality.lean`, `PolicyCombine.lean`, `Checks.lean`,
  `PatchMerge.lean`, `LeaderOps.lean`'s `FieldOps`, and
  `PlaceholdersQL.lean`: the ShepQL restatement round's other results,
  listed with the obligations below.
- `Disciplines.lean`: front-end discipline checks keep frozen facts at
  their creation value and permanent lifecycles unretracted
  (`frozen_eq_creation`, `present_mono`, `ty_unique`); counterexample
  `weakFrozen_unsound` (§4.1.2)

**Not yet proven:** composite indexes over several fields; the step
from each participant's validation to the validity of a two-phase
commit's merged delta; the planner results with more than one partition,
including cross-partition expansion of the self-prefix flag; a pending
`prevₖ` that is a prepared two-phase commit intent; `claim`; the link
between the key-value check `Planner.Respects` and the fact-level checks
`FrozenOK`/`PermOK` (argued, not proven); function inlining and
`shep::walk`'s `stop_at` in the totality model; guards that read other
nodes' fields in `Polarity.lean`; int64 overflow in leader operations;
and the liveness of the work
queue, which `WalValidate.tla` couldn't check. A claimed task eventually
completes only if workers retry after an empty claim, a lease lasts
longer than a task's completion time plus clock skew, and every leader
runs the lease sweeper (§8).

**Still to write:** `claim` leases (§8), shape-changing cutover steps
(§4.2), the interactions two-phase commit × registry barrier and split
× registry barrier, and a **controller-reconciler spec** (§9.6): the
controller log's start and finish commands, every node's reconciler
creating and removing replicas, a crash between start and finish, a new
controller leader resuming a split or move with fresh balancer state,
and replicas fenced by their creation revision. For the
consistency planner (§7.6), the TLA+ parts: skipping Stable guards
across partitions, field operations under pipelining with leader changes,
and the two-step rollout of a discipline weakening (`Planner.tla`, in
progress). Its Lean obligations are proven (above).

The Lean model in
`formal/lean` (no `sorry`, no new axioms) covers the semantics these
protocols rely on, and this revision's rules cite it:

- `Causal.lean`: every causally closed version is a valid state
  (`wf_stateAt_closed`), stated over abstract dots, so it holds unchanged
  with one dot per two-phase commit; merging closed versions keeps them
  closed (§9.5). Its scope: it assumes each dot's delta is a valid
  transition from the state at its context, that contexts are suitable,
  and that concurrent dots don't write the same key, and it proves that
  closed versions are sets, functional per field, and tree-shaped. It
  doesn't prove that the distributed protocols establish those
  assumptions, or anything about serializability, authorization
  freshness, recovery, or splits. Stamps, validation, closure, backfill,
  and splits are proven in the files above, and the TLA+ specs in progress
  cover recovery and the protocols' message flows.

- `Freshness.lean`: freshness checks are sound when the planner records
  ranges and every candidate's inputs, and unsound without ghosts or when
  checking only returned rows; compacting ghosts by a per-partition
  maximum is sound for prefix versions and unsound under dotted version
  vectors (§3, §11.7)
- `Placeholders.lean`: placeholders read exactly as absent keys, and a
  fill produces the same change as a create (§3)
- `Walks.lean`: a walk over the visible subgraph equals the walk with
  privacy checked per hop, including depth, and budgets must count only
  visible nodes (§7.1, §15.2)
- `SearchPrivacy.lean`: ranking by visible fields is unaffected by hidden
  content, given that field policies don't read hideable fields; corpus
  statistics do leak it (§16.6)
- `Principals.lean` (supersedes the mapping part of
  `ViewerMapping.lean`): credentials fix the actor and the viewer. These
  results are about `(actor, viewer)` as a credential states them, so
  they hold unchanged with tokens from an external auth service. Proven
  in both its first-match model and its `UMD` model (union minus deny,
  two-valued guards, an atom on an absent principal false):
  `canOp_deleted_actor`, `canOp_deleted_viewer`,
  `canOp_wrongtype_viewer`, `actorsDecl_enforced`,
  `visible_preserves_viewer`, `visible_crosstype_denies`, and
  `canOp_actfor_no_amplification` (an agent acting for a user never does
  more than the user, given no actor-reading allow and, under `UMD`, no
  `visible` in a deny; counterexample `visibleDeny_amplifies`) (§6.3).
  - In `UMD`, `visible` over an absent link is false; in the first-match
    model it was a deny.
  - **Pending:** `UMD`'s guards are conjunctions of atoms, with no `not`
    or `or`, so the per-atom rule is proven only for them; and its
    `appOnly` guards and `canOp_appOnly_with_viewer` model the app-only
    rule §6.1 drops.
  - Its `Issuance` section (two-party act-for keys,
    `actfor_key_two_party`, `bound_key_inert`) models key issuance inside
    Shep, which is now the auth service's, and is kept as history.
  - `LiveRevocation.tla`, `LiveFrontier.tla`, and `AuthHorizon.tla` still
    need updating to the credential model.

**Obligations for the Lean restatement round (§4.0, §6.1, §8):**

Every file uses only the axioms `propext`, `Classical.choice`, and
`Quot.sound`; findings are in `formal/lean/FINDINGS.md`, "ShepQL
restatement round".

1. **Done: `Polarity.lean` restated** for two-valued guards and union
   minus deny, with every allow Stable (`vis_mono`, `history_admit_mono`,
   `fieldVis_mono`; counterexamples `denyMutable_overgrants`,
   `allowMutable_overgrants`, `ignoreVisible_overgrants`,
   `denyDelegate_overgrants`, `fieldDenyMutable_overgrants`; polarity
   through `not`, `port_visible_safe`, `port_safe`). The CEL model is
   kept as `PolarityCEL.lean`, superseded; `ErrMode` and
   `allowError_overgrants` no longer apply.
2. **Done: `Planner.lean` over `Bool`** (`gstab_sound`, `stab_upClosed`,
   `stab_neg_closed`; counterexample `trueDefault_unsound`, §7.6).
3. **Pending: the per-atom absent-principal rule** over `Principals.lean`'s
   `UMD` guards, with `not`, `and`, and `or`, dropping the app-only
   guards. (The typing side is done: `viewerIn_one`,
   `viewerIn_not_differs` in `Totality.lean`.)
4. **Done: totality inference is sound** (`decision_two_valued`,
   `card_sound`, `total_eval` in `Totality.lean`; `placeholder_hop_not_total`).
5. **Done: field-level combination** is default deny and never exceeds
   the type (`field_default_deny`, `field_le_type`,
   `field_type_default_deny` in `PolicyCombine.lean`; `field_not_replace`,
   `first_field_allow_narrows`, §6.1).
6. **Done: the check encoding** (`enc_agrees`, `check_iff`, `guard_iff`
   in `Checks.lean`; `ite_naive_disagrees`, §4.5).
7. **Done: patch merging ≈ PutPut** (`merge_eq_seq`, `mergeAll_perm` in
   `PatchMerge.lean`; `merge_accepts_invalid_prefix`, §8).
8. **Done: leader operations equal the evaluator** (the `*_eq` theorems
   and `resolved_eq_eval_at_logpos` in `LeaderOps.lean`, `FieldOps`;
   `optionalArg_unsets`, `max_optionalArg_unset`, `add_max_not_comm`,
   §8).
9. **Done: placeholders read as absent in ShepQL,** in both read
   contexts (`evalQ_placeholder_eq_absent`, `typeFact_eq` in
   `PlaceholdersQL.lean`; the premise is necessary,
   `lifecycleExists_distinguishes`).
10. **Done: the Ent port is exact** (`port_exact`, `port_denies_first`,
    `port_entDefault` in `PolicyCombine.lean`, §6.1).

The TLA+ models are protocol models with no dependence on the policy or
query language, and are unaffected.

The earlier files cover the Z-set calculus, documents, change queries,
DBSP, and adapters.

## 19. Risks

- **Building a storage system.** Shep now owns replication, recovery, and
  storage instead of inheriting them from Postgres. Correctness under
  crashes, partitions, and leader changes is where storage systems spend
  their lives. The mitigation is TLA+ specs for every distributed protocol
  before its code (§18), fault-injection tests of the single-node and
  multi-node deployments, imported test suites, and the replay check on
  every test universe. Deterministic simulation isn't used for now.
- **Owning Raft, and SlateDB's maturity.** `shep-raft` is a fork of
  openraft 0.10 with targeted changes (heartbeat batching, recovery
  admission, flush rules), not a new runtime, so the risk is smaller
  than owning a Raft implementation: Shep owns the changed parts, and
  all of openraft's 837 tests pass in the fork, with Redpanda's Raft
  tests ported for the changes (§18). SlateDB is young. Shep depends on SlateDB's
  compaction, checkpoints, and object-store behavior. Shep's compaction
  needs to keep each key's newest version and every ghost while dropping
  older versions (§3), which may need a compaction hook upstream. Pin
  versions, run their test suites in CI, and keep both behind Shep's own
  enums or narrow wrapper types.
- **A single node's recovery point.** A single-node deployment loses
  everything committed since the last archived segment if its disk dies,
  and everything if the object store shares the disk (§9.6). Archiving
  by age bounds the loss to about a second with an object store
  elsewhere; installations that can't accept even that need three nodes.
- **Read latency on cache misses.** SlateDB serves reads from memory and
  local disk caches, but a miss goes to the object store, which takes tens
  of milliseconds. Hot partitions need warm caches and materializer
  replicas, and admission must price cold reads (§15).
- **The validation window.** A proposal whose read version is older than
  the leader's validation table is rejected with `too_old` (§10.3). Long
  mutations, or materializers that fall behind, turn into retries. Size the
  table by measurement, alert on `too_old` rates, and keep apply lag low.
- **Two-phase commit blocking.** Prepared intents block conflicting writes
  until the release, and a point read whose version holds a decision
  waits for the release. The home partition decides, so a coordinator
  crash can't block forever, but an unavailable home holds intents for as
  long as it's unavailable: the prepare timeout only starts recovery (§8).
  A split of a partition holding prepares waits for them too: while it
  drains, new two-phase commits touching the partition get a retryable
  `splitting` and wait, single-partition writes continue, and the split
  can't proceed until the prepares are released (§9.6). Placement keeps two-phase commits rare, and `shep ops check`
  names every operation that can need one, including those that only
  validate reads in another group.
- **Splits.** Moving a key range to new partitions has to keep stamps,
  versions, and the partition map consistent while writes continue, with
  the decision split between the parent's log and the controller.
  `Split.tla` specifies it (§18). Merges and splits inside
  a group are left out of v1 to keep it small.
- **Giant groups.** A placement group lives in one partition, so its write
  rate is capped at one Raft group's. `#general` or a monorepo's commits
  can hit that. Even a split inside the group (open question 20) wouldn't
  help a hot append tail: new messages all land at the end of the range.
  Measure with the Slack demo and SCM ingestion.
- **Materializer capacity and rebuilds.** Materializers are rebuildable,
  not free. Every independent replica applies and compacts the whole log
  of its partitions, and a rebuild from a checkpoint must catch up on the
  log behind it. Milestone 2 measures, per partition: apply throughput,
  storage per replica, and time to rebuild from a checkpoint. Those
  numbers set how many replicas and how much spare rebuild capacity each
  fleet keeps.
- **Maintenance starvation.** Under load, maintenance falls behind,
  retained logs and lag grow, reads wait and retry, and reconnecting
  subscribers resync, which adds load. Reserved maintenance capacity and
  admission that watches downstream lag break that loop (§15.2), the
  staged-admission lesson of SEDA
  ([Welsh et al., SOSP 2001](https://www.sosp.org/2001/papers/welsh.pdf)).
- **Owning the storage stack.** Proving the app-facing model on an
  established transactional store first, and owning storage later, was
  considered and set aside: owning the stack is the decision. So the first
  deliverable is evidence for the protocols: TLA+ specs, Lean proofs, and
  fault injection, before features are built on them.
- **The frontier stalls with its slowest partition.** Derived consumers
  step between causally closed versions, so a partition whose
  materializer falls behind holds back every view and topic whose
  versions name it. Alert on frontier lag per partition, and keep
  materializer replicas for hot partitions.
- **Revocation isn't instant.** A deleted actor or viewer stops working
  within the revocation bound (§6.3), on requests and live streams alike,
  not at once. Keep the bound small, and alert when front ends' tails or
  materializers fall behind their leaders.
- **A revoked credential lasts until its tokens expire.** Shep keeps no
  revocation list (§6.3, open question 26), so a leaked token works for
  its whole lifetime, and so do download links it minted. Keep token
  lifetimes to minutes.
- **The auth service is a dependency.** Shep can't authenticate anyone
  without its public keys, and clients can't refresh tokens without it.
  Shep caches the JWKS and keeps accepting tokens signed with cached keys
  while the auth service is down; new tokens wait for it.
- **Change-query coverage and cost.** Live queries and `observe` are only
  as useful as the
  set of queries the planner can incrementalize, and as cheap as their
  deltas.
  - **Audiences.** Audience queries and live queries on large channels
    (§13.2) depend on anchors working well.
  - **Coverage reports.** Report which queries incrementalize before apps
    rely on them.
- **DBSP integration.** Adopting an external runtime means depending on its
  storage model and evolution. Building a subset means owning subtle
  incremental-operator code. Decide with a prototype in milestone 6, before
  milestone 10.
- **Hot-window sizing.** `since` tokens older than the hot window force a
  resync. Window size is a trade-off between storage and how gracefully
  apps recover from long disconnects.
- **Routing bits in generated keys.** A node placed with another one has
  its group's routing hash in its generated key (§9.6), so its group can't
  change after creation and placement can't be changed for a type that has
  nodes. If apps often need to re-place data, that needs a migration that
  re-keys nodes, which is out of v1.
- **Privacy-filtered lists at scale.** Pushdown coverage, and possibly
  materialized `can_read`, determine whether activity feeds and channels
  stay fast.
- **Developer experience of proven adapters.**
  - **Expressiveness.** If teams often want changes the adapter library
    can't express, they'll work around the system.
  - **Lossy views.** Every lossy adapter means an old client sees less than
    the truth.
- **Graph-dependent invariants** are checked only at write time. The docs
  and the API must say so.
- **Policy comprehension.** §6.7's tooling is the mitigation, and it's
  core work. Union minus deny drops Ent's "this allow wins over a later
  deny"; authors coming from Ent write the exception into the deny, and
  the access-path matrix generates that text (§6.1, §6.7).
- **ShepQL's upstream is frozen.** Gel Data shut down in December 2025,
  and Gel's last release is final. There's no upstream to diverge from,
  but also no new docs, community, or tooling: Shep owns EdgeQL's future
  for its own use, and contributors learn the dialect from Shep's spec
  (§4.0).
- **A large, unverified checker.** Parsing plus type, cardinality, and
  volatility inference in Rust is roughly 15–30k lines (an estimate),
  plus the macro, TypeScript generation, error rendering, and an editor
  language server. The checker decides totality and Purity, which
  policies' soundness rests on, and nothing proves it correct.
  Mitigations: differential tests of the checker against the evaluator,
  and the phase 2 kill criterion (§18).
- **Unfamiliarity.** EdgeQL's user base is small and shrinking; CEL, which
  ShepQL replaces, is widely known. Gel users will hit ShepQL's semantic
  changes (§4.0); each comes with a checker error, but habits carry over.
  Client-side validation loses CEL's Go and C++ evaluators: generated
  code carries the constraint list instead.
- **Trademark.** Whether "EdgeQL" and "Gel" are trademarks is unverified.
  Shep calls its language ShepQL so it doesn't imply compatibility (open
  question 24). Copying the grammar and tokenizer is fine under
  Apache-2.0, with attribution.
- **Live-query fan-out.** A topic with many subscribers turns each change
  into many pushes. Phase 2 lookups are batched, but pushes still grow with
  subscribers. Measure early with the Slack demo. Coalescing (§15.2) is the
  release valve.
- **Permission routing in phase 2.** Every policy-relevant fact must reach
  the topics whose reference sets contain the affected nodes. A missed
  route leaks data after a revocation. The TLA+ spec, and differential
  tests against fresh queries, guard it (milestone 9).
- **Admission tuning.** Cost estimates can be wrong, and adaptive limits
  can oscillate. Measured costs correct the estimates, and load tests check
  the limiters.
- **Names must exist in production before development.** A brand-new app
  or schema can't start `shep dev` or tests until someone with production
  credentials runs `shep app create` or `shep schema reserve`, and the
  name propagates (§14.1). That friction is the price of a single
  authority for names: two fleets can never disagree about who owns one.
- **Declaring every operation.** Teams may find declarations slower than
  ad-hoc queries while prototyping. In development, `shep dev` registers
  on every change. In production, a new operation is a deploy of the app's
  operation set, never a schema version.
- **Old operation sets accumulate.** Every binary still running keeps its
  operation set, and the schema versions it pins, in service. Dormancy
  after a period without sessions (§4.6) bounds this, and `shep ops
  status` shows who is still connected.
- **Index builds on deploy.** Indexes come from operations (§7.1), so one
  app's new operation can add an index on a busy shared type, and a deploy
  waits for it. `shep ops check` shows the cost first, and builds run
  concurrently, in the background admission class.
- **Invariants that only a mutation enforces.** Operations belong to apps,
  so a rule written only in one app's mutation doesn't bind other apps.
  Schema owners must put invariants in checks and write policies (§8).
- **Search lag.** Search reads an index that trails the log. `at_least`
  hides the lag from the writer, but a slow indexer delays everyone's
  results, and a lost index means a rebuild. Alert on index lag, and keep
  segment copies current.
- **Several stateful components.** Only the WAL holds state that can't be
  rebuilt. Materializers, view maintainers, and search indexers hold
  derived state, rebuildable from the log and their copies in the object
  store, but they still need replicas, checkpoints, and capacity planning.
- **Parsing untrusted files.** PDF parsers have a long history of memory
  bugs. Extraction runs sandboxed, with limits and no network (§16.5), and
  a crash fails one file's text, not the service.
- **Content that never goes away.** Blobs are never deleted (§16.2), so
  object storage only grows. Deduplication slows the growth but doesn't
  stop it. Track bytes per universe and per app from the start (§15), and
  design GC before storage cost matters (open question 15).
- **The cost of versions and ghosts.** Every key keeps its versions down
  to the retention floor, and its newest version forever, including the
  ghosts of unlinked edges and index entries that moved. Measure the write
  and storage overhead in milestone 2, with SCM ingestion and the Slack
  demo's edge churn.
- **Deduplication and existence.** Uploads always send their bytes unless
  they copy a file the actor can read, so knowing a hash grants nothing,
  and an upload looks the same whether or not its bytes were stored
  (§16.2). Only completion timing differs slightly, which is accepted for
  now (open question 10).
- **Classification soundness.** The planner skips validation only where it
  proves a read can't change the outcome (§7.6). A bug in that proof is a
  silent serializability or authorization hole, not a crash. The
  mitigations are a syntactic, conservative classifier that defaults to
  "validate", classification-differential tests against "validate
  everything", and the Lean and TLA+ obligations listed in §18.
- **Discipline drift.** Plans depend on declared disciplines, policies,
  and schema heads, so a deploy that weakens one, such as allowing deletes
  of a `shep::Permanent` type, would silently invalidate other apps' plans if
  it weren't fenced. It rolls out in two steps: the re-planned operation
  sets activate first, and only then is the weaker rule enforced (§4.1.2,
  §7.6). `shep schema disciplines` shows who depends on what.

## 20. Open questions

1. **How much two-phase commit.** Mutations across placement groups use
   two-phase commit (§8), including those that only validate reads in
   another group, and placement keeps them rare. Which workloads need it
   often enough that its latency matters, and would they be better served
   by different placement, or by a cheaper protocol for read-only
   participants?
2. **Shared mutations.** Should a schema's owner be able to publish
   mutations that other apps call, such as `issues.close`, or are checks
   and write policies enough to protect shared data (§8)?
3. **How far back `since` must reach.** This sets the hot window
   (§11.4). Beyond it, a feed resyncs, but a freshness check tells it
   whether anything changed, so an unchanged result skips the resync
   (§11.7). The window then only has to cover deltas apps want, not every
   reconnect.
4. **Untyped documents.** Should `Document` fields exist at all, or should
   every nested structure be typed (§10.2)? Typed values get compatibility
   guarantees and adapters. Untyped documents are convenient for
   app-defined payloads, such as agent state or SCM metadata whose shape
   the app doesn't control.
5. **Live top N.** A live range grows from the first page in the query's
   order (§13.1). Do apps also need a live top N by a score that changes,
   such as a leaderboard?
6. **Ad-hoc writes.** Should schemas be able to enable ad-hoc mutations for
   people (§7.2), or should people always write through apps?
7. **Quota units.** One cost unit per resource class, or separate quotas
   for CPU, materializer reads, WAL writes, and subscriptions (§15.2)?
8. **Edges to nodes that don't exist yet.** Resolved: placeholders (§3).
   Linking to a missing key creates a placeholder that reads as absent
   until a create fills it.
9. **Search topology.** One index per node type and shard, with an indexed
   field per (field, analyzer, adapted form), sharded by placement group,
   and indexers that also serve searches, is the starting point (§16.6).
   When should indexing and searching split, as in Quickwit, and should
   small universes share indexers?
10. **Chunk-level deduplication.** Shep deduplicates whole blobs by
    SHA-256 (§16.2), so two versions of a large file that differ by one
    byte are two blobs. Content-defined chunking, as Smirk and backup tools
    use, would share unchanged chunks and allow verified range reads. Is
    that worth a manifest format? The same work could remove the timing
    difference that shows whether an upload's bytes were already stored.
11. **Live search.** Whether a row matches a search depends only on the row,
    so a search could incrementalize like a filter. Convex makes search
    reactive by requiring equality filters and capping each search at 1024
    results, and Shep already requires filters the index can apply. Since
    the indexers see every change, do apps need live search results, or is
    re-running the search enough (§16.6)?
12. **Search ranking and the whole index.** BM25 scores depend on the whole
    index. So a cursor lives only as long as its searcher (§16.6): is
    restarting a search after `cursor_expired` enough, or do apps need
    stable deep pages, such as an export of every match? And term
    statistics include text the actor can't see, which is accepted for
    now: do any apps need statistics computed only over what each actor
    can read, at a much higher cost?
13. **More derived data.** Image thumbnails, document previews, Office
    formats, and OCR are all processing steps (§16.5). Which do apps need,
    and should apps be able to register their own processors?
14. **Request attributes.** Should policies see facts about a request that
    aren't in the graph, such as MFA, identity-provider groups, or the
    network it came from? Guards that read them can't compile to relations
    (§6.8), so audiences couldn't include them, and a live subscription
    would fix them when it subscribes. Syncing such facts into the graph,
    such as groups as edges, avoids that (§6.3).
15. **Blob garbage collection.** Punted: blobs are never deleted (§16.2).
    The race to design around: an upload whose bytes turn out to be a
    duplicate, and a copy, both end up pointing at an existing blob
    without writing it, so collection must never delete a blob between
    that moment and the attach. Reading per-node counts one node at a time
    has the same race. A starting design: anything about to add a
    reference takes a pin on a per-blob record; attaching must commit
    before the pin expires, with a margin for clock skew; collection first
    condemns a blob with no live pins, which blocks new pins, then deletes
    it only if every partition counts zero references.
16. **Live walks.** A walk's result changes when an edge anywhere in it
    changes, or when a node in it becomes hidden, which can cut off
    everything beyond it. Recursive queries can incrementalize in DBSP,
    with fixed points, but the cost per change is hard to bound. Do apps
    need live or materialized walks, such as a live dependency closure,
    or is re-running enough (§7.1)?
17. **Compacting ghosts.** Ghost index entries and ghost links
    accumulate (§3). A range bucket's ghosts could be compacted into one
    summary per stamping partition: the highest index `m` among the
    bucket's ghosts stamped by that partition, with a check treating the
    bucket as unchanged since `V` only if `V`'s **prefix** for that
    partition is at least `m`. Extras never satisfy a summary. Ghosts of
    two-phase commits are stamped by their home's decision, so their
    summary uses the home's prefix. That's sound because a prefix holds
    every index below it; a "largest dot" test against the whole version
    isn't (`counterMax_compaction_unsound` in `formal/lean`), and `V` must
    be causally closed. Compaction can only make a check answer "changed"
    falsely. What bucket size keeps checks precise enough, and when does
    compaction run?
18. **What freshness checks reveal.** A check walks rows the actor can't
    see, so "changed" can reveal activity in a range the actor can't read,
    though never its content (§11.7). Is that acceptable for every type,
    or should a schema be able to forbid freshness checks over a type, or
    should checks only consider rows the actor could read at both
    versions?
19. **Learned placement.** `constraint shep::placement` is the explicit tool
    (§9.6). The log shows which nodes are written together, and read
    traces show which are read together. Should a background partitioner
    propose co-location from them, as Facebook's Social Hash partitioner
    and Akkio did? Placement is fixed once a type has nodes (§4.1.2), so
    acting on a proposal means re-keying, which is out of v1.
20. **Merges, and splits inside a group.** v1 splits only at group
    boundaries and never merges (§9.6). Giant groups are then capped at one
    Raft group's write rate, and partitions that shrink keep their
    capacity. Lifting either needs a key's partition to change by more
    than a split at a group boundary, with the lineage rules that brings.
    Which workloads need it, and when?
21. **Older reads at the leader.** A proposal whose read version is older
    than the validation table's window gets `too_old` (§10.3). A leader
    with a materializer beside it could instead check each read key's
    newest stamp. Is that worth a storage read on the validation path?
    Field operations already make the leader hold a last-value map for
    op-eligible keys written within the window (§8), so it holds some
    state either way.
22. **More disciplines, and inferring them.** v1 has only `readonly` and
    `shep::Permanent` (§4.1.2). Edges that are only ever added, and values
    that only move up an order (a generation, a forward-only enum), would
    make `exists`, `count >= k`, and `gen >= g` guards stable too. Are they
    worth two more concepts, given that those guards can stay validated?
    And should Shep ever adopt an inferred discipline automatically, as a
    checked registry change, or should declaring one always be an
    author's decision?
23. **Agents with no user.** An agent that works for no one, such as a
    scheduled maintenance agent, needs a token whose viewer is itself, or
    `global actor` policies in every schema it uses (§6.3). Is that
    enough, or should agents be viewers in their own right, with their
    own grants?
24. **The dialect's name.** ShepQL is a working name, chosen so the
    language doesn't imply compatibility with Gel or EdgeQL, whose
    trademark status is unverified (§4.0, §19). The file extensions
    (`.esdl`, `.edgeql`) are kept for editor support. Keep both, or rename
    the extensions too?
25. **Finer revocation sensitivity.** v1 treats a read decision as not
    RevocationSensitive only when every allow is Stable (§7.6). Asking
    only that the allow admitting each row be Stable would let more reads
    skip the data-partition horizon, but the argument is per row: a stale
    snapshot might admit a row through a different, mutable allow. Is the
    refinement worth its proof?
26. **A token revocation list.** Shep honors a token until it expires
    (§6.3), so revoking a leaked credential takes up to a token's
    lifetime. Should Shep also accept an optional revocation list from
    the auth service, such as revoked `jti`s or a "not before" time per
    app or subject, pushed or polled, and folded into the horizon? It
    would bound revocation like a registry change, at the cost of a
    dependency on the auth service's availability.
27. **Quotas.** Admission control measures every request by the token's
    `client_id` claim (§15), but how per-app and per-user quotas are configured,
    stored, and enforced, and whether the auth service or Shep owns them,
    is open. Open question 7 (quota units) depends on it.
28. **Own multi-raft core.** Decided: `shep-raft`, a fork of openraft
    0.10's engine and runtime, changed where Redpanda differs (§3, §17).
    Whether its atomic validate-and-append removes the void-entry rule is
    being checked (§9.1, §18).
29. **Controller voters.** Every WAL node is a controller voter (§9.6).
    Should voters be capped at 5, with other nodes as learners that still
    hold the whole state, as large Redpanda clusters do?

## 21. Appendix: ShepQL reference

The dialect is §4.0; this is its reference. Cardinalities: One (exactly
one), AtMostOne (`optional`), Many (`set of`). A **special form** is
parsed like a call but rebinds `.` or takes paths, so it isn't an
ordinary function.

### 21.1 Functions

| Function | Signature | Result | Where | Notes |
|---|---|---|---|---|
| `shep::visible` | `(n: optional Node) -> bool` | One | policies | False on `{}`. Per element over a `multi` link: `any(shep::visible(.quotes))` |
| `shep::can` | `(op: shep::Op, n: optional Node) -> bool` | One | policies | As `visible`, for another operation |
| `shep::decide` | `(op: shep::Op, n: Node, viewer := optional Node, actor := optional Node) -> bool` | One | policy tests only | §6.7 |
| `shep::walk` | `(root: optional T, step: path, max_depth := int64 literal, stop_at := bool expr) -> set of T` | Many | queries | Special form: `step` and `stop_at` are rebound to the walked node; `stop_at` is a decision, so total. Results carry `@depth` |
| `shep::search` | `(T, q: str, weights := set of (path, float64), analyzers := set of (path, shep::Analyzer)) -> set of T` | Many | query roots | Special form: paths rebound to `T`. Relevance order unless `order by` is given |
| `shep::snippet` | `(f: path) -> optional str` | AtMostOne | search shapes | Plain text with match offsets |
| `shep::matches` | `(f: path, q: str) -> bool` | One | filters | Unranked match |
| `shep::with_deleted` | `(T) -> set of T` | Many | query roots | Special form; `.deleted_at` resolves only beneath it |
| `shep::placeholder` | `(k: KeyCast) -> T` | One | mutations | Creates a placeholder if the key is absent; `T` must extend `shep::Placeholders` |
| `shep::claim` | `(q: set of T, n: int64) -> set of tuple<node: T, generation: int64>` | Many | mutations | §8's restricted shape |
| `shep::renew`, `shep::complete` | `(l: shep::Lease) -> shep::Lease` | One | mutations | Fenced |
| `shep::observe` | `(q: OpCall) -> shep::Delta` | One | mutations | Sees the merged post-state |
| `shep::audience` | `(n: Node, op: shep::Op, anchor := optional Node) -> shep::AudienceDelta` | One | mutations | §11.3 |
| `shep::unchanged` | `(q: OpCall, since := shep::Version) -> bool` | One | mutation `assert`s | §11.7 |
| `shep::now` | `() -> datetime` | One | mutations | The commit time |
| `shep::clear` | `() -> empty set` | — | `update … set` | Unsets an optional field explicitly |
| `shep::restore` | `(n: Node) -> Node` | One | mutations | Clears `deleted_at` |
| `shep::bounded` | `(s: set of T, n: int64 literal) -> set of T` | Many | `for` sources | More than `n` elements is an error |
| `shep::content_hash` | `(set of anytype) -> shep::Sha256` | One | `rewrite insert` | |
| `shep::wasm` | `(name: str literal, args…) -> declared type` | declared | checks | Opaque; never in adapters |
| `shep::truncate`, `clamp`, `saturate`, `map`, `first`, `head`, `reject`, `redact` | adapter built-ins | as the field | adapters | Laws proven once (§4.3) |

### 21.2 Constraints, mixins, enums, and types

- **Path constraints:** `shep::key on (path | tuple)`,
  `shep::placement on (path)`, and `shep::redactable on (path)` in schema
  files; `shep::contained_in on (path)`, `shep::independent_of on
  (path)`, and `shep::strict_revocation on (path)` in policy files.
- **Value constraints:** `shep::max_size(str literal)`,
  `shep::file(max_count :=, max_size :=, media :=)`.
- **Mixins:** `shep::Node` (implicit), `shep::Permanent`,
  `shep::Placeholders`, `shep::Embed`.
- **Enums:** `shep::Op`, `shep::Analyzer`, and `shep::Event` (`Deleted`,
  `ParentDeleted`, `ViewerRemoved`, `ViewerBlocked`).
- **Types:** `shep::File`, `Document`, `Lease`, `Version`, `Upload`,
  `App` (cast from a string literal, `<shep::App>'slack'`), `Sha1`, and
  `Sha256`.
- **Annotations:** `deprecated` and `shep::renamed_from := 'old'`.

### 21.3 Binding and resolution

- **`.`** In a shape or a `filter`, the current element. In a type-level
  constraint or a policy in `extend type`, the node. In a field or edge
  policy, still the node, with `__subject__` the field's value or the
  link's target. In an adapter, the node, with `__subject__` the field's
  value, `__old__` the stored value, `__specified__` the patch's fields,
  and `new::` and `old::` naming the two versions' types.
- **Globals.** `global viewer` (AtMostOne in policies; in operations, per
  the principal shapes, §4.0 item 11), `global actor` (declared in policy
  files), and `global shep::app` (One).
- **`ops::name(param := …)`** names another operation of the same set,
  with named arguments, only as the argument of `shep::observe` or
  `shep::unchanged`.
- **`shep::with_deleted(T)`** is a root of type `T` whose elements
  include deleted nodes; `.deleted_at` resolves only on paths that start
  there.
- **Deprecated fields** are absent from the version that deprecates them:
  naming one in that version's operations is a resolution error.
- **Embeds.** `<Address>(street := …, city := …)` builds a value, and
  equality is field by field.
- **Defaults** are Pure: constants, or expressions over the node's own
  fields.

### 21.4 Query and mutation rules

- **`limit`.** A top-level `limit` must be a literal, and is the
  operation's `max_first`. Nested `limit`s must be literals too.
- **Ordering.** `order by` must end in the type's key: `.id`, or the
  `shep::key` fields in order. The checker appends what's missing.
- **No `offset`,** and no `for` over an unbounded source.
- **Reads in mutations** see `S`, except DML result bindings and
  `shep::observe`, which see the merged post-state (§8).
- **Field operations** are the forms in §8's table, and only those.
