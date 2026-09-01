# Arkstore — Product Requirements Document

> A safe-Rust, MIT-licensed tool for backing up, restoring, retention-cleaning, and
> cold-tier archiving databases and files to object storage. Single static binary,
> opt-in engines, config-driven, safe by default.

- **Status:** Draft v0.1 (reverse-engineered requirements + forward-looking design)
- **Owner:** @shelkesays
- **Repo:** https://github.com/shelkesays/Arkstore (existing Python repo → reset to Rust)
- **License:** MIT
- **Language:** Rust (no `unsafe` in the crate; `#![forbid(unsafe_code)]`)

---

## 1. Overview & Vision

Arkstore is a single-binary command-line tool that manages the full lifecycle of
database and file backups against S3-compatible object storage:

1. **Backup** — dump databases / snapshot file trees to compressed archives in object storage.
2. **Restore** — reconstruct a database or file tree from a chosen backup.
3. **Cleanup** — apply calendar-tier retention (daily/weekly/monthly/yearly) to stored backups.
4. **Archive** — move aged rows out of a live database into columnar (Parquet) files in
   object storage, keeping only a recent window in the source table.

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

Arkstore consolidates the four operations behind one declarative config and one binary, with a
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
- One binary, four operations, one declarative config.
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

| Source type | Backup | Restore | Archive | Mechanism |
|---|---|---|---|---|
| PostgreSQL | ✅ | ✅ | ✅ | dump/restore via client tooling; archive via native async driver |
| MySQL/MariaDB | ✅ | ✅ | ✅ | dump/restore via client tooling; archive via native driver |
| MongoDB | ✅ | ✅ | ✅ | native driver for all ops |
| Files/directories | ✅ | ✅ | ❌ | tar + compress a path tree |

**Engine opt-in (Rust feature flags):** each engine is a Cargo feature (`postgres`, `mysql`,
`mongo`, `archive`, `files`). A default build may include a common set (e.g. `postgres,archive`);
`--all-features` builds everything. Using an engine that wasn't compiled in fails fast with a
clear message: *"PostgreSQL support was not built into this binary. Rebuild with
`--features postgres` or download the full release."* — mirroring the-predecessor's optional-extras model
(`the-predecessor[postgre,archive]`) but resolved at compile time into one artifact.

Prebuilt releases: publish a **full-feature** binary per platform, plus optionally slim
per-engine builds. Because it's one static binary, "install the right extra" becomes "download
the right release asset."

---

## 6. Functional Requirements

### 6.1 Backup (`arkstore backup`)

- Iterate all **enabled** sources (or a single source via `--source <name>`).
- For a **database** source: produce a dump using the engine's standard mechanism, stream it
  through compression, and upload to object storage as `<source>.<timestamp>.tar.gz` (or the
  engine-native dump format inside the archive).
- For a **file** source: tar + compress the configured path tree and upload the same way.
- Maintain a **`latest` pointer** object per source (`<source>.latest.tar.gz`) updated on each run.
- **Timestamped, immutable** versioned objects under `versioned/`; the latest pointer is the
  only mutable object.
- Per-source **failure isolation**: one source failing (bad creds, missing tool, upload error)
  is logged and recorded; the run continues. Exit `1` if any source failed, else `0`.
- Verify the upload (size / checksum) before considering the backup successful.
- Never require network egress beyond the DB and the object store.

**S3 layout:**
```
<folder>/<source>/versioned/<source>.<stamp>.tar.gz   # immutable, timestamped
<folder>/<source>/<source>.latest.tar.gz              # mutable pointer, rewritten each run
```

### 6.2 Restore (`arkstore restore`)

- Select a backup to restore: the `latest` pointer (default) or a specific timestamp/key.
- Download, decompress, and load into the configured **target** (which may differ from the
  backup source — restore to a staging DB, a different host, etc.).
- Engine-appropriate load (native restore tooling for SQL, native driver for Mongo).
- **Preview / safety:** confirm target before a destructive load; support `--dry-run` that
  reports what would be restored (source key, size, target) without writing.
- Per-target failure isolation and meaningful exit codes.

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
  can't be parsed** (unparseable ⇒ keep).
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

---

## 7. Configuration Model

Two layers, both declarative:

1. **Global policy** (`arkstore.yaml`): app settings (timezone), `aws`/object-store settings
   (bucket, region, folder, endpoint for S3-compatible), and per-operation blocks — `cleanup`
   (retention tiers, plans prefix, batch size, consolidate flag), `archive` (format, prefix,
   default retention days, whole_months, delete_after_archive, dry_run, compression, fetch batch
   size).
2. **Sources** (`sources.yaml`): a list of source entries — `name`, `type`, `enable`, connection
   details, optional `archive` rules (block-YAML style), and restore target overrides.

Requirements:
- Strongly typed deserialization (serde) with **clear validation errors** naming the offending
  field/source, not a stack trace.
- Sensible defaults so a minimal config works; every default documented.
- Config file locations discoverable via flag / env / conventional path.
- **CLI flags override config** (e.g. `--dry-run`, `--source`).

---

## 8. Secrets Management

