//! The correctness of this design lives almost entirely in failure paths, so
//! most of what follows constructs damage deliberately: torn writes via
//! `set_len`, corruption via a seek-and-XOR.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use wal_writer::{MAX_RECORD_SIZE, ReadError, WalReader, WalWriter, WriteError};

/// Frame overhead: 4-byte length prefix plus 4-byte checksum.
const OVERHEAD: u64 = 8;
/// Both fixture records are five bytes, so every frame is thirteen.
const FRAME: u64 = OVERHEAD + 5;

// --- fixtures ---------------------------------------------------------------

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("wal-test-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `records` to a fresh segment and returns its path.
fn segment(dir: &TmpDir, records: &[&[u8]]) -> PathBuf {
    let mut wal = WalWriter::new(dir.path(), "test.wal").unwrap();
    for record in records {
        wal.push(record).unwrap();
    }
    let path = wal.path().to_path_buf();
    wal.close().unwrap();
    path
}

/// A segment holding `b"hello"` then `b"world"` — two thirteen-byte frames.
fn two_records(dir: &TmpDir) -> PathBuf {
    segment(dir, &[b"hello", b"world"])
}

fn truncate_to(path: &Path, len: u64) {
    File::options().write(true).open(path).unwrap().set_len(len).unwrap();
}

fn flip_bit(path: &Path, at: u64) {
    let mut file = File::options().read(true).write(true).open(path).unwrap();
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(at)).unwrap();
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x01;
    file.seek(SeekFrom::Start(at)).unwrap();
    file.write_all(&byte).unwrap();
}

fn append_zeros(path: &Path, n: usize) {
    File::options()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(&vec![0u8; n])
        .unwrap();
}

/// Bounded on purpose. If the reader ever regresses to yielding an error
/// forever, an unbounded `collect()` here would exhaust the machine's memory
/// rather than fail a test. `iteration_ends_after_corruption` is what actually
/// pins the termination guarantee.
fn read_all(path: &Path) -> Vec<Result<Vec<u8>, ReadError>> {
    WalReader::open(path).unwrap().take(64).collect()
}

// --- happy path -------------------------------------------------------------

#[test]
fn round_trips_records_in_order() {
    let dir = TmpDir::new("roundtrip");
    let path = segment(&dir, &[b"alpha", b"beta", b"gamma"]);

    let records: Vec<_> = WalReader::open(&path)
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(records, vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]);
}

#[test]
fn push_returns_frame_start_offsets() {
    let dir = TmpDir::new("offsets");
    let mut wal = WalWriter::new(dir.path(), "test.wal").unwrap();

    assert_eq!(wal.push(b"hello").unwrap(), 0);
    assert_eq!(wal.push(b"world").unwrap(), FRAME);
    assert_eq!(wal.push(b"!").unwrap(), FRAME * 2);

    wal.close().unwrap();
}

#[test]
fn strings_decodes_utf8() {
    let dir = TmpDir::new("strings");
    let path = segment(&dir, &[b"one", b"two"]);

    let records: Vec<String> = WalReader::open(&path)
        .unwrap()
        .strings()
        .map(Result::unwrap)
        .collect();

    assert_eq!(records, vec!["one", "two"]);
}

#[test]
fn empty_segment_yields_nothing() {
    let dir = TmpDir::new("empty");
    let path = segment(&dir, &[]);

    assert!(read_all(&path).is_empty());
}

// --- torn writes ------------------------------------------------------------
//
// A crash mid-append can leave the file ending anywhere inside a frame. Each cut
// below lands in a different field, and each is a distinct code path in the
// reader.

#[test]
fn truncation_inside_length_prefix_is_a_truncated_tail() {
    let dir = TmpDir::new("cut-len");
    let path = two_records(&dir);
    truncate_to(&path, FRAME + 2);

    let results = read_all(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13 })
    ));
}

#[test]
fn truncation_inside_payload_is_a_truncated_tail() {
    let dir = TmpDir::new("cut-payload");
    let path = two_records(&dir);
    truncate_to(&path, FRAME + 4 + 2);

    let results = read_all(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13 })
    ));
}

#[test]
fn truncation_inside_checksum_is_a_truncated_tail() {
    let dir = TmpDir::new("cut-crc");
    let path = two_records(&dir);
    truncate_to(&path, FRAME + 4 + 5 + 2);

    let results = read_all(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13 })
    ));
}

// --- corruption -------------------------------------------------------------

#[test]
fn corrupt_payload_is_a_checksum_mismatch() {
    let dir = TmpDir::new("corrupt-payload");
    let path = two_records(&dir);
    flip_bit(&path, FRAME + 4); // first byte of the second record's payload

    let results = read_all(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::ChecksumMismatch { offset: 13, .. })
    ));
}

#[test]
fn corrupt_length_prefix_is_caught_by_the_checksum() {
    // The reason the checksum covers the length prefix. Were it payload-only, a
    // flipped length would be undetectable: the reader would consume the wrong
    // number of bytes and desynchronise from every frame after it.
    let dir = TmpDir::new("corrupt-len");
    let path = two_records(&dir);
    flip_bit(&path, FRAME); // low byte of the second record's length

    let results = read_all(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::ChecksumMismatch { offset: 13, .. })
            | Err(ReadError::TruncatedTail { offset: 13 })
    ));
}

