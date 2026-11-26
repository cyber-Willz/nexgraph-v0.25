//! Database handle: durability (WAL + checkpoint), single-writer
//! transactions, snapshot reads.

use crate::error::{invalid, Error, Result};
use crate::model::{Commit, Metric, Op};
use crate::store::{Inner, View};
use crate::value::{crc32, Dec, Id, Props, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};

/// Optional at-rest key ring for the file backend (always `None` without the `crypto` feature).
#[cfg(feature = "crypto")]
pub(crate) type Key = Option<std::sync::Arc<crate::crypto::Keyring>>;
#[cfg(not(feature = "crypto"))]
pub(crate) type Key = Option<std::convert::Infallible>;

const AAD_CKPT: &[u8] = b"nexgraph/file/checkpoint/v1";

fn wal_aad(n: u64) -> Vec<u8> {
    let mut a = b"nexgraph/file/wal/v1/".to_vec();
    a.extend_from_slice(&n.to_le_bytes());
    a
}

#[allow(unused_variables)]
fn seal_rec(key: &Key, aad: &[u8], plain: Vec<u8>) -> Result<Vec<u8>> {
    #[cfg(feature = "crypto")]
    if let Some(k) = key {
        return k.seal(aad, &plain);
    }
    Ok(plain)
}

#[allow(unused_variables)]
fn open_rec(key: &Key, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    #[cfg(feature = "crypto")]
    if let Some(k) = key {
        return k.open(aad, blob);
    }
    Ok(blob.to_vec())
}

const MARKER: &str = "encryption.marker";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// fsync the log on every commit (durable against power loss).
    Always,
    /// Leave flushing to the OS (durable against process crash only).
    Never,
}

pub(crate) struct Wal {
    file: File,
    sync: bool,
    key: Key,
    /// Frames in the log so far (binds each encrypted frame to its position).
    n: u64,
}

pub(crate) fn frame(payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 8);
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&crc32(payload).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// Parse frames; returns payloads and the byte length of the valid prefix.
/// A bad frame that reaches/passes EOF is a torn tail (truncated); a bad
/// frame followed by more data is reported as corruption.
pub(crate) fn parse_frames(buf: &[u8]) -> Result<(Vec<&[u8]>, usize)> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < buf.len() {
        if buf.len() - pos < 8 {
            break;
        }
        let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let end = pos + 8 + len;
        if end > buf.len() {
            break;
        }
        let payload = &buf[pos + 8..end];
        if crc32(payload) != crc {
            if end < buf.len() {
                return Err(Error::Corrupt(format!("checksum mismatch mid-log at byte {pos}")));
            }
            break;
        }
        out.push(payload);
        pos = end;
    }
    Ok((out, pos))
}

