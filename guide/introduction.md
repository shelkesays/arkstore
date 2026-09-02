# Arkstore

**Backup, restore, retention-cleanup, and cold-tier archival for databases and
files — to S3-compatible object storage. One safe-Rust binary.**

> ⚠️ **Early / work in progress.** The architecture, config model, and CLI are in
> place; the operation internals are being implemented. Not yet production-ready.

Arkstore is a single command-line binary that manages the full lifecycle of your
database and file backups, with a correctness bias — every destructive path is
preview-first and verify-before-delete:

- **backup** — dump databases / snapshot file trees to compressed archives in object storage.
- **restore** — reconstruct a database or file tree from a chosen backup.
- **cleanup** — apply calendar-tier retention (daily / weekly / monthly / yearly) to backups.
- **archive** — move aged rows out of a live database into Parquet, keeping a recent window.

It is **config-driven**, **safe by default** (dry-run everywhere; delete only after
an upload is verified; never deletes a key it can't parse), and **portable** (one
statically linked binary; engines compiled in as opt-in Cargo features).

## This guide

- **[Product Requirements (PRD)](./prd.md)** — what the product does and why, plus
  the Rust design: operations, config, concurrency model, packaging.
- **[Knowledge Base](./knowledge-base.md)** — the detailed behavioral spec that
  drives the implementation: the retention algorithm and its safety invariants, the
  archive whole-months policy, the cleanup plan schema, S3 layouts, and lifecycle
  guidance.

Source and issue tracker: <https://github.com/shelkesays/arkstore>.
