use std::fs::File;
use std::io::{self, BufReader, Read};
use std::iter::FusedIterator;
use std::path::{Path, PathBuf};

use crate::error::ReadError;
use crate::format::{CRC_SIZE, FRAME_OVERHEAD, LEN_SIZE, MAX_RECORD_SIZE, checksum};
use crate::segment::{ScanError, last_seq, segment_name};

/// A forward iterator over the records in a WAL directory.
///
/// [`open`](Self::open) snapshots matching `{seq:08}.wal` names at open and
/// concatenates them in sequence order. Later rotations are invisible to this
/// reader. [`open_file`](Self::open_file) reads a single segment, for tests and
/// damage injection.
///
/// Streams with a `BufReader`, so memory use is constant regardless of segment
/// size — recovering a multi-gigabyte log does not need a multi-gigabyte
/// allocation at the worst possible moment.
///
/// The reader sees a *consistent prefix* of each file as of open: records still
/// sitting in a live writer's buffer are simply absent. There is no tailing and
/// no waiting.
///
/// Iteration terminates in one of three ways:
///
/// - `None` — clean end of the last snapshotted file, exactly on a frame boundary.
/// - `Err(`[`TruncatedTail`](ReadError::TruncatedTail)`)` — a file ends inside a
///   frame. This is what a crash mid-append looks like. It fuses the whole
///   reader, including when later segments exist; those files are orphans until
///   the torn file is truncated in place.
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
pub struct WalReader {
    files: Vec<PathBuf>,
    index: usize,
    inner: Option<BufReader<File>>,
    path: PathBuf,
    /// Start of the next frame to be read in the current file.
    offset: u64,
    /// Start of the frame most recently returned, for error reporting.
    frame_start: u64,
    fused: bool,
}

impl WalReader {
    /// Opens every matching segment in `dir`, concatenated in sequence order.
    ///
    /// `dir` must exist. An empty directory, or one with no matching names, is
    /// a valid empty log. Matching names must be contiguous `1..=n`.
    pub fn open(dir: &Path) -> Result<Self, ReadError> {
        let last = match last_seq(dir) {
            Ok(n) => n,
            Err(ScanError::Io(e)) => return Err(e.into()),
            Err(ScanError::Missing { seq }) => return Err(ReadError::MissingSegment { seq }),
        };
        if last == 0 {
            return Ok(Self::empty());
        }
        let files: Vec<PathBuf> = (1..=last).map(|seq| dir.join(segment_name(seq))).collect();
        Self::from_files(files)
    }

    /// Opens a single segment file. Does not validate the name against the
    /// directory scheme.
    pub fn open_file(path: &Path) -> Result<Self, ReadError> {
        Self::from_files(vec![path.to_path_buf()])
    }

    fn empty() -> Self {
        Self {
            files: Vec::new(),
            index: 0,
            inner: None,
            path: PathBuf::new(),
            offset: 0,
            frame_start: 0,
            fused: false,
        }
    }

    fn from_files(files: Vec<PathBuf>) -> Result<Self, ReadError> {
        let path = files[0].clone();
        let inner = BufReader::new(File::open(&path)?);
        Ok(Self {
            files,
            index: 0,
            inner: Some(inner),
            path,
            offset: 0,
            frame_start: 0,
            fused: false,
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

    fn open_next(&mut self) -> Result<bool, ReadError> {
        let next = self.index + 1;
        if next >= self.files.len() {
            return Ok(false);
        }
        let path = self.files[next].clone();
        let inner = BufReader::new(File::open(&path)?);
        self.inner = Some(inner);
        self.path = path;
        self.index = next;
        self.offset = 0;
        self.frame_start = 0;
        Ok(true)
    }

    /// Reads one frame. `Ok(None)` means a clean EOF on a frame boundary.
    fn read_frame(&mut self) -> Result<Option<Vec<u8>>, ReadError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        let start = self.offset;
        self.frame_start = start;
        let path = self.path.clone();

        let mut len_bytes = [0u8; LEN_SIZE];
        match read_full(inner, &mut len_bytes)? {
            0 => return Ok(None),
            LEN_SIZE => {}
            _ => {
                return Err(ReadError::TruncatedTail {
                    path: path.clone(),
                    offset: start,
                });
            }
        }

        let len = u32::from_le_bytes(len_bytes);
        // Bounds-check before allocating. The checksum cannot vouch for this
        // length until `len` bytes have been read, so a corrupt prefix claiming
        // four billion bytes has to be caught here or not at all. `len == 0` is
        // never valid, which also rejects a zero-filled tail.
        if len == 0 || len as usize > MAX_RECORD_SIZE {
            return Err(ReadError::InvalidLength {
                path: path.clone(),
                offset: start,
                len,
            });
        }

        let mut payload = vec![0u8; len as usize];
        if read_full(inner, &mut payload)? != payload.len() {
            return Err(ReadError::TruncatedTail {
                path: path.clone(),
                offset: start,
            });
        }

        let mut crc_bytes = [0u8; CRC_SIZE];
        if read_full(inner, &mut crc_bytes)? != CRC_SIZE {
            return Err(ReadError::TruncatedTail {
                path: path.clone(),
                offset: start,
            });
        }

        let expected = u32::from_le_bytes(crc_bytes);
        let actual = checksum(&len_bytes, &payload);
        if expected != actual {
            return Err(ReadError::ChecksumMismatch {
                path: path.clone(),
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
        if self.fused {
            return None;
        }
        loop {
            match self.read_frame() {
                Ok(Some(record)) => return Some(Ok(record)),
                Ok(None) => match self.open_next() {
                    Ok(true) => continue,
                    Ok(false) => return None,
                    Err(e) => {
                        self.fused = true;
                        return Some(Err(e));
                    }
                },
                Err(e) => {
                    self.fused = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// Once `read_frame` reports a clean EOF it keeps doing so, and a fused reader
// only ever produces `None`, so `None` is never followed by `Some`.
impl FusedIterator for WalReader {}

/// Iterator returned by [`WalReader::strings`].
pub struct Strings(WalReader);

impl Iterator for Strings {
    type Item = Result<String, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        let record = self.0.next()?;
        let offset = self.0.frame_start;
        let path = self.0.path.clone();
        Some(record.and_then(|bytes| {
            String::from_utf8(bytes).map_err(|source| ReadError::InvalidUtf8 {
                path,
                offset,
                source,
            })
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
