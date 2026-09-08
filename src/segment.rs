use std::fs;
use std::io;
use std::path::Path;

/// Highest sequence that still fits `{seq:08}.wal`.
pub(crate) const MAX_SEGMENT_SEQ: u64 = 99_999_999;

pub(crate) enum ScanError {
    Io(io::Error),
    Missing { seq: u64 },
}

impl From<io::Error> for ScanError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub(crate) fn segment_name(seq: u64) -> String {
    format!("{seq:08}.wal")
}

pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".wal")?;
    if stem.len() != 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// Last sequence in a contiguous `1..=n` match set, or `0` if none match.
///
/// Non-matching dirents are ignored. A match set that does not start at 1 or
/// has a hole is [`ScanError::Missing`].
pub(crate) fn last_seq(dir: &Path) -> Result<u64, ScanError> {
    let mut seqs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(seq) = parse_segment_name(name) {
            seqs.push(seq);
        }
    }
    seqs.sort_unstable();
    seqs.dedup();
    if seqs.is_empty() {
        return Ok(0);
    }
    for (i, seq) in seqs.iter().enumerate() {
        let expected = i as u64 + 1;
        if *seq != expected {
            return Err(ScanError::Missing { seq: expected });
        }
    }
    Ok(*seqs.last().unwrap())
}