impl Wal {
    fn open(path: &Path, sync: bool, key: Key) -> Result<(Wal, Vec<Commit>)> {
        let mut file = OpenOptions::new().read(true).write(true).create(true).open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let (payloads, good) = parse_frames(&buf)?;
        let n = payloads.len() as u64;
        let commits = payloads
            .into_iter()
            .enumerate()
            .map(|(i, p)| Commit::decode(&open_rec(&key, &wal_aad(i as u64), p)?))
            .collect::<Result<Vec<_>>>()?;
        if good < buf.len() {
            file.set_len(good as u64)?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((Wal { file, sync, key, n }, commits))
    }
    fn append(&mut self, c: &Commit) -> Result<()> {
        let sealed = seal_rec(&self.key, &wal_aad(self.n), c.encode())?;
        self.file.write_all(&frame(&sealed))?;
        self.n += 1;
        if self.sync {
            self.file.sync_data()?;
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        self.n = 0;
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// Where commits are made durable.
pub(crate) enum Dur {
    /// Nothing is persisted.
    Mem,
    /// Checksummed log + checkpoint file in a directory.
    File(Wal),
    /// Commit log, checkpoint and relational mirror inside a PostgreSQL schema.
    #[cfg(feature = "postgres")]
    Pg(Box<crate::pg::PgStore>),
}

pub(crate) struct Writer {
    pub(crate) dur: Dur,
    pub(crate) next_id: Id,
    pub(crate) dir: Option<PathBuf>,
    /// Read-only replicas never write; `Txn::commit` and `checkpoint` refuse.
    pub(crate) read_only: bool,
    /// Exclusive advisory lock on `<dir>/LOCK`, held for the life of the handle (file backend only).
    pub(crate) _lock: Option<File>,
}

pub struct Database {
    pub(crate) inner: RwLock<Inner>,
    pub(crate) writer: Mutex<Writer>,
    pub(crate) last_ts: AtomicU64,
    pub(crate) horizon: AtomicU64,
}

/// Takes the exclusive lock on `<dir>/LOCK` without blocking. A lock the filesystem cannot provide at all
/// (e.g. some network filesystems) is not an error: the handle then opens unlocked, as before v0.22.
fn lock_dir(dir: &Path) -> Result<Option<File>> {
    let path = dir.join("LOCK");
    let busy = || Error::Invalid(format!("database directory {} is already open in another handle or process (lock held on LOCK)", dir.display()));
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
        // SAFETY: flock only reads the descriptor number of a file we own and open for the whole call.
        if unsafe { flock(f.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
            return Ok(Some(f));
        }
        let e = std::io::Error::last_os_error();
        return if e.kind() == std::io::ErrorKind::WouldBlock { Err(busy()) } else { Ok(None) };
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // share_mode(0): no other open of this file succeeds while we hold it (ERROR_SHARING_VIOLATION = 32).
        return match OpenOptions::new().read(true).write(true).create(true).share_mode(0).open(&path) {
            Ok(f) => Ok(Some(f)),
            Err(e) if e.raw_os_error() == Some(32) => Err(busy()),
            Err(e) => Err(e.into()),
        };
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, busy);
        Ok(None)
    }
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

impl Database {
    pub fn in_memory() -> Database {
        Database {
            inner: RwLock::new(Inner::new()),
            writer: Mutex::new(Writer { dur: Dur::Mem, next_id: 1, dir: None, read_only: false, _lock: None }),
            last_ts: AtomicU64::new(0),
            horizon: AtomicU64::new(0),
        }
    }

    pub fn open(dir: impl AsRef<Path>) -> Result<Database> {
        Self::open_with(dir, SyncMode::Always)
    }

    /// Open (or create) a database directory and recover its state. Only one
    /// handle (in this or any other process) may have a directory open at a time: the directory holds an
    /// exclusive advisory lock (`LOCK`, flock on Unix, a no-share open on Windows) and a second open fails
    /// with `Error::Invalid("... is already open ...")` instead of interleaving writes into the same log.
    /// The lock is released when the `Database` is dropped or the process dies, so it cannot go stale.
    pub fn open_with(dir: impl AsRef<Path>, sync: SyncMode) -> Result<Database> {
        Self::open_keyed(dir, sync, None)
    }

    /// Open (or create) a database directory whose commit log and checkpoints are encrypted at rest
    /// (XChaCha20-Poly1305, see [`crate::crypto`]). A directory is either encrypted or not: opening an
    /// encrypted one without a key, or existing plaintext data with a key, is refused (a silent mix would
    /// leave plaintext in the log). To rotate a key, open with a ring that holds the old key and call
    /// `checkpoint()`: the live state is re-sealed under the active key and the old log is discarded.
    #[cfg(feature = "crypto")]
    pub fn open_encrypted(dir: impl AsRef<Path>, sync: SyncMode, keys: crate::crypto::Keyring) -> Result<Database> {
        Self::open_keyed(dir, sync, Some(std::sync::Arc::new(keys)))
    }

    fn open_keyed(dir: impl AsRef<Path>, sync: SyncMode, key: Key) -> Result<Database> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = lock_dir(&dir)?; // before anything is read: a second process must not even see a half-written state
        let marker = dir.join(MARKER);
        let has_data = dir.join("checkpoint.bin").exists() || fs::metadata(dir.join("wal.log")).map(|m| m.len() > 0).unwrap_or(false);
        if marker.exists() && key.is_none() {
            return Err(Error::Invalid("this database is encrypted at rest: open it with open_encrypted and its key".into()));
        }
        if key.is_some() && !marker.exists() {
            if has_data {
                return Err(Error::Invalid("this directory holds unencrypted data; refusing to mix it with encrypted records (export and re-import into a new encrypted directory)".into()));
            }
            fs::write(&marker, b"nexgraph-encrypted-v1\n")?;
            sync_dir(&dir);
        }
        let mut inner = Inner::new();
        let mut last_ts = 0u64;
        let mut next_id: Id = 1;
        let ck = dir.join("checkpoint.bin");
        if ck.exists() {
            // The file bytes and the decrypted copy are dropped before the ops are applied, and the ops are moved
            // (not cloned) into the store, so opening never holds the data more than once.
            let c = {
                let buf = fs::read(&ck)?;
                let (p, good) = parse_frames(&buf)?;
                if p.len() != 1 || good != buf.len() {
                    return Err(Error::Corrupt("checkpoint file is damaged".into()));
                }
                let plain = open_rec(&key, AAD_CKPT, p[0])?;
                let mut d = Dec::new(&plain);
                next_id = d.varint()?;
                Commit::decode(d.rest())?
            };
            let ck_ts = c.ts;
            inner.apply_owned(c.ts, c.ops)?;
            last_ts = ck_ts;
        }
        let horizon = last_ts;
        let (wal, commits) = Wal::open(&dir.join("wal.log"), sync == SyncMode::Always, key)?;
        for c in commits {
            if c.ts <= last_ts {
                continue;
            }
            inner.apply(c.ts, &c.ops)?;
            last_ts = c.ts;
        }
        next_id = next_id.max(inner.max_id() + 1);
        Ok(Database {
            inner: RwLock::new(inner),
            writer: Mutex::new(Writer { dur: Dur::File(wal), next_id, dir: Some(dir), read_only: false, _lock: lock }),
            last_ts: AtomicU64::new(last_ts),
            horizon: AtomicU64::new(horizon),
        })
    }

    /// Latest committed timestamp.
    pub fn last_ts(&self) -> u64 {
        self.last_ts.load(Ordering::Acquire)
    }

    /// Oldest timestamp that can still be read (history before the last
    /// checkpoint has been compacted away).
    pub fn horizon(&self) -> u64 {
        self.horizon.load(Ordering::Acquire)
    }

    /// Run `f` against a consistent snapshot of the latest committed state.
    pub fn read<R>(&self, f: impl FnOnce(&View) -> R) -> R {
        let g = self.inner.read().unwrap();
        let v = View { inner: &g, ts: self.last_ts() };
        f(&v)
    }

    /// Time travel: run `f` against the state as of commit `ts`.
    pub fn read_at<R>(&self, ts: u64, f: impl FnOnce(&View) -> R) -> Result<R> {
        let g = self.inner.read().unwrap();
        if ts > self.last_ts() {
            return invalid(format!("timestamp {ts} is in the future (latest is {})", self.last_ts()));
        }
        if ts < self.horizon() {
            return invalid(format!("history before ts {} was compacted by a checkpoint", self.horizon()));
        }
        let v = View { inner: &g, ts };
        Ok(f(&v))
    }

    /// Begin a write transaction. Blocks while another writer is active.
    /// Dropping the transaction without `commit` rolls it back.
    pub fn begin(&self) -> Txn<'_> {
        let guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let start_id = guard.next_id;
        Txn { db: self, guard, ops: Vec::new(), start_id }
    }

    /// Write a checkpoint (live state only), truncate the WAL and drop
    /// version history older than the latest commit. Also usable on
    /// in-memory databases purely as vacuum.
    pub fn checkpoint(&self) -> Result<u64> {
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let w = &mut *guard;
        if w.read_only {
            return Err(Error::Unsupported("database is open read-only".into()));
        }
        let mut g = self.inner.write().unwrap();
        let ts = self.last_ts();
        let next_id = w.next_id;
        // Encode straight from the store, one entity at a time: the old path cloned every property into an op list first.
        let plain = crate::model::encode_stream(next_id, ts, g.snapshot_len(), g.snapshot_iter());
        match &mut w.dur {
            Dur::Mem => {}
            Dur::File(wal) => {
                if let Some(dir) = w.dir.clone() {
                    let tmp = dir.join("checkpoint.tmp");
                    let sealed = seal_rec(&wal.key, AAD_CKPT, plain.clone())?;
                    // Frames carry a u32 length; a larger payload would wrap and write an unreadable checkpoint.
                    if sealed.len() > u32::MAX as usize {
                        return Err(Error::Unsupported(format!("checkpoint is {} bytes; the single-record limit is 4 GiB", sealed.len())));
                    }
                    {
                        let mut f = File::create(&tmp)?;
                        f.write_all(&(sealed.len() as u32).to_le_bytes())?;
                        f.write_all(&crc32(&sealed).to_le_bytes())?;
                        f.write_all(&sealed)?;
                        f.sync_all()?;
                    }
                    drop(sealed);
                    fs::rename(&tmp, dir.join("checkpoint.bin"))?;
                    sync_dir(&dir);
                    wal.reset()?;
                }
            }
            #[cfg(feature = "postgres")]
            Dur::Pg(p) => {
                let mut d = Dec::new(&plain);
                d.varint()?;
                p.checkpoint(next_id, &Commit::decode(d.rest())?)?
            }
        }
        // Rebuild the store from the snapshot to drop history. The old store is released first so the two never
        // coexist in memory; the checkpoint is already durable, so a failure here (not expected: the bytes were just
        // produced from this store) is cured by reopening.
        drop(std::mem::replace(&mut *g, Inner::new()));
        let c = {
            let mut d = Dec::new(&plain);
            d.varint()?;
            Commit::decode(d.rest())?
        };
        drop(plain);
        g.apply_owned(ts, c.ops)?;
        self.horizon.store(ts, Ordering::Release);
        Ok(ts)
    }

    pub fn stats(&self) -> crate::store::StoreStats {
        self.read(|v| v.stats())
    }

    /// Parse and run one query statement.
    pub fn query(&self, text: &str) -> Result<crate::query::QueryResult> {
        crate::query::run(self, text, &Default::default())
    }

    pub fn query_with(&self, text: &str, params: &crate::query::Params) -> Result<crate::query::QueryResult> {
        crate::query::run(self, text, params)
    }
}

pub struct Txn<'a> {
    db: &'a Database,
    guard: MutexGuard<'a, Writer>,
    ops: Vec<Op>,
    start_id: Id,
}

impl<'a> Txn<'a> {
    fn alloc(&mut self) -> Id {
        let id = self.guard.next_id;
        self.guard.next_id += 1;
        id
    }

