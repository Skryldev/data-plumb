//! FastCDC content-defined chunking over an async byte stream.
//!
//! Wraps `fastcdc::v2020::AsyncStreamCDC` (tokio `AsyncRead` based), which
//! maintains its own rolling carry buffer, so chunk boundaries depend only on
//! content — never on how the input stream was packetised. This property is
//! asserted in the unit tests below.

use async_stream::try_stream;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio::io::AsyncRead;

use crate::config::ChunkingConfig;
use crate::error::StorageError;

/// Adapts an arbitrary byte stream into tokio's `AsyncRead` so it can feed
/// the fastcdc chunker.
struct StreamAdapter<S> {
    inner: S,
    buf: bytes::BytesMut,
}

impl<S> StreamAdapter<S>
where
    S: Stream<Item = Result<Bytes, StorageError>> + Unpin,
{
    fn new(inner: S) -> Self {
        Self {
            inner,
            buf: bytes::BytesMut::with_capacity(64 * 1024),
        }
    }
}

impl<S> AsyncRead for StreamAdapter<S>
where
    S: Stream<Item = Result<Bytes, StorageError>> + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.buf.is_empty() {
            match self.inner.poll_next_unpin(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    if bytes.is_empty() {
                        return self.poll_read(cx, out);
                    }
                    self.buf.extend_from_slice(&bytes);
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    // The original StorageError is carried inside the
                    // io::Error so `chunk_stream` can recover the precise
                    // variant (TooLarge, Validation, ...) downstream.
                    return std::task::Poll::Ready(Err(std::io::Error::other(e)));
                }
                std::task::Poll::Ready(None) => {
                    return std::task::Poll::Ready(Ok(())); // EOF
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        let n = self.buf.len().min(out.remaining());
        let chunk = self.buf.split_to(n);
        out.put_slice(&chunk);
        std::task::Poll::Ready(Ok(()))
    }
}

/// Transforms a byte stream into a stream of `(absolute_offset, chunk)`.
pub fn chunk_stream<S>(
    input: S,
    cfg: &ChunkingConfig,
) -> impl Stream<Item = Result<(u64, Bytes), StorageError>>
where
    S: Stream<Item = Result<Bytes, StorageError>> + Unpin,
{
    let cfg = ChunkingConfig {
        min: cfg.min,
        avg: cfg.avg,
        max: cfg.max,
    };
    try_stream! {
        let adapter = StreamAdapter::new(input);
        let mut chunker = fastcdc::v2020::AsyncStreamCDC::new(adapter, cfg.min, cfg.avg, cfg.max);
        let mut stream = Box::pin(chunker.as_stream());
        while let Some(item) = stream.as_mut().next().await {
            let c = item.map_err(recover_stream_error)?;
            yield (c.offset, Bytes::from(c.data));
        }
    }
}

/// Recovers the original [`StorageError`] from a fastcdc error. Input
/// stream errors cross the `AsyncRead` boundary wrapped as `io::Error`
/// (see [`StreamAdapter`]); everything else becomes an `Internal` error.
fn recover_stream_error(e: fastcdc::v2020::Error) -> StorageError {
    match e {
        fastcdc::v2020::Error::IoError(io) => {
            let msg = io.to_string();
            io.into_inner()
                .and_then(|inner| inner.downcast::<StorageError>().ok().map(|b| *b))
                .unwrap_or_else(|| StorageError::Internal(format!("chunker: {msg}")))
        }
        other => StorageError::Internal(format!("chunker: {other}")),
    }
}