#[test]
fn iteration_ends_after_corruption() {
    // The error is yielded once and iteration stops. A reader that kept
    // returning the error would be an infinite iterator, and `collect()` on it
    // would allocate until the process died.
    let dir = TmpDir::new("fused");
    let path = two_records(&dir);
    flip_bit(&path, FRAME + 4);

    let mut reader = WalReader::open(&path).unwrap();
    assert_eq!(reader.next().unwrap().unwrap(), b"hello");
    assert!(matches!(
        reader.next(),
        Some(Err(ReadError::ChecksumMismatch { offset: 13, .. }))
    ));

    // The length prefix that would say where the next frame begins failed the
    // same checksum, so there is nothing to resync to. The log is over.
    for _ in 0..3 {
        assert!(reader.next().is_none());
    }

    // ...but the reason outlives iteration.
    assert!(matches!(
        reader.failure(),
        Some(ReadError::ChecksumMismatch { offset: 13, .. })
    ));
}

#[test]
fn failure_distinguishes_clean_eof_from_damage() {
    let dir = TmpDir::new("failure");

    let clean = segment(&dir, &[b"hello"]);
    let mut reader = WalReader::open(&clean).unwrap();
    while reader.next().is_some() {}
    assert!(reader.failure().is_none(), "clean EOF is not a failure");

    let torn = TmpDir::new("failure-torn");
    let path = two_records(&torn);
    truncate_to(&path, FRAME + 2);
    let mut reader = WalReader::open(&path).unwrap();
    while reader.next().is_some() {}
    assert!(matches!(
        reader.failure(),
        Some(ReadError::TruncatedTail { offset: 13 })
    ));
}

#[test]
fn zero_filled_tail_is_rejected() {
    // Some filesystems pad a torn write with zeros rather than partial data.
    // crc32 of the empty input is exactly 0x00000000, so under a payload-only
    // checksum this region would have read back as an endless run of valid
    // empty records. Rejecting `len == 0` closes that off independently.
    let dir = TmpDir::new("zero-tail");
    let path = segment(&dir, &[b"hello"]);
    append_zeros(&path, 64);

    let results = read_all(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::InvalidLength { offset: 13, len: 0 })
    ));
}

// --- write-side rejections --------------------------------------------------

#[test]
fn empty_records_are_rejected() {
    let dir = TmpDir::new("reject-empty");
    let mut wal = WalWriter::new(dir.path(), "test.wal").unwrap();

    assert!(matches!(wal.push(b""), Err(WriteError::EmptyRecord)));

    wal.close().unwrap();
}

#[test]
fn oversized_records_are_rejected() {
    let dir = TmpDir::new("reject-large");
    let mut wal = WalWriter::new(dir.path(), "test.wal").unwrap();

    let too_big = vec![0u8; MAX_RECORD_SIZE + 1];
    assert!(matches!(
        wal.push(&too_big),
        Err(WriteError::RecordTooLarge { .. })
    ));

    wal.close().unwrap();
}

#[test]
fn creating_an_existing_segment_fails() {
    let dir = TmpDir::new("collision");
    let first = WalWriter::new(dir.path(), "test.wal").unwrap();

    // Two writers appending to one segment produce a log that cannot be read
    // back, so this is refused rather than resumed.
    assert!(matches!(
        WalWriter::new(dir.path(), "test.wal"),
        Err(WriteError::AlreadyExists { .. })
    ));

    first.close().unwrap();
}

// --- visibility -------------------------------------------------------------

#[test]
fn records_are_invisible_until_flushed() {
    let dir = TmpDir::new("visibility");
    let mut wal = WalWriter::new(dir.path(), "test.wal").unwrap();
    wal.push(b"hello").unwrap();
    let path = wal.path().to_path_buf();

    assert_eq!(wal.unflushed_bytes(), FRAME as usize);
    assert_eq!(wal.unsynced_bytes(), 0);
    assert!(read_all(&path).is_empty(), "unflushed records must not be visible");

    wal.flush().unwrap();
    assert_eq!(wal.unflushed_bytes(), 0);
    assert_eq!(wal.unsynced_bytes(), FRAME as usize);

    let results = read_all(&path);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");

    wal.close().unwrap();
}

#[test]
fn sync_flushes_first_and_clears_both_watermarks() {
    let dir = TmpDir::new("sync");
    let mut wal = WalWriter::new(dir.path(), "test.wal").unwrap();
    wal.push(b"hello").unwrap();
    assert_eq!(wal.unflushed_bytes(), FRAME as usize);

    // sync never reports success over data still sitting in userspace.
    wal.sync().unwrap();
    assert_eq!(wal.unflushed_bytes(), 0);
    assert_eq!(wal.unsynced_bytes(), 0);

    let path = wal.path().to_path_buf();
    wal.close().unwrap();
    assert_eq!(read_all(&path).len(), 1);
}
