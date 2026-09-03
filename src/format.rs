//! On-disk frame format shared by the writer and the reader.
//!
//! ```text
//! Frame := [ len: u32 LE ][ payload: len bytes ][ crc: u32 LE ]
//! crc    = crc32_ieee( len_le_bytes ++ payload )
//! ```
//!
//! The checksum deliberately covers the length prefix as well as the payload. A
//! corrupted length that was *not* checksummed would be undetectable: the reader
//! would consume the wrong number of bytes and desynchronise from every frame
//! that follows, turning one bad bit into a destroyed log.

/// Largest record the log will accept, in bytes.
///
/// This is a property of the *format*, not a tuning knob. A writer that emitted
/// larger records than a reader accepted would make a configuration mismatch
/// indistinguishable from corruption.
pub const MAX_RECORD_SIZE: usize = 64 * 1024 * 1024;

pub(crate) const LEN_SIZE: usize = 4;
pub(crate) const CRC_SIZE: usize = 4;
pub(crate) const FRAME_OVERHEAD: usize = LEN_SIZE + CRC_SIZE;

/// Default size of the writer's userspace buffer.
pub(crate) const DEFAULT_CAPACITY: usize = 8 * 1024;

/// Checksum of a frame, over the length prefix followed by the payload.
pub(crate) fn checksum(len_bytes: &[u8; LEN_SIZE], payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(len_bytes);
    hasher.update(payload);
    hasher.finalize()
}
