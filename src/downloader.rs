use crate::{error::DownloadError, progress::ProgressManager};
use fs2::FileExt;
use futures::StreamExt;
use reqwest::header::{HeaderValue, RANGE};
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::ffi::OsStr;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

fn to_mb(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

#[cfg(target_os = "windows")]
fn set_hidden_windows(path: &str) {
    let mut wide: Vec<u16> = OsStr::new(path).encode_wide().collect();
    wide.push(0); // Null terminator

    unsafe {
        SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN);
    }
}

fn md5_from_path(path: &str) -> String {
    let digest = md5::compute(path.as_bytes());
    format!("{:x}", digest)
}

pub struct Downloader<'a> {
    url: &'a str,
    title: &'a str,
    output_path: &'a str,
    progress: Option<(Arc<dyn ProgressManager + Send + Sync>, usize)>,
}

impl<'a> Downloader<'a> {
    pub fn new(
        url: &'a str,
        title: &'a str,
        output_path: &'a str,
        progress: Option<(Arc<dyn ProgressManager + Send + Sync>, usize)>,
    ) -> Self {
        Self {
            url,
            title,
            output_path,
            progress,
        }
    }

    async fn try_download(&mut self) -> Result<(), DownloadError> {
        let final_path = Path::new(self.output_path);
        let temp_path_string = format!("{}.part", self.output_path);
        let temp_path = Path::new(&temp_path_string);

        //
        // --- CHECK EXISTING FINAL FILE ---
        //
        if final_path.exists() && !temp_path.exists() {
            // Probe server for actual size using Range: bytes=0-0
            let client = reqwest::Client::new();
            let probe = client
                .get(self.url)
                .header("Range", "bytes=0-0")
                .send()
                .await?;

            let remote_len = if let Some(cr) = probe.headers().get("Content-Range") {
                let s = cr.to_str().unwrap();
                let total = s.split('/').nth(1).unwrap();
                total.parse::<u64>().unwrap()
            } else if let Some(cl) = probe.headers().get("Content-Length") {
                cl.to_str().unwrap().parse::<u64>().unwrap()
            } else {
                return Err(DownloadError::UnsupportedServer);
            };

            let local_len = std::fs::metadata(final_path)?.len();

            if local_len == remote_len {
                if let Some((ref manager, line)) = self.progress {
                    manager.update(
                        line,
                        &format!(
                            "File already complete: {} — skipping download",
                            self.output_path
                        ),
                    );
                }
                return Ok(());
            }

            std::fs::rename(final_path, temp_path)?;
        }

        //
        // If both final and .part exist -> .part is stale, remove it
        //
        if final_path.exists() && temp_path.exists() {
            std::fs::remove_file(temp_path)?;
        }

        //
        // --- START/RESUME DOWNLOAD ---
        //
        let download_path = temp_path;
        let existing_len = std::fs::metadata(download_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let client = reqwest::Client::new();
        let mut request = client.get(self.url);

        if existing_len > 0 {
            let range = format!("bytes={}-", existing_len);
            request = request.header(RANGE, HeaderValue::from_str(&range).unwrap());
        } else {
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(DownloadError::RangeNotSatisfiable);
        }

        let response = response.error_for_status()?;
        let total_size = response.content_length().map(|s| s + existing_len);
        let total_mb = total_size.map(to_mb);

        let hash = md5_from_path(&self.output_path);

        #[cfg(unix)]
        let lock_path = format!(".{}{}.lock", self.output_path, hash);
        #[cfg(windows)]
        let lock_path = format!("{}{}.lock", self.output_path, hash);

        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)?;

        #[cfg(windows)]
        set_hidden_windows(&lock_path);

        // try lock
        if let Err(_) = lock_file.try_lock_exclusive() {
            if let Some((ref manager, line)) = self.progress {
                manager.update(line, "Another instance is downloading — aborting");
            }
            return Ok(());
        }

        // open .part (no locking)
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(download_path)?;

        file.seek(SeekFrom::End(0))?;

        let mut stream = response.bytes_stream();
        let mut downloaded = existing_len;

        let mut last_instant = Instant::now();
        let mut bytes_since_last = 0u64;
        let mut last_speed_str = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk)?;
            downloaded += chunk.len() as u64;
            let downloaded_mb = to_mb(downloaded);

            bytes_since_last += chunk.len() as u64;

            let elapsed = last_instant.elapsed().as_secs_f64();

            if elapsed >= 1.0 {
                let speed_mb = (bytes_since_last as f64 / elapsed) / (1024.0 * 1024.0);
                last_speed_str = format!(" | {:.2} MB/s", speed_mb);

                last_instant = Instant::now();
                bytes_since_last = 0;
            }

            let truncated = {
                let max = 30; // or any width you prefer
                if self.title.chars().count() > max {
                    let mut s = self.title.chars().take(max).collect::<String>();
                    s.push_str("…");
                    s
                } else {
                    self.title.to_string()
                }
            };

            if let Some((ref manager, line)) = self.progress {
                if let Some(total_mb_val) = total_mb {
                    let total_bytes = total_mb_val * 1024.0 * 1024.0;
                    let pct = (downloaded as f64 / total_bytes) * 100.0;

                    manager.update(
                        line,
                        &format!(
                            "Downloading {}: {:.2} MB / {:.2} MB ({:.2}%){}",
                            truncated, downloaded_mb, total_mb_val, pct, last_speed_str
                        ),
                    );
                } else {
                    manager.update(line, &format!("Downloaded {}", truncated,));
                }
            }
        }

        // Atomic finalize
        std::fs::rename(download_path, final_path)?;
        // Delete lock file
        std::fs::remove_file(lock_path)?;

        Ok(())
    }

    pub async fn download(&mut self) -> Result<(), DownloadError> {
        const MAX_RETRIES: usize = 5;

        let mut attempt = 0;
        loop {
            match self.try_download().await {
                Ok(_) => return Ok(()),
                Err(DownloadError::RangeNotSatisfiable) => {
                    let final_path = self.output_path;
                    let temp_path = format!("{}.part", self.output_path);

                    if Path::new(&temp_path).exists() {
                        let _ = std::fs::rename(&temp_path, &final_path);
                    }

                    return Ok(());
                }
                Err(DownloadError::UnsupportedServer) => {
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        return Err(e);
                    }

                    let delay = std::time::Duration::from_secs(2_u64.pow(attempt as u32));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::StdoutProgressManager;
    use futures::future::join_all;

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
            TestDownloader::new(
                "https://ash-speed.hetzner.com/100MB.bin",
                "100MB.bin",
                "100MB.bin",
            ),
            TestDownloader::new(
                "https://ash-speed.hetzner.com/100MB.bin",
                "100MB.bin",
                "100MB.bin",
            ),
            TestDownloader::new(
                "https://ash-speed.hetzner.com/1GB.bin",
                "1GB.bin",
                "1GB.bin",
            ),
        ];
        let progress = Arc::new(StdoutProgressManager::new());
        let mut tasks = Vec::new();

        for test_download in test_downloads {
            let progress_clone = progress.clone();
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
