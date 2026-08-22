use ksuid::Ksuid;
use std::path::{Path, PathBuf};

struct WalWriter {
    pub file_path: PathBuf,
}

impl WalWriter {
    pub fn new(directory: &Path) -> Self {
        let id = Ksuid::new(None);
        let file_segment = std::fs::File::options().create_new(create_new);
        Self { file_path: () }
    }
}
