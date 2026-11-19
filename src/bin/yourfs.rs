//! `yourfs` — FUSE mount of a whitelisted, read-only view over an NFS export.
//!
//! Rust rewrite of the C `yourfs.c` main(). The FUSE and NFS layers are now
//! fully pure-Rust (`fuser` + `nfs-rs`), so this entry point builds a
//! `fuser::Config` from the parsed options, mounts the export through
//! [`nfscli::init`], and runs the `fuser` event loop. `fuser` does not
//! daemonize the way libfuse's `fuse_main` did, so we fork to the background
//! ourselves (suppressed with `YOURFS_NO_DAEMON=1` for foreground testing).

use std::os::fd::AsRawFd;

use fuser::{Config, MountOption, SessionACL};
use yourfs::{cli, db, fs, nfscli};

// Numeric options follow C atoi()/atof() semantics (longest numeric prefix),
// see `cli::parse_i64`/`cli::parse_f64`.
fn parse_i64(s: &str) -> i64 {
    cli::parse_i64(s)
}

fn parse_f64(s: &str) -> f64 {
    cli::parse_f64(s)
}

struct Opts {
    url: Option<String>,
    mnt: Option<String>,
    allow_other: bool,
    allow_root: bool,
    default_permissions: bool,
    multithreads: bool,
    fsname: Option<String>,
    subtype: Option<String>,
    database_type: String,
    database: String,
    table: String,
    cache_memory: i64,
    whitelist_cache: bool,
    fuse_threads: usize,
    unknown: Option<String>,
}

/// Translate the parsed options into a `fuser::Config`.
///
/// Only options with a kernel/`fuser` meaning are carried over: read-only
/// mount, the access ACL, `default_permissions`, `fsname`/`subtype`, and the
/// event-loop thread count. The old libfuse-specific I/O knobs (`direct_io`,
/// `kernel_cache`, `auto_cache`, timeouts, `large_read`, …) have no
/// `fuser` equivalent — the caching they requested is now applied directly in
/// the filesystem (`FOPEN_KEEP_CACHE` plus the attribute TTLs), so they are
/// accepted for CLI compatibility and otherwise ignored.
fn build_config(o: &Opts) -> Config {
    let mut cfg = Config::default();
    let mut opts = vec![
        // The whole filesystem is read-only (whitelist view, no write ops).
        MountOption::RO,
        // Do not update atime; a read-only export cannot honor it anyway.
        MountOption::NoAtime,
        MountOption::NoSuid,
        MountOption::NoDev,
    ];
    if o.default_permissions {
        opts.push(MountOption::DefaultPermissions);
    }
    if let Some(name) = &o.fsname {
        opts.push(MountOption::FSName(name.clone()));
    }
    if let Some(sub) = &o.subtype {
        opts.push(MountOption::Subtype(sub.clone()));
    }
    cfg.mount_options = opts;
    cfg.acl = match (o.allow_other, o.allow_root) {
        (true, _) => SessionACL::All,
        (false, true) => SessionACL::RootAndOwner,
        (false, false) => SessionACL::Owner,
    };
    if o.multithreads {
        cfg.n_threads = Some(o.fuse_threads.max(1));
        // One /dev/fuse fd per worker (Linux 4.5+) scales request handling.
        cfg.clone_fd = true;
    }
    cfg
}

