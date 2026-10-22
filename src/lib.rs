//! cortex9-lsm-engine: a log-structured merge-tree storage engine.
//!
//! - [`sst`]: sorted tables with checksummed blocks, index and bloom filter.
//! - [`vfs`]: file-system trait, real directory, and a simulated one that
//!   models unsynced data, unsynced directory entries and torn sectors.

#![warn(missing_docs)]

pub mod bloom;
pub mod codec;
pub mod crc;
pub mod error;
pub mod rng;
pub mod sst;
pub mod version;
pub mod vfs;

pub use error::Error;
pub use vfs::{CrashMode, FsVfs, SimVfs, Vfs, VfsError};
