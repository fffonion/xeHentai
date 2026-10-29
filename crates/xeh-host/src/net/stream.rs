use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::dto::{CommitGate, WriterFault};
use crate::net::cleanup_deadline;
use crate::net::client::OpenedResponse;
use crate::net::policy::until_deadline;
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
    pub write_hold: Option<&'a CommitGate>,
    pub buffer_hold: Option<&'a CommitGate>,
    pub uncancellable_hold: Option<&'a CommitGate>,
    pub cancel: &'a CancellationToken,
    pub deadline: Instant,
    pub idle: Duration,
    pub metrics: &'a Metrics,
}

pub async fn write_body(
    mut opened: OpenedResponse,
    root: &Path,
    relative: &str,
    job_id: i64,
    write: BodyWrite<'_>,
) -> Result<StagedFile, &'static str> {
    let BodyWrite {
        chunk_bytes,
        stream_limit,
        writer_fault,
        write_hold,
        buffer_hold,
        uncancellable_hold,
        cancel,
        deadline,
        idle,
        metrics,
    } = write;
    if let Some(length) = opened.content_length {
        if length > stream_limit as u64 {
            let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
            return Err("stream_limit");
        }
    }
    let final_path = resolve_inside(root, relative)?;
    if std::fs::symlink_metadata(&final_path).is_ok() {
        let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
        return Err("target_exists");
    }
    let (staging, file) = create_staging(&final_path, job_id)?;
    let mut guard = StagingGuard {
        path: staging.clone(),
        file: Some(file),
        keep: false,
    };
    let mut total = 0u64;
    let mut progress_at = Instant::now();
    loop {
        if cancel.is_cancelled() {
            let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
            return Err("cancelled");
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
            return Err("deadline");
        }
        let idle_at = progress_at.checked_add(idle).unwrap_or(deadline);
        if now >= idle_at {
            let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
            return Err("deadline");
        }
        let limit = if idle_at < deadline {
            idle_at
        } else {
            deadline
        };
        let next = match until_deadline(cancel, limit, opened.body.frame()).await {
            Ok(Some(Ok(frame))) => Some(frame),
            Ok(None) => None,
            Ok(Some(Err(_))) => {
                let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
                return Err("transport_failed");
            }
            Err(code) => {
                let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
                return Err(code);
            }
        };
        let Some(frame) = next else {
            break;
        };
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if chunk.is_empty() {
            continue;
        }
        progress_at = Instant::now();
        let bytes_hold = Hold::acquire(metrics, chunk.len(), 0);
        let mut offset = 0;
        while offset < chunk.len() {
            if let Some(fault) = writer_fault {
                if fault.is_armed() && total >= fault.fail_after_bytes {
                    fault.disarm();
                    drop(bytes_hold);
                    let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
                    return Err("io_error");
                }
            }
            let room = writer_fault
                .filter(|fault| fault.is_armed())
                .map(|fault| (fault.fail_after_bytes - total) as usize)
                .unwrap_or(usize::MAX);
            let n = (chunk.len() - offset).min(chunk_bytes).min(room);
            if total.saturating_add(n as u64) > stream_limit as u64 {
                drop(bytes_hold);
                let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
                return Err("stream_limit");
            }
            let mut owned = Vec::with_capacity(n);
            owned.extend_from_slice(&chunk[offset..offset + n]);
            let vec_hold = Hold::acquire(metrics, owned.len(), owned.capacity());
            if let Some(hold) = buffer_hold {
                hold.arrive();
                hold.wait_bounded(cancel, deadline)?;
            }
            if let Some(hold) = write_hold {
                hold.arrive();
                hold.wait_bounded(cancel, deadline)?;
            }
            let file = guard.file.as_mut().ok_or("io_error")?;
            let written = until_deadline(cancel, deadline, file.write_all(&owned)).await;
            drop(vec_hold);
            match written {
                Err(code) => {
                    drop(bytes_hold);
                    let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
                    return Err(code);
                }
                Ok(Err(_)) => {
                    drop(bytes_hold);
                    let _ = opened.connection.shutdown(cleanup_deadline(deadline)).await;
                    return Err("io_error");
                }
                Ok(Ok(())) => {}
            }
            offset += n;
            total += n as u64;
        }
        drop(chunk);
        drop(bytes_hold);
    }
    let file = guard.file.as_mut().ok_or("io_error")?;
    until_deadline(cancel, deadline, file.sync_all())
        .await?
        .map_err(|_| "io_error")?;
    until_deadline(cancel, deadline, file.shutdown())
        .await?
        .map_err(|_| "io_error")?;
    drop(guard.file.take());
    let joined = opened.connection.shutdown(cleanup_deadline(deadline)).await;
    if !joined {
        return Err("driver_pending");
    }
    guard.keep = true;
    if let Some(hold) = uncancellable_hold {
        hold.arrive();
        let _ = hold.wait_release_only();
    }
    Ok(StagedFile {
        staging,
        final_path,
        bytes: total,
    })
}

struct Hold<'a> {
    metrics: &'a Metrics,
    len: usize,
    capacity: usize,
}

impl<'a> Hold<'a> {
    fn acquire(metrics: &'a Metrics, len: usize, capacity: usize) -> Self {
        metrics.acquire_hold(len, capacity);
        Self {
            metrics,
            len,
            capacity,
        }
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.metrics.release_hold(self.len, self.capacity);
    }
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