    pub fn create_node(&mut self, labels: &[&str], props: Props) -> Result<Id> {
        props.values().try_for_each(|v| v.check_storable())?;
        let id = self.alloc();
        self.ops.push(Op::CreateNode { id, labels: labels.iter().map(|s| s.to_string()).collect(), props });
        Ok(id)
    }

    /// Create a directed binary edge `from -> to`.
    pub fn create_edge(&mut self, label: &str, from: Id, to: Id, weight: f64, props: Props) -> Result<Id> {
        self.create_hyperedge(label, &[from], &[to], weight, props)
    }

    /// Create a directed hyperedge (`tail -> head`). An empty `head` makes an
    /// undirected hyperedge over `tail`.
    pub fn create_hyperedge(&mut self, label: &str, tail: &[Id], head: &[Id], weight: f64, props: Props) -> Result<Id> {
        if tail.is_empty() && head.is_empty() {
            return invalid("an edge needs at least one member");
        }
        if label.is_empty() {
            return invalid("edges need a label");
        }
        props.values().try_for_each(|v| v.check_storable())?;
        let id = self.alloc();
        self.ops.push(Op::CreateEdge { id, label: label.into(), tail: tail.to_vec(), head: head.to_vec(), weight, props });
        Ok(id)
    }

    pub fn set_props(&mut self, id: Id, set: Props, remove: &[&str]) -> Result<()> {
        set.values().try_for_each(|v| v.check_storable())?;
        self.ops.push(Op::Update {
            id,
            set,
            remove: remove.iter().map(|s| s.to_string()).collect(),
            add_labels: vec![],
            remove_labels: vec![],
            weight: None,
        });
        Ok(())
    }

