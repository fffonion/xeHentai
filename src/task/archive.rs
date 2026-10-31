//! Archive work of `xeHentai/task.py` (`make_archive`).
//!
//! The reference writes a STORE-only `ZIP_STORED` archive next to the gallery
//! directory with a UTF-8 comment and then optionally removes the directory.
//! This implementation keeps that container contract and publishes the
//! finished archive atomically so a failed run can never destroy the images.
#![allow(dead_code)]

use std::fs::{self, File};
use std::path::PathBuf;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::file_ops;
use super::{Task, TaskError};

impl Task {
    pub fn make_archive(&mut self, remove: bool) -> Result<PathBuf, TaskError> {
        // Needed to lock between RPC get_img and archiving, as in task.py:401.
        let _guard = self.files.lock();
        let dpath = self.get_fpath()?;
        let arc = file_ops::zip_path(&dpath)?;
        if file_ops::path_exists(&arc)? {
            // Reference returns without releasing `_f_lock`; the guard is
            // dropped here so the lock stays usable (registered delta).
            return Ok(arc);
        }
        let title = self.meta_str("title")?.to_string();
        let comment = format!(
            "xeHentai Archiver v{}\nTitle:{}\nOriginal URL:{}",
            crate::r#const::VERSION,
            title,
            self.url
        );
        let parent = dpath.parent().ok_or_else(|| TaskError::UnsafePath {
            path: dpath.clone(),
            reason: "gallery directory has no parent",
        })?;
        let stage = file_ops::create_stage_with_prefix(parent, ".xehentai-archive-")?;

        let outcome = (|| -> Result<(), TaskError> {
            let file = File::create(&stage).map_err(|source| TaskError::Io {
                path: stage.clone(),
                source,
            })?;
            let mut writer = ZipWriter::new(file);
            writer.set_comment(comment.as_str());
            let mut entries: Vec<std::ffi::OsString> = fs::read_dir(&dpath)
                .map_err(|source| TaskError::Io {
                    path: dpath.clone(),
                    source,
                })?
                .map(|entry| {
                    entry
                        .map(|entry| entry.file_name())
                        .map_err(|source| TaskError::Io {
                            path: dpath.clone(),
                            source,
                        })
                })
                .collect::<Result<_, _>>()?;
            entries.sort_by(|left, right| left.as_encoded_bytes().cmp(right.as_encoded_bytes()));
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            for name in entries {
                let full = dpath.join(&name);
                let metadata = fs::symlink_metadata(&full).map_err(|source| TaskError::Io {
                    path: full.clone(),
                    source,
                })?;
                if metadata.file_type().is_symlink() {
                    return Err(TaskError::UnsafePath {
                        path: full,
                        reason: "refusing to archive a symlink entry",
                    });
                }
                let name = name.to_str().ok_or_else(|| TaskError::UnsafePath {
                    path: full.clone(),
                    reason: "archive entry name is not UTF-8",
                })?;
                if metadata.is_dir() {
                    writer
                        .add_directory(format!("{name}/"), options)
                        .map_err(|error| TaskError::Zip(error.to_string()))?;
                } else {
                    writer
                        .start_file(name, options)
                        .map_err(|error| TaskError::Zip(error.to_string()))?;
                    let mut source = File::open(&full).map_err(|source| TaskError::Io {
                        path: full.clone(),
                        source,
                    })?;
                    std::io::copy(&mut source, &mut writer)
                        .map_err(|error| TaskError::Zip(error.to_string()))?;
                }
            }
            let file = writer
                .finish()
                .map_err(|error| TaskError::Zip(error.to_string()))?;
            file.sync_all().map_err(|source| TaskError::Io {
                path: stage.clone(),
                source,
            })?;
            file_ops::publish(&stage, &arc)
        })();

        if let Err(error) = outcome {
            let _ = fs::remove_file(&stage);
            return Err(error);
        }
        if remove {
            file_ops::remove_tree(&dpath)?;
        }
        Ok(arc)
    }
}
