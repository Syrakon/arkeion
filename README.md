# Arkeion

> From the Greek ἀρχεῖον (*arkheîon*): the house of the archons of Athens, where the city's
> official records were kept — the direct root of *archivum* → **archive**.
> The place where the records of record live. — [arkeion.tech](https://arkeion.tech)

**Arkeion** is an **embedded, auditable, versioned** database engine, written in pure Rust.
The guiding analogy: *as if SQLite and Git had a child… and it was born in Europe*.

**Sovereign European** data infrastructure: designed, written and governed in Europe (holding
**Syrakon**), with no fork of or inheritance from SQLite's format, and a minimal, auditable
supply chain (4 runtime dependencies, all pure-Rust except the FIPS primitives).

## Features

| | |
|---|---|
| **Model** | Relational, **broad SQL dialect**: `JOIN`, `GROUP BY`/`HAVING`, `DISTINCT`, aggregates (incl. `GROUP_CONCAT`), **subqueries**, **CTEs** (`WITH`), **`UNION`**, `CASE`, `CAST`, `BETWEEN`, `\|\|`, plus a scalar + date/time function library |
| **Views & triggers** | `CREATE VIEW`; **row-level** triggers `BEFORE`/`AFTER` `INSERT`/`UPDATE`/`DELETE` with `OLD`/`NEW` |
| **Integrity** | Foreign keys (`REFERENCES`) with `ON DELETE RESTRICT`/`CASCADE`/`SET NULL` |
| **Schema evolution** | **Logical** `ALTER TABLE`: `ADD`/`DROP`/`RENAME`/`MOVE`/`REORDER COLUMN` without rewriting rows — time-travel-safe |
| **Indexes** | B-tree secondary: `CREATE [UNIQUE] INDEX`; the planner uses them for equality, ranges and multi-column, plus deterministic predicate pushdown in JOINs |
| **Packaging** | A single file per tenant — backup = copy the file |
| **Storage** | Append-only *copy-on-write* B-tree: the file **is** the WAL; nodes with a pointer array (binary search) |
| **Bulk load** | `bulk_insert`: the whole batch in one transaction, no per-row executor, indexes in bulk — 2.5M rows/s |
| **Engine API** | `Connection::table` → typed row access without SQL (`get`/`scan`/`scan_columns`), same guarantees — point lookups ~3.7× the SQL path |
| **Reads** | Simple SELECT in **streaming** (lazy decode: only the projected columns, straight from the page) |
| **Durability** | ACID, tail-scan recovery, a single `fsync` per commit; **group commit** batches concurrent committers' fsyncs (leader-follower, no added latency for a lone commit) — ~4.6× durable throughput at 16 threads |
| **Time-travel** | `SELECT … AS OF <version/timestamp>`; `history()` / `diff_versions()` / `changes()` |
| **Branching** | Data branches with 3-way *diff* and *merge*, Git conceptual model |
| **Audit** | Every commit hash-chained with SHA-256 — tampering with the past is detectable; `verify()` + anchors |
| **Encryption** | AES-256-GCM per page (optional); PII never touches the disk in the clear |
| **Compression** | Pure-Rust LZSS per page (optional); ≤ SQLite on compressible data |
| **Robustness** | Reed-Solomon per page (optional): corrects bit-rot, not just detects it; `scrub()` |
| **Maintenance** | `vacuum` with retention and key rotation; atomic rename |
| **Concurrency** | Concurrent reads with no global lock (immutable snapshots) |
| **Safety** | `#![forbid(unsafe_code)]` |

## Usage

```rust
use arkeion::{Database, Options, Value, params};

let db = Database::open("tenant.arkeion", Options::default())?;
let conn = db.connect()?;

conn.execute("CREATE TABLE invoices (id INTEGER PRIMARY KEY, total REAL, status TEXT)", &[])?;
conn.execute(
    "INSERT INTO invoices (total, status) VALUES (?1, ?2)",
    &params![120.0, "draft"],
)?;
let v1 = conn.version();

// Bulk load: the whole batch in ONE transaction (1 fsync), no per-row executor.
conn.bulk_insert(
    "invoices",
    (1..=1_000).map(|i| [Value::Null, Value::Real(i as f64), Value::Text("issued".into())]),
)?;

// Reads are immutable snapshots; a simple SELECT is served in streaming.
for row in conn.query("SELECT id, total FROM invoices LIMIT 10", &[])? {
    let row = row?;
    println!("{}: {}", row.get::<i64>("id")?, row.get::<f64>("total")?);
}

// Time-travel: the query sees the past exactly as it was (and `verify()` proves it).
let mut before = conn.query(&format!("SELECT COUNT(*) FROM invoices AS OF VERSION {v1}"), &[])?;
assert_eq!(before.next().unwrap()?.get::<i64>(0)?, 1); // only the invoice before the batch
```

## Status

**Working engine — milestones M0 to M10 + secondary indexes + bulk-load/streaming + a broad SQL
dialect (subqueries, CTEs, `UNION`, views, foreign keys, triggers, logical `ALTER TABLE`),
implemented and tested** (272 tests, `clippy -D warnings`, `#![forbid(unsafe_code)]`). **Pre-1.0**:
the format may change and there is no production release yet. The
[crates.io](https://crates.io/crates/arkeion) version is at **0.11.x** (0.10 = milestones M0–M10;
0.11 adds the broad SQL dialect above); `1.0.0` will mean exactly one thing: **on-disk format
frozen**. The full specification lives in [`docs/`](docs/):

| Doc | Contents |
|---|---|
| [01-arquitectura](docs/01-arquitectura.md) | Layers, modules and the central CoW decision |
| [02-formato-archivo](docs/02-formato-archivo.md) | Binary layout: header, pages, commits |
| [03-api](docs/03-api.md) | Public Rust API |
| [04-sql](docs/04-sql.md) | The SQL dialect and the `AS OF` extension |
| [05-decisiones](docs/05-decisiones.md) | Justified design decisions (ADRs) |
| [06-hitos](docs/06-hitos.md) | Incremental plan M0 → M10 |
| [07-riesgos](docs/07-riesgos.md) | Technical risks and borrow-checker hot spots |
| [08-soberania](docs/08-soberania.md) | Positioning: why it is genuinely European and not a fork |
| [09-m10-compresion](docs/09-m10-compresion.md) | Page compression + data stability (ECC) |
| [10-indices-secundarios](docs/10-indices-secundarios.md) | Secondary indexes: memcomparable encoding, plan, node format v3 |

## Benchmarks

**Machine**: AMD Ryzen 7 3700X (8c/16t), 32 GiB, **ext4 on a SATA SSD**, Arch Linux (kernel 7.0.11),
`rustc` 1.95.0, SQLite 3.50.2 (bundled via rusqlite). Single-threaded.

**Methodology** (read it before quoting a number): **median of N** repetitions (reads 5, durable 3
with a fresh DB per repetition); **both engines warmed up**; **both with prepared statements** — lexing/
parsing is excluded on both sides, the difference is execution (native Rust call + in-page binary search
vs SQLite's bytecode VM; and arkeion doesn't even cache the plan, it re-derives it on every lookup, so
that asymmetry plays *against it*). **fsync to real disk** (`ARKEION_BENCH_DIR` on the SSD; the default
tempdir may be `tmpfs`/RAM and then the fsyncs don't touch disk). Durability: arkeion **1**
`fdatasync`/commit; SQLite `synchronous=FULL` + rollback journal = **2** fsync/commit.

```sh
ARKEION_BENCH_DIR=/path/on/real/disk cargo bench --features bench-sqlite   # CRUD vs SQLite
cargo run --release --example dbsize --features bench-sqlite               # footprint
```

**Honesty first**: on real disk, arkeion **wins durable writes** (~2×, its core use case), **wins
point lookups** (by PK and by secondary index) as long as the working set fits in the page cache (64 MB
by default), and on **batch insert** it's on par with plain SQL SQLite (0.9–1.0×) and **beats it with the
bulk-load API** (`bulk_insert`, ~2.5M rows/s stable) and **with equivalent guarantees** (~2.2×); the
*full scan* still goes to SQLite, but streaming with lazy decode left it at ~0.5× (was 0.26×): what's
left is the per-cell walk of the CoW b-tree and the API's per-row `Vec`, not materializing the result
(that no longer happens).
*Lesson learned: an earlier run on `tmpfs` (RAM) hid arkeion's biggest strength — with free fsync SQLite
won the durable writes; on real disk it flips.*

### Single-threaded CRUD — 50k rows, real disk (operations/second, median)

| operation | arkeion | SQLite | ratio |
|---|--:|--:|:--:|
| **insert 1 row/commit (durable)** | **656** | 271 | **2.42×** |
| insert batch (1 commit) | 1.99M | 1.94M | 1.02× |
| **insert batch (bulk API)** | **2.59M** | 2.21M | **1.17×** |
| **select by PK** | **567.9k** | 181.5k | **3.13×** |
| **update by PK (durable)** | **572** | 278 | **2.06×** |
| full scan (rows/s) | 8.07M | 16.94M | 0.48× |
| **delete by PK (durable)** | **657** | 263 | **2.50×** |
| **select by 2nd index** | **358.7k** | 181.6k | **1.98×** |

> `ratio = arkeion / SQLite` (> 1 ⇒ arkeion faster). *Batch insert via SQL* does ~1.8–2.0M rows/s (was
> 1.1M — perf phase 2: explicit-PK dup-check without a descent, `Arc` in the schema cache, encoding with
> no clones or per-row `Vec`), **on par** with plain SQLite. The **bulk-load API** (`Connection::
> bulk_insert`: one transaction, no per-row executor, indexes in bulk) does **2.5–2.6M stable** and won
> every observed run (1.17–2.05×; plain SQLite's batch is noisy, 1.2–2.2M). The **stable** number is the
> fair comparison (same guarantees): arkeion **wins ~2.2×** (below). The *full scan* is served in
> **streaming with lazy decode** (8M rows/s, was 4.5M): it only decodes the projected columns, straight
> from the page, without materializing the result.

**Durable writes are the robust win**: on real disk the fsync dominates and arkeion's append-only commit
(1 `fdatasync`) beats SQLite's 2 fsync (FULL + journal). It doesn't depend on the dataset size (it's
disk-bound, not cache-bound) and it's what **every committed transaction** pays — exactly what an audited
engine does constantly.

### How it scales — the page cache

Lookups depend on the working set fitting in the **page cache** (CLOCK, **64 MB** by default): on every
cache miss, arkeion **re-verifies the page's integrity tag**, whereas SQLite doesn't, so once the table
exceeds the cache arkeion pays that cost and the edge erodes. The cache used to be 16 MB and at 1M rows
(~40 MB) the secondary index would even **invert** (0.57×, arkeion slower). With the cache at 64 MB the
working set fits again and lookups stay **nearly size-independent**:

| on real disk | 50k rows | 1M rows (~40 MB) |
|---|--:|--:|
| select by PK | 3.13× | 2.86× |
| select by 2nd index | 1.98× | 1.58× |
| durable insert | 2.42× | ~2.1× (size-independent) |

Beyond the cache the edge narrows again (inherent to re-verifying cold pages); the fix is to size the
cache to the working set with `Options::cache_bytes` (a faster integrity primitive is NOT it: BLAKE3 was
measured and rejected — SHA-256 with SHA-NI already does ~2 GB/s and BLAKE3 is *slower* over 4 KiB pages;
D8 in [docs/05](docs/05-decisiones.md)). Durable writes win at any size. (The figures also depend on the
config: `ARKEION_BENCH_SQLITE_WAL=1` puts SQLite in WAL — 1 sync — and shifts the durable and PK ratios.)

### Fair comparison — same guarantees

SQLite is given full history (a `t_log` table) + a per-write hash chain: **versioning + tamper-evidence
like arkeion**, in one transaction (1 fsync). The unassailable moat:

| insert (50k, real disk) | arkeion | SQLite + audit | ratio |
|---|--:|--:|:--:|
| **durable (1/commit)** | **656** | 261 | **2.51×** |
| **batch (1 commit)** | **1.99M** | 906.1k | **2.19×** |

> With equivalent guarantees, arkeion **wins both** — durable (2.51×) **and** batch (2.19×) — *and* with
> `AS OF`, `verify()` and branching thrown in. (This SQLite emulation reproduces neither the per-commit
> `content_hash` nor a queryable snapshot, so the ratios are a **lower bound** on the real overhead of
> matching arkeion.)

### On-disk footprint — 100k rows `(id PK, a INTEGER, b TEXT)`

| | size |
|---|--:|
| arkeion — insert (1 commit) | 3.4 MB |
| arkeion — **compressed** insert (opt-in) | 0.8 MB |
| arkeion — +1 update-everything (CoW history retained) | 6.9 MB |
| arkeion — after `vacuum KeepLast(1)` | 3.4 MB |
| SQLite — insert (1 commit) | 2.8 MB |
| SQLite — +1 update-everything (in-place, no history) | 2.8 MB |

**Honesty**: the 0.8 MB row compares arkeion's **compressed mode (optional, off by default)** against
**un**compressed SQLite, on a deliberately very compressible dataset (the TEXT column is a constant).
**Engine-to-engine in default mode: 3.4 MB (arkeion) vs 2.8 MB (SQLite)** — arkeion ~1.21× **larger**.
The **row key is variable-length** (v5: `[enc_oint(table_id)][enc_oint(rowid)]`, ~5 B typical instead of
the old fixed 13 — order-preserving integers that keep the b-tree's order, with no namespace byte because
their `0x80+` header already distinguishes them from the catalog and the indexes); the rest of the gap is
the flags byte and key length of the generic cell (a single b-tree for tables/indexes/catalog) plus the
self-describing record. The v3 pointer array (2 B/cell) and the per-page crypto reserve (28 B) are
marginal; the leaves are already nearly full (right-biased split on sequential insert, like SQLite). The
optional compression uses **Densa** (LZSS + an adaptive range coder, pure-Rust, no deps); SQLite **also**
supports compression (sqlite-zstd, ZIPVFS/CEROD), also not by default. What arkeion keeps and SQLite does
not: **CoW history** (an explicit cost, recoverable with `vacuum`), `verify()` and `AS OF`, all while
keeping compression and, optionally, Reed-Solomon correction.

## License

Dual-licensed, at your option — the Rust ecosystem convention:

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)

Unless you explicitly state otherwise, any contribution submitted for inclusion is licensed under
that same dual license, with no additional terms or conditions.

## Contributing

Issues and PRs welcome — read [CONTRIBUTING.md](CONTRIBUTING.md) (green suite, clean clippy,
Conventional Commits, and bench numbers if you touch performance).