    pub fn set_prop(&mut self, id: Id, key: &str, v: impl Into<Value>) -> Result<()> {
        let mut p = Props::new();
        p.insert(key.to_string(), v.into());
        self.set_props(id, p, &[])
    }

    pub fn set_weight(&mut self, id: Id, w: f64) {
        self.ops.push(Op::Update { id, set: Props::new(), remove: vec![], add_labels: vec![], remove_labels: vec![], weight: Some(w) });
    }

    pub fn add_label(&mut self, id: Id, label: &str) {
        self.ops.push(Op::Update { id, set: Props::new(), remove: vec![], add_labels: vec![label.into()], remove_labels: vec![], weight: None });
    }

    pub fn remove_label(&mut self, id: Id, label: &str) {
        self.ops.push(Op::Update { id, set: Props::new(), remove: vec![], add_labels: vec![], remove_labels: vec![label.into()], weight: None });
    }

    /// Delete a node (and every incident edge/hyperedge) or an edge.
    pub fn delete(&mut self, id: Id) {
        self.ops.push(Op::Delete { id });
    }

    pub fn create_property_index(&mut self, label: &str, key: &str) {
        self.ops.push(Op::CreatePropIndex { label: label.into(), key: key.into() });
    }

    pub fn create_vector_index(&mut self, label: &str, key: &str, dim: u32, metric: Metric) {
        self.ops.push(Op::CreateVectorIndex { label: label.into(), key: key.into(), dim, metric });
    }

