//! Bounded local image loading shared by user attachments and view_image.
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

pub fn load_data_uri(cwd: &Path, path: &str) -> Result<String> {
    let path = crate::tools::resolve_path_in(cwd, path)?;
    // Check metadata before open so directories and special files are rejected.
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("inspecting image {}", path.display()))?;
    if !metadata.is_file() {
        bail!("image must be a regular file: {}", path.display());
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        bail!("image too large ({} bytes, limit 8 MiB)", metadata.len());
    }
    let file = std::fs::File::open(&path)
        .with_context(|| format!("opening image {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes)
        .with_context(|| format!("reading image {}", path.display()))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!("image too large (limit 8 MiB)");
    }
    // Sniff signatures rather than trusting a renamed text file's extension.
    let mime = mime_type(&bytes)?;
    Ok(format!("data:{mime};base64,{}", crate::api::base64_encode(&bytes)))
}

fn mime_type(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Ok("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Ok("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Ok("image/webp")
    } else {
        bail!("unsupported image content; expected PNG, JPEG, GIF, or WebP")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_map_to_mimes() {
        assert_eq!(mime_type(b"\x89PNG\r\n\x1a\nrest").unwrap(), "image/png");
        assert_eq!(mime_type(b"\xff\xd8\xffdata").unwrap(), "image/jpeg");
        assert_eq!(mime_type(b"GIF89a....").unwrap(), "image/gif");
        assert_eq!(mime_type(b"RIFF1234WEBPVP8 ").unwrap(), "image/webp");
        assert!(mime_type(b"<html>").is_err());
        assert!(mime_type(b"").is_err());
    }

    #[test]
    fn loads_png_and_rejects_oversize_and_dirs() {
        let dir = std::env::temp_dir().join(format!("lc-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Minimal valid signature; content beyond the header is irrelevant.
        std::fs::write(dir.join("a.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let uri = load_data_uri(&dir, "a.png").unwrap();
        assert!(uri.starts_with("data:image/png;base64,"), "{uri}");

        std::fs::write(dir.join("fake.png"), b"not an image").unwrap();
        assert!(load_data_uri(&dir, "fake.png").is_err());
        assert!(load_data_uri(&dir, ".").is_err()); // directory
        assert!(load_data_uri(&dir, "missing.png").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
