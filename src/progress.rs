use std::io::{self, Write};

pub struct Progress {
    total: Option<u64>,
}

impl Progress {
    pub fn new(total: Option<u64>) -> Self {
        Self { total }
    }

    pub fn update(&self, downloaded: u64) {
        if let Some(total) = self.total {
            let percent = (downloaded as f64 / total as f64) * 100.0;
            print!("\rDownloaded: {} / {} bytes ({:.2}%)", downloaded, total, percent);
        } else {
            print!("\rDownloaded: {} bytes", downloaded);
        }
        io::stdout().flush().unwrap();
    }
}
