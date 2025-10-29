use crate::{error::DownloadError, progress::Progress};
use futures_util::StreamExt;
use reqwest::header::{HeaderValue, RANGE};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

pub struct Downloader<'a> {
    url: &'a str,
    title: &'a str,
    output_path: &'a str,
}

impl<'a> Downloader<'a> {
    pub fn new(url: &'a str, title: &'a str, output_path: &'a str) -> Self {
        Self {
            url,
            title,
            output_path,
        }
    }

    pub async fn download(&mut self) -> Result<(), DownloadError> {
        let path = Path::new(self.output_path);
        let existing_len = if path.exists() {
            std::fs::metadata(path)?.len()
        } else {
            0
        };

        let client = reqwest::Client::new();
        let mut request = client.get(self.url);
        println!("Downloading {}", self.title);

        if existing_len > 0 {
            let range_header = format!("bytes={}-", existing_len);
            request = request.header(RANGE, HeaderValue::from_str(&range_header).unwrap());
            println!("Resuming from byte {}", existing_len);
        } else {
            println!("Starting new download...");
        }

        let response = request.send().await?.error_for_status()?;
        let total_size = response.content_length().map(|s| s + existing_len);
        let progress = Progress::new(total_size);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.output_path)?;

        file.seek(SeekFrom::End(0))?;

        let mut stream = response.bytes_stream();
        let mut downloaded = existing_len;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk)?;
            downloaded += chunk.len() as u64;
            progress.update(downloaded);
        }

        println!("\nDownload complete!");
        Ok(())
    }
}
