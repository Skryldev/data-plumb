//! Upload validation: MIME sniffing (magic bytes) + size limits.

use crate::error::StorageError;

/// Validates declared content-type against sniffed magic bytes and enforces
/// the size ceiling.
pub struct FileValidator {
    max_size: u64,
}

/// Magic-byte table entries: `(prefix, offset, mime)`.
const MAGIC: &[(&[u8], usize, &str)] = &[
    (b"\x89PNG\r\n\x1a\n", 0, "image/png"),
    (b"\xFF\xD8\xFF", 0, "image/jpeg"),
    (b"%PDF-", 0, "application/pdf"),
    (b"PK\x03\x04", 0, "application/zip"),
    (b"PK\x05\x06", 0, "application/zip"),
    (b"PK\x07\x08", 0, "application/zip"),
    (b"\x1F\x8B", 0, "application/gzip"),
    (b"GIF87a", 0, "image/gif"),
    (b"GIF89a", 0, "image/gif"),
    (b"\x7FELF", 0, "application/x-elf"),
    (b"ftyp", 4, "video/mp4"),
    (b"RIFF", 0, "application/octet-stream"), // resolved specially below
];

pub const OCTET_STREAM: &str = "application/octet-stream";

impl FileValidator {
    pub fn new(max_size: u64) -> Self {
        Self { max_size }
    }

    /// Sniffs the MIME type from the first bytes of a payload.
    /// Returns `None` when nothing matches.
    pub fn sniff_mime(&self, head: &[u8]) -> Option<&'static str> {
        for (magic, offset, mime) in MAGIC {
            let off = *offset;
            if head.len() < off + magic.len() {
                continue;
            }
            if &head[off..off + magic.len()] == *magic {
                if *mime == "application/octet-stream" && magic == b"RIFF" {
                    // RIFF container: distinguish WEBP / WAV / AVI.
                    if head.len() >= 12 && &head[8..12] == b"WEBP" {
                        return Some("image/webp");
                    }
                    return None;
                }
                return Some(mime);
            }
        }
        None
    }

    /// Validates the declared MIME against sniffed bytes and returns the
    /// resolved MIME type.
    ///
    /// Rules:
    /// * `application/octet-stream` (or unknown declared types) adopt the
    ///   sniffed type when one is detected;
    /// * a mismatch between a specific declared type and the sniffed type is
    ///   rejected with 400;
    /// * unknown payloads keep the declared type as-is.
    pub fn validate_head(&self, declared: &str, head: &[u8]) -> Result<String, StorageError> {
        let declared = declared.trim().to_ascii_lowercase();
        let declared_base = declared.split(';').next().unwrap_or("").trim();
        match self.sniff_mime(head) {
            Some(detected) => {
                if declared_base == OCTET_STREAM || declared_base.is_empty() {
                    Ok(detected.to_string())
                } else if declared_base == detected {
                    Ok(declared)
                } else {
                    Err(StorageError::Validation(format!(
                        "content-type mismatch: declared '{declared_base}' but magic bytes indicate '{detected}'"
                    )))
                }
            }
            None => {
                if declared_base.is_empty() {
                    Ok(OCTET_STREAM.to_string())
                } else {
                    Ok(declared)
                }
            }
        }
    }

    /// Enforces the upload size ceiling (called once the full size is known,
    /// and abortively during streaming).
    pub fn validate_size(&self, size: u64) -> Result<(), StorageError> {
        if size > self.max_size {
            return Err(StorageError::TooLarge(format!(
                "file is {size} bytes; limit is {} bytes",
                self.max_size
            )));
        }
        Ok(())
    }

    pub fn max_size(&self) -> u64 {
        self.max_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator() -> FileValidator {
        FileValidator::new(1024 * 1024)
    }

    #[test]
    fn detects_png() {
        let v = validator();
        let head = [0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0];
        assert_eq!(v.sniff_mime(&head), Some("image/png"));
        assert_eq!(
            v.validate_head("application/octet-stream", &head).unwrap(),
            "image/png"
        );
    }

    #[test]
    fn detects_pdf_jpeg_zip_gzip() {
        let v = validator();
        assert_eq!(v.sniff_mime(b"%PDF-1.7 rest"), Some("application/pdf"));
        assert_eq!(v.sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(v.sniff_mime(b"PK\x03\x04...."), Some("application/zip"));
        assert_eq!(v.sniff_mime(&[0x1F, 0x8B, 0, 0]), Some("application/gzip"));
        assert_eq!(v.sniff_mime(b"GIF89a..."), Some("image/gif"));
    }

    #[test]
    fn detects_webp_riff() {
        let v = validator();
        let head = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
        assert_eq!(v.sniff_mime(head), Some("image/webp"));
        let wav = b"RIFF\x00\x00\x00\x00WAVEfmt ";
        assert_eq!(v.sniff_mime(wav), None);
    }

    #[test]
    fn mismatch_rejected() {
        let v = validator();
        let head = [0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        let err = v.validate_head("application/pdf", &head).unwrap_err();
        assert!(matches!(err, StorageError::Validation(_)));
    }

    #[test]
    fn declared_matches_detected_ok() {
        let v = validator();
        let head = b"%PDF-1.7";
        assert_eq!(
            v.validate_head("application/pdf", head).unwrap(),
            "application/pdf"
        );
    }

    #[test]
    fn unknown_keeps_declared() {
        let v = validator();
        assert_eq!(
            v.validate_head("text/csv", b"id,name\n1,x\n").unwrap(),
            "text/csv"
        );
        assert_eq!(v.validate_head("", b"whatever").unwrap(), OCTET_STREAM);
    }

    #[test]
    fn short_head_is_safe() {
        let v = validator();
        assert_eq!(v.sniff_mime(&[0x89]), None);
        assert_eq!(v.sniff_mime(&[]), None);
    }

    #[test]
    fn size_limits() {
        let v = validator();
        assert!(v.validate_size(0).is_ok());
        assert!(v.validate_size(1024 * 1024).is_ok());
        assert!(matches!(
            v.validate_size(1024 * 1024 + 1),
            Err(StorageError::TooLarge(_))
        ));
    }

    #[test]
    fn mp4_ftyp_offset() {
        let v = validator();
        let mut head = vec![0, 0, 0, 32];
        head.extend_from_slice(b"ftypisom");
        assert_eq!(v.sniff_mime(&head), Some("video/mp4"));
    }
}
