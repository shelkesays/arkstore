# Arkstore Knowledge Base

The detailed behavioral spec behind the [PRD](../PRD.md) — the algorithms,
invariants, and on-disk/S3 formats that the implementation must honor. This is
the reference to consult when implementing an operation; the PRD says *what*, this
says *exactly how*.

> **Implementation status:** like the PRD, this is a **specification ahead of
> implementation** (roadmap: [PRD §15](../PRD.md)). Present-tense wording states
> *required* behavior; a documented action, flag, or config field may not yet be
> wired into the binary — that is expected at the current M0 skeleton stage, not a
> defect.

---

## 1. S3 key layouts

**Backups** live under `aws.folder`:

```
<folder>/<source>/versioned/<source>.<stamp>.tar.gz   # immutable, timestamped
<folder>/<source>/<source>.latest.tar.gz              # mutable pointer, rewritten each run
```

- `<stamp>` is a sortable timestamp (`YYYY-MM-DD-HHMMSS`) in `app.timezone`.
- The `latest` pointer is the **only** mutable object; versioned objects are never overwritten.

**Archives** live under `archive.s3_prefix`, a **sibling top-level prefix outside
`aws.folder`** so cleanup never sees them:

```
<archive_prefix>/<source>/<table>/<table>.<YYYY-MM>.parquet
# e.g. archive/appdb/logs/logs.2026-04.parquet
```

**Plans / audit** live under `cleanup.plans_prefix`:

```
<plans_prefix><YYYY-MM-DD-HHMMSS>-retention-plan.json.gz
<plans_prefix><YYYY-MM-DD-HHMMSS>-cleanup-report.csv.gz
<plans_prefix>consolidated/{daily,weekly,monthly,yearly}/<period>.{json,csv}.gz
```

---

## 2. Backup

Per enabled source (narrowable to one engine type via `--type`, or one source via
`--source`):

### 2.1 Database source

1. **Open the snapshot** — one consistent, read-only snapshot for the whole
   source (§11.1). Cannot establish it ⇒ the source fails before reading anything.
2. **Enumerate objects** (tables / collections) from the catalogs inside that
   snapshot; apply the source's ignore rules (§2.3).
3. **Dump each object** through the engine's native wire-protocol backend (§11),
   inside the snapshot, per its `structure` / `data` toggles:
   - `structure: true` → dump the object's DDL/definition (`<obj>.schema.sql` /
     `metadata.json`).
   - `data: true` → dump the object's rows/documents (`<obj>.data.copy` /
     `.data.tsv` / `.bson` — §11.3).
   A source may be structure-only, data-only, or both.
4. **Completeness gate** — if *any* object fails to dump, **abort the source and
   upload nothing**. A partial archive is never produced.
5. **Write `manifest.json`** at the archive root (database sources; schema in
   §2.5): format version, source name, engine type + server version, snapshot identity,
   `created_at`; one entry per file `{path, object_name, kind, size, sha256}`; and
   per object its **dependency graph** (FK parents, view deps), **row/document
   count**, and (SQL) an order-independent **content hash** — the baseline for
   restore ordering (§5.5) and `verify` (§12).
6. **Package** — tar + stream through gzip to `versioned/<source>.<stamp>.tar.gz`.
7. **Upload** (when `backup_to_s3`), then **verify** (size / checksum) before
   declaring success. `backup_to_s3: false` keeps the backup local only.
8. **Local lifecycle** — always remove the per-source working dir; remove the
   local finished archive only when `delete_after_upload` *and* the upload
   verified. Otherwise **`local_retention: N`** bounds kept copies: `N ≥ 1` keeps
   the newest `N` **versioned** archives per source (oldest deleted first);
   **`N = 0` disables pruning and retains all versioned archives.** The `latest`
   pointer is always kept and **never counted** toward `N`.

### 2.2 File source

- tar + gzip the configured `path` tree; upload the same way.
- Honours `ignore` / `ignore_startswith` (fnmatch on the entry **basename**) and
  `ignore_extensions`. **Symlinks are preserved but never followed.** Top-level
  entries under `path` are copied preserving their subtrees.

### 2.3 Ignore semantics (per source)

- **`ignore_startswith`** — object-name prefixes excluded **outright** (no
  structure, no data). Typical defaults: Postgres `pg_` / `rds_` / `awsdms_`;
  Mongo `system.` / `local.`.
- **`ignore`** — per engine:
  - **PostgreSQL** — **data skipped, structure kept** (recreated empty on restore,
    §5.7). This data-skip semantic is Postgres-only.
  - **MySQL/MariaDB, MongoDB, file sources** — **outright exclusion** (no structure,
    no data), same as `ignore_startswith`.

### 2.4 Invariants

