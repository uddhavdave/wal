use std::path::Path;

use wal_writer::{ReadError, WalReader, WalWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("wal-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;

    let mut wal = WalWriter::new(&dir)?;
    for i in 0..5 {
        let offset = wal.push(format!("record {i}").as_bytes())?;
        println!(
            "pushed record {i} at offset {offset} (unflushed {}, unsynced {})",
            wal.unflushed_bytes(),
            wal.unsynced_bytes()
        );
    }

    wal.close()?;
    println!("\nlog: {}\n", dir.display());

    for record in WalReader::open(&dir)?.strings() {
        match record {
            Ok(s) => println!("read: {s}"),
            Err(ReadError::TruncatedTail { path, offset }) => {
                println!(
                    "log ends mid-frame in {} at {offset} (a crash landed here)",
                    path.display()
                );
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
