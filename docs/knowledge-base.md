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

1. **Enumerate objects** (tables / collections) from the live source; apply the
   source's ignore rules (§2.3).
2. **Dump each object** per the source's `dump_strategy` (§8) and its
   `structure` / `data` toggles:
   - `structure: true` → dump the object's DDL/definition.
   - `data: true` → dump the object's rows/documents.
   A source may be structure-only, data-only, or both.
3. **Completeness gate** — if *any* object fails to dump, **abort the source and
   upload nothing**. A partial archive is never produced.
4. **Write `manifest.json`** at the archive root (database sources): format
   version, source name, engine type, `created_at`, and one entry per file
   `{path, object_name, size, sha256}`.
5. **Package** — tar + stream through gzip to `versioned/<source>.<stamp>.tar.gz`.
6. **Upload** (when `backup_to_s3`), then **verify** (size / checksum) before
   declaring success. `backup_to_s3: false` keeps the backup local only.
7. **Local lifecycle** — always remove the per-source working dir; remove the
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

- Per-source **failure isolation**: a bad source (creds, missing tool, upload
  error) is logged + recorded; the run continues to the next source.
- An empty / no-match selection is a clean warning, not an error.
- Exit `1` if any source failed, else `0`.
- The `latest` pointer is the only mutable object (§Design decision 4, PRD §13).

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
   before any transfer) — except a local single-dump restore.
4. Resolve `--from` → download → **safe-extract** (§8/PRD §9.6).
5. **Validate against `manifest.json`.** The manifest is the authority on **which
   files each object should have** — "missing" means missing *relative to the
   manifest*, not "a file another object type would carry":
   - A **data file the manifest records** that is missing/corrupt ⇒ the object
     **fails**. An object the manifest records as **structure-only** (or
     data-skipped, §2.3) legitimately has **no** data file — expected, not a
     failure; recreated empty (§5.7).
   - A **structure file the manifest records** that is missing/mismatched is
     non-fatal **only when the schema is otherwise available** — the data file is
     self-describing (own DDL, as a full per-table dump has) *or* the target
     already defines the object; then fall back to deriving FK order (§5.5) from
     the data file. Otherwise a genuinely **data-only** object with no target
     schema and no DDL **fails** — never load data into a non-existent table
     (§5.7).
6. **Compute load order** (§5.5).
7. **Load** per layer, then verify object presence. Per-object failure isolation:
   a failed object is recorded + skipped, never aborting the run.
8. Emit a redacted `{restored, skipped, failed}` summary; temp dir always cleaned.

### 5.5 Foreign-key ordering

- Parse FK relationships (`FOREIGN KEY … REFERENCES <parent>`) **COPY-block-aware**,
  and topologically **layer** objects so parents load before children. Mongo is a
  single layer (no FKs).
- **Cycles**: load the tables with FK **application deferred**, then apply the
  deferred constraints in a final pass.

### 5.6 SQL preprocessing (before load)

Makes dumps portable across servers/privilege levels — all **COPY-aware** so data
blocks are untouched:

- strip `ALTER … OWNER TO` and `GRANT` / `REVOKE`;
- drop statements a target server can't accept (e.g. a newer server's
  `SET transaction_timeout`);
- defer `ADD CONSTRAINT … FOREIGN KEY` (feeds the cycle handling in §5.5).

**Retry-with-fallback:** if a permission error blocks disabling constraint triggers
(`session_replication_role = replica`), retry the load once without it.

### 5.7 Structure-only, data-only & single-item restore

- **Structure-only** — an object backed up structure-only (or in the data-skip
  `ignore` set, §2.3) is **recreated empty** from its structure file when no data
  file is present.
- **Data-only** — an object backed up data-only (`structure: false`) carries no
  DDL, so its **schema must already exist in the target** (or be recreated by a
  prior structure restore / migration). Restoring data-only into a target that
  lacks the object **fails that object** (§5.4 step 5) — the loader never
  fabricates a schema or writes rows to a non-existent table.
- **Single-item** — a local single dump (`.sql` / engine archive) restores on its
  own; the target object must be **absent or empty** first. (Single-item **file**
  restore is unsupported — file restores use the full-tree path.)

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
| `dump_strategy` | DB | `auto` (§8, PRD §5.1) |
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
- **Never log secret values.** DB passwords never reach argv — the client tool's
  env var (`PGPASSWORD` / `MYSQL_PWD`) or a `0600` temp credentials file (Mongo),
  removed after use. Restore target password may also come from a TTY prompt.

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
- **Error redaction** — on child-tool failure surface only the exit code + a safe
  message, never argv/stdout/stderr (can carry secrets).
- **Signals** — Ctrl-C aborts promptly, cleans temp dirs, exits `130`; an
  interrupt during a child dump tool is surfaced as an interrupt, not a failure.

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
- **No multiprocessing.** Crash isolation for the risky part is already free: dump
  tools (`pg_dump`, …) run as child processes at that boundary.
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
- The **only OS-dependent edge** is a Postgres *full logical* backup/restore under
  the `external` dump strategy (`pg_dump`/`pg_restore` on `PATH`). Mongo and MySQL
  backup/restore use native Rust backends, and all archival + file ops need nothing
  external. See the dump-strategy design in [PRD §5.1](../PRD.md). A native Postgres
  dump is a roadmap item to close even that edge.
- Prebuilt release binaries: Linux `x86_64` (gnu + musl) + `aarch64`, macOS
  `aarch64` + `x86_64`, Windows `x86_64`. CI builds/tests on all three OSes; the
  release workflow cross-builds the matrix on version tags.
