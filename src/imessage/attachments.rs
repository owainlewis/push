//! Safe, worker-time loading of local Messages image attachments.

use std::ffi::CString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;
use uuid::Uuid;

use crate::channel::InboundImage;
use crate::image::{DownloadedImage, MAX_IMAGE_BYTES};

const MAX_HEIC_INPUT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub locator: String,
    pub file_size: Option<usize>,
    pub mime_type: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageKind {
    Direct,
    Heic,
}

pub fn needs_conversion(locator: &str, mime_type: Option<&str>) -> bool {
    image_kind(Path::new(locator), mime_type).ok() == Some(ImageKind::Heic)
}

pub async fn download(attachment_root: &Path, image: &InboundImage) -> Result<DownloadedImage> {
    if let Some(bytes) = &image.data {
        return Ok(DownloadedImage {
            bytes: bytes.clone(),
        });
    }
    download_with_converter(attachment_root, image, Path::new("/usr/bin/sips")).await
}

async fn download_with_converter(
    attachment_root: &Path,
    image: &InboundImage,
    converter: &Path,
) -> Result<DownloadedImage> {
    let (file, path) = open_attachment(attachment_root, &image.locator)?;
    match image_kind(&path, image.mime_type.as_deref())? {
        ImageKind::Direct => read_bounded(file),
        ImageKind::Heic => convert_heic(file, converter).await,
    }
}

fn open_attachment(attachment_root: &Path, locator: &str) -> Result<(File, PathBuf)> {
    open_attachment_with_hook(attachment_root, locator, || {})
}

fn open_attachment_with_hook(
    attachment_root: &Path,
    locator: &str,
    before_open: impl FnOnce(),
) -> Result<(File, PathBuf)> {
    if locator.trim().is_empty() {
        bail!("iMessage attachment omitted its local path");
    }
    let root =
        std::fs::canonicalize(attachment_root).context("open the Messages attachment directory")?;
    let root_metadata = root
        .metadata()
        .context("inspect the Messages attachment directory")?;
    if !root_metadata.is_dir() {
        bail!("Messages attachment path is not a directory");
    }
    let root_identity = (root_metadata.dev(), root_metadata.ino());
    let locator = Path::new(locator);
    let candidate = if locator.is_absolute() {
        locator.to_path_buf()
    } else if let Ok(relative) = locator.strip_prefix("~/Library/Messages/Attachments") {
        root.join(relative)
    } else {
        root.join(locator)
    };
    let path = std::fs::canonicalize(candidate).context("open the iMessage attachment")?;
    if !path.starts_with(&root) {
        bail!("iMessage attachment is outside the Messages attachment directory");
    }
    let relative = path
        .strip_prefix(&root)
        .context("verify the iMessage attachment path")?;
    before_open();
    let file = open_beneath(&root, root_identity, relative)?;
    Ok((file, path))
}

fn open_beneath(root: &Path, root_identity: (u64, u64), relative: &Path) -> Result<File> {
    let parts = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => CString::new(value.as_bytes())
                .context("iMessage attachment path contains an invalid name"),
            _ => bail!("iMessage attachment path is not relative to the attachment directory"),
        })
        .collect::<Result<Vec<_>>>()?;
    if parts.is_empty() {
        bail!("iMessage attachment is not a regular file");
    }

    let mut parent = open_canonical_directory(root)?;
    let opened_metadata = parent
        .metadata()
        .context("inspect the opened Messages attachment directory")?;
    if (opened_metadata.dev(), opened_metadata.ino()) != root_identity {
        bail!("Messages attachment directory changed during validation");
    }
    for (index, part) in parts.iter().enumerate() {
        let is_last = index + 1 == parts.len();
        let child = open_component(&parent, part, !is_last)?;
        if is_last {
            if !child
                .metadata()
                .context("inspect the iMessage attachment")?
                .is_file()
            {
                bail!("iMessage attachment is not a regular file");
            }
            return Ok(child);
        }
        parent = child;
    }
    unreachable!("empty attachment paths are rejected above")
}

fn open_canonical_directory(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        bail!("Messages attachment directory is not absolute");
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")
        .context("open the filesystem root")?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(value) => {
                let part = CString::new(value.as_bytes())
                    .context("Messages attachment path contains an invalid name")?;
                directory = open_component(&directory, &part, true)?;
            }
            _ => bail!("Messages attachment directory is not canonical"),
        }
    }
    Ok(directory)
}

