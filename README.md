# yourfs

**English** | [简体中文](README.zh-CN.md)

A **read-only file system with file-level isolation over NFS**, written in Rust.

`yourfs` mounts a remote NFS export through a pure-Rust NFS client ([nfs-rs],
NFSv3) and exposes to the user a
*virtual* directory tree that is defined entirely by a **whitelist** stored in a
database (SQLite3, PostgreSQL or Redis). Anything not listed in the whitelist is
simply invisible; a listed entry can also be shown under a different name than it
has on the server. The result is a read-only view where you control, per file,
exactly what is exposed and under what name.

> This is a Rust rewrite of the original C project
> [TechWander/yourfs](https://github.com/TechWander/yourfs). The behaviour of the
> C version is preserved.

---

## How it works

The virtual tree is described by one table of triples:

| column     | meaning                                                        |
|------------|----------------------------------------------------------------|
| `parent`   | the virtual directory the entry lives in (`/` = root)          |
| `name`     | the name shown to the user                                     |
| `realpath` | the real path of the file on the NFS export                    |

```
            user sees            whitelist row            reads over NFS
   /mnt ─── a.txt   ───────►  (/, a.txt, /a.txt)  ───────►  nfs://host/export/a.txt
            (b.txt is absent from the table, so it does not exist here)
```

* **`readdir`** lists the children of a directory (`WHERE parent = ? ORDER BY name`).
* **`lookup`/`open`/`getattr`** resolve a virtual path to its `realpath` and fetch
  it over NFS. A path with no matching row returns `ENOENT`.
* The mount is **read-only**: any write returns `EROFS`.

Two binaries are produced:

* **`yourfs`** — the FUSE file system; mounts a whitelist-defined view of an NFS export.
* **`yourfs-tool`** — builds the whitelist: scans a local tree into a text file and
  loads it into SQLite3 / PostgreSQL / Redis.

## Backends and `-X` connection strings

| backend    | `-F` value  | `-X` format                                                |
|------------|-------------|------------------------------------------------------------|
| SQLite3    | `sqlite3`   | `/path/to/data.db`                                         |
| PostgreSQL | `postgres`  | `ip,port,user,passwd,dbname`                               |
| Redis      | `redis`     | `redis://host:port/db` · `ip,port[,db[,passwd]]` · `host:port` |

For Redis the whitelist is kept as, per directory: a sorted set `{table}|d|{parent}`
(member = child name, giving byte-ordered `readdir`) and a hash `{table}|h|{parent}`
(`name → realpath`, giving `O(1)` resolution).

## Build

```bash
cargo build --release
```

This produces `target/release/yourfs` and `target/release/yourfs-tool`. The FUSE
and NFS layers are **pure Rust** (`fuser` + `nfs-rs`), so the build links no
`libfuse` or `libnfs` at all — there is nothing of theirs to install. The only
dynamic dependencies on the gnu build are the glibc base libraries, present on
every Linux.

### Fully static (musl) build — runs on musl/Alpine too

```bash
./scripts/build-musl-static.sh
# -> target/x86_64-unknown-linux-musl/release/yourfs{,-tool}
```

`yourfs` is already free of C libraries, so the script only needs the Rust musl
target and a musl C toolchain (to compile the SQLite bundled inside `rusqlite`);
it cross-compiles everything into a single ELF with **zero dynamic dependencies**
(`ldd` reports *statically linked*, no `INTERP` segment; `file` calls such
static-PIE ELFs "dynamically linked" — trust `ldd`/`readelf`). See
[Deployment](#deployment-and-runtime-requirements) for what the target machine
still needs for FUSE itself.

### Prebuilt binaries

GitHub Releases ship two x86_64 Linux archives plus `SHA256SUMS.txt`:

* `yourfs-<ver>-linux-x86_64-gnu.tar.gz` — glibc build (needs glibc ≥ 2.30);
* `yourfs-<ver>-linux-x86_64-musl.tar.gz` — fully static, no shared libraries.

Each contains `yourfs` and `yourfs-tool`.

## Quick start

```bash
# 1. Scan the export tree into a whitelist text file (parent name realpath per line)
yourfs-tool -s /srv/nfs/export -o list.txt

# 2. Load it into a SQLite3 database, table "data"
yourfs-tool -F sqlite3 -X /srv/data.db -Z data -i list.txt

# 3. Mount a read-only, isolated view of the NFS export
mkdir -p /mnt/yourfs
yourfs -n nfs://127.0.0.1/srv/nfs/export -m /mnt/yourfs \
       -F sqlite3 -X /srv/data.db -Z data

ls /mnt/yourfs        # only whitelisted entries are visible
umount /mnt/yourfs    # or: fusermount -u /mnt/yourfs
```

**`-n` URL format.** The export is addressed as `nfs://host[:port]/export/path`.
`yourfs` speaks **NFSv3** and, when you leave them out, automatically appends
`?version=3&noresvport=true` to the URL: version 3 matches the stateless,
file-handle-plus-offset READ path your read-only mount uses, and `noresvport`
lets a non-root mounter connect without a privileged (< 1024) source port — the
export must then allow insecure source ports (the `insecure` option). Any query
the underlying `nfs-rs` client understands can still be set explicitly
(`version`, `uid`, `gid`, `mountport`, `nfsport`, `noresvport`) and an explicit
value is never overridden by the defaults; `version` accepts `3`, `4.0`, `4.1`,
`4.2` (a bare `4` is rejected), though only NFSv3 is exercised here. UID/GID are
fixed once at mount time and sent on every request — see
[Design notes](#design-notes).

The database named by `-X`/`-Z` must already be built (steps 1–2): `yourfs`
validates the table at startup and exits with an actionable message instead of
mounting a view where every access would fail with `EIO`.

## Worked example

Publish part of an export, with different names, and change your mind later:

```
NFS server export /srv/data                user sees /mnt/yourfs
├── public/welcome.txt          ────►      ├── welcome.txt
├── internal/2025/report.pdf               └── reports/
└── secret/keys.txt  (not listed)               └── report.pdf
```

**1. Describe the tree** — one line per entry, whitespace-separated
`parent name realpath` (paths are export-root-relative, leading `/`;
`secret/keys.txt` simply has no line, so it cannot exist in the view):

```
/          welcome.txt    /public/welcome.txt
/          reports        /internal/2025
/reports   report.pdf     /internal/2025/report.pdf
```

A directory is just a row too (`/ reports /internal/2025`); its children
reference it through `parent` — and note `parent` is the **virtual** path, so
`/reports` may alias the server's `/internal/2025` (verified: `ls` shows
`reports` as a directory, `cat` reads through).

**2. Build the whitelist** (scan automates step 1 for a whole tree, then hand-edit):

```bash
yourfs-tool -s /srv/data -o list.txt     # or start from a hand-written file
yourfs-tool -F sqlite3 -X /srv/data.db -Z data -i list.txt
```

> An import **replaces** everything for that table: `DROP TABLE` + reload
> (Redis: `SCAN`+`DEL {table}|*` + rewrite). Rows added or edited after the
> import are gone on the next one.

**3. Mount and read**:

```bash
yourfs -n nfs://<server>/srv/data -m /mnt/yourfs -F sqlite3 -X /srv/data.db -Z data
ls /mnt/yourfs        # reports  welcome.txt
```

### Changing the virtual tree while mounted

Every operation reads the live database, so edits take effect on the very next
`ls`/`open` — no remount (unless you mounted with `-w`; then the cache holds
the old answer until you restart `yourfs`).

```sql
-- publish another file right now
INSERT INTO data(parent,name,realpath) VALUES ('/','keys.txt','/secret/keys.txt');
-- rename what users see (row stays the same file)
UPDATE data SET name='welcome-v2.txt' WHERE name='welcome.txt';
-- revoke a file
DELETE FROM data WHERE name='keys.txt';
```

(Redis instead: publish = `ZADD data|d|/ <name>` **and** `HSET data|h|/ <name>
<realpath>`; revoke = the matching `ZREM` + `HDEL`. PostgreSQL: same SQL, via
`psql` — but beware its `CHAR` columns ignore trailing spaces in comparisons,
so never end a name or path with one.)

For a wholesale reshuffle prefer editing the list file and re-importing while
the mount is idle — re-import first drops the table, and a read landing in
that window gets an error.

## Options

**`yourfs`**

| option | meaning |
|--------|---------|
| `-n <url>` | NFS export to mount: `nfs://host/path` (**required**; a path after the host is mandatory — use `nfs://host/` for a root export; trailing slash otherwise optional) |
| `-m <path>` | local mount point (**required**, implied read-only) |
| `-F <type>` | whitelist backend: `sqlite3` \| `postgres` \| `redis` |
| `-X <db>` | backend connection string (see table above); for sqlite3 the file must exist — `yourfs` never creates it |
| `-Z <table>` | whitelist table / key prefix |
| `-M <MB>` | SQLite page cache size (default 10) |
| `-w` | cache the whitelist in memory (opt-in; enable only if it will not change during the mount) |
| `-V <N>` | FUSE event-loop threads (default 4, range 1–64). `nfs-rs` multiplexes every RPC over a single connection, so this no longer sizes an NFS connection pool |
| `-?` | usage |

Plus flags kept for command-line compatibility with the C version. The ones with a
kernel/`fuser` meaning are honoured: `-a`/`-o` allow_other, `-r` allow_root, `-p`
toggle default_permissions, `-t` toggle multithreading, `-f` fsname, `-s` subtype.
The old libfuse I/O knobs are **accepted but ignored** — the caching they selected
now happens inside the filesystem (`FOPEN_KEEP_CACHE` plus short attribute TTLs),
not through libfuse: `-d` direct_io, `-k` kernel_cache, `-c` auto_cache,
`-A`/`-S` async/sync read, `-Y` nonempty, `-u`/`-g` uid/gid and the various
timeouts. The per-caller credential flags `-U`/`-G` are accepted but do nothing
(see the fixed-credential note in [Design notes](#design-notes)).

**`yourfs-tool`**

| option | meaning |
|--------|---------|
| `-s <path>` | scan this tree into `-o` output file; if omitted, the tool writes the DB |
| `-o <file>` | output text file when scanning (default `output.txt`) |
| `-i <file>` | input text file when writing the DB (default `input.txt`) |
| `-F`/`-X`/`-Z`/`-M` | same as above (write target) |
| `-?` | usage |

## Deployment and runtime requirements

The pure-Rust stack already links no `libnfs`/`libfuse`, but running **any** FUSE
file system still requires, on the target machine:

* the **`fuse` kernel module** loaded and a **`/dev/fuse`** device;
* the **`fusermount`** (or `fusermount3`) helper from the `fuse`/`fuse3` package.
  `fuser` mounts directly via `mount(2)` when it has the privilege to and otherwise
  falls back to exec'ing `fusermount`; either way it is a separate setuid program
  that cannot be bundled into the `yourfs` binary.

Beyond that:

* the default (`x86_64-unknown-linux-gnu`) binary needs **glibc ≥ 2.30** and runs
  on mainstream glibc distros;
* the **musl** binary is fully static and runs on musl/Alpine as well;
* network reachability to the NFS server is required at runtime.

## Testing note

`yourfs` interacts with FUSE, the kernel and a live NFS server, so a unit test
alone proves very little. The end-to-end behaviour (whitelist isolation, symlink
resolution, `EROFS` on write, `statfs`, connection-loss recovery, clean
unmount) was verified on a real local NFS export for all three backends
(SQLite3, PostgreSQL, Redis via docker), on the glibc **and** the fully static
musl release binaries.

## Design notes

* **Pure Rust, no C libraries.** The FUSE layer is [`fuser`][fuser] (its `libfuse`
  features stay off, so the default pure-Rust mount path is used) and the NFS layer
  is [`nfs-rs`][nfs-rs]. The earlier hand-written libfuse/libnfs FFI bindings
  (`fuse2.rs`, `libnfs.rs`) and the `build.rs` that linked them are gone, and neither
  `libfuse` nor `libnfs` is linked (verified with `ldd`/`readelf -d` on the gnu and
  the static musl build).
* **Inode-based, whitelist-driven.** `fuser`'s high-level API is inode-based, so
  `src/fs.rs` keeps an `inode ⇄ virtual-path` map (the libfuse path was name-based).
  `readdir` is driven entirely by the database whitelist — each child is resolved and
  `getattr`-ed for its type and inode — so listing never depends on an NFS
  `READDIR`, and any path with no matching row is simply invisible (`ENOENT`).
* **Fixed mount credentials.** `nfs-rs` fixes its AUTH_UNIX credential once, at
  mount time, from the URL `?uid=&gid=` (or the mounting user's real ids when
  absent); it exposes no per-request setter. yourfs therefore forwards the mount
  user's identity on every operation instead of each individual FUSE caller's. The
  whitelist still isolates visibility per row, so this only relaxes server-side
  permission checks on the exported files themselves, not the isolation model.
* **NFSv3, stateless reads.** `nfs-rs` would otherwise negotiate 4.1, so the URL is
  defaulted to `version=3`. NFSv3 `READ` is addressed by file handle plus offset, so
  the read-only mount needs no stateful NFS `OPEN`: opening a file just resolves it
  to a handle. The client multiplexes concurrent RPCs over a single connection, so
  there is no per-caller connection pool any more and `-V <N>` now sizes the FUSE
  event loop instead.
* **Caching that stays consistent.** libfuse's `auto_cache` has no `fuser` mount
  option (it is not part of the kernel FUSE ABI), so read caching is applied
  directly: `open` sets `FOPEN_KEEP_CACHE`, letting the kernel retain a file's page
  cache across opens, paired with short (≈1 s) entry/attribute TTLs. Because every
  operation still resolves the whitelist live, revoking a row makes the file
  disappear within the TTL even if its pages were cached — the same safety property
  `auto_cache` provided, measured at ~500 MB/s cold and ~3.9 GB/s hot on a 256 MB
  file.
* **Concurrency without staleness.** `fuser` dispatches callbacks on several worker
  threads; each backend (SQLite3 / PostgreSQL / Redis) checks out a connection from
  a small fixed-size pool rather than queueing on one mutex. Nothing is cached by
  default: every operation reads the live database, so an externally added, edited or
  revoked row takes effect immediately.
* **Opt-in whitelist cache (`-w`).** When enabled, resolved paths and directory
  listings are memoized so repeat operations skip the database round trip — a real
  latency win for the networked backends. It is **off by default** because a revoked
  row or an edited `realpath` would keep being served from the cache; turn it on only
  when the whitelist is guaranteed static for the lifetime of the mount.
* **Self-daemonising.** `fuser` does not fork the way libfuse's `fuse_main` did, so
  `bin/yourfs.rs` daemonises explicitly. The NFS runtime and connection cannot
  survive that `fork`, so the parent only *preflights* the export on a throwaway
  connection (surfacing a bad URL on the terminal) and the daemon child builds the
  real session — the preflight deliberately leaves the process-wide statics unset so
  the child's one-time init can populate them.

## License

GPL-2.0-or-later (same as the original C project).

[nfs-rs]: https://github.com/NetAppLabs/nfs-rs
[fuser]: https://github.com/cberner/fuser