    pub fn pending_ops(&self) -> usize {
        self.ops.len()
    }

    pub fn rollback(mut self) {
        self.guard.next_id = self.start_id;
    }

    /// Validate, make durable, then apply atomically. Returns the commit
    /// timestamp. On error nothing is applied.
    pub fn commit(mut self) -> Result<u64> {
        if self.ops.is_empty() {
            self.guard.next_id = self.start_id;
            return Ok(self.db.last_ts());
        }
        if self.guard.read_only {
            self.guard.next_id = self.start_id;
            return Err(Error::Unsupported("database is open read-only".into()));
        }
        let ts = self.db.last_ts() + 1;
        let ops = std::mem::take(&mut self.ops);
        let res = (|| -> Result<()> {
            let c = Commit { ts, ops };
            {
                // Validate and (for PostgreSQL) derive the mirror delta against the
                // current snapshot while holding only the read lock; the writer mutex
                // guarantees nobody else changes the state in between.
                let g = self.db.inner.read().unwrap();
                g.validate(&c.ops)?;
                match &mut self.guard.dur {
                    Dur::Mem => {}
                    Dur::File(w) => w.append(&c)?,
                    #[cfg(feature = "postgres")]
                    Dur::Pg(p) => p.append(&c, &g, ts - 1)?,
                }
            }
            let mut g = self.db.inner.write().unwrap();
            g.apply_owned(ts, c.ops)?;
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.db.last_ts.store(ts, Ordering::Release);
                Ok(ts)
            }
            Err(e) => {
                self.guard.next_id = self.start_id;
                Err(e)
            }
        }
    }
}
