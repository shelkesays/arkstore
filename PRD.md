# Arkstore — Product Requirements Document

> A safe-Rust, MIT-licensed tool for backing up, restoring, retention-cleaning, and
> cold-tier archiving databases and files to object storage. Single static binary,
> opt-in engines, config-driven, safe by default.

- **Status:** Draft v0.1 (reverse-engineered requirements + forward-looking design)
- **Owner:** @shelkesays
- **Repo:** https://github.com/shelkesays/arkstore (existing Python repo → reset to Rust)
- **License:** MIT
- **Language:** Rust (no `unsafe` in the crate; `#![forbid(unsafe_code)]`)

> **Implementation status — read this first.** This document is a **specification that leads
> implementation**: it defines the target behavior, CLI contract, and config model the binary is
> being built to satisfy. The crate is currently at the **M0 skeleton** stage (see the roadmap,
> §15) — the architecture and command surface exist, but most operation internals are stubs.
> **Present-tense wording in the sections below states *required* behavior, not a claim that it
> already ships.** Any flag, sub-action, or config field described here that is not yet wired into
> the binary is expected and intentional — it is the contract, not a bug. Do not read a section as
> a description of the current build.

---

## 1. Overview & Vision

Arkstore is a single-binary command-line tool that manages the full lifecycle of
database and file backups against S3-compatible object storage:

1. **Backup** — dump databases / snapshot file trees to compressed archives in object storage.
2. **Restore** — reconstruct a database or file tree from a chosen backup.
3. **Cleanup** — apply calendar-tier retention (daily/weekly/monthly/yearly) to stored backups.
4. **Archive** — move aged rows out of a live database into columnar (Parquet) files in
   object storage, keeping only a recent window in the source table.
5. **Verify** — prove a backup is restorable: round-trip it into a throwaway target and diff it
   against the baseline recorded at dump time.

