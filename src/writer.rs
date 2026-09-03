use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::WriteError;
use crate::format::{DEFAULT_CAPACITY, FRAME_OVERHEAD, MAX_RECORD_SIZE, checksum};

/// An append handle for a single WAL segment.
///
/// The handle owns one file and appends length-prefixed, checksummed frames to
/// it. Durability is the caller's decision: [`push`](Self::push) never touches
/// the disk, and the caller drives [`flush`](Self::flush) and
/// [`sync`](Self::sync) using the two watermarks below.
///
/// There are two barriers, and they form a hierarchy:
///
/// - [`flush`](Self::flush) moves bytes from this handle's userspace buffer into
///   the kernel page cache. After it, a [`WalReader`](crate::WalReader) opened on
///   the segment can see them. They are *not* yet durable.
/// - [`sync`](Self::sync) flushes and then forces the kernel's copy to disk.
///   Durable implies visible.
///
/// `&mut self` throughout: a WAL is a serialised append stream, and the ordering
/// *is* the data structure. The borrow checker enforces one-writer-at-a-time at
/// zero runtime cost. A caller who needs to share the handle across threads can
/// wrap it in a `Mutex` with lock granularity they control.
pub struct WalWriter {
    file: BufWriter<File>,
    path: PathBuf,
    /// Logical end of the segment, counting bytes still sitting in the buffer.
    offset: u64,
    /// Logical offset known to be on disk.
    synced_offset: u64,
    closed: bool,
}

impl WalWriter {
    /// Creates a new segment named `name` inside `dir` and opens it for append.
    ///
    /// Fails with [`WriteError::AlreadyExists`] if the segment is already there.
    /// Resuming an unknown file is worse than refusing it: two writers appending
    /// to one segment produce a log that cannot be read back.
    pub fn new(dir: &Path, name: &str) -> Result<Self, WriteError> {
        Self::with_capacity(dir, name, DEFAULT_CAPACITY)
    }

    /// Like [`new`](Self::new), with an explicit userspace buffer size.
    ///
    /// The buffer is pure runtime tuning — no reader can observe it — so unlike
    /// [`MAX_RECORD_SIZE`] it is safe to vary per writer.
    pub fn with_capacity(dir: &Path, name: &str, capacity: usize) -> Result<Self, WriteError> {
        let path = dir.join(name);
        let file = File::options()
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => WriteError::AlreadyExists { path: path.clone() },
                _ => WriteError::Io(e),
            })?;

        // Sync the *directory*, not the file. Until the directory entry is
        // durable the segment's name can vanish in a crash, taking every record
        // with it — no amount of syncing the file's contents would help.
        File::open(dir)?.sync_all()?;

        Ok(Self {
            file: BufWriter::with_capacity(capacity, file),
            path,
            offset: 0,
            synced_offset: 0,
            closed: false,
        })
    }

    /// Appends one record and returns the byte offset at which its frame begins.
    ///
    /// The offset is *logical*: it only becomes a valid seek target once
    /// [`flush`](Self::flush) has run. Until then it names bytes that exist
    /// nowhere but this process.
    pub fn push(&mut self, record: &[u8]) -> Result<u64, WriteError> {
        if record.is_empty() {
            return Err(WriteError::EmptyRecord);
        }
        if record.len() > MAX_RECORD_SIZE {
            return Err(WriteError::RecordTooLarge { len: record.len() });
        }

        let len_bytes = (record.len() as u32).to_le_bytes();
        let crc = checksum(&len_bytes, record);

        // Three writes rather than one assembled buffer: against a BufWriter
        // these are memcpys into the existing buffer, so this avoids an
        // allocation per record without costing extra syscalls.
        self.file.write_all(&len_bytes)?;
        self.file.write_all(record)?;
        self.file.write_all(&crc.to_le_bytes())?;

        let start = self.offset;
        self.offset += (FRAME_OVERHEAD + record.len()) as u64;
        Ok(start)
    }

    /// Moves buffered bytes into the kernel, making them visible to readers.
    ///
    /// Cheap relative to [`sync`](Self::sync) — one `write(2)`, or no syscall at
    /// all if the buffer is empty — but it grants visibility, not durability.
    pub fn flush(&mut self) -> Result<(), WriteError> {
        self.file.flush()?;
        Ok(())
    }

    /// Flushes, then forces the kernel's copy of the segment to disk.
    ///
    /// The internal flush is free: those bytes have to reach the kernel via
    /// `write(2)` before any sync can push them to the platter, so this is the
    /// same syscall count as flushing yourself first. It exists so `sync` can
    /// never report success over data still sitting in userspace.
    ///
    /// Uses `sync_data`, which is `fdatasync(2)` on Linux — file *size* is
    /// flushed because readers need it, but timestamps are left to be updated
    /// lazily. On macOS the platform collapses this onto `F_FULLFSYNC`, which is
    /// stronger and considerably slower; that is a platform artifact, not a bug.
    pub fn sync(&mut self) -> Result<(), WriteError> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;
        self.synced_offset = self.offset;
        Ok(())
    }

    /// Flushes, syncs, and closes the segment.
    ///
    /// The only way to *observe* a failing final flush. [`Drop`] tries too, but
    /// it has nowhere to return an error to.
    pub fn close(mut self) -> Result<(), WriteError> {
        self.sync()?;
        self.closed = true;
        Ok(())
    }

    /// Path of the segment, for handing to [`WalReader::open`](crate::WalReader::open).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written but not yet in the kernel. Resets on [`flush`](Self::flush).
    ///
    /// Read from the buffer itself rather than tracked by hand, so it stays
    /// correct when `BufWriter` flushes on its own after filling up.
    pub fn unflushed_bytes(&self) -> usize {
        self.file.buffer().len()
    }

    /// Bytes in the kernel but not yet on disk. Resets on [`sync`](Self::sync).
    pub fn unsynced_bytes(&self) -> usize {
        let in_kernel = self.offset - self.unflushed_bytes() as u64;
        in_kernel.saturating_sub(self.synced_offset) as usize
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        debug_assert!(
            self.closed || self.unflushed_bytes() == 0,
            "WalWriter dropped with {} unflushed bytes; call close()",
            self.unflushed_bytes()
        );
        // Best effort, so an unwind does not silently discard records. Logged
        // rather than swallowed: BufWriter's own Drop would ignore this failure
        // entirely, which is how buffered writes disappear without a trace.
        if let Err(e) = self.file.flush() {
            eprintln!("wal: final flush of {} failed: {e}", self.path.display());
        }
    }
}
