use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::WriteError;
use crate::format::{DEFAULT_CAPACITY, FRAME_OVERHEAD, MAX_RECORD_SIZE, checksum};
use crate::segment::{MAX_SEGMENT_SEQ, ScanError, last_seq, segment_name};

/// Largest a live segment may grow, in bytes. Tuning, not format: a reader
/// concatenates whatever files are there, so this can change without breaking
/// existing logs. Kept crate-private so it is not a constructor knob.
pub(crate) const MAX_SEGMENT_SIZE: u64 = 128 * 1024 * 1024;

const _: () = assert!(MAX_SEGMENT_SIZE >= MAX_RECORD_SIZE as u64 + FRAME_OVERHEAD as u64);

/// An append handle for a WAL directory.
///
/// The handle owns the directory exclusively: it names files `{seq:08}.wal`
/// starting at `00000001.wal`, never resumes an existing file, and rotates to
/// the next sequence when a record would push the live file past
/// [`MAX_SEGMENT_SIZE`]. Durability of the *live* file is the caller's
/// decision: [`push`](Self::push) never touches the disk, and the caller drives
/// [`flush`](Self::flush) and [`sync`](Self::sync) using the two watermarks
/// below. Rotation itself is a durability barrier for the file it seals.
///
/// There are two barriers, and they form a hierarchy:
///
/// - [`flush`](Self::flush) moves bytes from this handle's userspace buffer into
///   the kernel page cache. After it, a [`WalReader`](crate::WalReader) opened on
///   the directory can see them. They are *not* yet durable.
/// - [`sync`](Self::sync) flushes and then forces the kernel's copy to disk.
///   Durable implies visible.
///
/// `&mut self` throughout: a WAL is a serialised append stream, and the ordering
/// *is* the data structure. The borrow checker enforces one-writer-at-a-time at
/// zero runtime cost. A caller who needs to share the handle across threads can
/// wrap it in a `Mutex` with lock granularity they control.
pub struct WalWriter {
    dir: PathBuf,
    seq: u64,
    file: BufWriter<File>,
    path: PathBuf,
    capacity: usize,
    /// Logical end of the live segment, counting bytes still sitting in the buffer.
    offset: u64,
    /// Logical offset of the live segment known to be on disk.
    synced_offset: u64,
    closed: bool,
}

impl WalWriter {
    /// Creates the next segment in `dir` and opens it for append.
    ///
    /// `dir` must already exist; this does not create it. An empty directory is
    /// a valid empty log and yields `00000001.wal`. Matching `{seq:08}.wal`
    /// names must be contiguous `1..=n`; a gap is [`WriteError::MissingSegment`].
    /// Never appends to an existing file: the live segment is always `n+1`.
    ///
    /// Fails with [`WriteError::AlreadyExists`] if two writers race to create
    /// the same next name.
    pub fn new(dir: &Path) -> Result<Self, WriteError> {
        Self::with_capacity(dir, DEFAULT_CAPACITY)
    }