It is **config-driven** (declarative source definitions + global policy), **safe by default**
(dry-run everywhere, delete-after-verify, never deletes what it can't parse), and **portable**
(one statically linked binary, engines compiled in as opt-in features).

The vision is the tool a small team can drop into a cron/ECS schedule and trust: predictable,
observable, and impossible to make silently destructive.

---

## 2. Background & Motivation

Operational teams repeatedly re-implement the same backup plumbing: pg_dump to S3, a naming
convention, a retention script that eventually deletes the wrong thing, and — for high-volume
log/event tables — an ad-hoc "move old rows somewhere cheaper" job. These are generic,
well-understood capabilities, but the glue is fragile and rarely reusable across engines.

Arkstore consolidates these operations behind one declarative config and one binary, with a
correctness bias: every destructive path is preview-first and verify-before-delete.

**Why Rust:** memory safety without a GC, a single dependency-free static binary (no Python
runtime to provision in a container or on a host), true parallelism for multi-source backups
and multi-month archival, first-class async I/O for object storage, and a strong type system
for the config/secrets model. This is the "much better approach" over a Python predecessor:
faster, smaller, safer to deploy, and easier to distribute.

### 2.1 Prior art & clean-room note

This PRD describes **requirements and observable behavior** of a generic database backup and
archival tool — a standard software category (pg_dump/pgBackRest/restic/etc. occupy the same
space). It intentionally contains no proprietary source. The Arkstore implementation is written
fresh in Rust from these requirements.

---

## 3. Goals & Non-Goals

### Goals
- One binary, five operations, one declarative config — and **no database client tools**: every
  engine is driven over its wire protocol from inside the binary (§5.1).
- **Opt-in engines** at compile time (Cargo features) and clear runtime errors when an engine
  isn't built in — never a cryptic failure.
- **Safety first:** dry-run for every destructive operation; delete only after upload is
  verified; never delete objects whose key/layout can't be parsed.
- **Observability:** structured, multi-level logging; per-source isolation; meaningful exit codes.
- **S3-compatible** object storage (AWS S3 first; MinIO/other S3 APIs supported).
- Secrets from a secrets manager or a local file, never hard-coded.
- Runs unattended on a schedule (cron / ECS scheduled task / k8s CronJob).

### Non-Goals (v1)
- No continuous/PITR/WAL streaming replication (this is snapshot/dump-based).
- No built-in scheduler (rely on cron/ECS/k8s).
- No GUI/web console.
- No cross-region replication logic (delegate to object-store lifecycle/replication).
- No encryption-at-rest management beyond what the object store provides (SSE) in v1;
  client-side encryption is a roadmap item.

---

## 4. Target Users & Use Cases

- **Platform / DevOps engineers** running scheduled backups for a handful of app databases.
- **Data engineers** aging out high-volume log/event tables (e.g. an app's `logs` table) to
  cheap columnar storage while keeping a recent window hot.
- **Small teams** who want restic-like ergonomics but for *databases*, with a retention model
  they can reason about.

Representative use cases:
1. Nightly backup of N Postgres/MySQL/Mongo databases + a config directory to S3.
2. Weekly retention cleanup that keeps daily/weekly/monthly/yearly survivors and prunes the rest.
3. Monthly archival of everything older than ~3 months from an append-only log table to Parquet.
4. On-demand restore of a specific database from a specific timestamped backup.

---

## 5. Supported Sources & Engines

| Source type | Backup | Restore | Archive | Mechanism (default) |
|---|---|---|---|---|
| PostgreSQL | ✅ | ✅ | ✅ | **native** wire-protocol driver: schema from the system catalogs, data via the `COPY` sub-protocol, one consistent snapshot. See §5.1 |
| MySQL/MariaDB | ✅ | ✅ | ✅ | **native** wire-protocol driver: `SHOW CREATE …` for schema, streamed rows, consistent snapshot. See §5.1 |
| MongoDB | ✅ | ✅ | ✅ | **native** wire-protocol driver: BSON per collection + index/option metadata (`mongodump`-compatible layout). See §5.1 |
| Files/directories | ✅ | ✅ | ❌ | tar + compress a path tree (pure Rust) |

**No database client tools — ever.** Every engine is driven over its own wire protocol by a pure-Rust
driver compiled into the binary. `pg_dump`, `mysqldump`, `mongodump` and their restore
counterparts are never invoked, never required on `PATH`, and never shipped in the container image.

**Engine opt-in (Rust feature flags):** each engine is a Cargo feature (`postgres`, `mysql`,
`mongo`, `archive`, `files`). The default build is `postgres,archive,files`;
`--all-features` builds everything. Using an engine that wasn't compiled in fails fast with a
clear message: *"PostgreSQL support was not built into this binary. Rebuild with
`--features postgres` or download the full release."* — the same "opt in to the engines you use"
idea as package extras, but resolved at compile time into one self-contained artifact.

Prebuilt releases: publish a **full-feature** binary per platform, plus optionally slim
per-engine builds. Because it's one static binary, "install the right extra" becomes "download
the right release asset."

### 5.1 Native wire-protocol backends (no client tools)

Arkstore is **fully self-contained**: backup, restore, and archive for every engine are performed
by a pure-Rust driver speaking the engine's **wire protocol**, compiled into the binary. There is
no "external" strategy and no `dump_strategy` knob — one code path per engine.

**Why this is sound, not a compromise.** `pg_dump`, `mysqldump`, and `mongodump` are themselves
nothing more than ordinary wire-protocol clients: they hold no privileged access and use no server
facility a driver cannot. Schema is *data* read from the catalogs — Postgres exposes
`pg_get_viewdef()`, `pg_get_functiondef()`, `pg_get_indexdef()`, `pg_get_constraintdef()`, and
`pg_get_triggerdef()`, and tables are assembled from `pg_attribute`/`pg_type`/`pg_attrdef`; MySQL
hands DDL back directly via `SHOW CREATE …`; Mongo via `listCollections`/`listIndexes`. Bulk data
moves over each protocol's bulk path (Postgres `COPY`, MySQL streamed result sets, Mongo cursors).
Everything those tools do is therefore reimplementable. What they *guarantee* that a naive dump
lacks — **consistency** and **fidelity** — becomes an explicit requirement here (§5.1.1, §5.1.2)
rather than something inherited by shelling out.

**Drivers (pure Rust, no C, statically linkable):** Postgres — `tokio-postgres` (`copy_in` /
`copy_out`) or `sqlx`'s native Postgres driver (`copy_in_raw` / `copy_out_raw`); MySQL —
`mysql_async` (or `sqlx` MySQL); MongoDB — the official `mongodb` crate with `bson`. TLS via
`rustls`, never OpenSSL, so the musl static build stays free of system libraries — concretely:
`tokio-postgres-rustls` (`MakeRustlsConnect`, MIT) for Postgres; `mysql_async`'s `rustls-tls`
feature (it enables **no** TLS backend by default, so this must be selected explicitly); and
`mongodb`'s default `rustls-tls` feature (OpenSSL is opt-in there and never chosen).

#### 5.1.1 Consistency: one snapshot per source (required)

A backup must be **transactionally consistent across all objects** of a source, exactly as the
classic tools guarantee (`pg_dump` runs in one `REPEATABLE READ` transaction; `mysqldump
--single-transaction` does the same). A set of per-table reads taken at different moments is not a
backup — it is a collection of mutually inconsistent tables. Native dumps must therefore:

- **PostgreSQL** — open one `REPEATABLE READ` read-only transaction for the whole source and read
  every catalog and table inside it. When tables are dumped **in parallel**, export that
  transaction's snapshot with `pg_export_snapshot()` and have every worker connection adopt it via
  `SET TRANSACTION SNAPSHOT`, so all workers see the identical point in time. Two rules the server
  imposes: each worker's transaction must itself be `REPEATABLE READ` (or `SERIALIZABLE`) with
  `SET TRANSACTION SNAPSHOT` as its **first** statement, and the **leader transaction must stay
  open** until the last worker finishes — an exported snapshot is importable only while the
  exporting transaction lives. (This is the same mechanism `pg_dump -j` uses.)
- **MySQL/MariaDB** — `START TRANSACTION WITH CONSISTENT SNAPSHOT` at `REPEATABLE READ` on InnoDB.
  MySQL cannot share a snapshot across connections, so the tables of one MySQL source are dumped
  **sequentially on the snapshot connection** (parallelism stays at the source level). Non-
  transactional engines (MyISAM) cannot be snapshotted — surfaced as a per-table warning and
  documented as a limitation.
- **MongoDB** — there is **no cross-collection snapshot** without the oplog. v1 dumps each
  collection with a single cursor; cross-collection point-in-time consistency is out of scope and
  stated plainly in the docs (an oplog-based mode is a roadmap item, §15).

A dump that cannot establish its snapshot **fails the source before reading any data**.

#### 5.1.2 Fidelity contract

"Full fidelity" is defined, not asserted. The native backends must capture and restore:

- **PostgreSQL** — schemas; tables (columns, types, defaults, identity/serial, generated columns,
  `NOT NULL`, collation); primary/unique/foreign-key/check/exclusion constraints; indexes
  (including partial/expression); sequences **and their current values**; views and materialized
  views (dependency-ordered); functions/procedures; triggers; user-defined types (enum, composite,
  domain, range); extensions (as `CREATE EXTENSION`); comments; declarative partitioning (parents,
  partitions, attachment); row-level-security policies when present. **Ownership and privileges
  (`OWNER TO`, `GRANT`) are opt-in** (`include_privileges`, default off) so dumps are portable across
  roles by construction. Large objects (`pg_largeobject`) and replication slots are **out of scope
  for v1** (documented).
- **MySQL/MariaDB** — tables (`SHOW CREATE TABLE`, incl. engine, charset/collation, `AUTO_INCREMENT`
  value, generated columns); views (dependency-ordered); triggers; stored routines and events
  (`SHOW CREATE …`); foreign keys.
- **MongoDB** — every collection's documents as BSON; indexes (`listIndexes`); collection options
  (capped, validators, collation); views.

Anything outside the contract that a backend encounters is **logged as unsupported**, never
silently dropped — and the completeness gate (§6.1) still applies to every listed object.

#### 5.1.3 Archive file formats (what lands in the tar)

| Engine | Structure file | Data file | Restore path |
|---|---|---|---|
| PostgreSQL | `<obj>.schema.sql` — DDL Arkstore emits, portable by construction | `<obj>.data.copy` — `COPY … TO STDOUT` **text** format (`\N` nulls). Binary `COPY` is an opt-in for same-version/same-architecture speed; text is the portable default | apply DDL, then `COPY … FROM STDIN` |
| MySQL/MariaDB | `<obj>.schema.sql` — `SHOW CREATE …` output | `<obj>.data.tsv` — tab-separated, backslash-escaped, `\N` nulls (the server's own text row format) | apply DDL, then batched multi-row `INSERT` (never `LOAD DATA LOCAL INFILE`, which needs server-side opt-in) |
| MongoDB | `<coll>.metadata.json` — indexes + options | `<coll>.bson` | `insertMany` batches, then `createIndexes` |

Every archive carries `manifest.json` (§6.1), which also records each object's **dependency graph**
(foreign-key parents, view dependencies), its **row/document count**, and (SQL engines) an
**order-independent content hash** as seen in the snapshot — the inputs for restore ordering (§6.2)
and `verify` (§6.5).

#### 5.1.4 Supported server versions

Catalog layouts change across releases, so each backend declares a **supported server-version
range** and version-gates its catalog queries; connecting to an unsupported version fails fast with
a clear message rather than producing a dump of unknown fidelity. Initial targets: PostgreSQL 13+,
MySQL 8.0+ / MariaDB 10.6+, MongoDB 5.0+ — widened only as the fidelity suite (§6.5) passes for a
version.

#### 5.1.5 Why the wire protocol, not a REST API

A database's real programmatic interface is its **binary wire protocol** over TCP (the Postgres
and MySQL client/server protocols; MongoDB's BSON-over-TCP; each with bulk sub-protocols like
Postgres `COPY` and logical replication). SQL is the *language* carried over that protocol — the
protocol is the API, and it is exactly what the Rust drivers speak. Structure (DDL) is not a
separate API either: it is read as data from the system catalogs (`pg_catalog` /
`information_schema`, or MySQL `SHOW CREATE`).

Neither engine ships a core **REST/HTTP** interface; REST layers (PostgREST, Supabase, Hasura,
MySQL REST Service) are third-party add-ons *built on top of* the wire protocol. They are useful
for application CRUD but the wrong tool for backup: row-by-row HTTP/JSON is far slower than the
binary bulk path, adds a service dependency, and exposes no schema-dump fidelity. So the native
strategy deliberately uses the **wire protocol via the driver** — the efficient, official,
self-contained API — rather than a REST layer.

---

## 6. Functional Requirements

### 6.1 Backup (`arkstore backup`)

Iterate all **enabled** sources — optionally narrowed to one engine type via `--type <engine>`
or a single named source via `--source <name>`.

**Per-database-source pipeline:**

1. **Enumerate the objects to dump** (tables / collections) from the live source, then apply the
   source's ignore rules (below).
2. **Dump each object** through the engine's **native wire-protocol backend** (§5.1), inside the
   source's single consistent snapshot (§5.1.1). What is dumped per object is governed by two
   per-source toggles:
   - **`structure`** (schema/DDL) — when true, dump the object's definition.
   - **`data`** (rows/documents) — when true, dump the object's contents.
   A source may back up structure-only, data-only, or both.
3. **Completeness gate** — if *any* object fails to dump, abort the source and upload **nothing**.
   A backup archive is all-or-nothing; a partial archive that silently omits an object is a
   restore-time data-loss trap and must never be produced.
4. **Write a manifest** (database sources) — a `manifest.json` inside the archive recording a
   format version, the source name, engine type and **server version**, a creation timestamp, the
   snapshot identity, and one entry per dumped file with its `{path, object_name, kind, size,
   sha256}`. Per object it also records the **dependency graph** (FK parents, view dependencies),
   the **row/document count**, and (SQL engines) an **order-independent content hash** observed in
   the snapshot. This is the integrity record restore validates against (§6.2) and the baseline
   `verify` compares to (§6.5).
5. **Package** — tar + stream through compression to a timestamped archive.
6. **Upload** (when `backup_to_s3` is enabled) to object storage, then **verify** the upload
   (size / checksum) before declaring success. A source may be configured local-only
   (`backup_to_s3: false`).
7. **Local artifact lifecycle** — the per-source working directory is always removed; local
   copies of the finished archive are removed only when `delete_after_upload` is true *and* the
   upload verified. When copies are kept, **`local_retention: N`** bounds them: with `N ≥ 1` it
   prunes to the newest `N` **versioned** archives per source (oldest deleted first); **`N = 0`
   disables pruning entirely and retains all versioned archives.** The `latest` pointer is always
   kept and is **never counted** toward `N`.

**Ignore rules (per source):**

- **`ignore_startswith`** — object-name prefixes excluded **outright** (no structure, no data).
  Intended for engine/system objects (e.g. Postgres `pg_`, `rds_`, `awsdms_`; Mongo `system.`,
  `local.`).
- **`ignore`** — behaviour is per engine:
  - **PostgreSQL** — the object's **data is skipped but its structure is still captured** (recreated
    empty on restore). This data-skip semantic is Postgres-only.
  - **MySQL/MariaDB, MongoDB, and file sources** — `ignore` is an **outright exclusion** (no
    structure, no data), the same as `ignore_startswith`.

**Per-file-source pipeline:** tar + compress the configured `path` tree and upload the same way.
File sources honour `ignore` / `ignore_startswith` (fnmatch on the entry basename) and
`ignore_extensions`; **symlinks are preserved but never followed**; the top-level entries under
`path` are copied preserving their subtrees.

**Versioning & pointer:**

- **Timestamped, immutable** versioned objects live under `versioned/`.
- A per-source **`latest` pointer** (`<source>.latest.tar.gz`) is rewritten each run and is the
  **only** mutable object.

**Invariants:**

- Per-source **failure isolation**: one source failing (bad credentials, unreachable host,
  snapshot failure, upload error) is logged and recorded; the run continues. Exit `1` if any
  source failed, else `0`.
- An empty / no-match source selection is a clean warning, not an error.
- Never require network egress beyond the DB and the object store.

**S3 layout:**
```
<folder>/<source>/versioned/<source>.<stamp>.tar.gz   # immutable, timestamped
<folder>/<source>/<source>.latest.tar.gz              # mutable pointer, rewritten each run
```

### 6.2 Restore (`arkstore restore`)

Restore reconstructs a single named source into an operator-provided **target** that may differ
from the original (a staging DB, a different host/database, a different directory). It targets one
`--source` at a time — it never iterates all sources — and is the most correctness-sensitive
operation, so it is preview-first, target-guarded, and integrity-checked throughout.

**Sub-actions (positional, consistent with `cleanup`):**

- **`restore`** (default when no action is given) — the full restore flow below.
- **`list-backups`** — list the versioned backups available for the source (key, size,
  last-modified), newest first. Reads only; writes nothing.

(The action is a positional value — `arkstore restore list-backups` — matching the `cleanup`
sub-action style, not a `--action` flag.)

**Target model** — resolved in two stages:

1. **Which target entry** — pick the named entry from the `targets` config by
   **`--target <name>` > env `ARKSTORE_TARGET` > the source's own name** (i.e. by default a source
   restores to the target sharing its name); if no `targets` entry matches, fall back to an inline
   `restore.target` block. Restoring with no resolvable target is an error.
