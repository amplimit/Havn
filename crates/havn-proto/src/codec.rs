//! Newline-delimited JSON frame codec for the gateway↔agent socket.
//!
//! `serde_json::to_vec` emits compact JSON without literal newlines, so a single
//! `\n` terminator unambiguously separates frames. Per-frame size is bounded by
//! [`MAX_FRAME_BYTES`] to defend against malformed peers — exceeding it returns
//! [`FrameError::FrameTooLarge`] rather than allocating unbounded memory.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Maximum bytes accepted in a single frame (4 MiB). Larger payloads are
/// considered malformed and the connection is closed by the caller.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("frame exceeds {MAX_FRAME_BYTES} bytes (got {0})")]
    FrameTooLarge(usize),
}

/// Read one newline-delimited JSON frame from `reader`. Returns `Ok(None)` on
/// clean EOF (peer closed before sending more data). The reader must be
/// buffered (typically `BufReader::new(stream)`).
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    let mut buf = Vec::with_capacity(512);
    let mut total = 0usize;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // EOF
            if buf.is_empty() {
                return Ok(None);
            }
            // Trailing data without a terminator — treat as malformed.
            return Err(FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "frame missing trailing newline",
            )));
        }

        if let Some(nl_pos) = chunk.iter().position(|&b| b == b'\n') {
            // Copy up to (but not including) the newline.
            let take = nl_pos;
            if total + take > MAX_FRAME_BYTES {
                return Err(FrameError::FrameTooLarge(total + take));
            }
            buf.extend_from_slice(&chunk[..take]);
            // Consume bytes including the newline.
            reader.consume(take + 1);
            break;
        }

        // No newline yet — append the whole chunk and keep reading.
        if total + chunk.len() > MAX_FRAME_BYTES {
            return Err(FrameError::FrameTooLarge(total + chunk.len()));
        }
        buf.extend_from_slice(chunk);
        total += chunk.len();
        let consumed = chunk.len();
        reader.consume(consumed);
    }

    let parsed = serde_json::from_slice(&buf)?;
    Ok(Some(parsed))
}

/// Write `frame` as compact JSON followed by a `\n` terminator.
/// Flushes the underlying writer so the peer sees the frame immediately.
pub async fn write_frame<W, T>(writer: &mut W, frame: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut buf = serde_json::to_vec(frame)?;
    if buf.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge(buf.len()));
    }
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use serde::Deserialize;
    use tokio::io::BufReader;
    use tokio::io::duplex;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct Sample {
        kind: String,
        n: u32,
    }

    #[tokio::test]
    async fn round_trip_single_frame() {
        let (a, b) = duplex(1024);
        let mut a_writer = a;
        let mut b_reader = BufReader::new(b);

        let frame = Sample {
            kind: "ping".into(),
            n: 7,
        };
        write_frame(&mut a_writer, &frame).await.expect("write");
        drop(a_writer);

        let received: Sample = read_frame(&mut b_reader)
            .await
            .expect("read")
            .expect("frame");
        assert_eq!(received, frame);

        let eof: Option<Sample> = read_frame(&mut b_reader).await.expect("eof read");
        assert!(eof.is_none(), "expected clean EOF");
    }

    #[tokio::test]
    async fn round_trip_many_frames() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = a;
        let mut reader = BufReader::new(b);

        for i in 0..50 {
            write_frame(
                &mut writer,
                &Sample {
                    kind: "tick".into(),
                    n: i,
                },
            )
            .await
            .expect("write");
        }
        drop(writer);

        for i in 0..50 {
            let r: Sample = read_frame(&mut reader).await.expect("read").expect("frame");
            assert_eq!(
                r,
                Sample {
                    kind: "tick".into(),
                    n: i
                }
            );
        }
        let eof: Option<Sample> = read_frame(&mut reader).await.expect("eof");
        assert!(eof.is_none());
    }

    #[tokio::test]
    async fn frame_too_large_is_rejected() {
        let big = "x".repeat(MAX_FRAME_BYTES + 1);
        let frame = Sample { kind: big, n: 0 };
        let (a, _b) = duplex(1024);
        let mut writer = a;
        let err = write_frame(&mut writer, &frame)
            .await
            .expect_err("should reject");
        assert!(matches!(err, FrameError::FrameTooLarge(_)), "{err:?}");
    }

    #[tokio::test]
    async fn malformed_json_returns_error() {
        let (a, b) = duplex(1024);
        let mut writer = a;
        let mut reader = BufReader::new(b);
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"{not json}\n")
            .await
            .expect("write");
        drop(writer);

        let result: Result<Option<Sample>, _> = read_frame(&mut reader).await;
        assert!(matches!(result, Err(FrameError::Json(_))), "{result:?}");
    }
}