- Credentials **never** live in the tracked config. Two backends:
  1. **Secrets manager** (AWS Secrets Manager first; pluggable) — gated by an env toggle.
  2. **Local secrets file** (e.g. `arkstore_secrets.yaml`) for dev/self-hosted.
- Secrets merge into source connection details at load time.
- The secret payload may also carry logging/observability config (e.g. ship logs to a collector),
  so a prod deployment can route logs centrally while local runs print to console.
- Never log secret values; redact connection strings in output.

---

## 9. Cross-Cutting Requirements

### 9.1 Logging & Observability
- **Multi-level** structured logging: `debug` / `info` / `warning` / `error`, chosen by config/flag.
- Progress feedback for long operations (e.g. per-month `[i/N]` during archive, per-source
  during backup) so a long-running job never looks hung.
- Optional structured/JSON logs and shipping to a collector (Grafana/Loki/Alloy-style) via the
  secret/config-provided logger settings; console by default.
- A concise run summary per operation (scanned/kept/deleted, bytes reclaimed, elapsed).

### 9.2 Safety & Correctness
- `--dry-run` on **every** destructive operation, doing zero writes/deletes.
- **Verify-before-delete** for both archive (upload verified before row delete) and backup.
- **Never delete the unparseable** in cleanup.
- Plan validation before any cleanup execution.
- Per-source/per-target **failure isolation**; aggregate failures into the exit code.

### 9.3 Exit Codes
- `0` clean run; `1` any per-item failure or a known top-level error (e.g. missing bucket/region);
  distinct handling for expected vs. unexpected errors (traceback only for the unexpected).

### 9.4 Object Store
- S3 first via an object-store abstraction (`object_store` crate) so MinIO / S3-compatible and,
  later, GCS/Azure work with minimal change.
- Server-side encryption honored where configured.
- Cold-tiering & expiry are **delegated to object-store lifecycle rules**, not managed by
  Arkstore — but documented (recommended lifecycle schedules for backup vs. archive prefixes,
  and the "one lifecycle document, many prefix-scoped rules" gotcha).

---

## 10. CLI Design (proposed)

```
arkstore backup   [--source <name>] [--dry-run] [--config <path>]
arkstore restore  [--source <name>] [--target <name>] [--from <stamp|latest>] [--dry-run]
arkstore cleanup  [generate-plan | execute-plan <plan> | run | consolidate-plans]
                  [--source <name>] [--dry-run]
arkstore archive  [--source <name>] [--dry-run]

Global: --config, --log-level, --timezone, --version, --help
```

- Subcommand-per-operation (clap-derive), consistent flags across operations.
- `--dry-run` and `--source` available wherever they make sense.
- `arkstore --version` prints version **and the engines compiled in**.

---

## 11. Packaging & Distribution

- **Single static binary** per platform (musl for Linux to avoid glibc coupling); no runtime deps.
- **Cargo features** = the engine opt-in model: `postgres`, `mysql`, `mongo`, `archive`, `files`.
  Default feature set is a sensible common case; `full` / `--all-features` builds everything.
- GitHub Releases with prebuilt full-feature binaries (Linux x86_64/arm64, macOS, Windows) +
  checksums; optional slim per-engine builds.
- Container image: distroless/minimal base + the static binary; no interpreter, tiny image.
- `cargo install arkstore --features …` for source installs.

---

## 12. Improvements Over the Predecessor (the "much better" part)

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
9. **Roadmap: client-side encryption**, PITR-adjacent options, and a `verify` operation that
   round-trips a backup (restore into a throwaway target and diff).

---

## 13. Non-Functional Requirements

- **Safety:** no `unsafe`; every destructive op preview-first and verify-before-delete.
- **Performance:** parallel sources; streaming I/O; bounded memory independent of dataset size;
  cheap dry-runs (metadata/count only).
- **Portability:** static binaries for major platforms; S3-compatible endpoints.
- **Reliability:** idempotent archive; per-item isolation; validated cleanup plans.
- **Testability:** ≥80% coverage target; trait-injected dependencies; integration tests against
  containerized DBs + MinIO.
- **Documentation:** per-operation docs, config reference, lifecycle guidance, migration notes.

---

## 14. Milestones / Roadmap

- **M0 — Skeleton:** CLI, config/secrets loading, object-store abstraction, logging, dry-run
  plumbing, one engine (Postgres) backup + restore.
- **M1 — Cleanup:** full retention model, plan/execute/consolidate, audit trail, validation.
- **M2 — Archive:** Postgres archive engine, Parquet writer, whole-months policy, verify-before-delete.
- **M3 — Multi-engine:** MySQL + Mongo backup/restore/archive; file sources.
- **M4 — Distribution:** prebuilt releases, container image, docs site, `verify` operation.
- **M5 — Extensions:** client-side encryption, additional object stores (GCS/Azure).

---

## 15. Open Questions

1. Default feature set for the primary release build — `postgres,archive,files` only, or `full`?
2. Config format — stay YAML, or offer TOML (more idiomatic in Rust) too?
3. Restore target model — reuse source entries with overrides, or a separate `targets` list?
4. Is a `verify` (round-trip restore) operation in scope for v1 or roadmap?
5. Minimum supported object stores for v1 (AWS S3 + MinIO only, or GCS/Azure day one)?
```
