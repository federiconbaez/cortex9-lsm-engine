//! cortex9-lsm-engine: a log-structured merge-tree storage engine.
//!
//! - [`db`]: write path (WAL + memtable), flushes, leveled compaction,
//!   manifest and recovery.
//! - [`sst`]: sorted tables with checksummed blocks, index and bloom filter.
//! - [`vfs`]: file-system trait, real directory, and a simulated one that
//!   models unsynced data, unsynced directory entries and torn sectors.

#![warn(missing_docs)]

pub mod bloom;
pub mod codec;
pub mod crc;
pub mod db;
pub mod error;
pub mod rng;
pub mod sst;
pub mod version;
pub mod vfs;

pub use db::{Bugs, Db, Options, Stats};
pub use error::Error;
pub use vfs::{CrashMode, FsVfs, SimVfs, Vfs, VfsError};
