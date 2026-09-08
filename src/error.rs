use std::fmt;
use std::io;
use std::path::PathBuf;
use std::string::FromUtf8Error;

/// Failures that can occur while appending to a segment.
///
/// Kept separate from [`ReadError`] because the two domains are disjoint: a
/// checksum can only fail on the way in from disk, and a record can only be
/// rejected for size on the way out to it. One combined enum would force every
/// caller to match arms that are structurally unreachable.
#[derive(Debug)]
pub enum WriteError {
    Io(io::Error),
    /// A segment with this name already exists. Two writers racing to create
    /// the same next file is a corrupt log, so this is refused rather than
    /// resumed.
    AlreadyExists {
        path: PathBuf,
    },
    /// Matching `{seq:08}.wal` names in the directory are not contiguous `1..=n`.
    MissingSegment {
        seq: u64,
    },
    /// Zero-length records carry no information, and forbidding them means
    /// `len == 0` is *always* an invalid frame — a cheap second signal that the
    /// reader is looking at padding rather than data.
    EmptyRecord,
    RecordTooLarge {
        len: usize,
    },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "wal write: {e}"),
            Self::AlreadyExists { path } => {
                write!(f, "wal segment already exists: {}", path.display())
            }
            Self::MissingSegment { seq } => {
                write!(f, "wal write: missing segment {seq:08}")
            }
            Self::EmptyRecord => write!(f, "wal write: empty records are not allowed"),
            Self::RecordTooLarge { len } => write!(
                f,
                "wal write: record of {len} bytes exceeds the {} byte maximum",
                crate::format::MAX_RECORD_SIZE
            ),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Failures that can occur while iterating a log.
///
/// Every variant that indicates damage carries the segment `path` and the byte
/// `offset` of the frame it was found at; diagnosing a corrupt log without a
/// location is close to impossible. Offset is per-file.
#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    /// Matching `{seq:08}.wal` names in the directory are not contiguous `1..=n`.
    MissingSegment {
        seq: u64,
    },
    /// The bytes at `offset` in `path` did not match their checksum. Something rotted.
    ChecksumMismatch {
        path: PathBuf,
        offset: u64,
        expected: u32,
        actual: u32,
    },
    /// The file ends part-way through the frame at `offset`.
    ///
    /// This is *normal* — it is what a crash mid-append looks like on disk — and
    /// is deliberately distinct from [`ReadError::ChecksumMismatch`] so a caller
    /// can tell "we crashed here" from "these bytes are damaged".
    TruncatedTail {
        path: PathBuf,
        offset: u64,
    },
    /// The length prefix at `offset` is not a value the writer could have
    /// produced. Checked *before* allocating, since the checksum cannot vouch
    /// for a length until `len` bytes have already been read.
    InvalidLength {
        path: PathBuf,
        offset: u64,
        len: u32,
    },
    /// Only produced by [`crate::WalReader::strings`].
    InvalidUtf8 {
        path: PathBuf,
        offset: u64,
        source: FromUtf8Error,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "wal read: {e}"),
            Self::MissingSegment { seq } => {
                write!(f, "wal read: missing segment {seq:08}")
            }
            Self::ChecksumMismatch {
                path,
                offset,
                expected,
                actual,
            } => write!(
                f,
                "wal read: checksum mismatch in {} at offset {offset} \
                 (stored {expected:#010x}, computed {actual:#010x})",
                path.display()
            ),
            Self::TruncatedTail { path, offset } => {
                write!(
                    f,
                    "wal read: truncated frame in {} at offset {offset}",
                    path.display()
                )
            }
            Self::InvalidLength { path, offset, len } => {
                write!(
                    f,
                    "wal read: invalid record length {len} in {} at offset {offset}",
                    path.display()
                )
            }
            Self::InvalidUtf8 {
                path,
                offset,
                source,
            } => {
                write!(
                    f,
                    "wal read: record in {} at offset {offset} is not utf-8: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::InvalidUtf8 { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
