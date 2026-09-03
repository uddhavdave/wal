use std::path::Path;

use wal_writer::{ReadError, WalReader, WalWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("wal-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    let mut wal = WalWriter::new(&dir, "demo.wal")?;
    for i in 0..5 {
        let offset = wal.push(format!("record {i}").as_bytes())?;
        println!(
            "pushed record {i} at offset {offset} (unflushed {}, unsynced {})",
            wal.unflushed_bytes(),
            wal.unsynced_bytes()
        );
    }

    let path = wal.path().to_path_buf();
    wal.close()?;
    println!("\nsegment: {}\n", path.display());

    for record in WalReader::open(&path)?.strings() {
        match record {
            Ok(s) => println!("read: {s}"),
            Err(ReadError::TruncatedTail { offset }) => {
                println!("log ends mid-frame at {offset} (a crash landed here)");
                break;
            }
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        }
    }

    std::fs::remove_dir_all(Path::new(&dir))?;
    Ok(())
}