/// Synchronous helper used by benchmarks and tests: chunks an in-memory
/// buffer in one pass.
pub fn chunk_slice(data: &[u8], cfg: &ChunkingConfig) -> Vec<(u64, Bytes)> {
    let scanner = fastcdc::v2020::FastCDC::new(data, cfg.min, cfg.avg, cfg.max);
    let mut out = Vec::new();
    let mut offset = 0u64;
    for c in scanner {
        out.push((
            offset,
            Bytes::copy_from_slice(&data[c.offset..c.offset + c.length]),
        ));
        offset += c.length as u64;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn cfg() -> ChunkingConfig {
        ChunkingConfig {
            min: 1024,
            avg: 4096,
            max: 16384,
        }
    }

    /// Deterministic pseudo-random bytes so CDC cut points are exercised.
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push((state >> 33) as u8);
        }
        out
    }

    fn piece_stream(
        data: &[u8],
        piece: usize,
    ) -> impl Stream<Item = Result<Bytes, StorageError>> + Unpin {
        stream::iter(
            data.chunks(piece)
                .map(|c| Ok::<Bytes, StorageError>(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
    }

    async fn collect(data: &[u8], piece: usize, cfg: &ChunkingConfig) -> Vec<(u64, Bytes)> {
        let s = chunk_stream(piece_stream(data, piece), cfg);
        s.map(|r| r.expect("chunk_stream error")).collect().await
    }

    #[tokio::test]
    async fn reassembles_exactly() {
        let data = pseudo_random(1_000_000, 42);
        for piece in [997usize, 4096, 65_533] {
            let chunks = collect(&data, piece, &cfg()).await;
            let total: usize = chunks.iter().map(|(_, b)| b.len()).sum();
            assert_eq!(total, data.len(), "piece={piece}");
            let mut offset = 0u64;
            for (o, b) in &chunks {
                assert_eq!(*o, offset);
                offset += b.len() as u64;
            }
        }
    }

    #[tokio::test]
    async fn boundaries_independent_of_packetisation() {
        let data = pseudo_random(2_000_000, 7);
        let a = collect(&data, 1000, &cfg()).await;
        let b = collect(&data, 250_000, &cfg()).await;
        let a_hashes: Vec<[u8; 32]> = a
            .iter()
            .map(|(_, c)| crate::core::hasher::hash_bytes(c))
            .collect();
        let b_hashes: Vec<[u8; 32]> = b
            .iter()
            .map(|(_, c)| crate::core::hasher::hash_bytes(c))
            .collect();
        assert_eq!(a_hashes, b_hashes);
    }

    #[tokio::test]
    async fn respects_min_and_max() {
        let data = pseudo_random(3_000_000, 99);
        let chunks = collect(&data, 8192, &cfg()).await;
        let cfg = cfg();
        for (i, (_, c)) in chunks.iter().enumerate() {
            let is_last = i == chunks.len() - 1;
            if !is_last {
                assert!(c.len() >= cfg.min, "chunk {} too small: {}", i, c.len());
            }
            assert!(c.len() <= cfg.max, "chunk {} too large: {}", i, c.len());
        }
        assert!(chunks.len() > 100);
    }

    #[tokio::test]
    async fn empty_input_yields_nothing() {
        let chunks = collect(&[], 100, &cfg()).await;
        assert!(chunks.is_empty());
    }

    #[tokio::test]
    async fn tiny_file_single_chunk() {
        let chunks = collect(b"tiny payload", 3, &cfg()).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1.as_ref(), b"tiny payload");
        assert_eq!(chunks[0].0, 0);
    }

    #[tokio::test]
    async fn matches_single_pass_scanner() {
        let data = pseudo_random(400_000, 1234);
        let streamed = collect(&data, 13_000, &cfg()).await;
        let onepass = chunk_slice(&data, &cfg());
        let s_hashes: Vec<[u8; 32]> = streamed
            .iter()
            .map(|(_, c)| crate::core::hasher::hash_bytes(c))
            .collect();
        let o_hashes: Vec<[u8; 32]> = onepass
            .iter()
            .map(|(_, c)| crate::core::hasher::hash_bytes(c))
            .collect();
        assert_eq!(s_hashes, o_hashes);
    }

    #[tokio::test]
    async fn default_cfg_sizes_hold() {
        let cfg = ChunkingConfig {
            min: 8192,
            avg: 32768,
            max: 131072,
        };
        let data = pseudo_random(8 * 1024 * 1024, 2024);
        let chunks = collect(&data, 65_000, &cfg).await;
        for (i, (_, c)) in chunks.iter().enumerate() {
            let is_last = i == chunks.len() - 1;
            assert!(c.len() <= cfg.max);
            if !is_last {
                assert!(c.len() >= cfg.min);
            }
        }
        assert!(chunks.len() > 8 * 1024 * 1024 / 131072);
    }

    #[tokio::test]
    async fn propagates_upstream_errors() {
        // An upstream error must surface (the CDC scanner aborts; buffered
        // bytes that never reached a cut point are discarded with it).
        let s = stream::iter(vec![
            Ok(Bytes::from_static(b"abc")),
            Err(StorageError::Internal("boom".into())),
        ]);
        let mut st = Box::pin(chunk_stream(s, &cfg()));
        let first = st.as_mut().next().await.expect("an item");
        assert!(matches!(first, Err(StorageError::Internal(_))));
    }

    #[tokio::test]
    async fn error_in_stream_surfaces() {
        let data = pseudo_random(300_000, 55);
        let mut items: Vec<Result<Bytes, StorageError>> = data
            .chunks(1000)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        items.insert(50, Err(StorageError::Internal("boom".into())));
        let mut got_err = false;
        let mut st = Box::pin(chunk_stream(stream::iter(items), &cfg()));
        while let Some(item) = st.as_mut().next().await {
            if item.is_err() {
                got_err = true;
                break;
            }
        }
        assert!(got_err);
    }
}
