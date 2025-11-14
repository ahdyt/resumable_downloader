use crate::{error::DownloadError, progress::ProgressManager};
use futures_util::StreamExt;
use reqwest::header::{HeaderValue, RANGE};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use futures::future::join_all;

pub struct Downloader<'a> {
    url: &'a str,
    title: &'a str,
    output_path: &'a str,
    progress: Option<(Arc<ProgressManager>, usize)>,
}

impl<'a> Downloader<'a> {
    pub fn new(url: &'a str, title: &'a str, output_path: &'a str, progress: Option<(Arc<ProgressManager>, usize)>) -> Self {
        Self {
            url,
            title,
            output_path,
            progress,
        }
    }

    async fn try_download(&mut self) -> Result<(), DownloadError> {
        let path = Path::new(self.output_path);
        let existing_len = if path.exists() {
            std::fs::metadata(path)?.len()
        } else {
            0
        };

        let client = reqwest::Client::new();
        let mut request = client.get(self.url);

        if existing_len > 0 {
            let range = format!("bytes={}-", existing_len);
            request = request.header(RANGE, HeaderValue::from_str(&range).unwrap());
            println!("Resuming from byte {}", existing_len);
        } else {
            println!("Starting new download...");
        }

        let response = request.send().await?;
        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            println!("416 Range Not Satisfiable — skipping (file likely complete)");
            return Err(DownloadError::RangeNotSatisfiable);
        }
        let response = response.error_for_status()?;
        let total_size = response.content_length().map(|s| s + existing_len);

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

            if let Some((ref manager, line)) = self.progress {
                if let Some(total) = total_size {
                    let pct = downloaded as f64 / total as f64 * 100.0;
                    manager.update(
                        line,
                        &format!(
                            "Downloaded {}: {} / {} bytes ({:.2}%)",
                            self.title,
                            downloaded,
                            total,
                            pct
                        ),
                    );
                } else {
                    manager.update(
                        line,
                        &format!(
                            "Downloaded {}: {} bytes",
                            self.title,
                            downloaded,
                        ),
                    );
                }
            }
        }

        println!("\nDownload complete!");
        Ok(())
    }

    pub async fn download(&mut self) -> Result<(), DownloadError> {
        const MAX_RETRIES: usize = 5;

        let mut attempt = 0;
        loop {
            match self.try_download().await {
                Ok(_) => return Ok(()),
                Err(DownloadError::RangeNotSatisfiable) => {
                       println!("Skip retry due to 416 Range Not Satisfiable");
                       return Ok(());
                },
                Err(e) => {
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        return Err(e);
                    }

                    let delay = std::time::Duration::from_secs(2_u64.pow(attempt as u32));
                    eprintln!("retry {attempt}/{MAX_RETRIES} after error: {e}, waiting {:?}", delay);
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_download() {
        struct TestDownloader<'a> {
            url: &'a str,
            title: &'a str,
            output_path: &'a str,
        }
        impl<'a> TestDownloader<'a> {
            fn new(url: &'a str, title: &'a str, output_path: &'a str) -> Self {
                TestDownloader {
                    url: url,
                    title: title,
                    output_path: output_path,
                }
            }
        }
        let test_downloads = vec![
            TestDownloader::new("https://ash-speed.hetzner.com/100MB.bin", "100MB.bin", "100MB.bin"),
            TestDownloader::new("https://ash-speed.hetzner.com/1GB.bin", "1GB.bin", "1GB.bin"),
        ];
        let progress = Arc::new(ProgressManager::new());
        let mut tasks = Vec::new();

        for test_download in test_downloads {
            let progress_clone = progress.clone();
            let line = progress.register();
            let url = test_download.url;
            let title = test_download.title;
            let output_path = test_download.output_path;
            let handle = tokio::spawn(async move {
                        let line = progress_clone.register();
                        let mut downloader =
                            Downloader::new(&url, &title, &output_path, Some((progress_clone, line)));
                        downloader.download().await
                    });

            tasks.push(handle);
        }
        let results = join_all(tasks).await;
        for r in results {
                assert!(r.unwrap().is_ok());
        }
    }
}