fn open_component(parent: &File, part: &CString, directory: bool) -> Result<File> {
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    // SAFETY: parent is an open directory descriptor, part is a NUL-terminated
    // single path component, and a successful descriptor is immediately owned.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), part.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("open the iMessage attachment safely");
    }
    // SAFETY: openat returned a new owned descriptor that is not used elsewhere.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn image_kind(path: &Path, mime_type: Option<&str>) -> Result<ImageKind> {
    let mime_type = mime_type.map(|value| value.trim().to_ascii_lowercase());
    match mime_type.as_deref() {
        Some("image/jpeg" | "image/jpg" | "image/png" | "image/webp") => {
            return Ok(ImageKind::Direct);
        }
        Some("image/heic" | "image/heif") => return Ok(ImageKind::Heic),
        Some("") | Some("application/octet-stream") | None => {}
        Some(_) => bail!("iMessage attachment is not a supported image type"),
    }
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg" | "png" | "webp") => Ok(ImageKind::Direct),
        Some("heic" | "heif") => Ok(ImageKind::Heic),
        _ => bail!("iMessage attachment is not a supported image type"),
    }
}

fn read_bounded(file: File) -> Result<DownloadedImage> {
    if file.metadata().context("inspect the iMessage image")?.len() > MAX_IMAGE_BYTES as u64 {
        bail!("iMessage image exceeds the 6 MiB limit");
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read the iMessage image")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("iMessage image exceeds the 6 MiB limit");
    }
    Ok(DownloadedImage { bytes })
}

async fn convert_heic(source: File, converter: &Path) -> Result<DownloadedImage> {
    let directory = std::env::temp_dir().join(format!("push-heic-{}", Uuid::new_v4()));
    DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .context("create a private iMessage conversion directory")?;
    let mut cleanup = ConversionCleanup {
        directory,
        input: None,
        output: None,
    };
    let input = cleanup.directory.join("image.heic");
    let mut private_source = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&input)
        .context("create a private HEIC conversion input")?;
    cleanup.input = Some(input.clone());
    let copied = std::io::copy(
        &mut source.take(MAX_HEIC_INPUT_BYTES + 1),
        &mut private_source,
    )
    .context("copy the HEIC image into the private conversion directory")?;
    if copied > MAX_HEIC_INPUT_BYTES {
        bail!("HEIC or HEIF image exceeds the 32 MiB conversion input limit");
    }
    private_source
        .flush()
        .context("finish the private HEIC conversion input")?;
    drop(private_source);
    let output = cleanup.directory.join("image.jpg");
    cleanup.output = Some(output.clone());

    let mut command = Command::new(converter);
    command
        .arg("-s")
        .arg("format")
        .arg("jpeg")
        .arg(&input)
        .arg("--out")
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(60), command.status())
        .await
        .context("macOS image conversion timed out")?
        .context("start macOS image conversion")?;
    if !status.success() {
        bail!("macOS could not convert the HEIC or HEIF image");
    }
    let mut permissions = std::fs::metadata(&output)
        .context("inspect the converted iMessage image")?
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(&output, permissions)
        .context("protect the converted iMessage image")?;
    let converted = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&output)
        .context("open the converted iMessage image")?;
    read_bounded(converted)
}

struct ConversionCleanup {
    directory: PathBuf,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
}

