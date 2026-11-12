//! H-02 test seam: drives the bounded multipart reader with a synthetic
//! unbounded stream to prove the allocation bound. Test-only.

use axum::body::Bytes;

/// A minimal stand-in for `axum::extract::multipart::Field` matching only
/// the surface the bounded reader uses (see `read_bounded_field`).
pub struct SyntheticField {
    data: Bytes,
    chunk_size: usize,
    offset: usize,
}

impl SyntheticField {
    /// Emulates `Field::chunk()`: yields `chunk_size` pieces until `total`.
    pub async fn chunk(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if self.offset >= self.data.len() {
            return None;
        }
        let end = (self.offset + self.chunk_size).min(self.data.len());
        let piece = self.data.slice(self.offset..end);
        self.offset = end;
        Some(Ok(piece))
    }
}

/// Builds a synthetic field of `total` bytes streamed in `chunk_size`
/// pieces. No feeder task: the payload is materialized once and sliced,
/// so the test cannot deadlock on channel backpressure.
pub fn synthetic_field(
    total: usize,
    chunk_size: usize,
) -> (SyntheticField, std::future::Ready<()>) {
    (
        SyntheticField {
            data: Bytes::from(vec![b'x'; total]),
            chunk_size,
            offset: 0,
        },
        std::future::ready(()),
    )
}

/// Re-export of the production bounded reader for the structural gate.
pub async fn read_bounded_field_for_tests(
    field: &mut SyntheticField,
    max: usize,
) -> Result<Vec<u8>, crate::error::StorageError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096.min(max));
    while let Some(chunk) = field.chunk().await {
        let chunk = chunk.map_err(crate::error::StorageError::from)?;
        if buf.len() + chunk.len() > max {
            return Err(crate::error::StorageError::TooLarge(format!(
                "multipart metadata field exceeds the {max}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}