2. **Per-field overrides on top** — each connection field is then resolved with precedence
   **CLI flag > env var > the chosen target entry > engine default**:
   `--target-host` / `--target-port` / `--target-db` / `--target-user` / `--target-path` (and env
   equivalents). Ports default per engine (Postgres 5432 / MySQL 3306 / Mongo 27017); Mongo
   `auth_db` defaults to the target db.

- The **target password is never taken from the command line** — environment variable, config, or
  an interactive `getpass` prompt on a TTY only (see §8, §9.6).

**Never-production guard (mandatory, runs before anything is read or written):**

- Abort if the target host+port+db is identical to the source (refuse to restore onto the origin).
- Warn if the target is the same server but a different database.
- For file targets, reject a target path that overlaps the source path.

**Backup selection (`--from`):**

- `latest` (default) — the source's `latest` pointer.
- a specific timestamp / object key — a chosen versioned backup.
- a local dump file or archive path — for offline / single-item restores.

**Restore flow (database source):**

1. Resolve target → run the never-production guard.
2. Build the engine loader (fails fast if that engine wasn't compiled in — §5).
3. **Prove the target is empty before downloading or extracting** (a non-empty target aborts
   early, before any transfer), unless it is a local single-dump restore.
4. Resolve `--from`, download, and **safely extract** the archive (see §9.6 extraction hardening).
5. **Validate the archive against its `manifest.json`.** The manifest is the authority on **which
   files each object is expected to have** — so "missing" always means *missing relative to what the
   manifest records*, never "a file some other object type would have." For each object the manifest
   lists, verify its recorded files are present and match their `sha256`/size:
   - A **data file the manifest records** that is missing or corrupt ⇒ the object **fails**
     (recorded, skipped). An object the manifest records as **structure-only** (or data-skipped,
     §6.1) legitimately has **no** data file — that is expected, not a failure; it is recreated
     empty (§6.2 structure-only).
   - A **structure file the manifest records** that is missing/mismatched is non-fatal **only when
     the target already defines the object** (its rows are then loaded into the existing schema).
     Native data files carry **no DDL** (§5.1.3), so there is no "self-describing data file"
     fallback: if the target lacks the object, it **fails** rather than loading partial or
     unusable data.
   - A **data-only** object (`structure: false`) likewise requires the target to already define it.
   The never-silently-incomplete rule wins: an object is "restored" only if the files the manifest
   promised are intact and its schema exists or is created.
6. **Compute load order** from the **dependency graph recorded in the manifest** (captured from the
   catalogs at dump time — no parsing of SQL text): topologically **layer** the objects so parents
   load before children and views after their base relations (Mongo is a single layer). Cyclic
   foreign-key dependencies are handled by creating the tables and loading their data with
   **foreign-key constraints deferred**, then applying the deferred constraints in a final pass.
7. **Load** each object per its layer, then verify presence of the expected objects. Per-object
   **failure isolation**: a failed object is recorded and skipped, never aborting the run
   mid-way.
8. Emit a **redacted summary** `{restored, skipped, failed}`; the temp working directory is always
   cleaned up.

**Structure-only restore:** an object that was backed up structure-only (or was in the `ignore`
data-skip set, §6.1) is **recreated empty** from its structure file when no data file is present.

**Single-item restore:** a single object's Arkstore-produced dump (its structure and/or data file,
with the manifest) can be restored on its own; the target object must be **absent or empty** first.
Because it is one object, no dependency ordering applies. (Single-item file restore is not
supported — file restores go through the full-tree path.)

