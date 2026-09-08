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
    let mut wal = WalWriter::new(dir.path()).unwrap();
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
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
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
fn read_file(path: &Path) -> Vec<Result<Vec<u8>, ReadError>> {
    WalReader::open_file(path).unwrap().take(64).collect()
}

fn read_dir(dir: &Path) -> Vec<Result<Vec<u8>, ReadError>> {
    WalReader::open(dir).unwrap().take(64).collect()
}

// --- happy path -------------------------------------------------------------

#[test]
fn round_trips_records_in_order() {
    let dir = TmpDir::new("roundtrip");
    let path = segment(&dir, &[b"alpha", b"beta", b"gamma"]);

    let records: Vec<_> = WalReader::open_file(&path)
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(
        records,
        vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
    );
}

#[test]
fn push_returns_frame_start_offsets() {
    let dir = TmpDir::new("offsets");
    let mut wal = WalWriter::new(dir.path()).unwrap();

    assert_eq!(wal.push(b"hello").unwrap(), 0);
    assert_eq!(wal.push(b"world").unwrap(), FRAME);
    assert_eq!(wal.push(b"!").unwrap(), FRAME * 2);
    assert!(wal.path().ends_with("00000001.wal"));

    wal.close().unwrap();
}

#[test]
fn strings_decodes_utf8() {
    let dir = TmpDir::new("strings");
    let path = segment(&dir, &[b"one", b"two"]);

    let records: Vec<String> = WalReader::open_file(&path)
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

    assert!(read_file(&path).is_empty());
}

#[test]
fn empty_directory_is_a_valid_empty_log() {
    let dir = TmpDir::new("empty-dir");
    assert!(read_dir(dir.path()).is_empty());
}

#[test]
fn missing_directory_is_an_error() {
    let dir = TmpDir::new("missing-dir");
    let gone = dir.path().join("nope");
    assert!(matches!(WalWriter::new(&gone), Err(WriteError::Io(_))));
    assert!(matches!(WalReader::open(&gone), Err(ReadError::Io(_))));
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

    let results = read_file(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13, .. })
    ));
}

#[test]
fn truncation_inside_payload_is_a_truncated_tail() {
    let dir = TmpDir::new("cut-payload");
    let path = two_records(&dir);
    truncate_to(&path, FRAME + 4 + 2);

    let results = read_file(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13, .. })
    ));
}

#[test]
fn truncation_inside_checksum_is_a_truncated_tail() {
    let dir = TmpDir::new("cut-crc");
    let path = two_records(&dir);
    truncate_to(&path, FRAME + 4 + 5 + 2);

    let results = read_file(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13, .. })
    ));
}

// --- corruption -------------------------------------------------------------

#[test]
fn corrupt_payload_is_a_checksum_mismatch() {
    let dir = TmpDir::new("corrupt-payload");
    let path = two_records(&dir);
    flip_bit(&path, FRAME + 4); // first byte of the second record's payload

    let results = read_file(&path);
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

    let results = read_file(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::ChecksumMismatch { offset: 13, .. })
            | Err(ReadError::TruncatedTail { offset: 13, .. })
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

    let mut reader = WalReader::open_file(&path).unwrap();
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

    let results = read_file(&path);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::InvalidLength {
            offset: 13,
            len: 0,
            ..
        })
    ));
}

#[test]
fn damage_errors_name_the_torn_file() {
    let dir = TmpDir::new("error-path");
    let path = two_records(&dir);
    truncate_to(&path, FRAME + 2);

    match &read_file(&path)[1] {
        Err(ReadError::TruncatedTail {
            path: err_path,
            offset: 13,
        }) => {
            assert_eq!(err_path, &path);
        }
        other => panic!("expected TruncatedTail, got {other:?}"),
    }
}

// --- write-side rejections --------------------------------------------------

#[test]
fn empty_records_are_rejected() {
    let dir = TmpDir::new("reject-empty");
    let mut wal = WalWriter::new(dir.path()).unwrap();

    assert!(matches!(wal.push(b""), Err(WriteError::EmptyRecord)));

    wal.close().unwrap();
}

#[test]
fn oversized_records_are_rejected() {
    let dir = TmpDir::new("reject-large");
    let mut wal = WalWriter::new(dir.path()).unwrap();

    let too_big = vec![0u8; MAX_RECORD_SIZE + 1];
    assert!(matches!(
        wal.push(&too_big),
        Err(WriteError::RecordTooLarge { .. })
    ));

    wal.close().unwrap();
}

// --- visibility -------------------------------------------------------------

#[test]
fn records_are_invisible_until_flushed() {
    let dir = TmpDir::new("visibility");
    let mut wal = WalWriter::new(dir.path()).unwrap();
    wal.push(b"hello").unwrap();
    let path = wal.path().to_path_buf();

    assert_eq!(wal.unflushed_bytes(), FRAME as usize);
    assert_eq!(wal.unsynced_bytes(), 0);
    assert!(
        read_file(&path).is_empty(),
        "unflushed records must not be visible"
    );

    wal.flush().unwrap();
    assert_eq!(wal.unflushed_bytes(), 0);
    assert_eq!(wal.unsynced_bytes(), FRAME as usize);

    let results = read_file(&path);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].as_ref().unwrap(), b"hello");

    wal.close().unwrap();
}

