//! A flat directory of named files, with a real backend and a simulated
//! one that models what a crash can do:
//!
//! - file data is volatile until `sync(name)`;
//! - creating or removing a name is volatile until `sync_dir()`
//!   (as on Linux, where `fsync` of a file does not persist its
//!   directory entry);
//! - on a crash every volatile operation is independently lost, kept or,
//!   for appends, torn at 512-byte sector granularity.

use crate::rng::Rng;
use std::collections::BTreeMap;
use std::fmt;

/// Storage errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VfsError {
    /// Simulated power loss: this and every later call fails.
    Crashed,
    /// The file does not exist.
    NotFound(String),
    /// Underlying I/O error.
    Io(String),
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsError::Crashed => write!(f, "simulated crash"),
            VfsError::NotFound(n) => write!(f, "no such file: {n}"),
            VfsError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

/// Minimal file-system API used by the engine.
pub trait Vfs {
    /// Names of every file.
    fn list(&mut self) -> Result<Vec<String>, VfsError>;
    /// Create an empty file, replacing any file with that name.
    fn create(&mut self, name: &str) -> Result<(), VfsError>;
    /// File length.
    fn size(&mut self, name: &str) -> Result<u64, VfsError>;
    /// Read up to `len` bytes at `off` (shorter at end of file).
    fn read(&mut self, name: &str, off: u64, len: usize) -> Result<Vec<u8>, VfsError>;
    /// Append at the end of the file.
    fn append(&mut self, name: &str, data: &[u8]) -> Result<(), VfsError>;
    /// Make the file's data durable (`fsync`).
    fn sync(&mut self, name: &str) -> Result<(), VfsError>;
    /// Remove a name.
    fn remove(&mut self, name: &str) -> Result<(), VfsError>;
    /// Make every create/remove so far durable (`fsync` on the directory).
    fn sync_dir(&mut self) -> Result<(), VfsError>;

    /// Whole file.
    fn read_all(&mut self, name: &str) -> Result<Vec<u8>, VfsError> {
        let n = self.size(name)? as usize;
        self.read(name, 0, n)
    }
}

impl<T: Vfs + ?Sized> Vfs for &mut T {
    fn list(&mut self) -> Result<Vec<String>, VfsError> {
        (**self).list()
    }
    fn create(&mut self, name: &str) -> Result<(), VfsError> {
        (**self).create(name)
    }
    fn size(&mut self, name: &str) -> Result<u64, VfsError> {
        (**self).size(name)
    }
    fn read(&mut self, name: &str, off: u64, len: usize) -> Result<Vec<u8>, VfsError> {
        (**self).read(name, off, len)
    }
    fn append(&mut self, name: &str, data: &[u8]) -> Result<(), VfsError> {
        (**self).append(name, data)
    }
    fn sync(&mut self, name: &str) -> Result<(), VfsError> {
        (**self).sync(name)
    }
    fn remove(&mut self, name: &str) -> Result<(), VfsError> {
        (**self).remove(name)
    }
    fn sync_dir(&mut self) -> Result<(), VfsError> {
        (**self).sync_dir()
    }
}

/// Real files in one directory.
pub struct FsVfs {
    dir: std::path::PathBuf,
    open: BTreeMap<String, std::fs::File>,
}

impl FsVfs {
    /// Use (creating) `dir`.
    pub fn open(dir: &std::path::Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            open: BTreeMap::new(),
        })
    }

    fn file(&mut self, name: &str) -> Result<&mut std::fs::File, VfsError> {
        if !self.open.contains_key(name) {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .append(true)
                .open(self.dir.join(name))
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => VfsError::NotFound(name.to_string()),
                    _ => io(e),
                })?;
            self.open.insert(name.to_string(), f);
        }
        Ok(self.open.get_mut(name).expect("just inserted"))
    }
}

fn io(e: std::io::Error) -> VfsError {
    VfsError::Io(e.to_string())
}

