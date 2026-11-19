//! The read-only FUSE filesystem, expressed against `fuser`'s high-level,
//! inode-based `Filesystem` trait (a port of the previous libfuse high-level
//! path-based ops: getattr/open/read/readdir/readlink/release/statfs).
//!
//! ## Inode namespace
//! `fuser` (unlike libfuse 2.9's path API) drives every callback by inode, so
//! the kernel must first `lookup` a name to obtain its inode. We maintain a
//! permanent `inode <-> virtual path` map: inode 1 is the root (`"/"`), and
//! every other inode is allocated the first time a virtual path is looked up.
//! Because the visible namespace is exactly the (finite) whitelist, an entry is
//! never freed — `forget` is a deliberate no-op — which keeps `st_ino` stable
//! and identical across `readdir` and `lookup`.
//!
//! ## Consistency
//! Every callback re-resolves its virtual path through the live whitelist
//! (`db.resolve`) before touching NFS, so a revoked file disappears as soon as
//! the kernel's attribute cache expires (`ENTRY_TTL`/`ATTR_TTL`, one second) —
//! the same freshness guarantee as the C client, and orthogonal to the
//! `FOPEN_KEEP_CACHE` data-page caching below (which caches file *content*, not
//! whitelist membership).

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fuser::{
    FileAttr, FileHandle, FileType, FopenFlags, INodeNo, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

use crate::db::Database;
use crate::nfscli as nfs;

/// Root inode, as the kernel expects (libfuse/fuser both reserve inode 1).
const ROOT: u64 = 1;
/// How long the kernel may cache a positive dentry before re-`lookup`.
const ENTRY_TTL: Duration = Duration::from_secs(1);
/// How long the kernel may reuse cached attributes before re-`getattr`.
const ATTR_TTL: Duration = Duration::from_secs(1);
/// Allocation block size reported by `getattr` (matches the NFS export's norm).
const BLKSIZE: u32 = 4096;

/// Whitelist handle shared by every callback.
pub struct DbCtx {
    pub db: Box<dyn Database>,
    pub table: String,
}

/// One snapshot directory entry, precomputed at `opendir` time so `readdir`
/// can page through it by the transparent offset cookie.
#[derive(Clone)]
struct DirEnt {
    ino: INodeNo,
    kind: FileType,
    name: OsString,
}

/// The filesystem. Shared by `&self` across fuser's worker threads, so all
/// mutable state lives behind locks (interior mutability).
pub struct YourFs {
    ctx: DbCtx,
    /// inode -> virtual path.
    nodes: RwLock<HashMap<u64, String>>,
    /// virtual path -> inode (permanent; see module docs).
    node_ids: RwLock<HashMap<String, u64>>,
    next_ino: AtomicU64,
    /// open-file handle id -> NFS file handle.
    files: RwLock<HashMap<u64, Bytes>>,
    next_fh: AtomicU64,
    /// opendir handle id -> child snapshot.
    dirs: RwLock<HashMap<u64, Vec<DirEnt>>>,
    next_dir: AtomicU64,
}

/// Join a parent virtual path with a child name.
fn join(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Parent of a virtual path (`"/a/b" -> "/a"`, `"/a" -> "/"`).
fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// Map an NFS `fattr3`/`ftype3` to a `fuser` file type.
fn file_kind(type_: u32) -> FileType {
    match type_ {
        1 => FileType::RegularFile,
        2 => FileType::Directory,
        3 => FileType::BlockDevice,
        4 => FileType::CharDevice,
        5 => FileType::Symlink,
        6 => FileType::Socket,
        7 => FileType::NamedPipe,
        _ => FileType::RegularFile,
    }
}

/// `u32` seconds + `u32` nanos since the epoch -> `SystemTime`, saturating on
/// the (never-seen in practice) pre-epoch value.
fn to_system_time(seconds: u32, nseconds: u32) -> SystemTime {
    UNIX_EPOCH + Duration::new(seconds as u64, nseconds)
}

/// Turn a negative-errno `nfscli` error into a `fuser` `Errno` (which stores a
/// positive code).
fn errno_of(neg: i32) -> fuser::Errno {
    fuser::Errno::from_i32(-neg)
}

impl YourFs {
    pub fn new(ctx: DbCtx) -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, "/".to_string());
        let mut node_ids = HashMap::new();
        node_ids.insert("/".to_string(), ROOT);
        YourFs {
            ctx,
            nodes: RwLock::new(nodes),
            node_ids: RwLock::new(node_ids),
            next_ino: AtomicU64::new(ROOT + 1),
            files: RwLock::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            dirs: RwLock::new(HashMap::new()),
            next_dir: AtomicU64::new(1),
        }
    }

    /// Virtual path of an inode, or `None` when it is unknown (a stale inode).
    fn path_of(&self, ino: u64) -> Option<String> {
        self.nodes.read().unwrap().get(&ino).cloned()
    }

    /// Allocate or reuse the inode for a virtual path (permanent mapping).
    fn inode_of(&self, vpath: &str) -> u64 {
        {
            let ids = self.node_ids.read().unwrap();
            if let Some(&i) = ids.get(vpath) {
                return i;
            }
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        self.node_ids
            .write()
            .unwrap()
            .insert(vpath.to_string(), ino);
        self.nodes.write().unwrap().insert(ino, vpath.to_string());
        ino
    }

    /// Resolve a virtual path to its real export path through the whitelist.
    /// `Ok(None)` means "not whitelisted" (the caller replies `ENOENT`).
    fn resolve(&self, vpath: &str) -> Result<Option<String>, i32> {
        if vpath == "/" {
            return Ok(Some("/".to_string()));
        }
        self.ctx
            .db
            .resolve(&self.ctx.table, vpath)
            .map_err(|_| -libc::EIO)
    }

    /// Build a `FileAttr` for `vpath` from a fresh NFS `getattr`, or return the
    /// negative errno to reply with.
    fn file_attr(&self, vpath: &str) -> Result<(String, FileAttr), i32> {
        let real = match self.resolve(vpath)? {
            Some(r) => r,
            None => return Err(-libc::ENOENT),
        };
        let a = nfs::getattr(&real)?;
        let ino = self.inode_of(vpath);
        let attr = FileAttr {
            ino: INodeNo(ino),
            size: a.filesize,
            // `used` is bytes allocated; `blocks` is 512-byte units.
            blocks: a.used / 512,
            atime: to_system_time(a.atime.seconds, a.atime.nseconds),
            mtime: to_system_time(a.mtime.seconds, a.mtime.nseconds),
            ctime: to_system_time(a.ctime.seconds, a.ctime.nseconds),
            crtime: UNIX_EPOCH,
            kind: file_kind(a.type_),
            perm: (a.file_mode & 0o7777) as u16,
            nlink: a.nlink,
            uid: a.uid,
            gid: a.gid,
            rdev: a.spec_data[0],
            blksize: BLKSIZE,
            flags: 0,
        };
        Ok((real, attr))
    }

    /// Enumerate a directory's children (whitelist-driven), each resolved and
    /// stat'd to obtain its file type and inode.
    fn list_dir(&self, vpath: &str) -> Result<Vec<DirEnt>, i32> {
        let names = self
            .ctx
            .db
            .list(&self.ctx.table, vpath)
            .map_err(|_| -libc::EIO)?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if name.is_empty() {
                continue;
            }
            let child_vpath = join(vpath, &name);
            // Best effort: a child that has vanished from the whitelist or the
            // export is simply omitted from the listing.
            let kind = match self.file_attr(&child_vpath) {
                Ok((_, attr)) => attr.kind,
                Err(_) => continue,
            };
            let ino = self.inode_of(&child_vpath);
            out.push(DirEnt {
                ino: INodeNo(ino),
                kind,
                name: OsString::from(&name),
            });
        }
        Ok(out)
    }
}