- Per-source **failure isolation**: a bad source (creds, unreachable host,
  snapshot failure, upload error) is logged + recorded; the run continues to the
  next source.
- An empty / no-match selection is a clean warning, not an error.
- Exit `1` if any source failed, else `0`.
- The `latest` pointer is the only mutable object (§Design decision 4, PRD §13).

### 2.5 `manifest.json` v1

The manifest is the archive's authority on what it contains (§5.4). Version 1:

```json
{
  "manifest_version": 1,
  "source": "appdb",
  "engine": "postgre",
  "server_version": "16.3",
  "created_at": "2026-09-04T02:15:07Z",
  "stamp": "2026-09-04-074507",
  "timezone": "UTC",
  "snapshot": { "kind": "pg_snapshot", "id": "00000003-00000002-1" },
  "session": {
    "DateStyle": "ISO, YMD", "IntervalStyle": "postgres", "extra_float_digits": "3",
    "bytea_output": "hex", "TimeZone": "UTC", "client_encoding": "UTF8",
    "standard_conforming_strings": "on"
  },
  "objects": [
    {
      "name": "public.orders",
      "kind": "table",
      "depends_on": ["public.customers"],
      "row_count": 12345,
      "content_hash": "sum256:9f2c…64 hex…",
      "files": [
        { "path": "public.orders.schema.sql", "role": "structure", "size": 812,   "sha256": "…" },
        { "path": "public.orders.data.copy",  "role": "data",      "size": 90211, "sha256": "…" }
      ]
    }
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `manifest_version` | int | `1`. Readers reject unknown versions. |
| `source`, `engine` | string | Source name (§6.2 grammar); `postgre` \| `mysql` \| `mongo`. |
| `server_version` | string | As reported by the server; gates restore-side behaviour (§11.4). |
| `created_at` | RFC 3339 UTC | Wall-clock at snapshot open. |
| `stamp`, `timezone` | string | The key stamp (§1) and the `app.timezone` it was rendered in. |
| `snapshot` | object | `{kind, id}` — `pg_snapshot` + exported id; `mysql_consistent` + `null`; `mongo_none` + `null`. |
| `session` | object | The pinned session settings the data was encoded under (§11.3); empty for Mongo. |
| `objects[].name` | string | Schema-qualified (`schema.object`) or `db.collection`. |
| `objects[].kind` | enum | `table` \| `view` \| `matview` \| `sequence` \| `function` \| `trigger` \| `type` \| `extension` \| `collection` \| `mongo_view`. |
| `objects[].depends_on` | string[] | Names this object must be created/loaded **after** (FK parents, base relations, referenced types). Drives §5.5. |
| `objects[].row_count` | int \| null | Rows/documents in the snapshot; `null` for kinds without data. |
| `objects[].content_hash` | string \| null | `sum256:<64 hex>` (§11.3); SQL tables only. |
| `objects[].files[]` | object[] | `{path, role, size, sha256}`; `role` ∈ `structure` \| `data` \| `metadata`; `path` relative to the archive root, no `..`, no leading `/`. |

Rules: every file in the tar **must** appear in `objects[].files` — a file present in the archive but
absent from the manifest is logged and **never loaded**; every listed file must exist and match
its `size` + `sha256` (§5.4). A structure-only object simply lists no `data` file.

---

## 3. Cleanup — retention algorithm

Cleanup scans the bucket **directly** (not the config), so it also prunes backups
from sources removed from the config.

### 3.1 Parse

Parse each key into `(source, stamp, kind)` from the layout in §1. Three classes
of key are **untouchable**:

- the `latest` pointer,
- today's backups (live),
- **any key whose layout or timestamp cannot be parsed** — unparsable ⇒ **keep**.

### 3.2 Band

All decisions are timezone-aware (`app.timezone`); **weeks start Monday**. Each
band keeps the **newest backup per period** and deletes the rest:

| Band | Tier | Period | Kept |
|---|---|---|---|
| Today | — | — | everything |
| Earlier days, this week | daily | day | newest per day |
| Earlier weeks, this month | weekly | ISO week | newest per week |
| Earlier months, this year | monthly | month | newest per month |
| Prior years | yearly | year | newest per year |

### 3.3 Invariants (safety)

- **Grouping is per source** — one source's backups never thin another's.
- **A period group is never emptied** — a period with a single backup keeps it.
- **Disabling a tier keeps its whole band**, or folds it into the next coarser
  enabled tier — never a wholesale delete. (Disabling `yearly` ⇒ prior years fall
  back to `monthly`, not deletion.)

### 3.4 Plan / execute / consolidate

- **`generate-plan`** — scan, emit a plan (JSON) + report (CSV) locally, upload
  gzipped/timestamped copies to `plans_prefix`. Deletes nothing.
- **`execute-plan`** — execute a plan (local path, full S3 key, or bare filename
  resolved under `plans_prefix`; reject `..` or leading `/`).
- **`run`** (default) — generate → persist → execute → consolidate.
- **`consolidate-plans`** — merge audit files into one file per period at the
  finest enabled tier; **write the merged file before deleting the originals** so
  the trail is never lost mid-way.

**Plan JSON** carries: scan statistics, per-storage-class totals, a per-source
keep/delete summary, and full `keep` / `delete` lists with a `reason` per object
(`latest_pointer`, `current_day`, `latest_daily`, `older_weekly`, `unparsed_keep`, …).

**Plan CSV** — one row per scanned object:
`source, s3_key, timestamp, storage_class, size_bytes, action, reason`.

**Plan validation (before any delete)** — required keys present; `keep`/`delete`
are lists; no delete entry missing its key; no duplicate delete keys; keep and
delete sets **disjoint**. Any violation ⇒ abort, delete nothing.

**Execution** — delete in batches of `delete_batch_size` (S3 max 1000); log the
storage-class breakdown of the delete set first; report scanned/kept/deleted,
bytes reclaimed, elapsed. Dry-run counts batches, sends none.

---

## 4. Archive — whole-months algorithm

Config-driven: a source is processed only if enabled, archivable, and it declares
a non-empty `archive` list. Empty/absent ⇒ log and skip.

### 4.1 Cutoff

For each rule: `retention = rule.retention_days ?? archive.default_retention_days`.

```
cutoff = midnight_today(app.timezone) - retention_days
if archive.whole_months:      # default true
    cutoff = first_of_month(cutoff)