#[cfg(unix)]
impl Vfs for FsVfs {
    fn list(&mut self) -> Result<Vec<String>, VfsError> {
        let mut v = Vec::new();
        for e in std::fs::read_dir(&self.dir).map_err(io)? {
            let e = e.map_err(io)?;
            if e.file_type().map_err(io)?.is_file() {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        v.sort();
        Ok(v)
    }
    fn create(&mut self, name: &str) -> Result<(), VfsError> {
        self.open.remove(name);
        std::fs::File::create(self.dir.join(name)).map_err(io)?;
        Ok(())
    }
    fn size(&mut self, name: &str) -> Result<u64, VfsError> {
        Ok(self.file(name)?.metadata().map_err(io)?.len())
    }
    fn read(&mut self, name: &str, off: u64, len: usize) -> Result<Vec<u8>, VfsError> {
        use std::os::unix::fs::FileExt;
        let f = self.file(name)?;
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = f.read_at(&mut buf[done..], off + done as u64).map_err(io)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }
    fn append(&mut self, name: &str, data: &[u8]) -> Result<(), VfsError> {
        use std::io::Write;
        self.file(name)?.write_all(data).map_err(io)
    }
    fn sync(&mut self, name: &str) -> Result<(), VfsError> {
        self.file(name)?.sync_all().map_err(io)
    }
    fn remove(&mut self, name: &str) -> Result<(), VfsError> {
        self.open.remove(name);
        std::fs::remove_file(self.dir.join(name)).map_err(io)
    }
    fn sync_dir(&mut self) -> Result<(), VfsError> {
        std::fs::File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(io)
    }
}

/// What survives a crash among volatile operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashMode {
    /// Every volatile operation is lost.
    LoseAll,
    /// Every volatile operation made it.
    KeepAll,
    /// Each volatile operation independently: lost, kept or (appends)
    /// torn at sector granularity. Also models reordering.
    Torn,
}

/// Sector size used to tear appends.
pub const SECTOR: usize = 512;

#[derive(Debug, Clone, Default)]
struct Inode {
    durable: Vec<u8>,
    current: Vec<u8>,
    /// Unsynced appends as (offset, data).
    pending: Vec<(usize, Vec<u8>)>,
}

#[derive(Debug, Clone)]
enum DirOp {
    Link(String, u64),
    Unlink(String),
}

/// In-memory file system with a crash switch.
#[derive(Debug, Clone, Default)]
pub struct SimVfs {
    inodes: BTreeMap<u64, Inode>,
    names: BTreeMap<String, u64>,
    durable_names: BTreeMap<String, u64>,
    dir_pending: Vec<DirOp>,
    next_ino: u64,
    ops: u64,
    crash_after: Option<u64>,
    crashed: bool,
    /// `sync` + `sync_dir` calls.
    pub syncs: u64,
}

impl SimVfs {
    /// Empty file system.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lose power on the mutating operation after `n` successful ones.
    pub fn crash_after(&mut self, n: u64) {
        self.crash_after = Some(n);
    }

    /// Mutating operations (create, append, sync, remove, sync_dir) so far.
    pub fn ops(&self) -> u64 {
        self.ops
    }

    /// Whether the crash switch fired.
    pub fn crashed(&self) -> bool {
        self.crashed
    }

    /// The file system as it looks after power comes back.
    pub fn after_crash(&self, mode: CrashMode, rng: &mut Rng) -> SimVfs {
        let keep = |rng: &mut Rng| match mode {
            CrashMode::LoseAll => false,
            CrashMode::KeepAll => true,
            CrashMode::Torn => rng.below(2) == 0,
        };
        let mut names = self.durable_names.clone();
        for op in &self.dir_pending {
            if keep(rng) {
                match op {
                    DirOp::Link(n, i) => {
                        names.insert(n.clone(), *i);
                    }
                    DirOp::Unlink(n) => {
                        names.remove(n);
                    }
                }
            }
        }
        let mut out = SimVfs::new();
        for (name, ino) in names {
            let node = &self.inodes[&ino];
            let mut buf = node.durable.clone();
            for (off, data) in &node.pending {
                let choice = match mode {
                    CrashMode::LoseAll => 0,
                    CrashMode::KeepAll => 1,
                    CrashMode::Torn => rng.below(3),
                };
                match choice {
                    0 => {}
                    1 => write_at(&mut buf, *off, data),
                    _ => {
                        let (start, end) = (*off, *off + data.len());
                        let mut s = start - start % SECTOR;
                        while s < end {
                            let e = (s + SECTOR).min(end);
                            let a = s.max(start);
                            if rng.below(2) == 0 {
                                write_at(&mut buf, a, &data[a - start..e - start]);
                            }
                            s += SECTOR;
                        }
                    }
                }
            }
            let ino = out.next_ino;
            out.next_ino += 1;
            out.inodes.insert(
                ino,
                Inode {
                    durable: buf.clone(),
                    current: buf,
                    pending: Vec::new(),
                },
            );
            out.names.insert(name.clone(), ino);
            out.durable_names.insert(name, ino);
        }
        out
    }

    fn step(&mut self) -> Result<(), VfsError> {
        if self.crashed {
            return Err(VfsError::Crashed);
        }
        if self.crash_after.is_some_and(|n| self.ops >= n) {
            self.crashed = true;
            return Err(VfsError::Crashed);
        }
        self.ops += 1;
        Ok(())
    }

    fn live(&self) -> Result<(), VfsError> {
        if self.crashed {
            Err(VfsError::Crashed)
        } else {
            Ok(())
        }
    }

