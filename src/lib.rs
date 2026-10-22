//! cortex9-lsm-engine: a log-structured merge-tree storage engine.
//!
//! - [`vfs`]: file-system trait, real directory, and a simulated one that
//!   models unsynced data, unsynced directory entries and torn sectors.

#![warn(missing_docs)]

pub mod codec;
pub mod crc;
pub mod error;
pub mod rng;
pub mod vfs;

pub use error::Error;
pub use vfs::{CrashMode, FsVfs, SimVfs, Vfs, VfsError};