**Engine load mechanics (native, §5.1):**

- **PostgreSQL** — apply each object's `schema.sql` over the driver, then stream its `data.copy`
  via `COPY … FROM STDIN`; restore sequence values last. Constraint-trigger suppression
  (`SET session_replication_role = replica`) is attempted for speed and FK-cycle tolerance, with
  **retry-with-fallback**: if a permission error blocks it, the load is retried once without it
  rather than failing.
- **MySQL/MariaDB** — apply DDL, then load `data.tsv` as **batched multi-row `INSERT`** statements
  under session settings `FOREIGN_KEY_CHECKS=0, UNIQUE_CHECKS=0` (restored afterwards). `LOAD DATA
  LOCAL INFILE` is deliberately **not** used — it requires server-side opt-in and is a deployment
  landmine.
- **MongoDB** — create each collection with its recorded options, `insertMany` in batches, then
  `createIndexes` from `metadata.json`.
- **Portable DDL by construction** — because Arkstore emits the DDL it restores, ownership and
  privileges are simply **not emitted** unless `include_privileges` is on, foreign-key constraints
  are emitted separately so they can be deferred, and no server-version-specific statements are
  ever written. There is therefore **no text-preprocessing step** (no regex stripping of
  `OWNER TO`/`GRANT`, no `COPY`-block-aware rewriting): portability is a property of the dump, not
  a repair applied at load time.

**Preview / safety:** `--dry-run` runs every check and computes the load order but writes nothing;
per-target failure isolation and meaningful exit codes throughout.

### 6.3 Cleanup / Retention (`arkstore cleanup`)

Applies **calendar-tier retention** by scanning the bucket directly (so it also prunes backups
from sources removed from config).

**Retention bands** (timezone-aware; weeks start Monday), each keeps the newest per period:

| Band | Tier | Kept |
|---|---|---|
| Today | — | everything (live) |
| Earlier days this week | daily | latest per day |
| Earlier weeks this month | weekly | latest per week |
| Earlier months this year | monthly | latest per month |
| Prior years | yearly | latest per year |

**Invariants (safety):**
- Grouping is **per source** — one source's backups never thin another's.
- Never delete: the `latest` pointer, today's backups, or **any key whose layout/timestamp
  can't be parsed** (unparsable ⇒ keep).
- A period group is never emptied — a period with a single backup keeps it.
- Disabling a tier keeps its whole band (or falls back to the next-coarser tier), never a
  wholesale delete.

**Plan/execute workflow (auditability):**
- `generate-plan` — scan + emit a plan (JSON) and a report (CSV: `source, key, timestamp,
  storage_class, size, action, reason`) locally, and upload gzipped/timestamped copies to a
  plans prefix. Deletes nothing.
- `execute-plan` — execute a previously generated plan (local path or object key). Every plan is
  **validated** before execution (required keys, keep/delete disjoint, no dup deletes, no delete
  missing its key) — any violation aborts with zero deletions.
- `run` (default) — generate → persist → execute → consolidate.
- `consolidate-plans` — roll audit files up into one file per period at the finest enabled tier;
  merged file written **before** originals are removed (audit trail never lost mid-way).
- Deletions batched (object-store max per request); dry-run counts batches but sends none.

### 6.4 Archive (`arkstore archive`)

Moves aged rows from a live DB table to **Parquet** in object storage, keeping a recent window.

