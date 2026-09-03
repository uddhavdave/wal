//! A write-ahead log: append length-prefixed, checksummed records to a segment
//! file, and iterate them back.
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
//! let mut wal = WalWriter::new(std::path::Path::new("/var/log/wal"), "0001.wal")?;
//! wal.push(b"first")?;
//! wal.push(b"second")?;
//!
//! // Durability is the caller's call — sync every record, every N bytes, or
//! // once at the end.
//! if wal.unsynced_bytes() + wal.unflushed_bytes() > 4 * 1024 * 1024 {
//!     wal.sync()?;
//! }
//!
//! let path = wal.path().to_path_buf();
//! wal.close()?;
//!
//! for record in WalReader::open(&path)? {
//!     println!("{:?}", record?);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Durability
//!
//! [`WalWriter::push`] performs no I/O beyond a buffer copy. Two barriers turn
//! pushed records into stored ones, and they form a hierarchy:
//! [`flush`](WalWriter::flush) makes records *visible* to a reader,
//! [`sync`](WalWriter::sync) makes them *durable*, and sync implies flush.
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
//! means damage. It stops at the first sign of either. A log that reads as a
//! short prefix is recoverable; one that resumes past a hole is a lie.

mod error;
mod format;
mod reader;
mod writer;

pub use error::{ReadError, WriteError};
pub use format::MAX_RECORD_SIZE;
pub use reader::{Strings, WalReader};
pub use writer::WalWriter;
