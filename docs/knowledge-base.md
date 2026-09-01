# Arkstore Knowledge Base

The detailed behavioral spec behind the [PRD](../PRD.md) — the algorithms,
invariants, and on-disk/S3 formats that the implementation must honor. This is
the reference to consult when implementing an operation; the PRD says *what*, this
says *exactly how*.

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

Per enabled source (or one via `--source`):

1. **Database** — produce a dump via the engine's standard mechanism, stream it
   through gzip, upload to `versioned/<source>.<stamp>.tar.gz`.
2. **File** — tar + gzip the configured `path` tree, upload the same way.
3. **Verify** the upload (size / checksum) before declaring success.
4. **Update** `<source>.latest.tar.gz` to point at / contain the new backup.

**Invariants**

- Per-source **failure isolation**: a bad source (creds, missing tool, upload
  error) is logged + recorded; the run continues to the next source.
- Exit `1` if any source failed, else `0`.
- Engine mechanism: SQL engines use their dump CLIs (`pg_dump`/`mysqldump`) where
  practical; Mongo uses the native driver. (Archive uses native drivers for all —
  see §4.)

---

## 3. Cleanup — retention algorithm

Cleanup scans the bucket **directly** (not the config), so it also prunes backups
from sources removed from the config.

### 3.1 Parse

Parse each key into `(source, stamp, kind)` from the layout in §1. Three classes
of key are **untouchable**:

- the `latest` pointer,
- today's backups (live),
- **any key whose layout or timestamp cannot be parsed** — unparseable ⇒ **keep**.

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

- Select the backup: the `latest` pointer (default) or a specific timestamp/key.
- Download → decompress → load into the configured **target** (which may differ
  from the source: a staging DB, a different host).
- Engine-appropriate load; per-target failure isolation; `--dry-run` reports the
  key/size/target without writing.

---

## 6. Configuration & secrets

- **Global policy** (`arkstore.yaml`): `app` (timezone, log level), `aws` (bucket,
  region, folder, endpoint), `cleanup`, `archive`. See
  [`arkstore.example.yaml`](../arkstore.example.yaml).
- **Sources**: `name`, `type` (`postgre|mysql|mongo|file`), `enable`, connection
  details, optional `archive` rules (block-YAML style), file `path`.
- **Precedence:** CLI flags (`--dry-run`, `--source`) override config.
- **Secrets** never live in the tracked config — merged in at load time from a
  secrets manager (AWS Secrets Manager first) or a local secrets file, keyed by
  source name. The secret payload may also carry logging/observability config
  (e.g. ship logs to a collector), so prod can route logs centrally while local
  runs print to console. Never log secret values.

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
  be classified unparseable and kept.

---

## 8. Cross-cutting

- **Logging:** multi-level (`debug`/`info`/`warn`/`error`); progress feedback for
  long ops (per-month `[i/N]` in archive, per-source in backup) so a long run
  never looks hung; concise per-op summary; optional JSON/collector shipping.
- **Exit codes:** `0` clean; `1` any per-item failure or a known top-level error
  (missing `aws.bucket`/`aws.region`, invalid plan, unknown action). Traceback
  only for the unexpected.
- **Dry-run** on every destructive operation: zero writes, zero deletes.
- **Verify-before-delete** for both archive (upload verified before row delete)
  and backup.