/// Fork into the background, mirroring libfuse's implicit daemonization.
/// After `fork` only the calling thread survives in the child, which is why
/// the Tokio runtime and NFS mount are created *after* this returns — see
/// `main`. The parent exits `0`; the child continues with a new session.
///
/// Returns `true` if the caller is now the daemon child.
fn daemonize() -> bool {
    // SAFETY: standard double-fork daemonization; we do not touch any
    // file-locked state across the fork (the runtime/mount come later).
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            eprintln!("yourfs: fork failed: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
        if pid > 0 {
            // Original parent: the mount is now running in the child.
            std::process::exit(0);
        }
        // Child: detach from the controlling terminal.
        libc::setsid();
        libc::chdir(std::ptr::null());
        // Reopen stdin/stdout/stderr on /dev/null so the daemon holds no
        // terminal. Errors here are non-fatal for the mount itself.
        if let Ok(devnull) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            let fd = devnull.as_raw_fd();
            libc::dup2(fd, libc::STDIN_FILENO);
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
        }
    }
    true
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts {
        url: None,
        mnt: None,
        allow_other: false,
        allow_root: false,
        default_permissions: true,
        multithreads: true,
        fsname: None,
        subtype: None,
        database_type: String::new(),
        database: String::new(),
        table: String::new(),
        cache_memory: 10000,
        whitelist_cache: false,
        fuse_threads: 4,
        unknown: None,
    };

    let mut p = cli::Parser::new(cli::YOURFS_SPECS, &argv);
    while let Some(cmd) = p.next_command() {
        match cmd {
            cli::Command::Opt(c, v) => {
                let v = v.unwrap_or_default();
                match c {
                    '?' => {
                        print_usage();
                        return;
                    }
                    'n' => o.url = Some(v),
                    'm' => o.mnt = Some(v),
                    // `uid=`/`gid=` on the mount control the FUSE-reported
                    // owner; with fixed credentials they are recorded as fsname
                    // style metadata only. Kept parseable for CLI compatibility.
                    'U' | 'G' => {} // per-caller credential mapping removed
                    'o' => o.allow_other = true,
                    'a' => o.allow_other = true,
                    'r' => o.allow_root = true,
                    'p' => o.default_permissions = parse_i64(&v) != 0,
                    't' => o.multithreads = parse_i64(&v) != 0,
                    // `-V` once sized the libnfs connection pool; `nfs-rs`
                    // multiplexes over one connection, so it now sizes the FUSE
                    // event loop instead (a sensible reuse that keeps the flag).
                    'V' => o.fuse_threads = parse_i64(&v).clamp(1, 64) as usize,
                    'f' => o.fsname = Some(v),
                    's' => o.subtype = Some(v),
                    // Accepted for CLI compatibility; no `fuser` effect (the
                    // caching these libfuse knobs selected is applied inside
                    // the filesystem). See `build_config`.
                    'u' | 'g' | 'D' | 'd' | 'k' | 'c' | 'l' | 'h' | 'b' | 'i' | 'R' | 'H'
                    | 'A' | 'S' | 'K' | 'E' | 'N' | 'T' | 'C' | 'Y' | 'I' | 'q' | 'Q' | 'O'
                    | 'y' => {}
                    'X' => o.database = v,
                    'Z' => o.table = v,
                    'M' => o.cache_memory = (parse_f64(&v) * 1000.0) as i64,
                    'w' => o.whitelist_cache = true,
                    'F' => o.database_type = v,
                    'P' => {} // accepted like the C version; PASSWD module off
                    _ => {}
                }
            }
            cli::Command::Unknown(msg) => {
                o.unknown = Some(msg);
                break;
            }
        }
    }
    if let Some(msg) = &o.unknown {
        eprintln!("yourfs: unrecognized option {msg}");
    }

    let url = match &o.url {
        Some(u) => u.clone(),
        None => {
            eprintln!("-n was not specified.");
            print_usage();
            std::process::exit(10);
        }
    };
    let mnt = match &o.mnt {
        Some(m) => m.clone(),
        None => {
            eprintln!("-m was not specified.");
            print_usage();
            std::process::exit(10);
        }
    };

    if !matches!(o.database_type.as_str(), "sqlite3" | "postgres" | "redis") {
        eprintln!("Please use -F to setup the database type, support sqlite3, postgres, redis");
        std::process::exit(1);
    }
    if db::check_ident(&o.table).is_err() {
        eprintln!("invalid -Z table name: {:?}", o.table);
        std::process::exit(1);
    }

    let database = match db::open(&o.database_type, &o.database, o.cache_memory) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to open database: {e}");
            std::process::exit(1);
        }
    };
    // Fail fast when -Z names an unusable table (see db.rs `check_table`).
    if let Err(e) = database.check_table(&o.table) {
        eprintln!("{e}");
        std::process::exit(1);
    }
    let database = if o.whitelist_cache {
        db::caching(database)
    } else {
        database
    };
    let ctx = fs::DbCtx {
        db: database,
        table: o.table.clone(),
    };

    let config = build_config(&o);

    // Mount the export. The Tokio runtime and its NFS connection must be
    // created in the daemon child (threads do not survive `fork`), so for a
    // foreground run (`YOURFS_NO_DAEMON=1`) init happens here and we skip the
    // fork; otherwise we preflight, fork, then init in the child.
    let foreground = std::env::var_os("YOURFS_NO_DAEMON").is_some();

    if foreground {
        if let Err(e) = nfscli::init(&url) {
            eprintln!("Failed to mount nfs share : {e}");
            std::process::exit(10);
        }
    } else {
        // Preflight the NFS mount so an unreachable server / bad URL surfaces
        // on the terminal with the right exit code before we detach. This uses
        // a throwaway connection and deliberately does *not* populate the
        // `OnceLock` statics, so the child's real `init` below still can.
        match nfscli::preflight(&url) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Failed to mount nfs share : {e}");
                std::process::exit(10);
            }
        }
        daemonize();
        // Build the real runtime/mount in the child. A failure here is
        // unrecoverable and cannot reach a terminal we no longer own, so exit
        // quietly with the same code as the preflight.
        if nfscli::init(&url).is_err() {
            std::process::exit(10);
        }
    }

    // Blocking FUSE event loop; returns once the filesystem is unmounted.
    let ret = fs::run(ctx, &mnt, &config);

    nfscli::shutdown();
    match ret {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("yourfs: fuse loop ended with error: {e}");
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "Yourfs Usage(v1.0, Rust rewrite, pure-Rust FUSE/NFS stack)
	 [-?|--help] 

yourfs options : 
	 [-n share_point| The server export to be mounted] 
	 [-m mount_path|  The client mount path] 
fuse options (subset with a kernel/fuser meaning): 
	 [-p [0|1]|--default_permissions=[0|1]] 
	 [-t [0|1]| Multi-threaded by default (1)] 
	 [-a|--allow_other] 
	 [-r|--allow_root] 
	 [-f fsname]  [-s subtype] 
	 [-V N|--nfs_connections N| FUSE event-loop threads (default 4, 1..=64)] 
	 (legacy libfuse knobs -d/-k/-c/-l/... are accepted but ignored; read/write
	  caching is applied automatically by the filesystem.)
database options : 
	 [-F database_type ]  It is used to filter database type to read.(sqlite3, postgres or redis)
	 [-X database ] 
		sqlite3:  /path/data.db
		postgres: ip,port,user,passwd,dbname
		redis:    redis://host:port/db  OR  ip,port[,db[,passwd]]  OR  host:port
	 [-Z tablename| the table name to read] 
	 [-M MB| default 10, only used to setup sqlite cache]
	 [-w|--whitelist_cache| cache the whitelist in memory (enable ONLY if it will not change during the mount)] 
	 (If you have any question or bug report, please connect hanjun@nao.cas.cn) 
"
    );
}
