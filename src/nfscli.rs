//! Pure-Rust NFS client façade, built on `nfs-rs`.
//!
//! This replaces the previous hand-written `libnfs` FFI bindings and the
//! slot-actor connection pool. `nfs-rs` is an async client, so we own one
//! multi-threaded Tokio [`Runtime`] and bridge the synchronous FUSE callbacks
//! (which run on fuser worker threads) into it with `block_on`. The single
//! `Box<dyn Mount>` returned by [`parse_url_and_mount`] is `Send + Sync`, and
//! `nfs-rs` multiplexes concurrent RPCs over its internal connection, so no
//! per-caller context pool or credential switching is needed here.
//!
//! ## Fixed mount credentials
//! `nfs-rs` fixes the AUTH_UNIX credential once, at mount time, from the URL
//! `?uid=&gid=` parameters (or the process's real ids when absent); it exposes
//! no per-request credential setter. yourfs therefore forwards the *mount
//! user's* identity to the server for every operation instead of the individual
//! FUSE caller's, which is the accepted trade-off of going fully pure-Rust.
//!
//! ## Read-only, handle-based reads
//! NFSv3 `READ` addresses a file by its file handle plus an offset, so a
//! read-only export needs no stateful `OPEN`: [`open`] resolves the path to a
//! file handle ([`Bytes`]) and [`read`] issues `READ` against it directly.

use std::sync::OnceLock;

use bytes::Bytes;
use nfs_rs::{parse_url_and_mount, Attr, FSStat, Mount, NfsError, OPEN_READ};
use tokio::runtime::Runtime;

/// Shared Tokio runtime the blocking FUSE callbacks drive the async client on.
static RT: OnceLock<Runtime> = OnceLock::new();
/// The single mounted NFS session, credential-fixed at mount time.
static MNT: OnceLock<Box<dyn Mount>> = OnceLock::new();

fn runtime() -> &'static Runtime {
    RT.get().expect("nfs not initialized")
}

fn mount() -> &'static dyn Mount {
    &**MNT.get().expect("nfs not initialized")
}

/// Map an [`NfsError`] onto a negative POSIX errno, the convention the rest of
/// yourfs (and the original C client) uses for FUSE error returns.
///
/// The NFSv3 status words were deliberately defined to mirror Unix errno
/// values (`NFS3ERR_NOENT == ENOENT`, `NFS3ERR_ACCES == EACCES`, …), so for a
/// plain `Nfs3(code)` the discriminant is already the errno. NFS-specific
/// codes above the errno range and every non-protocol error collapse to `EIO`.
fn to_neg_errno(e: NfsError) -> i32 {
    use libc::{c_int, ENOENT};
    match e {
        // NFSv3 status: the discriminant equals the matching errno for the
        // Unix-aligned values; anything outside a plausible errno range is an
        // opaque server condition we surface as a generic I/O error.
        NfsError::Nfs3(code) => {
            let v = code as c_int;
            if (1..=122).contains(&v) {
                -v
            } else {
                -libc::EIO
            }
        }
        NfsError::Nfs4(_) => -libc::EIO,
        // A missing component during path resolution is the common "no such
        // file" case for lookup_path on a vanished whitelist target.
        other if other.is_not_found() => -ENOENT,
        _ => -libc::EIO,
    }
}

/// Fill in the two URL defaults the operator usually leaves out, without ever
/// overriding an explicit value:
/// * `version=3` — the old libnfs client spoke NFSv3; `nfs-rs` would otherwise
///   negotiate 4.1 first, which this read-only export path does not need.
/// * `noresvport=true` — `nfs-rs` binds a privileged (<1024) source port by
///   default, which a non-root mounter cannot use (the RPC/portmapper connect
///   fails `EACCES`). Non-reserved ports are accepted by every `sec=sys`
///   export and match modern nfs-utils clients.
fn full_url(url: &str) -> String {
    let mut full = url.to_string();
    for (key, val) in [("version", "3"), ("noresvport", "true")] {
        if !full.contains(&format!("{key}=")) {
            let sep = if full.contains('?') { '&' } else { '?' };
            full = format!("{full}{sep}{key}={val}");
        }
    }
    full
}

/// Validate the export without publishing it: open a throwaway runtime and
/// mount, then drop both. Used by the daemon path to surface an unreachable
/// server / bad URL on the terminal *before* forking, without touching the
/// `OnceLock` statics the daemon child needs to populate itself.
pub fn preflight(url: &str) -> Result<(), String> {
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(parse_url_and_mount(&full_url(url)))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Mount the export described by `url` and publish the shared runtime.
///
/// Runs *after* the daemon fork (see `bin/yourfs.rs`): a Tokio runtime's
/// worker threads would not survive `fork`, so it must be built in the child,
/// and this is called exactly once so the `OnceLock` statics are still empty.
pub fn init(url: &str) -> Result<(), String> {
    let full = full_url(url);
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    let mnt = rt
        .block_on(parse_url_and_mount(&full))
        .map_err(|e| e.to_string())?;

    // Publish before returning; both OnceLocks are set exactly once at startup.
    if RT.set(rt).is_err() {
        return Err("nfs already initialized".to_string());
    }
    if MNT.set(mnt).is_err() {
        return Err("nfs already initialized".to_string());
    }
    Ok(())
}

/// Best-effort teardown. The mount and runtime are `OnceLock` statics that are
/// dropped with the process; nothing to do here but keep the call site stable.
pub fn shutdown() {}

/// `GETATTR` on a real path. Resolves the path to a file handle and returns
/// its attributes (reusing the post-operation attributes `LOOKUP` already
/// carries when present, otherwise issuing a `GETATTR`).
pub fn getattr(path: &str) -> Result<Attr, i32> {
    runtime().block_on(async {
        let res = mount().lookup_path(path).await.map_err(to_neg_errno)?;
        match res.attr {
            Some(attr) => Ok(attr),
            None => mount().getattr(res.fh).await.map_err(to_neg_errno),
        }
    })
}

/// `READLINK` on a real path.
pub fn readlink(path: &str) -> Result<String, i32> {
    runtime().block_on(async {
        let res = mount().lookup_path(path).await.map_err(to_neg_errno)?;
        mount().readlink(res.fh).await.map_err(to_neg_errno)
    })
}

/// Resolve a real path to a file handle usable by [`read`]. Read-only, so no
/// stateful NFS `OPEN` is required.
pub fn open(path: &str) -> Result<Bytes, i32> {
    runtime().block_on(async {
        let res = mount().lookup_path(path).await.map_err(to_neg_errno)?;
        Ok(res.fh)
    })
}

/// `READ` `size` bytes from `fh` at `offset`. Returns the bytes actually read
/// (may be fewer at EOF; NFS clamps the request to the server's `rtmax`).
pub fn read(fh: &Bytes, offset: u64, size: u32) -> Result<Bytes, i32> {
    if size == 0 {
        return Ok(Bytes::new());
    }
    runtime().block_on(async { mount().read(fh.clone(), offset, size).await.map_err(to_neg_errno) })
}

/// Mount-wide `FSSTAT`. NFS statistics are per superblock, not per path, so no
/// argument is needed (this is why the previous per-path `statvfs` worked for
/// any file).
pub fn statfs() -> Result<FSStat, i32> {
    runtime().block_on(async { mount().fsstat().await.map_err(to_neg_errno) })
}

/// Expose the negotiated read size hint so the FUSE layer can pick an `open`
/// access mode; kept deliberately minimal for the read-only mount.
pub const READ_ACCESS: u32 = OPEN_READ;