#[test]
fn sync_flushes_first_and_clears_both_watermarks() {
    let dir = TmpDir::new("sync");
    let mut wal = WalWriter::new(dir.path()).unwrap();
    wal.push(b"hello").unwrap();
    assert_eq!(wal.unflushed_bytes(), FRAME as usize);

    // sync never reports success over data still sitting in userspace.
    wal.sync().unwrap();
    assert_eq!(wal.unflushed_bytes(), 0);
    assert_eq!(wal.unsynced_bytes(), 0);

    let path = wal.path().to_path_buf();
    wal.close().unwrap();
    assert_eq!(read_file(&path).len(), 1);
}

// --- directory identity -----------------------------------------------------

#[test]
fn restart_creates_the_next_segment() {
    let dir = TmpDir::new("restart");
    let mut first = WalWriter::new(dir.path()).unwrap();
    first.push(b"one").unwrap();
    assert!(first.path().ends_with("00000001.wal"));
    first.close().unwrap();

    let mut second = WalWriter::new(dir.path()).unwrap();
    assert_eq!(second.push(b"two").unwrap(), 0);
    assert!(second.path().ends_with("00000002.wal"));
    second.close().unwrap();

    let records: Vec<_> = read_dir(dir.path())
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(records, vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn reader_concatenates_segments_in_seq_order() {
    let dir = TmpDir::new("concat");
    segment(&dir, &[b"a", b"b"]);
    let mut wal = WalWriter::new(dir.path()).unwrap();
    wal.push(b"c").unwrap();
    wal.close().unwrap();

    let records: Vec<_> = read_dir(dir.path())
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(records, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
}

#[test]
fn foreign_dirents_are_ignored() {
    let dir = TmpDir::new("foreign");
    fs::write(dir.path().join(".DS_Store"), b"noise").unwrap();
    fs::write(dir.path().join("test.wal"), b"nope").unwrap();
    segment(&dir, &[b"hello"]);

    let records: Vec<_> = read_dir(dir.path())
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(records, vec![b"hello".to_vec()]);
}

#[test]
fn planted_high_seq_is_a_missing_first_segment() {
    let dir = TmpDir::new("planted");
    fs::write(dir.path().join("00000099.wal"), b"").unwrap();

    assert!(matches!(
        WalWriter::new(dir.path()),
        Err(WriteError::MissingSegment { seq: 1 })
    ));
    assert!(matches!(
        WalReader::open(dir.path()),
        Err(ReadError::MissingSegment { seq: 1 })
    ));
}

#[test]
fn gap_in_the_middle_is_a_missing_segment() {
    let dir = TmpDir::new("gap");
    segment(&dir, &[b"one"]);
    let mut wal = WalWriter::new(dir.path()).unwrap();
    wal.push(b"two").unwrap();
    wal.close().unwrap();
    fs::remove_file(dir.path().join("00000002.wal")).unwrap();
    fs::write(dir.path().join("00000003.wal"), b"").unwrap();

    assert!(matches!(
        WalWriter::new(dir.path()),
        Err(WriteError::MissingSegment { seq: 2 })
    ));
    assert!(matches!(
        WalReader::open(dir.path()),
        Err(ReadError::MissingSegment { seq: 2 })
    ));
}

#[test]
fn torn_non_last_segment_fuses_the_reader() {
    let dir = TmpDir::new("orphan");
    let first = segment(&dir, &[b"hello", b"world"]);
    truncate_to(&first, FRAME + 2);

    let mut wal = WalWriter::new(dir.path()).unwrap();
    wal.push(b"orphan").unwrap();
    wal.close().unwrap();

    let results = read_dir(dir.path());
    assert_eq!(results[0].as_ref().unwrap(), b"hello");
    assert!(matches!(
        results[1],
        Err(ReadError::TruncatedTail { offset: 13, .. })
    ));
    assert_eq!(results.len(), 2, "later segments must stay unreachable");
}

#[test]
fn rotating_push_resets_offset_and_changes_path() {
    let dir = TmpDir::new("rotate");
    let mut wal = WalWriter::new(dir.path()).unwrap();
    let first = wal.path().to_path_buf();
    let payload = vec![b'x'; 1024 * 1024];
    let mut rotated = false;
    for _ in 0..200 {
        let offset = wal.push(&payload).unwrap();
        if wal.path() != first.as_path() {
            assert_eq!(offset, 0);
            assert!(wal.path().ends_with("00000002.wal"));
            rotated = true;
            break;
        }
    }
    assert!(rotated, "writer never rotated");
    assert!(
        !read_file(&first).is_empty(),
        "rotation must flush the sealed file"
    );
    wal.close().unwrap();

    let records = read_dir(dir.path());
    assert!(records.iter().all(|r| r.as_ref().unwrap() == &payload));
    assert!(records.len() >= 2);
}
