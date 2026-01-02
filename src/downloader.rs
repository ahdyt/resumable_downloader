use crate::{error::DownloadError, progress::ProgressManager};
use fs2::FileExt;
use futures::StreamExt;
use reqwest::header::{HeaderValue, RANGE};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

const MAX_TITLE_WIDTH: usize = 30;
const MAX_RETRIES: usize = 5;
const SPEED_UPDATE_INTERVAL: f64 = 1.0; // seconds

/// Converts bytes to megabytes
fn bytes_to_mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Calculates download speed in MB/s
fn calculate_speed_mb(bytes: u64, elapsed_seconds: f64) -> f64 {
    bytes as f64 / elapsed_seconds / (1024.0 * 1024.0)
}

#[cfg(target_os = "windows")]
fn set_hidden_attribute(path: &Path) -> std::io::Result<()> {
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0)) // Null terminator
        .collect();

    // SAFETY: `wide_path` is properly null-terminated
    unsafe {
        SetFileAttributesW(wide_path.as_ptr(), FILE_ATTRIBUTE_HIDDEN);
    }

    Ok(())
}

fn path_md5_hash(path: &Path) -> String {
    let digest = md5::compute(path.to_string_lossy().as_bytes());
    format!("{:x}", digest)
}

pub struct ProgressTracker {
    manager: Arc<dyn ProgressManager + Send + Sync>,
    task_id: usize,
}

impl ProgressTracker {
    pub fn new(manager: Arc<dyn ProgressManager + Send + Sync>, task_id: usize) -> Self {
        Self { manager, task_id }
    }

    fn update_progress(&self, message: &str) {
        self.manager.update(self.task_id, message);
    }
}

pub struct Downloader<'a> {
    url: &'a str,
    title: &'a str,
    output_path: PathBuf,
    progress: Option<ProgressTracker>,
}

impl<'a> Downloader<'a> {
    pub fn new(
        url: &'a str,
        title: &'a str,
        output_path: &'a str,
        progress: Option<ProgressTracker>,
    ) -> Self {
        Self {
            url,
            title,
            output_path: PathBuf::from(output_path),
            progress,
        }
    }

    /// Truncates title to fit within display width
    fn truncated_title(&self) -> String {
        if self.title.chars().count() > MAX_TITLE_WIDTH {
            let mut truncated = self
                .title
                .chars()
                .take(MAX_TITLE_WIDTH - 1)
                .collect::<String>();
            truncated.push('…');
            truncated
        } else {
            self.title.to_string()
        }
    }

    fn temp_path(&self) -> PathBuf {
        let mut path = self.output_path.clone();
        path.set_extension("part");
        path
    }

    fn lock_path(&self) -> PathBuf {
        let hash = path_md5_hash(&self.output_path);
        let lock_name = if cfg!(windows) {
            format!("{}.lock", hash)
        } else {
            format!(".{}.lock", hash)
        };

        self.output_path.with_file_name(lock_name)
    }

    async fn probe_remote_size(&self, client: &reqwest::Client) -> Result<u64, DownloadError> {
        let response = client
            .get(self.url)
            .header("Range", "bytes=0-0")
            .send()
            .await?;

        // Try to extract size from Content-Range header first
        if let Some(content_range) = response.headers().get("Content-Range") {
            let content_range_str = content_range
                .to_str()
                .map_err(|_| DownloadError::UnsupportedServer)?;
            let total_size_str = content_range_str
                .split('/')
                .nth(1)
                .ok_or(DownloadError::UnsupportedServer)?;

            return total_size_str
                .parse()
                .map_err(|_| DownloadError::UnsupportedServer);
        }

        // Fall back to Content-Length
        if let Some(content_length) = response.headers().get("Content-Length") {
            let size_str = content_length
                .to_str()
                .map_err(|_| DownloadError::UnsupportedServer)?;
            return size_str
                .parse()
                .map_err(|_| DownloadError::UnsupportedServer);
        }

        Err(DownloadError::UnsupportedServer)
    }

