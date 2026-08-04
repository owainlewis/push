//! Provider-neutral inbound image validation and temporary-file lifecycle.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use uuid::Uuid;

pub const MAX_IMAGE_COUNT: usize = 4;
pub const MAX_IMAGE_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug)]
pub struct DownloadedImage {
    pub bytes: Vec<u8>,
}

pub struct PreparedImages {
    directory: PathBuf,
    paths: Vec<PathBuf>,
}

impl PreparedImages {
    pub fn create(cache_dir: &Path, images: Vec<DownloadedImage>) -> Result<Self> {
        if images.is_empty() {
            return Ok(Self {
                directory: PathBuf::new(),
                paths: Vec::new(),
            });
        }
        if images.len() > MAX_IMAGE_COUNT {
            bail!("image message has more than {MAX_IMAGE_COUNT} attachments");
        }
        let total = images
            .iter()
            .try_fold(0usize, |total, image| total.checked_add(image.bytes.len()))
            .context("image message size overflow")?;
        if total > MAX_IMAGE_BYTES {
            bail!("image message exceeds the 6 MiB limit");
        }

        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("create image cache directory {}", cache_dir.display()))?;
        crate::util::restrict_permissions(cache_dir, true)
            .with_context(|| format!("restrict image cache directory {}", cache_dir.display()))?;
        let directory = cache_dir.join(format!("images-{}", Uuid::new_v4()));
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .with_context(|| format!("create temporary image directory {}", directory.display()))?;

        let mut prepared = Self {
            directory,
            paths: Vec::with_capacity(images.len()),
        };
        for (index, image) in images.into_iter().enumerate() {
            let extension = image_extension(&image.bytes)?;
            let path = prepared
                .directory
                .join(format!("image-{}.{}", index + 1, extension));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("create temporary image {}", path.display()))?;
            prepared.paths.push(path.clone());
            file.write_all(&image.bytes)
                .with_context(|| format!("write temporary image {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync temporary image {}", path.display()))?;
        }
        Ok(prepared)
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

impl Drop for PreparedImages {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = std::fs::remove_file(path);
        }
        if !self.directory.as_os_str().is_empty() {
            let _ = std::fs::remove_dir(&self.directory);
        }
    }
}

pub(crate) fn media_type(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok("image/jpeg");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok("image/png");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Ok("image/webp");
    }
    bail!("image attachment is not a supported JPEG, PNG, or WebP file")
}

fn image_extension(bytes: &[u8]) -> Result<&'static str> {
    match media_type(bytes)? {
        "image/jpeg" => Ok("jpg"),
        "image/png" => Ok("png"),
        "image/webp" => Ok("webp"),
        _ => unreachable!("media_type returns only supported image types"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn validates_writes_and_removes_supported_images() {
        let cache = temp_dir("prepared-images");
        let prepared = PreparedImages::create(
            &cache,
            vec![
                DownloadedImage {
                    bytes: b"\x89PNG\r\n\x1a\nbody".to_vec(),
                },
                DownloadedImage {
                    bytes: b"RIFF\x04\x00\x00\x00WEBPbody".to_vec(),
                },
            ],
        )
        .unwrap();

        assert_eq!(prepared.paths().len(), 2);
        assert_eq!(prepared.paths()[0].extension().unwrap(), "png");
        assert_eq!(prepared.paths()[1].extension().unwrap(), "webp");
        let directory = prepared.directory.clone();
        for path in prepared.paths() {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        drop(prepared);
        assert!(!directory.exists());
        let _ = std::fs::remove_dir(cache);
    }

    #[test]
    fn rejects_unsupported_oversized_and_excess_images() {
        let cache = temp_dir("rejected-images");
        assert!(PreparedImages::create(
            &cache,
            vec![DownloadedImage {
                bytes: b"not an image".to_vec(),
            }],
        )
        .is_err());
        assert!(PreparedImages::create(
            &cache,
            vec![DownloadedImage {
                bytes: vec![0xff; MAX_IMAGE_BYTES + 1],
            }],
        )
        .is_err());
        assert!(PreparedImages::create(
            &cache,
            (0..=MAX_IMAGE_COUNT)
                .map(|_| DownloadedImage {
                    bytes: b"\xff\xd8\xffbody".to_vec(),
                })
                .collect(),
        )
        .is_err());
        let _ = std::fs::remove_dir(cache);
    }
}