- **Config-driven:** each source declares an `archive` list of rules; nothing is archived unless
  listed. Empty/absent ⇒ log and skip that source.
  ```yaml
  archive:
    - table: logs
      time_column: dttm
      retention_days: 90
  ```
- **Policy:** cutoff = `today - retention_days` at midnight in the configured timezone. Rows with
  `time_column >= cutoff` stay; older rows are archived in **whole-calendar-month partitions**,
  one Parquet file per month.
- **`whole_months` (default true):** a cutoff mid-month snaps back to the 1st, so only complete
  months are archived and the boundary month is retained whole until the next cycle. `false`
  archives to the exact cutoff (trims the boundary month).
- **Move semantics:** `delete_after_archive` (default true) deletes a month's rows **only after**
  that month's Parquet is uploaded and verified. `false` = copy-only.
- **Safe on append-only tables:** a row inserted after a partition is read carries a timestamp
  ≥ now > cutoff, so it can never fall inside an already-archived month.
- **Idempotent:** a re-run only sees rows still older than the cutoff.
- **Dry-run:** reports month partitions, per-month and per-table row counts, per-source totals,
  and whether it would delete — via a single grouped `count(*)`, so it stays cheap. Reads nothing
  in bulk, uploads nothing, deletes nothing.
- **Schema:** inferred per file. SQL columns keep native types; Mongo docs are flattened
  (scalars/dates pass through, nested docs/arrays → JSON strings, BSON-only types stringified) so
  every collection yields a stable columnar schema.
- Per-source failure isolation; verified-upload failure never proceeds to delete; exit `1` on any
  source failure.

**S3 layout** (dedicated top-level prefix, **outside** the backup folder so cleanup never sees it):
```
<archive_prefix>/<source>/<table>/<table>.<YYYY-MM>.parquet
# e.g. archive/appdb/logs/logs.2026-04.parquet
```

### 6.5 Verify (`arkstore verify`)

Verification proves a backup is restorable — the safety net that makes native dumps trustworthy
(§13.12).

- **Select** a backup exactly as restore does (`--from`, default `latest`).
- **Restore it into a throwaway target** — a `targets` entry flagged `ephemeral`, or a target
  Arkstore creates for the run — through the normal restore path (§6.2): same code, same guards;
  the never-production guard applies unchanged.
- **Compare** the result against the **manifest baseline** captured at dump time (§6.1): every
  expected object exists; per-object row/document counts match; for SQL engines the per-table
  **order-independent content hash** matches the value recorded in the snapshot; for Mongo, document
  counts and index definitions match.
- **Report** `{verified, mismatched, failed}` per object with reasons; exit `1` on any mismatch.
- **Tear down** the throwaway target only if Arkstore created it; never touch a pre-existing one.

`verify` runs on demand and in CI against containerized engines for every backend, gating the
fidelity contract (§5.1.2). `--dry-run` reports what would be verified without restoring.

---

## 7. Configuration Model

Three declarative layers:

1. **Global policy** (`arkstore.yaml`): `app` settings (`name`, `timezone`, log level), `logger`
   settings (§9.1 handlers), `aws`/object-store settings (`enable`, bucket, region, credentials or
   instance-role, `folder`, endpoint for S3-compatible), and per-operation blocks:
   - `cleanup` — retention tiers, `plans_prefix`, `delete_batch_size`, `dry_run`,
     `consolidate_plans`.
   - `archive` — `format`, `s3_prefix`, `default_retention_days`, `whole_months`,
     `delete_after_archive`, `dry_run`, `compression`, `fetch_batch_size`.
   - `concurrency` — `max_sources`, `cpu_workers` (§9.5).
2. **Sources** (`sources.yaml`): a list of source entries. Common: `name`, `type`
   (`postgre` | `mysql` | `mongo` | `file`), `enable`. Databases add connection details
   (`host`, `port`, `user`, password-via-secret, optional TLS settings), and per source:
   `structure`, `data`, `ignore_startswith`, `ignore`, `include_privileges` (Postgres/MySQL,
   default `false` — §5.1.2), `backup_to_s3`, `delete_after_upload`, `local_retention`, and
   optional `archive` rules (block-YAML style). File sources add `path`, `ignore_extensions`. Mongo
   adds `authentication_database` (defaults to the db name).
3. **Targets** (`targets.yaml`, optional): named restore targets — DBs `name`, `host`, `db`,
   `user`, password-via-secret, optional `port` and Mongo `auth_db`; file `name`, `path`. A missing
   targets file is not an error; targets may also be given inline as `restore.target` or overridden
   entirely by CLI/env (§6.2).

Requirements:
- Strongly typed deserialization (serde) with **clear validation errors** naming the offending
  field/source, not a stack trace.