impl Drop for ConversionCleanup {
    fn drop(&mut self) {
        if let Some(output) = &self.output {
            let _ = std::fs::remove_file(output);
        }
        if let Some(input) = &self.input {
            let _ = std::fs::remove_file(input);
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{sh_arg, temp_dir, FakeCli};

    fn image(locator: &Path, mime_type: &str) -> InboundImage {
        InboundImage {
            locator: locator.to_string_lossy().to_string(),
            file_size: None,
            mime_type: Some(mime_type.to_string()),
            data: None,
        }
    }

    #[tokio::test]
    async fn reads_supported_files_only_from_the_attachment_root() {
        let root = temp_dir("imessage-attachments");
        let image_path = root.join("nested/image.png");
        std::fs::create_dir_all(image_path.parent().unwrap()).unwrap();
        std::fs::write(&image_path, b"\x89PNG\r\n\x1a\nbody").unwrap();

        let downloaded = download(&root, &image(&image_path, "image/png"))
            .await
            .unwrap();
        assert_eq!(downloaded.bytes, b"\x89PNG\r\n\x1a\nbody");

        let outside = root.parent().unwrap().join("outside.png");
        std::fs::write(&outside, b"\x89PNG\r\n\x1a\nbody").unwrap();
        assert!(download(&root, &image(&outside, "image/png"))
            .await
            .unwrap_err()
            .to_string()
            .contains("outside"));

        let symlink = root.join("escaping.png");
        std::os::unix::fs::symlink(&outside, &symlink).unwrap();
        assert!(download(&root, &image(&symlink, "image/png"))
            .await
            .unwrap_err()
            .to_string()
            .contains("outside"));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn rejects_a_symlink_swap_between_validation_and_open() {
        let root = temp_dir("imessage-attachment-race");
        let image_path = root.join("image.png");
        std::fs::write(&image_path, b"trusted").unwrap();
        let outside = root.parent().unwrap().join("raced-outside.png");
        std::fs::write(&outside, b"outside").unwrap();

        let error = open_attachment_with_hook(&root, image_path.to_str().unwrap(), || {
            std::fs::remove_file(&image_path).unwrap();
            std::os::unix::fs::symlink(&outside, &image_path).unwrap();
        })
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("open the iMessage attachment safely"));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn rejects_an_attachment_root_parent_swap_after_validation() {
        let sandbox = temp_dir("imessage-attachment-root-race");
        let messages = sandbox.join("Messages");
        let root = messages.join("Attachments");
        std::fs::create_dir_all(&root).unwrap();
        let image_path = root.join("image.png");
        std::fs::write(&image_path, b"trusted").unwrap();

        let replacement = sandbox.join("replacement");
        let replacement_root = replacement.join("Attachments");
        std::fs::create_dir_all(&replacement_root).unwrap();
        std::fs::write(replacement_root.join("image.png"), b"outside").unwrap();
        let original_messages = sandbox.join("Messages-original");

        let error = open_attachment_with_hook(&root, image_path.to_str().unwrap(), || {
            std::fs::rename(&messages, &original_messages).unwrap();
            std::os::unix::fs::symlink(&replacement, &messages).unwrap();
        })
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("open the iMessage attachment safely"));
        std::fs::remove_file(&messages).unwrap();
        std::fs::rename(&original_messages, &messages).unwrap();
        let _ = std::fs::remove_dir_all(sandbox);
    }

    #[tokio::test]
    async fn rejects_missing_directories_unsupported_types_and_oversized_files() {
        let root = temp_dir("imessage-rejected-attachments");
        let missing = root.join("missing.png");
        assert!(download(&root, &image(&missing, "image/png"))
            .await
            .is_err());
        assert!(download(&root, &image(&root, "image/png")).await.is_err());

        let pdf = root.join("document.pdf");
        std::fs::write(&pdf, b"%PDF").unwrap();
        assert!(download(&root, &image(&pdf, "application/pdf"))
            .await
            .is_err());

        let oversized = root.join("large.png");
        let file = File::create(&oversized).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        assert!(download(&root, &image(&oversized, "image/png"))
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn converts_heic_to_private_jpeg_and_removes_conversion_files() {
        let root = temp_dir("imessage-heic-attachments");
        let source = root.join("photo.heic");
        std::fs::write(&source, b"heic source").unwrap();
        let marker_dir = temp_dir("imessage-converter-marker");
        let marker = marker_dir.join("conversion-paths");
        let converter = FakeCli::new(
            "sips",
            &format!(
                "#!/bin/sh\ninput=\"$4\"\noutput=\"$6\"\nprintf '%s\\n%s' \"$input\" \"$output\" > {}\nprintf '\\377\\330\\377body' > \"$output\"\n",
                sh_arg(&marker)
            ),
        );

        let downloaded = download_with_converter(
            &root,
            &image(&source, "image/heic"),
            Path::new(&converter.bin()),
        )
        .await
        .unwrap();

        assert!(downloaded.bytes.starts_with(&[0xff, 0xd8, 0xff]));
        let paths = std::fs::read_to_string(&marker).unwrap();
        let paths = paths.lines().map(Path::new).collect::<Vec<_>>();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], source);
        assert!(!paths[0].exists());
        assert!(!paths[1].exists());
        assert!(!paths[0].parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(marker_dir);
    }

    #[tokio::test]
    async fn bounds_heic_input_before_starting_conversion() {
        let root = temp_dir("imessage-large-heic");
        let source = root.join("large.heic");
        let source_file = File::create(&source).unwrap();
        source_file.set_len(MAX_HEIC_INPUT_BYTES + 1).unwrap();
        let marker_dir = temp_dir("imessage-large-heic-marker");
        let marker = marker_dir.join("converter-ran");
        let converter = FakeCli::new("sips", &format!("#!/bin/sh\ntouch {}\n", sh_arg(&marker)));

        let error = download_with_converter(
            &root,
            &image(&source, "image/heic"),
            Path::new(&converter.bin()),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("32 MiB"));
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(marker_dir);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn converts_a_real_heic_with_macos_sips() {
        use base64::Engine;

        let root = temp_dir("imessage-real-heic");
        let png = root.join("source.png");
        let heic = root.join("photo.heic");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9Zl1sAAAAASUVORK5CYII=")
            .unwrap();
        std::fs::write(&png, bytes).unwrap();
        let status = Command::new("/usr/bin/sips")
            .arg("-s")
            .arg("format")
            .arg("heic")
            .arg(&png)
            .arg("--out")
            .arg(&heic)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success());

        let downloaded = download_with_converter(
            &root,
            &image(&heic, "image/heic"),
            Path::new("/usr/bin/sips"),
        )
        .await
        .unwrap();

        assert!(downloaded.bytes.starts_with(&[0xff, 0xd8, 0xff]));
        let _ = std::fs::remove_dir_all(root);
    }
}
