use std::fs::File;
use std::io::{self, BufReader, Read};
use std::iter::FusedIterator;
use std::path::Path;

use crate::error::ReadError;
use crate::format::{CRC_SIZE, FRAME_OVERHEAD, LEN_SIZE, MAX_RECORD_SIZE, checksum};

/// Why the reader latched into a failed state.
///
/// Stored instead of the [`ReadError`] itself because `io::Error` is not
/// `Clone`, and the reader has to be able to hand back the same failure on every
/// subsequent call.
#[derive(Clone, Copy)]
enum Fused {
    Checksum {
        offset: u64,
        expected: u32,
        actual: u32,
    },
    Truncated {
        offset: u64,
    },
    InvalidLength {
        offset: u64,
        len: u32,
    },
    Io,
}

impl Fused {
    fn to_error(self) -> ReadError {
        match self {
            Self::Checksum {
                offset,
                expected,
                actual,
            } => ReadError::ChecksumMismatch {
                offset,
                expected,
                actual,
            },
            Self::Truncated { offset } => ReadError::TruncatedTail { offset },
            Self::InvalidLength { offset, len } => ReadError::InvalidLength { offset, len },
            Self::Io => ReadError::Io(io::Error::other(
                "wal reader is fused after an earlier I/O error",
            )),
        }
    }

    fn of(error: &ReadError) -> Self {
        match *error {
            ReadError::ChecksumMismatch {
                offset,
                expected,
                actual,
            } => Self::Checksum {
                offset,
                expected,
                actual,
            },
            ReadError::TruncatedTail { offset } => Self::Truncated { offset },
            ReadError::InvalidLength { offset, len } => Self::InvalidLength { offset, len },
            ReadError::Io(_) | ReadError::InvalidUtf8 { .. } => Self::Io,
        }
    }
}

/// A forward iterator over the records in one WAL segment.
///
/// Streams with a `BufReader`, so memory use is constant regardless of segment
/// size — recovering a multi-gigabyte log does not need a multi-gigabyte
/// allocation at the worst possible moment.
///
/// The reader sees a *consistent prefix* of the segment as of [`open`](Self::open):
/// records still sitting in a live writer's buffer are simply absent. There is no
/// tailing and no waiting.
///
/// Iteration terminates in one of three ways:
///
/// - `None` — clean end of file, exactly on a frame boundary.
/// - `Err(`[`TruncatedTail`](ReadError::TruncatedTail)`)` — the file ends inside a
///   frame. This is what a crash mid-append looks like, and is normal.
/// - `Err(`[`ChecksumMismatch`](ReadError::ChecksumMismatch)`)` — damaged bytes.
///
/// An error is yielded **once** and then iteration ends. The reader never
/// resumes: once a checksum fails, the length prefix that would say where the
/// next frame begins is part of the data that just failed verification, so
/// "skip to the next record" would mean seeking to an offset derived from bytes
/// already proven untrustworthy.
///
/// Terminating rather than repeating the error is what keeps this a finite
/// iterator — a reader that yielded `Some(Err(..))` forever would turn
/// `.collect()` or a `for` loop without a `break` into an unbounded allocation.
/// The reason survives the end of iteration and can be re-read as many times as
/// you like via [`failure`](Self::failure).
pub struct WalReader {
    inner: BufReader<File>,
    /// Start of the next frame to be read.
    offset: u64,
    /// Start of the frame most recently returned, for error reporting.
    frame_start: u64,
    fused: Option<Fused>,
}

impl WalReader {
    /// Opens a segment for reading from the beginning.
    pub fn open(path: &Path) -> Result<Self, ReadError> {
        Ok(Self {
            inner: BufReader::new(File::open(path)?),
            offset: 0,
            frame_start: 0,
            fused: None,
        })
    }

    /// Consumes the reader, yielding records decoded as UTF-8.
    ///
    /// Consuming rather than borrowing on purpose: interleaving byte reads and
    /// string reads against one cursor is a confusing capability, so the element
    /// type is chosen once. For anything other than UTF-8, use `Iterator::map`
    /// over the byte records — a closure is less work than implementing a trait.
    pub fn strings(self) -> Strings {
        Strings(self)
    }

    /// Why iteration stopped, or `None` if the segment ended cleanly.
    ///
    /// Iteration yields a failure once and then ends, so this is how a caller
    /// distinguishes "the log ran out" from "the log broke" after the fact —
    /// including after a `for` loop or a `collect()` that discarded the reason.
    /// Safe to call any number of times.
    pub fn failure(&self) -> Option<ReadError> {
        self.fused.map(Fused::to_error)
    }

    /// Reads one frame. `Ok(None)` means a clean EOF on a frame boundary.
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, ReadError> {
        let start = self.offset;
        self.frame_start = start;

        let mut len_bytes = [0u8; LEN_SIZE];
        match read_full(&mut self.inner, &mut len_bytes)? {
            0 => return Ok(None),
            LEN_SIZE => {}
            _ => return Err(ReadError::TruncatedTail { offset: start }),
        }

        let len = u32::from_le_bytes(len_bytes);
        // Bounds-check before allocating. The checksum cannot vouch for this
        // length until `len` bytes have been read, so a corrupt prefix claiming
        // four billion bytes has to be caught here or not at all. `len == 0` is
        // never valid, which also rejects a zero-filled tail.
        if len == 0 || len as usize > MAX_RECORD_SIZE {
            return Err(ReadError::InvalidLength { offset: start, len });
        }

        let mut payload = vec![0u8; len as usize];
        if read_full(&mut self.inner, &mut payload)? != payload.len() {
            return Err(ReadError::TruncatedTail { offset: start });
        }

        let mut crc_bytes = [0u8; CRC_SIZE];
        if read_full(&mut self.inner, &mut crc_bytes)? != CRC_SIZE {
            return Err(ReadError::TruncatedTail { offset: start });
        }

        let expected = u32::from_le_bytes(crc_bytes);
        let actual = checksum(&len_bytes, &payload);
        if expected != actual {
            return Err(ReadError::ChecksumMismatch {
                offset: start,
                expected,
                actual,
            });
        }

        self.offset = start + (FRAME_OVERHEAD + payload.len()) as u64;
        Ok(Some(payload))
    }
}

impl Iterator for WalReader {
    type Item = Result<Vec<u8>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        // The error was yielded on the call that set this; the log is over.
        if self.fused.is_some() {
            return None;
        }
        match self.read_frame() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => None,
            Err(e) => {
                self.fused = Some(Fused::of(&e));
                Some(Err(e))
            }
        }
    }
}

// Once `read_frame` reports a clean EOF it keeps doing so, and a fused reader
// only ever produces `None`, so `None` is never followed by `Some`.
impl FusedIterator for WalReader {}

/// Iterator returned by [`WalReader::strings`].
pub struct Strings(WalReader);

impl Strings {
    /// See [`WalReader::failure`].
    pub fn failure(&self) -> Option<ReadError> {
        self.0.failure()
    }
}

impl Iterator for Strings {
    type Item = Result<String, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        let record = self.0.next()?;
        let offset = self.0.frame_start;
        Some(record.and_then(|bytes| {
            String::from_utf8(bytes).map_err(|source| ReadError::InvalidUtf8 { offset, source })
        }))
    }
}

impl FusedIterator for Strings {}

/// Fills `buf` and returns how many bytes were read, stopping short only at EOF.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match reader.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(read)
}