- **Source names must be a safe single path segment** (they become S3 key components and local
  directory/file names). The grammar is `^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$` — it must start with a
  letter or digit, then contain only letters, digits, underscore (`_`), dot (`.`), or hyphen (`-`),
  and be **1–64 characters** long. No path separators (`/`, `\`), no `..`, no whitespace, no leading
  punctuation, non-empty. Anything else is rejected up front with an error naming the offending
  source.
  - **ASCII-only is deliberate:** Unicode names are rejected to avoid case-folding, Unicode
    normalization, homoglyph, and cross-filesystem encoding hazards in the keys/paths derived from
    the name (a design decision, not an oversight).
  - **Length cap rationale:** 64 characters keeps every derived name — the local path components and
    the S3 keys `<folder>/<source>/versioned/<source>.<stamp>.tar.gz` (which embed the source name
    twice) — comfortably within the 255-byte filesystem path-component limit and the 1024-byte S3
    object-key limit.
- Sensible defaults so a minimal config works; every default documented (see the KB for the full
  default table).
- Config file locations discoverable via flag / env / conventional path.
- **CLI flags override config** (e.g. `--dry-run`, `--source`, `--type`, restore target flags).

---

## 8. Secrets Management

- Credentials **never** live in the tracked config. Two backends:
  1. **Secrets manager** (AWS Secrets Manager first; pluggable) — gated by an env toggle.
  2. **Local secrets file** (e.g. `arkstore_secrets.yaml`) for dev/self-hosted.
- Secrets merge into source/target connection details at load time.
- The secret payload may also carry logging/observability config (e.g. ship logs to a collector),
  so a prod deployment can route logs centrally while local runs print to console.
- Never log secret values; redact connection strings in output.
- **Credentials never touch the command line, a child process's environment, or a temp file** —
  there are no child processes (§5.1). Passwords flow from the secret store straight into the
  driver's in-memory connection configuration, are held in zeroizing buffers, and are dropped once
  the connection is established. Nothing is ever written to disk or `argv`. A restore target
  password may additionally come from an interactive prompt on a TTY (§9.6) — again in-memory only.

---

## 9. Cross-Cutting Requirements

### 9.1 Logging & Observability
- **Multi-level** structured logging: `debug` / `info` / `warning` / `error`, chosen by config/flag.
- **Pluggable log sinks (handlers)**, each independently enable-able via config:
  - **console** (default, always available);
  - **file** — a rotating file handler (time-based rotation, e.g. rotate at midnight with a
    bounded number of retained files);
  - **error-reporting** — an optional error/exception reporting sink (Sentry-style), off by
    default;
  - **collector** — optional structured/JSON log shipping to a log collector (Grafana/Loki/
    Alloy-style), off by default.
- Handler settings (including the collector/error-reporting endpoints) may be delivered through
  the secret payload (§8), so a prod deployment routes logs centrally while local runs print to
  console.
- Progress feedback for long operations (e.g. per-month `[i/N]` during archive, per-source
  during backup) so a long-running job never looks hung.
- A concise run summary per operation (scanned/kept/deleted, bytes reclaimed, elapsed).

### 9.2 Safety & Correctness
- `--dry-run` on **every** destructive operation, doing zero writes/deletes.
- **Verify-before-delete** for both archive (upload verified before row delete) and backup.
- **Never delete the unparsable** in cleanup.
- Plan validation before any cleanup execution.
- Per-source/per-target **failure isolation**; aggregate failures into the exit code.

### 9.3 Exit Codes
- `0` clean run; `1` any per-item failure or a known top-level error (e.g. missing bucket/region);
  `130` on user interrupt (SIGINT, §9.6); distinct handling for expected vs. unexpected errors
  (traceback only for the unexpected).

### 9.4 Object Store
- S3 first via an object-store abstraction (`object_store` crate) so MinIO / S3-compatible and,
  later, GCS/Azure work with minimal change.
- Server-side encryption honored where configured.
- Cold-tiering & expiry are **delegated to object-store lifecycle rules**, not managed by
  Arkstore — but documented (recommended lifecycle schedules for backup vs. archive prefixes,
  and the "one lifecycle document, many prefix-scoped rules" gotcha).

### 9.5 Concurrency & parallelism

Arkstore processes sources in parallel rather than one at a time — but the model is chosen
around the workload's shape, not raw core count.

**The workload is mostly I/O-bound with bursts of CPU.** Per source: dump/fetch (DB network
I/O) → compress / Parquet-encode (**CPU**) → upload (object-store network I/O) → optional delete
(DB I/O). Cleanup is almost entirely list/delete network I/O.

Two independent limits, resolved from the `concurrency` config block:

- **`max_sources`** — how many sources are processed concurrently, as bounded async tasks (a
  semaphore over a shared runtime), **not** a thread or process per source. Because the per-source
  work is mostly network wait, this is **not tied to core count**; its purpose is to protect the
  **database and object store** from overload. `auto` = a conservative default (4); raise it when
  the target can take the load.
- **`cpu_workers`** — parallel workers for the CPU-bound stages (compression, Parquet encoding),
  run on a blocking/rayon pool. `auto` = number of available cores (`std::thread::available_
  parallelism`); a fixed value is **clamped to the core count**, since oversubscription only adds
  contention.

Design choices and rationale:

- **Async tasks, not processes — and no child processes at all.** Every engine is driven in-process
  over its wire protocol (§5.1), so there is nothing to fork. Threads/async avoid per-process cost
  and secret/config sharing complexity. **Failure isolation is `Result`-based:** each source runs
  as its own task whose errors are collected, never propagated as a crash. A **panic is not an
  isolable failure** — the release profile uses `panic = "abort"`, so a panic ends the whole run by
  design (fail-fast on a bug). That is exactly why the codebase forbids `unwrap`/`expect`/`panic!`
  in non-test code (SafeLint): the invariant is "errors are values", not "panics are caught".
- **The real limiter is the DB and object store**, not local hardware: many heavy dumps against
  one shared instance hurt production, and many parallel uploads invite object-store throttling
  (needs retry/backoff). A later refinement is a **per-host cap** so sources sharing a database
  instance serialize while sources on different hosts run in parallel.
- **A ceiling, not a floor.** For a batch job only a maximum is meaningful; there is no
  "minimum parallelism" knob.

Invariants preserved under parallelism: per-source/per-target **failure isolation** (tasks are
joined; failures aggregate into the exit code), **verify-before-delete** (each partition's delete
waits on its own verified upload; parallel months are disjoint partitions), and readable output
(every log line is tagged with its `source`).

### 9.6 Robustness & hardening

Backup/restore handle untrusted or hostile inputs (archives pulled from object storage, dump files
whose contents originate outside the tool), so the following are hard requirements, not nice-to-haves:

- **Safe archive extraction.** Extracting a downloaded archive must reject path traversal
  (`..`, absolute paths), escaping symlinks/hardlinks, and special members — extract data members
  only. No archive may write outside its intended temp directory.
- **Object-key confinement.** A restore-selected object key is confined under the source's prefix;
  reject `..` and absolute/rooted keys. Persisted-plan paths (cleanup) are resolved under the
  plans prefix with the same rejection.
- **Disk-space headroom check** before downloading/extracting a backup — require comfortable
  headroom (e.g. a multiple of the archive size) so a restore can't fill the disk mid-extract.
- **Dump-file validation.** A dump file must be non-empty and shape-checked (e.g. a `.sql` file
  must contain SQL) before it is fed to a loader; zero-byte or garbage files are skipped and
  reported, never silently "restored".
- **Identifier validation.** Table/collection identifiers derived from untrusted dump/file names
  are validated against a strict charset and safely quoted before use in any statement — no
  interpolation of unvalidated names.
- **Error redaction.** Driver and object-store errors are surfaced through Arkstore's own error
  types with connection strings, credentialed hosts, and secret values **redacted** before they
  reach a log line, a summary, or an error-reporting sink. Raw driver error text is never echoed
  verbatim.
- **Signal handling.** A user interrupt (Ctrl-C / SIGINT) cancels in-flight tasks promptly and
  cleanly: open database transactions are rolled back, partially written uploads are aborted (no
  dangling multipart uploads), temp working directories are removed, and the process exits with
  the conventional interrupted code (`130`). Because all work is in-process, cancellation is a
  cooperative task cancel — there is no child process to reap.

---

## 10. CLI Design (proposed)

```
arkstore backup   [--type <engine>] [--source <name>] [--dry-run] [--config <path>]
arkstore restore  [restore | list-backups]
                  [--source <name>] [--from <stamp|key|latest>]
                  [--target <name>]
                  [--target-host <h>] [--target-port <p>] [--target-db <d>]
                  [--target-user <u>] [--target-path <path>] [--dry-run]
arkstore cleanup  [generate-plan | execute-plan <plan> | run | consolidate-plans]
                  [--source <name>] [--dry-run]
arkstore archive  [--source <name>] [--dry-run]
arkstore verify   [--source <name>] [--from <stamp|key|latest>] [--target <name>] [--dry-run]

Global: --config, --log-level, --timezone, --version, --help
```

- Subcommand-per-operation (clap-derive), consistent flags across operations.
- `--dry-run` and `--source` available wherever they make sense; `--type` narrows `backup` to one
  engine type.
- Restore takes a **positional action** (`restore` default, `list-backups` — same style as
  `cleanup`), a `--from` selector, a `--target <name>` entry selector (defaults to the source's
  name), and per-field **target overrides** (each: flag > env > chosen entry > default — §6.2). The
  target password is never a flag (§8).
- `arkstore --version` prints version **and the engines compiled in**.

---

## 11. Packaging & Distribution

**Cross-platform / OS-agnostic is a first-class requirement.** The same tool runs on Linux, macOS,
and Windows; one static binary per platform, built from one codebase.

- **Single static binary** per platform (musl for Linux to avoid glibc coupling); no runtime deps.
  TLS is `rustls`, never OpenSSL, so the musl build has no system-library dependency.
- **Prebuilt release binaries** for every supported target, published on GitHub Releases with
  checksums (see the release workflow):
  - Linux `x86_64` (gnu + musl) and `aarch64`
  - macOS `aarch64` (Apple Silicon) and `x86_64` (Intel)
  - Windows `x86_64`
- **Cargo features** = the engine opt-in model: `postgres`, `mysql`, `mongo`, `archive`, `files`.
  Default feature set is a sensible common case; `full` / `--all-features` builds everything.
  Release binaries ship `full`; optional slim per-engine builds are possible.
- **OS-agnostic by construction:** the crate uses cross-platform std and portable crates
  (`std::path::PathBuf`, `available_parallelism`, native `tar`/`flate2`/`zstd`/`arrow` rather than
  shelling out to `tar`/`gzip`), and every database engine is driven in-process over its wire
  protocol (§5.1). There is **no OS-dependent code path and no external tool on any path** —
  backup, restore, verify, archive, and file operations need nothing beyond the binary.
- **The container image is a `scratch`/distroless base plus the binary — and nothing else**: no
  database client packages, no shell, no interpreter. Its size and attack surface are those of the
  static binary alone.
- **CI builds and tests on Linux, macOS, and Windows** on every change; the release workflow
  cross-builds the full target matrix on tags.
- `cargo install arkstore --features …` for source installs.

---

## 12. Design Advantages

1. **One static binary, zero runtime** — no Python/venv/lockfile to provision; trivial to ship in
   a scratch/distroless container or drop on a host.
2. **Compile-time engine selection** — opt-in engines become build features producing one
   artifact, instead of install-time extras resolved against a package index at deploy.
3. **Memory & concurrency safety** — `#![forbid(unsafe_code)]`; fearless parallel multi-source
   backup and multi-month archival via async + bounded task pools.
4. **Streaming everywhere** — stream dump→compress→upload and query→parquet→upload without
   buffering whole datasets in memory (bounded memory even on large tables).
5. **Native Parquet via arrow-rs** — no separate columnar runtime dependency.
6. **Pluggable object store** — S3-compatible now, GCS/Azure later, behind one abstraction.
7. **Typed config with precise errors** — serde-validated config that points at the bad field.
8. **Deterministic, testable core** — inject clock/object-store/DB behind traits; fast unit tests
   without live infra.
9. **No database client tools, anywhere** — every engine is spoken to over its wire protocol from
   inside the binary (§5.1); nothing to install, version-match, or ship in an image, and
   credentials never leave process memory.
10. **Verification is built in** — the `verify` operation (§6.5) round-trips a backup into a
    throwaway target and diffs it against the manifest baseline, so fidelity is proven, not assumed.
11. **Roadmap: client-side encryption**, PITR-adjacent options, additional object stores.

---

## 13. Design Decisions & Rationale

These are deliberate choices baked into the requirements above. They are recorded here so the
"why" survives independently of any implementation — each stands on its own reasoning.

1. **Sortable timestamps in keys.** Backup object stamps use a lexicographically-sortable form
   (`YYYY-MM-DD-HHMMSS`) so a plain object listing sorts chronologically without parsing, and the
   newest backup is trivially the max key. The stamp is rendered in `app.timezone`. *(The same
   value is what the cleanup parser reads back — one format, writer and reader agree.)*

2. **One timezone for every calendar decision.** Both cleanup banding and the archive cutoff are
   computed in `app.timezone` (weeks start Monday; cutoffs at local midnight). Using a single
   configured timezone — rather than the host's local time or raw UTC — makes retention and archive
   boundaries deterministic across hosts and DST changes, and keeps "which day/week/month is this"
   consistent between the two operations. Rationale: a backup taken at 23:30 local should belong to
   *that* local day, not slip into the next UTC day.

3. **Config is a small fixed set of files, not a merged directory.** Configuration is three
   explicit layers — global policy, `sources`, and (optional) `targets`. This keeps the effective
   config obvious and diffable; secrets are the only thing merged in at load time (§8), and CLI/env
   override at the edges (§7). Rationale: predictable precedence beats "whatever happened to be on
   the config path."

4. **The `latest` pointer is the only mutable object.** Every versioned backup is write-once and
   never overwritten; only `<source>.latest.tar.gz` is rewritten each run. Rationale: immutable
   history is safe to cold-tier and safe to reason about in retention; a single mutable pointer
   gives O(1) "give me the newest" without listing.

5. **Archives live outside the backup folder.** Archived Parquet uses a sibling top-level prefix
   (`archive.s3_prefix`), never under `aws.folder`. Rationale: cleanup scans the backup folder by
   key shape, and archives must be structurally invisible to it — a Parquet key can never be
   mistaken for a backup key and pruned. (Even if scanned, its shape is unparsable ⇒ kept — §6.3.)

6. **Cleanup scans object storage, not the config.** Retention decisions come from what is actually
   in the bucket, so backups from sources later removed from config are still pruned. Rationale:
   the bucket is the source of truth for what exists; config drift must not orphan storage forever.

7. **Native wire-protocol drivers for everything; no client tools.** Backup, restore, and archive
   for every engine go through a pure-Rust driver speaking the wire protocol, compiled into the
   binary (§5.1). `pg_dump`/`mysqldump`/`mongodump` are themselves only wire-protocol clients, so
   nothing they do is out of reach; what they *guarantee* — a consistent snapshot and a defined
   fidelity scope — is written into the requirements (§5.1.1, §5.1.2) and proven by `verify` (§6.5)
   instead of being inherited by shelling out. Rationale: zero host prerequisites, no
   version-matching of client tools, credentials that never leave process memory, one code path
   per engine, and a container image that is just the binary.

8. **Engine selection is compile-time, not runtime.** Engines are Cargo features producing one
   self-contained artifact (§5); using an engine not built in fails fast with a rebuild message.
   Rationale: a single static binary with no package-index resolution at deploy, and no way to
   "accidentally" depend on an engine that isn't actually present.

9. **Backups are all-or-nothing, with a manifest.** A source aborts and uploads nothing if any
   object fails to dump, and every database archive carries a `manifest.json` with a `sha256` per
   file that restore validates (§6.1/§6.2). Rationale: a partial archive that silently drops an
   object is indistinguishable from a good one until a restore fails — so it must never be produced,
   and integrity must be checkable at restore time, not assumed.

10. **`structure`/`data` toggles and split `ignore` semantics.** A source controls schema and data
    independently, and `ignore` skips an object's *data while keeping its structure* (recreated
    empty on restore), distinct from `ignore_startswith` which drops the object entirely. Rationale:
    real deployments need "keep the shape of this table but not its (huge/sensitive/transient) rows"
    without losing the schema — a single exclude list can't express that.