    /// Like [`new`](Self::new), with an explicit userspace buffer size.
    ///
    /// The buffer is pure runtime tuning — no reader can observe it — so unlike
    /// [`MAX_RECORD_SIZE`] it is safe to vary per writer.
    pub fn with_capacity(dir: &Path, capacity: usize) -> Result<Self, WriteError> {
        let last = match last_seq(dir) {
            Ok(n) => n,
            Err(ScanError::Io(e)) => return Err(e.into()),
            Err(ScanError::Missing { seq }) => return Err(WriteError::MissingSegment { seq }),
        };
        let seq = last + 1;
        let (file, path) = create_segment(dir, seq, capacity)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            seq,
            file,
            path,
            capacity,
            offset: 0,
            synced_offset: 0,
            closed: false,
        })
    }

    /// Appends one record and returns the byte offset at which its frame begins
    /// in the live segment.
    ///
    /// The offset is *logical*: it only becomes a valid seek target once
    /// [`flush`](Self::flush) has run. Until then it names bytes that exist
    /// nowhere but this process. After a rotation the first record of the new
    /// file returns `0`; [`path`](Self::path) names the file that record landed
    /// in.
    pub fn push(&mut self, record: &[u8]) -> Result<u64, WriteError> {
        if record.is_empty() {
            return Err(WriteError::EmptyRecord);
        }
        if record.len() > MAX_RECORD_SIZE {
            return Err(WriteError::RecordTooLarge { len: record.len() });
        }

        let frame_len = (FRAME_OVERHEAD + record.len()) as u64;
        if self.offset + frame_len > MAX_SEGMENT_SIZE {
            if self.offset == 0 {
                // Unreachable while MAX_SEGMENT_SIZE >= MAX_RECORD_SIZE + overhead.
                return Err(WriteError::RecordTooLarge { len: record.len() });
            }
            self.rotate()?;
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
        self.offset += frame_len;
        Ok(start)
    }

    fn rotate(&mut self) -> Result<(), WriteError> {
        self.file.flush()?;
        self.file.get_ref().sync_data()?;

        let next = self.seq + 1;
        let (file, path) = create_segment(&self.dir, next, self.capacity)?;
        self.file = file;
        self.path = path;
        self.seq = next;
        self.offset = 0;
        self.synced_offset = 0;
        Ok(())
    }

    /// Moves buffered bytes into the kernel, making them visible to readers.
    ///
    /// Cheap relative to [`sync`](Self::sync) — one `write(2)`, or no syscall at
    /// all if the buffer is empty — but it grants visibility, not durability.
    pub fn flush(&mut self) -> Result<(), WriteError> {
        self.file.flush()?;
        Ok(())
    }

    /// Flushes, then forces the kernel's copy of the live segment to disk.
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

    /// Flushes, syncs, and closes the live segment.
    ///
    /// The only way to *observe* a failing final flush. [`Drop`] tries too, but
    /// it has nowhere to return an error to. Previously sealed files were
    /// already synced at rotation.
    pub fn close(mut self) -> Result<(), WriteError> {
        self.sync()?;
        self.closed = true;
        Ok(())
    }

    /// Path of the live segment. After a rotating [`push`](Self::push) this is
    /// the file the record just written landed in. Hand to
    /// [`WalReader::open_file`](crate::WalReader::open_file) for a single-file
    /// view; [`WalReader::open`](crate::WalReader::open) takes the directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written but not yet in the kernel. Resets on [`flush`](Self::flush).
    ///
    /// Read from the buffer itself rather than tracked by hand, so it stays
    /// correct when `BufWriter` flushes on its own after filling up. Live
    /// segment only: sealed files were flushed at rotation.
    pub fn unflushed_bytes(&self) -> usize {
        self.file.buffer().len()
    }

    /// Bytes in the kernel but not yet on disk. Resets on [`sync`](Self::sync).
    ///
    /// Live segment only: sealed files were synced at rotation.
    pub fn unsynced_bytes(&self) -> usize {
        let in_kernel = self.offset - self.unflushed_bytes() as u64;
        in_kernel.saturating_sub(self.synced_offset) as usize
    }
}

fn create_segment(
    dir: &Path,
    seq: u64,
    capacity: usize,
) -> Result<(BufWriter<File>, PathBuf), WriteError> {
    if seq > MAX_SEGMENT_SEQ {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "wal segment sequence overflow").into(),
        );
    }
    let path = dir.join(segment_name(seq));
    let file = File::options()
        .append(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => WriteError::AlreadyExists { path: path.clone() },
            _ => WriteError::Io(e),
        })?;

    // Sync the *directory*, not the file. Until the directory entry is
    // durable the segment's name can vanish in a crash, taking every record
    // with it — no amount of syncing the file's contents would help.
    File::open(dir)?.sync_all()?;

    Ok((BufWriter::with_capacity(capacity, file), path))
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
