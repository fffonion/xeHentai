use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::dto::WriterFault;
use crate::observe::Metrics;

pub struct StagedFile {
    pub staging: PathBuf,
    pub final_path: PathBuf,
    pub bytes: u64,
}

pub struct BodyWrite<'a> {
    pub chunk_bytes: usize,
    pub stream_limit: usize,
    pub writer_fault: Option<&'a WriterFault>,
    pub cancel: &'a CancellationToken,
    pub deadline: tokio::time::Instant,
    pub metrics: &'a Metrics,
}

pub async fn write_body(
    mut response: reqwest::Response,
    root: &Path,
    relative: &str,
    job_id: i64,
    write: BodyWrite<'_>,
) -> Result<StagedFile, &'static str> {
    let BodyWrite {
        chunk_bytes,
        stream_limit,
        writer_fault,
        cancel,
        deadline,
        metrics,
    } = write;
    if let Some(length) = response.content_length() {
        if length > stream_limit as u64 {
            return Err("stream_limit");
        }
    }
    let final_path = resolve_inside(root, relative)?;
    if std::fs::symlink_metadata(&final_path).is_ok() {
        return Err("target_exists");
    }
    let (staging, file) = create_staging(&final_path, job_id)?;
    let mut guard = StagingGuard {
        path: staging.clone(),
        file: Some(file),
        keep: false,
    };
    let mut owned = Vec::new();
    owned.reserve_exact(chunk_bytes);
    let mut total = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled");
        }
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                drop(response);
                tokio::task::yield_now().await;
                return Err("cancelled");
            }
            _ = tokio::time::sleep_until(deadline) => return Err("stream_deadline"),
            result = response.chunk() => result.map_err(chunk_code)?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let mut offset = 0;
        while offset < chunk.len() {
            if let Some(fault) = writer_fault {
                if fault.is_armed() && total >= fault.fail_after_bytes {
                    fault.disarm();
                    return Err("io_error");
                }
            }
            let room = writer_fault
                .filter(|fault| fault.is_armed())
                .map(|fault| (fault.fail_after_bytes - total) as usize)
                .unwrap_or(usize::MAX);
            let n = (chunk.len() - offset).min(chunk_bytes).min(room);
            if total.saturating_add(n as u64) > stream_limit as u64 {
                return Err("stream_limit");
            }
            owned.clear();
            owned.extend_from_slice(&chunk[offset..offset + n]);
            metrics.note_application(owned.len(), owned.capacity());
            let file = guard.file.as_mut().ok_or("io_error")?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    drop(response);
                    tokio::task::yield_now().await;
                    return Err("cancelled");
                }
                result = file.write_all(&owned) => result.map_err(|_| "io_error")?,
            }
            offset += n;
            total += n as u64;
        }
        drop(chunk);
        metrics.note_application(0, owned.capacity());
    }
    let file = guard.file.as_mut().ok_or("io_error")?;
    file.sync_all().await.map_err(|_| "io_error")?;
    file.shutdown().await.map_err(|_| "io_error")?;
    drop(guard.file.take());
    guard.keep = true;
    Ok(StagedFile {
        staging,
        final_path,
        bytes: total,
    })
}

struct StagingGuard {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    keep: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn create_staging(
    final_path: &Path,
    job_id: i64,
) -> Result<(PathBuf, tokio::fs::File), &'static str> {
    for seq in 1..9 {
        let name = format!(".xeh-stage-{job_id}-{seq}");
        let path = final_path.with_file_name(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, tokio::fs::File::from_std(file))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("io_error"),
        }
    }
    Err("io_error")
}

pub fn resolve_inside(root: &Path, relative: &str) -> Result<PathBuf, &'static str> {
    if relative.is_empty() || relative.contains('\0') || relative.contains('\\') {
        return Err("path_rejected");
    }
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err("path_rejected");
    }
    let root = root.canonicalize().map_err(|_| "path_rejected")?;
    if !root.is_dir() {
        return Err("path_rejected");
    }
    let comps: Vec<Component> = rel.components().collect();
    if comps.is_empty() {
        return Err("path_rejected");
    }
    let mut acc = root.clone();
    for (index, comp) in comps.iter().enumerate() {
        let Component::Normal(part) = comp else {
            return Err("path_rejected");
        };
        let text = part.to_str().ok_or("path_rejected")?;
        if text.is_empty() || text == "." || text == ".." {
            return Err("path_rejected");
        }
        acc.push(part);
        let last = index + 1 == comps.len();
        match std::fs::symlink_metadata(&acc) {
            Ok(meta) if meta.file_type().is_symlink() => return Err("path_rejected"),
            Ok(meta) if !last && !meta.is_dir() => return Err("path_rejected"),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && last => {}
            Err(_) => return Err("path_rejected"),
        }
    }
    let parent = acc.parent().ok_or("path_rejected")?;
    let parent_canon = parent.canonicalize().map_err(|_| "path_rejected")?;
    if !parent_canon.starts_with(&root) {
        return Err("path_rejected");
    }
    Ok(parent_canon.join(acc.file_name().ok_or("path_rejected")?))
}

fn chunk_code(error: reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "idle_timeout"
    } else {
        "transport_failed"
    }
}