11. **One consistent snapshot per source.** A backup reads every object of a source at a single
    transactional point in time (`REPEATABLE READ` + `pg_export_snapshot()` / `WITH CONSISTENT
    SNAPSHOT` — §5.1.1), and a dump that cannot establish its snapshot fails before reading
    anything. Rationale: a set of per-table reads taken at different moments is not a backup of the
    database — it is a collection of mutually inconsistent tables that may violate every foreign
    key on restore. Consistency is the property that makes a dump restorable, so it is a hard
    requirement, not an optimization.

12. **Fidelity is a contract, verified.** "Full fidelity" is an enumerated object list per engine
    (§5.1.2); unsupported objects are logged rather than silently dropped; and the `verify`
    operation (§6.5) round-trips real backups in CI and on demand. Rationale: a native dump's
    correctness must be demonstrable — the enumerated contract is what "done" means, and the
    round-trip is what checks it.

---

## 14. Non-Functional Requirements

- **Safety:** no `unsafe`; every destructive op preview-first and verify-before-delete.
- **Performance:** parallel sources; streaming I/O; bounded memory independent of dataset size;
  cheap dry-runs (metadata/count only).
- **Portability:** static binaries for major platforms; S3-compatible endpoints.
- **Reliability:** idempotent archive; per-item isolation; validated cleanup plans; one consistent
  snapshot per backup (§5.1.1).