    /// Check if the file already exists and is complete
    async fn should_skip_download(&self) -> Result<bool, DownloadError> {
        let final_path = &self.output_path;

        // Only check if final file exists and there's no temp file
        if !final_path.exists() {
            return Ok(false);
        }

        let temp_path = self.temp_path();
        if temp_path.exists() {
            // Temp file exists, don't skip
            return Ok(false);
        }

        // Check if file size matches remote size
        let client = reqwest::Client::new();
        let remote_size = match self.probe_remote_size(&client).await {
            Ok(size) => size,
            Err(DownloadError::UnsupportedServer) => {
                // If we can't determine remote size, we can't verify completeness
                // So we won't skip
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        let local_size = match std::fs::metadata(final_path) {
            Ok(meta) => meta.len(),
            Err(_) => {
                // If we can't read local file, don't skip
                return Ok(false);
            }
        };

        if local_size == remote_size {
            if let Some(ref progress) = self.progress {
                progress.update_progress(&format!(
                    "File already complete: {} — skipping download",
                    self.truncated_title()
                ));
            }
            return Ok(true);
        }

        // File exists but is incomplete - rename to temp for resumption
        std::fs::rename(final_path, temp_path)?;
        Ok(false)
    }

    fn create_lock_file(&self) -> Result<std::fs::File, std::io::Error> {
        let lock_path = self.lock_path();
        let lock_file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)?;

        #[cfg(target_os = "windows")]
        set_hidden_attribute(&lock_path)?;

        Ok(lock_file)
    }

    async fn download_chunks(
        &self,
        response: reqwest::Response,
        existing_len: u64,
        mut file: std::fs::File,
    ) -> Result<(), DownloadError> {
        let total_size = response.content_length().map(|size| size + existing_len);

        let mut stream = response.bytes_stream();
        let mut downloaded = existing_len;

        let mut last_update = Instant::now();
        let mut bytes_since_update = 0u64;
        let mut speed_message = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk)?;
            downloaded += chunk.len() as u64;
            bytes_since_update += chunk.len() as u64;

            let elapsed = last_update.elapsed().as_secs_f64();
            if elapsed >= SPEED_UPDATE_INTERVAL {
                let speed_mb = calculate_speed_mb(bytes_since_update, elapsed);
                speed_message = format!(" | {:.2} MB/s", speed_mb);

                last_update = Instant::now();
                bytes_since_update = 0;
            }

            if let Some(ref progress) = self.progress {
                let downloaded_mb = bytes_to_mb(downloaded);
                let truncated_title = self.truncated_title();

                if let Some(total) = total_size {
                    let total_mb = bytes_to_mb(total);
                    let percentage = (downloaded as f64 / total as f64) * 100.0;

                    progress.update_progress(&format!(
                        "Downloading {}: {:.2} MB / {:.2} MB ({:.2}%){}",
                        truncated_title, downloaded_mb, total_mb, percentage, speed_message
                    ));
                } else {
                    progress.update_progress(&format!(
                        "Downloaded {}: {:.2} MB{}",
                        truncated_title, downloaded_mb, speed_message
                    ));
                }
            }
        }

        Ok(())
    }

    async fn try_download(&mut self) -> Result<(), DownloadError> {
        // First, check if we should skip downloading entirely
        if self.should_skip_download().await? {
            return Ok(());
        }

        let temp_path = self.temp_path();
        let existing_len = temp_path.metadata().map(|meta| meta.len()).unwrap_or(0);

        // Prepare request with range if resuming
        let client = reqwest::Client::new();
        let mut request = client.get(self.url);

        if existing_len > 0 {
            let range_value = HeaderValue::from_str(&format!("bytes={}-", existing_len))
                .map_err(|_| DownloadError::InvalidRange)?;
            request = request.header(RANGE, range_value);
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(DownloadError::RangeNotSatisfiable);
        }

        let response = response.error_for_status()?;

        // Create and lock lock file
        let lock_file = self.create_lock_file()?;
        if lock_file.try_lock_exclusive().is_err() {
            if let Some(ref progress) = self.progress {
                progress.update_progress("Another instance is downloading — aborting");
            }
            return Ok(());
        }

        // Open temp file for appending
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&temp_path)?;

        // Download chunks
        self.download_chunks(response, existing_len, file).await?;

        // Atomic finalize
        std::fs::rename(&temp_path, &self.output_path)?;
        std::fs::remove_file(self.lock_path())?;

        Ok(())
    }

    pub async fn download(&mut self) -> Result<(), DownloadError> {
        for attempt in 0..MAX_RETRIES {
            match self.try_download().await {
                Ok(()) => return Ok(()),
                Err(DownloadError::RangeNotSatisfiable) => {
                    // Try to finalize if temp file exists
                    let temp_path = self.temp_path();
                    if temp_path.exists() {
                        let _ = std::fs::rename(&temp_path, &self.output_path);
                    }
                    return Ok(());
                }
                Err(DownloadError::UnsupportedServer) => return Ok(()),
                Err(e) if attempt == MAX_RETRIES - 1 => return Err(e),
                Err(_) => {
                    let delay = Duration::from_secs(2_u64.pow(attempt as u32));
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
        }

        unreachable!("Loop should always return or break before reaching end");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::StdoutProgressManager;
    use futures::future::join_all;

    struct TestDownload<'a> {
        url: &'a str,
        title: &'a str,
        output_path: &'a str,
    }

    impl<'a> TestDownload<'a> {
        const fn new(url: &'a str, title: &'a str, output_path: &'a str) -> Self {
            Self {
                url,
                title,
                output_path,
            }
        }
    }

    const TEST_DOWNLOADS: [TestDownload; 3] = [
        TestDownload::new(
            "https://ash-speed.hetzner.com/100MB.bin",
            "100MB.bin",
            "100MB.bin",
        ),
        TestDownload::new(
            "https://ash-speed.hetzner.com/100MB.bin",
            "100MB.bin",
            "100MB.bin",
        ),
        TestDownload::new(
            "https://ash-speed.hetzner.com/1GB.bin",
            "1GB.bin",
            "1GB.bin",
        ),
    ];

    #[tokio::test]
    async fn test_concurrent_downloads() {
        let progress = Arc::new(StdoutProgressManager::new());
        let tasks: Vec<_> = TEST_DOWNLOADS
            .iter()
            .map(|test| {
                let progress_clone = progress.clone();
                let url = test.url;
                let title = test.title;
                let output_path = test.output_path;

                tokio::spawn(async move {
                    let task_id = progress_clone.register();
                    let mut downloader = Downloader::new(
                        url,
                        title,
                        output_path,
                        Some(ProgressTracker::new(progress_clone, task_id)),
                    );
                    downloader.download().await
                })
            })
            .collect();

        let results = join_all(tasks).await;
        for result in results {
            assert!(result.unwrap().is_ok());
        }
    }
}