impl fuser::Filesystem for YourFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_vpath = match self.path_of(parent.0) {
            Some(p) => p,
            None => return reply.error(errno_of(-libc::ENOENT)),
        };
        // Resolve "." and ".." ourselves; the rest come from the whitelist.
        let bytes = name.as_bytes();
        let vpath = if bytes == b"." {
            parent_vpath
        } else if bytes == b".." {
            parent_of(&parent_vpath)
        } else {
            match name.to_str() {
                Some(s) => join(&parent_vpath, s),
                None => return reply.error(errno_of(-libc::ENOENT)),
            }
        };
        match self.file_attr(&vpath) {
            Ok((_, attr)) => reply.entry(&ENTRY_TTL, &attr, fuser::Generation(0)),
            Err(e) => reply.error(errno_of(e)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let vpath = match self.path_of(ino.0) {
            Some(p) => p,
            None => return reply.error(errno_of(-libc::ENOENT)),
        };
        match self.file_attr(&vpath) {
            Ok((_, mut attr)) => {
                // getattr is keyed by the requested inode, not by the
                // (re)allocated one, so the kernel matches its cache entry.
                attr.ino = ino;
                reply.attr(&ATTR_TTL, &attr)
            }
            Err(e) => reply.error(errno_of(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let vpath = match self.path_of(ino.0) {
            Some(p) => p,
            None => return reply.error(errno_of(-libc::ENOENT)),
        };
        let real = match self.resolve(&vpath) {
            Ok(Some(r)) => r,
            Ok(None) => return reply.error(errno_of(-libc::ENOENT)),
            Err(e) => return reply.error(errno_of(e)),
        };
        match nfs::readlink(&real) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(e) => reply.error(errno_of(e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        let vpath = match self.path_of(ino.0) {
            Some(p) => p,
            None => return reply.error(errno_of(-libc::ENOENT)),
        };
        let real = match self.resolve(&vpath) {
            Ok(Some(r)) => r,
            Ok(None) => return reply.error(errno_of(-libc::ENOENT)),
            Err(e) => return reply.error(errno_of(e)),
        };
        match nfs::open(&real) {
            Ok(fh) => {
                let id = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.files.write().unwrap().insert(id, fh);
                // KEEP_CACHE mirrors libfuse's auto_cache: the kernel may reuse
                // the page cache for this inode across opens; content is
                // invalidated on mtime change via the short ATTR_TTL above.
                reply.opened(
                    FileHandle(id),
                    FopenFlags::FOPEN_KEEP_CACHE,
                );
            }
            Err(e) => reply.error(errno_of(e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let nfs_fh = match self.files.read().unwrap().get(&fh.0) {
            Some(b) => b.clone(),
            None => return reply.error(errno_of(-libc::EBADF)),
        };
        match nfs::read(&nfs_fh, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno_of(e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // NFSv3 reads are stateless, so there is nothing to close upstream;
        // dropping our handle table entry is enough.
        self.files.write().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        let vpath = match self.path_of(ino.0) {
            Some(p) => p,
            None => return reply.error(errno_of(-libc::ENOENT)),
        };
        let children = match self.list_dir(&vpath) {
            Ok(c) => c,
            Err(e) => return reply.error(errno_of(e)),
        };
        // Synthesize "." and ".." with stable inodes.
        let self_ino = self.inode_of(&vpath);
        let parent_ino = self.inode_of(&parent_of(&vpath));
        let mut entries = Vec::with_capacity(children.len() + 2);
        entries.push(DirEnt {
            ino: INodeNo(self_ino),
            kind: FileType::Directory,
            name: OsString::from("."),
        });
        entries.push(DirEnt {
            ino: INodeNo(parent_ino),
            kind: FileType::Directory,
            name: OsString::from(".."),
        });
        entries.extend(children);

        let id = self.next_dir.fetch_add(1, Ordering::Relaxed);
        self.dirs.write().unwrap().insert(id, entries);
        reply.opened(FileHandle(id), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        let mut reply = reply;
        let entries = match self.dirs.read().unwrap().get(&fh.0) {
            Some(e) => e.clone(),
            None => return reply.error(errno_of(-libc::EBADF)),
        };
        // `offset` is the cookie of the first entry to emit (== its index).
        for (i, ent) in entries.iter().enumerate().skip(offset as usize) {
            // Cookie for the next call is this entry's index + 1; `add` returns
            // true once the reply buffer is full, so stop after that entry and
            // let the kernel resume from the cookie we just handed it.
            if reply.add(ent.ino, (i + 1) as u64, ent.kind, &ent.name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: fuser::OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.dirs.write().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // NFS statistics are per superblock, so the path/inode is irrelevant.
        let st = match nfs::statfs() {
            Ok(s) => s,
            Err(e) => return reply.error(errno_of(e)),
        };
        let bsize = BLKSIZE;
        let div = |bytes: u64| bytes / bsize as u64;
        reply.statfs(
            div(st.tbytes),
            div(st.fbytes),
            div(st.abytes),
            st.tfiles,
            st.ffiles,
            bsize,
            255,
            bsize,
        );
    }

    // `forget` is intentionally left at the default (no-op): the whitelist is a
    // finite namespace and we keep inodes forever for stable `st_ino`.
}

/// Start the FUSE event loop. Builds a `YourFs`, publishes the root inode, and
/// hands control to `fuser::mount`, which blocks until the filesystem is
/// unmounted. `config` carries the mount options assembled by the CLI layer.
pub fn run(ctx: DbCtx, mountpoint: &str, config: &fuser::Config) -> std::io::Result<()> {
    let fs = YourFs::new(ctx);
    fuser::mount(fs, mountpoint, config)
}