- **Fidelity:** every engine backend satisfies its enumerated fidelity contract (§5.1.2), proven by
  `verify` round-trips against containerized databases in CI.
- **Testability:** ≥80% coverage target; trait-injected dependencies; integration tests against
  containerized DBs + MinIO.
- **Documentation:** per-operation docs, config reference, lifecycle guidance, migration notes.

---

## 15. Milestones / Roadmap

- **M0 — Skeleton + first native engine:** CLI, config/secrets loading, object-store abstraction,
  logging, dry-run plumbing; **PostgreSQL backup + restore fully native** (snapshot §5.1.1,
  catalog-driven schema dump, `COPY` data, manifest with dependency graph); a first `verify`
  round-trip (§6.5) gating the fidelity contract in CI.
- **M1 — Cleanup:** full retention model, plan/execute/consolidate, audit trail, validation.
- **M2 — Archive:** Postgres archive engine, Parquet writer, whole-months policy, verify-before-delete.
- **M3 — Multi-engine:** MySQL/MariaDB and MongoDB backup/restore/archive via their native
  backends (§5.1); file sources; `verify` extended to each engine.
- **M4 — Distribution:** prebuilt releases, container image (binary only), docs site.
- **M5 — Extensions:** client-side encryption; additional object stores (GCS/Azure); oplog-based
  point-in-time consistency for MongoDB; Postgres large objects; optional `pg_dump`
  custom-format output (via `libpgdump`, BSD-3-Clause) so stock `pg_restore` can consume Arkstore
  archives.

---

## 16. Open Questions

1. Default feature set for the primary release build — `postgres,archive,files` only, or `full`?
2. Config format — stay YAML, or offer TOML (more idiomatic in Rust) too?
3. Minimum supported object stores for v1 (AWS S3 + MinIO only, or GCS/Azure day one)?
4. Postgres binary `COPY` — keep as opt-in only, or auto-select when source and target server
   versions and architectures match?
5. `tokio-postgres` vs `sqlx` as the Postgres driver (both pure Rust, both support `COPY`) — the
   choice decides whether one driver stack (`sqlx`) can serve Postgres and MySQL together.
**Resolved (now specified above):**
- *Reuse vs. build for the Postgres schema dump* — **build.** A crates.io survey (2026-09) found no
  pure-Rust crate usable as a dependency: `pg_dbmigrator` shells out to `pg_dump`/`pg_restore` (per
  its own README) — the very thing being avoided; `elefant-tools`/`elefant-sync` is the one genuine
  pure-Rust wire-protocol implementation, but it carries **no license** (none on crates.io, no
  `LICENSE` in its repository — unusable as a dependency), is self-described experimental (`0.0.8`),
  explicitly does **not** run in one transaction (no snapshot consistency, §5.1.1), rides a `0.1`
  custom driver (`elefant-client`, COPY support undocumented), and lacks exclusion constraints,
  collations, and RLS from the fidelity contract (§5.1.2). It remains the best public *reference*
  for catalog→DDL behaviour, and its "not supported" list is a useful checklist of hard cases.
  `pgpushy-core` parses DDL source files via `libpg_query` (C) rather than introspecting a catalog —
  not applicable. `libpgdump` (BSD-3-Clause) reads/writes `pg_dump` custom/directory/tar archives
  without a server: not a dumper, but a viable path to emitting `pg_restore`-compatible archives
  (roadmap, §15). No longer open.
- *Dump mechanism* — fully native wire-protocol backends for every engine; no external client
  tools and no `dump_strategy` knob (§5.1, §13.7). No longer open.
- *`verify` operation* — in scope as a core operation from M0 (§6.5). No longer open.
- *Restore target model* — a dedicated optional `targets` layer, overridable inline and by
  CLI/env with per-field precedence (§6.2, §7). No longer open.
- *Calendar timezone* — all calendar math (cleanup banding + archive cutoff) uses `app.timezone`
  (§13.2). No longer open.
