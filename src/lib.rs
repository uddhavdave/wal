//! A write-ahead log: append length-prefixed, checksummed records to an
//! exclusive directory of segment files, and iterate them back.
//!
//! ```text
//! Frame := [ len: u32 LE ][ payload: len bytes ][ crc: u32 LE ]
//! crc    = crc32_ieee( len_le_bytes ++ payload )
//! ```
//!
//! ```no_run
//! use wal_writer::{WalReader, WalWriter};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = std::path::Path::new("/var/log/wal");
//! let mut wal = WalWriter::new(dir)?;
//! wal.push(b"first")?;
//! wal.push(b"second")?;
//!
//! // Durability of the live file is the caller's call — sync every record,
//! // every N bytes, or once at the end. Rotation syncs the file it seals.
//! if wal.unsynced_bytes() + wal.unflushed_bytes() > 4 * 1024 * 1024 {
//!     wal.sync()?;
//! }
//!
//! wal.close()?;
//!
//! for record in WalReader::open(dir)? {
//!     println!("{:?}", record?);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Durability
//!
//! [`WalWriter::push`] performs no I/O beyond a buffer copy, unless the record
//! would exceed the live segment and rotation has to seal the previous file.
//! Two barriers turn pushed records into stored ones, and they form a hierarchy:
//! [`flush`](WalWriter::flush) makes records *visible* to a reader,
//! [`sync`](WalWriter::sync) makes them *durable*, and sync implies flush.
//! Rotation flushes and syncs the sealed file before creating the next one.
//!
//! # Recovery
//!
//! A record on disk can be a prefix of the record that was written: buffers
//! split frames, `write(2)` returns short, page-cache writeback happens per
//! page, and hardware guarantees atomicity only per sector. That is why frames
//! carry a checksum rather than a length alone.
//!
//! [`WalReader`] therefore distinguishes a clean end of file (`None`) from a
//! [`TruncatedTail`](ReadError::TruncatedTail) — the ordinary result of a crash
//! mid-append — from a [`ChecksumMismatch`](ReadError::ChecksumMismatch), which
//! means damage. It stops at the first sign of either, including a torn tail on
//! a non-last segment: later files are orphans until the torn file is truncated
//! in place. A log that reads as a short prefix is recoverable; one that resumes
//! past a hole is a lie.

mod error;
mod format;
mod reader;
mod segment;
mod writer;

pub use error::{ReadError, WriteError};
pub use format::MAX_RECORD_SIZE;
pub use reader::{Strings, WalReader};
pub use writer::WalWriter;