    fn ino(&self, name: &str) -> Result<u64, VfsError> {
        self.names
            .get(name)
            .copied()
            .ok_or_else(|| VfsError::NotFound(name.to_string()))
    }
}

fn write_at(buf: &mut Vec<u8>, off: usize, data: &[u8]) {
    let end = off + data.len();
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[off..end].copy_from_slice(data);
}

impl Vfs for SimVfs {
    fn list(&mut self) -> Result<Vec<String>, VfsError> {
        self.live()?;
        Ok(self.names.keys().cloned().collect())
    }
    fn create(&mut self, name: &str) -> Result<(), VfsError> {
        self.step()?;
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, Inode::default());
        self.names.insert(name.to_string(), ino);
        self.dir_pending.push(DirOp::Link(name.to_string(), ino));
        Ok(())
    }
    fn size(&mut self, name: &str) -> Result<u64, VfsError> {
        self.live()?;
        Ok(self.inodes[&self.ino(name)?].current.len() as u64)
    }
    fn read(&mut self, name: &str, off: u64, len: usize) -> Result<Vec<u8>, VfsError> {
        self.live()?;
        let cur = &self.inodes[&self.ino(name)?].current;
        let off = (off as usize).min(cur.len());
        let end = (off + len).min(cur.len());
        Ok(cur[off..end].to_vec())
    }
    fn append(&mut self, name: &str, data: &[u8]) -> Result<(), VfsError> {
        let ino = self.ino(name)?;
        self.step()?;
        let node = self.inodes.get_mut(&ino).expect("inode");
        let off = node.current.len();
        node.current.extend_from_slice(data);
        node.pending.push((off, data.to_vec()));
        Ok(())
    }
    fn sync(&mut self, name: &str) -> Result<(), VfsError> {
        let ino = self.ino(name)?;
        self.step()?;
        self.syncs += 1;
        let node = self.inodes.get_mut(&ino).expect("inode");
        node.durable = node.current.clone();
        node.pending.clear();
        Ok(())
    }
    fn remove(&mut self, name: &str) -> Result<(), VfsError> {
        self.ino(name)?;
        self.step()?;
        self.names.remove(name);
        self.dir_pending.push(DirOp::Unlink(name.to_string()));
        Ok(())
    }
    fn sync_dir(&mut self) -> Result<(), VfsError> {
        self.step()?;
        self.syncs += 1;
        self.durable_names = self.names.clone();
        self.dir_pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_needs_sync_and_names_need_sync_dir() {
        let mut v = SimVfs::new();
        v.create("a").unwrap();
        v.append("a", b"hello").unwrap();
        v.sync("a").unwrap();
        let mut rng = Rng::new(1);
        // The file's data was synced but its name was not.
        let mut c = v.after_crash(CrashMode::LoseAll, &mut rng);
        assert!(c.list().unwrap().is_empty());
        v.sync_dir().unwrap();
        v.append("a", b" world").unwrap();
        let mut c = v.after_crash(CrashMode::LoseAll, &mut rng);
        assert_eq!(c.read_all("a").unwrap(), b"hello");
        let mut c = v.after_crash(CrashMode::KeepAll, &mut rng);
        assert_eq!(c.read_all("a").unwrap(), b"hello world");
    }

    #[test]
    fn unsynced_remove_can_come_back() {
        let mut v = SimVfs::new();
        v.create("a").unwrap();
        v.append("a", b"x").unwrap();
        v.sync("a").unwrap();
        v.sync_dir().unwrap();
        v.remove("a").unwrap();
        assert!(v.list().unwrap().is_empty());
        let mut rng = Rng::new(2);
        let mut c = v.after_crash(CrashMode::LoseAll, &mut rng);
        assert_eq!(c.read_all("a").unwrap(), b"x");
    }

    #[test]
    fn torn_appends_keep_whole_sectors() {
        let mut v = SimVfs::new();
        v.create("a").unwrap();
        v.sync_dir().unwrap();
        v.append("a", &vec![7u8; 4 * SECTOR]).unwrap();
        let mut rng = Rng::new(3);
        let mut partial = false;
        for _ in 0..50 {
            let mut c = v.after_crash(CrashMode::Torn, &mut rng);
            let b = c.read_all("a").unwrap();
            for s in b.chunks(SECTOR) {
                assert!(s.iter().all(|x| *x == 7) || s.iter().all(|x| *x == 0));
            }
            let n = b.iter().filter(|x| **x == 7).count();
            partial |= n > 0 && n < 4 * SECTOR;
        }
        assert!(partial);
    }

    #[test]
    fn crash_switch_fails_everything_after() {
        let mut v = SimVfs::new();
        v.crash_after(1);
        v.create("a").unwrap();
        assert_eq!(v.sync_dir(), Err(VfsError::Crashed));
        assert_eq!(v.list(), Err(VfsError::Crashed));
        assert!(v.crashed());
    }
}