```

Rows with `time_column >= cutoff` **stay**; everything older is archived in
**whole-calendar-month partitions**, one Parquet file per month.

### 4.2 Worked example

Run with a 90-day window; today puts the raw cutoff at **20 May**.

- `whole_months: true` → cutoff snaps to **1 May**. Everything through **30 April**
  is archived (April kept whole, one file per prior month); the table retains
  **1 May onward**.
- `whole_months: false` → archive to the exact **20 May** cutoff, trimming the
  boundary month (the same month can then be archived in pieces across runs).

### 4.3 Per-month loop

For each whole month older than the cutoff, oldest first:

1. `count(*)` for the month (grouped query — makes dry-run cheap).
2. Fetch the month's rows in batches of `fetch_batch_size`.
3. Write Parquet (`compression`), stream to `…/<table>.<YYYY-MM>.parquet`.
4. **Verify** the uploaded object.
5. If `delete_after_archive` (default true): **only now** `DELETE` that month's
   rows from the source.

### 4.4 Why deleting after the fetch is safe

These are append-only tables. A row inserted after a partition is read carries a
timestamp ≥ now > cutoff, so it can never fall inside an already-archived month.
The operation is therefore **idempotent** — a re-run only sees rows still older
than the cutoff.

### 4.5 Schema inference

- **SQL** — columns keep native types.
- **Mongo** — documents are flattened: scalars/dates pass through; nested
  docs/arrays → JSON strings; BSON-only types (`ObjectId`, `Decimal128`, …) are
  stringified. Every collection yields a stable, columnar schema.

### 4.6 Dry-run

Reports month partitions, per-month and per-table row counts, per-source totals,
and whether it would delete — via the single grouped `count(*)`. Reads nothing in
bulk, uploads nothing, deletes nothing. A verified-upload failure never proceeds
to the delete for that month.

---

## 5. Restore

Restore reconstructs **one** named `--source` into an operator-provided target
that may differ from the origin. It never iterates sources. It is the most
correctness-sensitive operation: preview-first, target-guarded, integrity-checked.

### 5.1 Actions & selection

- Action is a **positional** value (like `cleanup`), not a `--action` flag:
  - **`restore`** (default when omitted) — the full flow below.
  - **`list-backups`** — list versioned backups for the source (key / size /
    last-modified), newest first; reads only.
- **`--from`** selects the backup: `latest` (default), a specific stamp/key, or a
  local dump/archive path (offline / single-item).

### 5.2 Target resolution (two stages)

1. **Which target entry** — chosen by **`--target <name>` > env `ARKSTORE_TARGET`
   > the source's own name** (a source restores by default to the target sharing
   its name); if no `targets` entry matches, fall back to inline `restore.target`.
   No resolvable target ⇒ error.
2. **Per-field overrides** — each field then resolves **CLI flag > env var > chosen
   target entry > engine default**: `--target-host/-port/-db/-user/-path`. Ports
   default per engine (5432 / 3306 / 27017); Mongo `auth_db` defaults to the target
   db.
- **Password never comes from argv** — env / config / interactive `getpass` on a
  TTY only (§6, §8).

### 5.3 Never-production guard (runs first, before any read/write)

- **Abort** if target host+port+db is identical to the source.
- **Warn** if same server, different db.
- **Reject** a file target path that overlaps the source path.

### 5.4 Flow (database source)

1. Resolve target → never-production guard.
2. Build the engine loader (fails fast if that engine wasn't compiled in).
3. **Prove the target is empty before download/extract** (non-empty ⇒ abort early,
   before any transfer) — except a local single-dump restore. "Empty", strictly:
   **Postgres** — no relations/views/sequences/functions/types in any non-system
   schema (outside `pg_catalog`, `information_schema`, `pg_toast`); **MySQL** — no
   tables/views/routines/triggers/events in the db; **Mongo** — no non-system
   collections; **file** — directory absent or has no entries.
4. Resolve `--from` → download → **safe-extract** (§8/PRD §9.6).
5. **Validate against `manifest.json`.** The manifest is the authority on **which
   files each object should have** — "missing" means missing *relative to the
   manifest*, not "a file another object type would carry":
   - A **data file the manifest records** that is missing/corrupt ⇒ the object
     **fails**. An object the manifest records as **structure-only** (or
     data-skipped, §2.3) legitimately has **no** data file — expected, not a
     failure; recreated empty (§5.7).
   - A **structure file the manifest records** that is missing/mismatched is
     non-fatal **only when the target already defines the object** (rows load
     into the existing schema). Native data files carry **no DDL** (§11.3), so
     there is no "self-describing data file" fallback: target lacks the object ⇒
     **fail** — never load data into a non-existent table (§5.7).
6. **Compute load order** (§5.5).
7. **Load** per layer, then verify object presence. Per-object failure isolation:
   a failed object is recorded + skipped, never aborting the run.
8. Emit a redacted `{restored, skipped, failed}` summary; temp dir always cleaned.

### 5.5 Foreign-key ordering

- Load order comes from the **dependency graph recorded in the manifest** (read
  from the catalogs at dump time — `pg_constraint` / `information_schema` — never
  by parsing SQL text). Topologically **layer** objects so FK parents load before
  children and views after their base relations. Mongo is a single layer.
- **Cycles**: create the tables and load their data with FK constraints
  **deferred**, then apply the deferred constraints in a final pass.

### 5.6 Load mechanics (native)

No text-preprocessing step exists: Arkstore **emits the DDL it restores**, so
portability is a property of the dump (§11.2) — ownership/privileges are not
emitted unless `include_privileges` is on, FK constraints are emitted separately
so they can be deferred (§5.5), and no server-version-specific statements are
ever written.

- **PostgreSQL** — apply `schema.sql` over the driver; stream `data.copy` with
  `COPY … FROM STDIN`; set sequence values last. Attempt
  `SET session_replication_role = replica` for speed / cycle tolerance, with
  **retry-with-fallback**: on a permission error, retry the load once without it.
- **MySQL/MariaDB** — apply DDL; load `data.tsv` as **batched multi-row `INSERT`**
  under `FOREIGN_KEY_CHECKS=0, UNIQUE_CHECKS=0` (restored after). Never
  `LOAD DATA LOCAL INFILE` (needs server-side opt-in).
- **MongoDB** — create the collection with its recorded options, `insertMany`
  batches, then `createIndexes` from `metadata.json`.

### 5.7 Structure-only, data-only & single-item restore

- **Structure-only** — an object backed up structure-only (or in the data-skip
  `ignore` set, §2.3) is **recreated empty** from its structure file when no data
  file is present.
- **Data-only** — an object backed up data-only (`structure: false`) carries no
  DDL, so its **schema must already exist in the target** (or be recreated by a
  prior structure restore / migration). Restoring data-only into a target that
  lacks the object **fails that object** (§5.4 step 5) — the loader never
  fabricates a schema or writes rows to a non-existent table.
- **Single-item** — one object's Arkstore-produced dump (structure and/or data
  file + manifest) restores on its own; the target object must be **absent or
  empty** first; no dependency ordering applies to a single object. (Single-item
  **file** restore is unsupported — file restores use the full-tree path.)

### 5.8 File restore

- Extract the archive; copy the tree into an **empty** target directory.
- `--dry-run` reports what would be written without writing.

### 5.9 Dry-run

Runs every check and computes the load order but writes nothing.

---

## 6. Configuration & secrets

Three declarative layers (PRD §7). See [`arkstore.example.yaml`](../arkstore.example.yaml).

### 6.1 Global policy (`arkstore.yaml`)

- `app` — `name`, `timezone` (drives **all** calendar math — §3, §4), log level.
- `logger` — handler toggles: `console` (default), `file` (rotating; time-based,
  e.g. rotate at midnight, keep N files), `error-reporting` (Sentry-style, off),
  `collector` (JSON/Loki/Alloy shipping, off). Endpoints may arrive via the secret.
- `aws` — `enable`, `region`, credentials **or** instance-role, `bucket`,
  `folder`, S3-compatible `endpoint`.
- `cleanup` — `retention.{daily,weekly,monthly,yearly}`, `plans_prefix`,
  `delete_batch_size` (≤1000), `dry_run`, `consolidate_plans`.
- `archive` — `format`, `s3_prefix`, `default_retention_days`, `whole_months`,
  `delete_after_archive`, `dry_run`, `compression`, `fetch_batch_size`.
- `concurrency` — `max_sources`, `cpu_workers` (§9).

### 6.2 Sources (`sources.yaml`)

Common: `name`, `type` (`postgre|mysql|mongo|file`), `enable`. Databases add
`host`, `port`, `user`, password-via-secret. Per-source options and typical
defaults:

| Option | Applies | Default |
|---|---|---|
| `structure` | DB (mongo: false) | `true` |
| `data` | DB | `true` |
| `ignore_startswith` | DB/file | pg: `['pg_','rds_','awsdms_']`; mongo: `['system.','local.']`; else `[]` |
| `ignore` | DB/file | mongo: `['system.profile','local.startup_log']`; else `[]` |
| `ignore_extensions` | file | `['photoslibrary','DS_Store','localized']` |
| `backup_to_s3` | all | `true` |
| `delete_after_upload` | all | `true` |
| `local_retention` | all | `0` (disabled) |
| `include_privileges` | postgre/mysql | `false` (§11.2) |
| `copy_format` | postgre | `text` (`binary` opt-in — §11.3) |
| `archive` | DB | `[]` (rules `{table, time_column, retention_days?}`) |
| `path` | file | — (required) |
| `authentication_database` | mongo | source `name` |

**Source `name`** must be a safe single path segment — it becomes an S3 key
component and a local dir/file name. Grammar: `^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$`
(starts with a letter/digit; then letters, digits, `_`, `.`, `-` only; **1–64
chars**; no `/`/`\`, no `..`, no whitespace; non-empty). **ASCII-only by design**
— Unicode is rejected to avoid case-fold / normalization / homoglyph and
cross-filesystem encoding hazards. The 64-char cap keeps derived S3 keys
(`<folder>/<source>/versioned/<source>.<stamp>.tar.gz`) and filenames within the
255-byte filesystem component and 1024-byte S3 key limits. Anything else is
rejected up front.

### 6.3 Targets (`targets.yaml`, optional)

Named restore targets: DBs `name`, `host`, `db`, `user`, password-via-secret,
optional `port` and mongo `auth_db`; file `name`, `path`. Missing file is fine;
targets may be inline (`restore.target`) or fully overridden by CLI/env (§5.2).

### 6.4 Precedence & secrets

- **Precedence:** CLI flags (`--dry-run`, `--source`, `--type`, target flags)
  override env, which override config.
- **Secrets** never live in the tracked config — merged in at load time from a
  secrets manager (AWS Secrets Manager first) or a local secrets file, keyed by
  source/target name. The secret payload may also carry logging/observability
  config, so prod can route logs centrally while local runs print to console.
- **Never log secret values.** There are no child processes (§11), so passwords
  never touch argv, a child environment, or a temp file: they flow from the
  secret store into the driver's in-memory connection config (zeroizing buffers)
  and are dropped once connected. A restore target password may also come from a
  TTY prompt — in-memory only.

---

## 7. S3 lifecycle (cold-tiering & expiry)

Arkstore decides **which** objects to keep; it does **not** change storage class
or expire objects. That is delegated to **S3 lifecycle rules**, documented here so
deployments get it right.

### 7.1 Recommended schedules (measured from object creation)

| Age | Backups (`aws.folder`) | Archives (`archive.s3_prefix`) |
|---|---|---|
| 0 d | STANDARD | STANDARD |
| 30 d | STANDARD_IA | STANDARD_IA |
| 90 d | GLACIER_IR | GLACIER_IR |
| 180 d | GLACIER | GLACIER |
| expire | **365 d** | **2555 d (7 years)** |

### 7.2 The one-document / many-rules gotcha

A bucket holds **one** lifecycle document with many prefix-scoped rules.
`put-bucket-lifecycle-configuration` **REPLACES the entire document** — so apply
backup and archive rules **together in one call**, or existing rules are wiped.

- Scope each rule to its own prefix ending in `/` (e.g. `dbbackup/`, `archive/`)
  so they never overlap. (A trailing `*` also matches for lifecycle, but the plain
  form is clearer and is what `list-objects` expects.)
- **Transition days must be distinct and increasing** — you can't transition to
  two classes on the same day (why Glacier Flexible is 180, not 90).

### 7.3 Interaction with cleanup

- Cleanup deletes the daily/weekly backups within their week/month, so they never
  live long enough to reach the 90/180-day Glacier transitions. Only monthly and
  yearly survivors actually cold-tier — which is the intent.
- **`Expiration: 365` on the backup rule deletes any backup older than a year**,
  including `yearly`-tier survivors cleanup would otherwise keep. So it effectively
  caps backup retention at ~1 year. If you need backups beyond a year, raise/drop
  the expiration.
- The `latest` pointer is rewritten each run, so its age resets and it stays in
  Standard/IA (instant restore) while the source is still backed up.
- Cleanup **never** touches archives: archives live under a different prefix, and
  even if scanned, a `.parquet` key doesn't match the backup key format, so it'd
  be classified unparsable and kept.

---

## 8. Cross-cutting

- **Logging:** multi-level (`debug`/`info`/`warn`/`error`); pluggable handlers
  (console / rotating file / error-reporting / collector — §6.1); progress
  feedback for long ops (per-month `[i/N]` in archive, per-source in backup) so a
  long run never looks hung; concise per-op summary.
- **Exit codes:** `0` clean; `1` any per-item failure or a known top-level error
  (missing `aws.bucket`/`aws.region`, invalid plan, unknown action); `130` on user
  interrupt (SIGINT). Traceback only for the unexpected.
- **Dry-run** on every destructive operation: zero writes, zero deletes.
- **Verify-before-delete** for both archive (upload verified before row delete)
  and backup.

### 8.1 Robustness & hardening (handling untrusted input)

Archives and dump files can originate outside the tool, so these are hard
requirements (PRD §9.6):

- **Safe extraction** — reject path traversal (`..`, absolute), escaping
  symlinks/hardlinks and special members; extract data members only; never write
  outside the temp dir.
- **Object-key / plan-path confinement** — restore keys confined under the
  source prefix; persisted-plan paths resolved under `plans_prefix`; reject `..`
  and rooted paths.
- **Disk-space headroom** check (a multiple of the archive size) before
  download/extract.
- **Dump-file validation** — non-empty + shape-checked (a `.sql` must contain SQL)
  before loading; zero-byte/garbage files skipped + reported, never restored.
- **Identifier validation** — table/collection names from untrusted dump/file
  names validated to a strict charset and safely quoted; no unvalidated
  interpolation.
- **Error redaction** — driver / object-store errors pass through Arkstore's own
  error types with connection strings, credentialed hosts, and secret values
  redacted before any log line, summary, or error-reporting sink; raw driver
  error text is never echoed verbatim.
- **Signals** — Ctrl-C cancels in-flight tasks cooperatively: open DB
  transactions rolled back, in-progress uploads aborted (no dangling multipart
  uploads), temp dirs removed, exit `130`. All work is in-process — no child to
  reap.

---

## 9. Concurrency & parallelism

Sources are processed in parallel rather than serially. The model is
built around the workload's shape, not raw core count.

**Workload shape:** mostly I/O-bound with CPU bursts. Per source: dump/fetch (DB
network I/O) → compress / Parquet-encode (**CPU**) → upload (object-store I/O) →
optional delete (DB I/O). Cleanup is almost entirely list/delete network I/O.

### 9.1 Two independent limits (`concurrency` config block)

| Knob | Bounds | `auto` default | Fixed value |
|---|---|---|---|
| `max_sources` | sources processed at once (I/O) | **4** (conservative; protects a shared DB) | honored as-is (min 1); **not** clamped to cores |
| `cpu_workers` | compress / Parquet encode (CPU) | number of available cores | **clamped to core count** (min 1) |

- `max_sources` is deliberately *not* tied to core count — the per-source work is
  mostly network wait, so a 2-core host can still run several sources at once. Its
  job is to avoid overwhelming the database / object store.
- `cpu_workers` resolves `auto` via `std::thread::available_parallelism()` and
  clamps fixed values down to that, since oversubscribing cores only adds contention.
- Accepted YAML values: the keyword `auto` or a positive integer; `0` or any other
  string is a validation error naming the field.

### 9.2 Implementation model

- **Bounded async tasks (tokio) + a semaphore** for source parallelism — not a
  thread or process per source. CPU-bound stages offload to a blocking/rayon pool
  sized to `cpu_workers`.
- **No multiprocessing, no child processes.** Every engine is driven in-process
  over its wire protocol (§11). **Failure isolation is `Result`-based** — each
  source task's errors are collected, never propagated as a crash. A **panic is
  not isolable**: the release profile is `panic = "abort"`, so a panic ends the
  run by design (fail-fast on a bug); hence SafeLint forbids `unwrap`/`expect`/
  `panic!` outside tests — the invariant is "errors are values".
- **The real limiter is the DB and object store**, not local hardware. Watch for:
  DB load from concurrent heavy dumps against one instance; object-store throttling
  (503 SlowDown) under many parallel uploads → retry with backoff; DB connection
  limits. Future refinement: a **per-host cap** so sources sharing an instance
  serialize while different hosts run in parallel.
- **Only a maximum is meaningful** for a batch job — there is no "minimum
  parallelism" setting.

### 9.3 Invariants under parallelism

- **Failure isolation** per source/target — tasks are joined; failures aggregate
  into the exit code (one bad source never aborts the run).
- **Verify-before-delete** holds — each partition's delete waits on its own verified
  upload; parallel months are disjoint partitions, so the append-only safety
  argument (§4.4) is unaffected.
- **Readable logs** — parallel output interleaves, but every line is tagged with its
  `source`.

---

## 10. Portability (OS-agnostic)

The same binary target runs on Linux, macOS, and Windows; the crate has **no
OS-specific code path**.

- Uses cross-platform std and portable crates: `std::path::PathBuf`,
  `available_parallelism`, and native `tar`/`flate2`/`zstd`/`arrow` rather than
  shelling out to `tar`/`gzip`.
- **No OS-dependent edge and no external tool on any path.** Every engine is
  driven in-process over its wire protocol (§11); backup, restore, verify,
  archive, and file ops need nothing beyond the binary. TLS is `rustls` (no
  OpenSSL), so the musl static build has no system-library dependency. The
  container image is the binary alone.
- Prebuilt release binaries: Linux `x86_64` (gnu + musl) + `aarch64`, macOS
  `aarch64` + `x86_64`, Windows `x86_64`. CI builds/tests on all three OSes; the
  release workflow cross-builds the matrix on version tags.

---

## 11. Native engine backends

Every engine is driven over its **wire protocol** by a pure-Rust driver compiled
into the binary (PRD §5.1). No client tool is ever invoked. `pg_dump`,
`mysqldump`, and `mongodump` are themselves ordinary wire-protocol clients, so
nothing they do is out of reach; what they guarantee — a consistent snapshot and
a defined fidelity scope — is specified here instead of inherited.

**Drivers (decided, PRD §16):** Postgres `tokio-postgres` + `tokio-postgres-rustls`
— `copy_out` / `copy_in`; MySQL `mysql_async` with `rustls-tls` (no TLS backend
is enabled by default — select it); Mongo the official `mongodb` crate (default
`rustls-tls`) + `bson`. `sqlx` is not used. All pure Rust, statically linkable.
Each driver is an **optional dependency** behind its Cargo feature (`postgres` /
`mysql` / `mongo`), so a slim build contains no code for engines it lacks — the
Rust counterpart of Python install extras, resolved at build time (PRD §5).

### 11.1 Snapshot consistency (required)

| Engine | How | Parallel tables within a source |
|---|---|---|
| PostgreSQL | one `REPEATABLE READ` read-only transaction for the whole source; every catalog and table read inside it | `pg_export_snapshot()` on the leader; each worker connection runs `SET TRANSACTION SNAPSHOT '<id>'`, so all see one point in time |
| MySQL/MariaDB | `START TRANSACTION WITH CONSISTENT SNAPSHOT` at `REPEATABLE READ` (InnoDB) | not shareable across connections — tables are dumped **sequentially on the snapshot connection**; parallelism stays at the source level (`max_sources`) |
| MongoDB | single cursor per collection; **no cross-collection snapshot** without the oplog (stated in docs; oplog mode is roadmap) | — |

Non-transactional MySQL engines (MyISAM) cannot be snapshotted → per-table
warning. A source whose snapshot cannot be established **fails before any read**.

### 11.2 Fidelity contract (what "full fidelity" means)

- **PostgreSQL** — schemas; tables (columns, types, defaults, identity/serial,
  generated columns, `NOT NULL`, collation); PK/unique/FK/check/exclusion
  constraints; indexes (partial/expression included); sequences **and current
  values**; views + materialized views (dependency-ordered); functions/procedures;
  triggers; user-defined types (enum, composite, domain, range); extensions
  (`CREATE EXTENSION`); comments; declarative partitioning (parent + partitions +
  attachment); RLS policies when present. Ownership/privileges **opt-in**
  (`include_privileges: false` default). **Out of scope v1:** large objects
  (`pg_largeobject`), replication slots. Schema is read from `pg_catalog` using
  `pg_get_viewdef` / `pg_get_functiondef` / `pg_get_indexdef` /
  `pg_get_constraintdef` / `pg_get_triggerdef`; tables are assembled from
  `pg_attribute` / `pg_type` / `pg_attrdef`.
- **MySQL/MariaDB** — tables via `SHOW CREATE TABLE` (engine, charset/collation,
  `AUTO_INCREMENT`, generated columns); views (dependency-ordered); triggers;
  routines and events (`SHOW CREATE …`); foreign keys.
- **MongoDB** — documents as BSON; indexes (`listIndexes`); collection options
  (capped, validators, collation); views.

Anything encountered outside the contract is **logged as unsupported**, never
silently dropped; the completeness gate (§2.1) still applies to every listed
object.

### 11.3 Archive file formats

| Engine | Structure | Data | Restore |
|---|---|---|---|
| PostgreSQL | `<obj>.schema.sql` (emitted DDL, portable by construction) | `<obj>.data.copy` — `COPY` **text** format, `\N` nulls (binary `COPY` opt-in for same-version/arch) | DDL, then `COPY … FROM STDIN` |
| MySQL/MariaDB | `<obj>.schema.sql` (`SHOW CREATE …`) | `<obj>.data.tsv` — tab-separated, backslash-escaped, `\N` nulls | DDL, then batched multi-row `INSERT` |
| MongoDB | `<coll>.metadata.json` (indexes + options) | `<coll>.bson` | `insertMany`, then `createIndexes` |

Plus `manifest.json` (§2.5) with the dependency graph, counts, and content hashes.

**Content hash (SQL tables).** Streaming, order-independent, and sensitive to every
row: `content_hash = Σ SHA-256(row_bytes) mod 2^256`, rendered `sum256:` + 64 hex,
where `row_bytes` is the exact canonical row line the engine emits — Postgres: the
`COPY … TO STDOUT` text line without its trailing newline; MySQL: the TSV line.
Addition is commutative, so `verify` (§12) recomputes it from the restored target
in any row order and batch size; duplicated or dropped rows both change the value.
`row_count` is stored alongside. It is a corruption detector, not a cryptographic
commitment.

**Canonical text depends on session settings**, so every dump *and* verify session
pins the same set before any `COPY`/`SELECT`, and records it in the manifest
(`session`) so a later verify encodes identically:

| Engine | Pinned per session |
|---|---|
| PostgreSQL | `DateStyle='ISO, YMD'`, `IntervalStyle='postgres'`, `extra_float_digits=3`, `bytea_output='hex'`, `TimeZone='UTC'`, `client_encoding='UTF8'`, `standard_conforming_strings=on` |
| MySQL/MariaDB | `SET NAMES utf8mb4`, `SET time_zone='+00:00'`, a fixed `sql_mode` (`STRICT_ALL_TABLES,NO_ZERO_DATE,...`), `SET SESSION group_concat_max_len` irrelevant (no aggregation) |
| MongoDB | none — BSON is self-describing; `verify` compares counts and index definitions only |

**Binary `COPY`** (`copy_format: binary`, opt-in) is faster for same-version/same-arch
round-trips but its bytes are not the canonical text, so the content hash is still
computed from a text export in a second pass — which is why text is the default.

### 11.4 Supported server versions

Each backend declares a supported range and **version-gates its catalog
queries**; an unsupported server fails fast. Initial targets: PostgreSQL 13+,
MySQL 8.0+ / MariaDB 10.6+, MongoDB 5.0+ — widened only as the fidelity suite
(§12) passes for that version.

---

## 12. Verify — round-trip

`arkstore verify` proves a backup is restorable (PRD §6.5):

1. Select the backup as restore does (`--from`, default `latest`).
2. Restore into a **throwaway** target through the normal restore path (§5),
   guards included. Either a `targets` entry flagged `ephemeral: true` (used
   as-is; must pass the empty check; **never dropped** — Arkstore didn't create
   it), or one Arkstore **creates** on a configured `verify.server` (a user with
   create rights: `CREATEDB` / `CREATE` / `dbAdmin`): database
   `arkstore_verify_<source>_<stamp>`, restored into, then **always dropped** —
   on success, failure, or interrupt. Never drops a database it did not create.
3. Compare against the **manifest baseline** (§2.1): every expected object
   exists; row/document counts match; SQL per-table **order-independent content
   hash** matches; Mongo index definitions match.
4. Report `{verified, mismatched, failed}` per object with reasons; exit `1` on
   any mismatch.
5. Tear down only a target Arkstore created; never touch a pre-existing one.

Runs on demand and in CI against containerized engines for every backend — the
fidelity contract (§11.2) is gated by it. `--dry-run` reports without restoring.
