# Arkstore

**Backup, restore, retention-cleanup, and cold-tier archival for databases and
files — to S3-compatible object storage. One safe-Rust binary.**

> ⚠️ **Early / work in progress.** The architecture, config model, and CLI are
> in place; the operation internals are being implemented (see the roadmap). Not
> yet ready for production use.

Arkstore is a single command-line binary that manages the full lifecycle of your
database and file backups:

- **`backup`** — dump databases / snapshot file trees to compressed archives in object storage.
- **`restore`** — reconstruct a database or file tree from a chosen backup.
- **`cleanup`** — apply calendar-tier retention (daily / weekly / monthly / yearly) to stored backups.
- **`archive`** — move aged rows out of a live database into Parquet, keeping only a recent window in the source.

It is **config-driven**, **safe by default** (dry-run everywhere, delete only
after an upload is verified, never deletes a key it can't parse), and **portable**
(one statically linked binary, engines compiled in as opt-in features).

## Why

Teams keep re-implementing the same fragile backup plumbing: dump to S3, a naming
convention, a retention script that eventually deletes the wrong thing, and an
ad-hoc job to move old log rows somewhere cheap. Arkstore consolidates those four
operations behind one declarative config and one binary, with a correctness bias:
every destructive path is preview-first and verify-before-delete.

Written in Rust (`#![forbid(unsafe_code)]`) for a single dependency-free static
binary, true parallelism, streaming I/O, and a strongly typed config.

## Supported sources & engines

| Source | backup | restore | archive | Cargo feature |
|---|:---:|:---:|:---:|---|
| PostgreSQL | ✅ | ✅ | ✅ | `postgres` |
| MySQL / MariaDB | ✅ | ✅ | ✅ | `mysql` |
| MongoDB | ✅ | ✅ | ✅ | `mongo` |
| Files / directories | ✅ | ✅ | — | `files` |

Engines are **opt-in at compile time**. Using an engine that wasn't built into
the binary fails fast with a clear rebuild message. The default build is
`postgres,archive,files`; `--features full` (or `--all-features`) builds everything.

## Build

```bash
cargo build --release                       # default: postgres + archive + files
cargo build --release --features full       # every engine
cargo build --release --no-default-features --features "mysql,archive"
```

The binary lands at `target/release/arkstore`.

## Usage

```bash
arkstore backup   [--source <name>] [--dry-run]
arkstore restore  [--source <name>] [--dry-run]
arkstore cleanup  [generate-plan | execute-plan | run | consolidate-plans] [--source <name>] [--dry-run]
arkstore archive  [--source <name>] [--dry-run]

# global: --config <path>  --log-level <level>
```

Configuration lives in a YAML file (`arkstore.yaml` by default); see
[`arkstore.example.yaml`](arkstore.example.yaml). Credentials come from a secrets
manager or a local secrets file, never the tracked config.

Sources are processed **in parallel**, bounded by the `concurrency` block:
`max_sources` (how many at once — a ceiling that protects the DB/object store) and
`cpu_workers` (parallel compression / Parquet encoding — `auto` = cores). See
[PRD §9.5](PRD.md) and [knowledge base §9](docs/knowledge-base.md).

## Platforms

Runs on **Linux, macOS, and Windows** from one codebase — no OS-specific code path.
CI builds and tests on all three; tagged releases publish prebuilt binaries for
Linux (`x86_64` gnu/musl, `aarch64`), macOS (`aarch64`, `x86_64`), and Windows
(`x86_64`).

## Development

Run the full check suite before every commit (the git hook is intentionally not
installed — run it manually; CI runs the same set):

```bash
pre-commit run --all-files
```

This runs hygiene hooks, **SafeLint** (Holzmann Power-of-Ten safety rules, Rust
rule set), `rustfmt --check`, and `clippy -D warnings`. SafeLint's grammar is
provided automatically inside the hook environment; to run it directly, install
`pip install 'safelint[rust]'`.

## Documentation

- [`PRD.md`](PRD.md) — product requirements and design.
- [`docs/knowledge-base.md`](docs/knowledge-base.md) — the detailed behavioral
  spec (retention algorithm, archive whole-months policy, plan schema, S3 layout
  and lifecycle guidance) that drives the implementation.

## Roadmap

- **M0** — CLI, config/secrets, object-store abstraction, Postgres backup + restore. *(in progress)*
- **M1** — full cleanup: retention model, plan/execute/consolidate, audit trail.
- **M2** — archive: Postgres engine, Parquet writer, whole-months policy.
- **M3** — MySQL + Mongo backup/restore/archive; file sources.
- **M4** — prebuilt releases, container image, docs, a `verify` (round-trip) op.
- **M5** — client-side encryption; additional object stores (GCS/Azure).

## License

[MIT](LICENSE) © Rahul Shelke
