//! yourfs: a read-only file system with file isolation capabilities over NFS.
//!
//! Rust rewrite of the original C implementation
//! (github.com/TechWander/yourfs). The virtual directory tree shown to users
//! is defined by a whitelist stored in sqlite3, postgres or redis; data is
//! fetched from a remote NFS export through the pure-Rust `nfs-rs` client and
//! served over FUSE by the pure-Rust `fuser` crate.

pub mod cli;
pub mod db;
pub mod fs;
pub mod key;
pub mod nfscli;
pub mod paths;
pub mod scan;
