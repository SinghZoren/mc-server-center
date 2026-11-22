//! A message-oriented connection over any async byte stream.

use msm_api::Message;
use msm_api::codec::{CodecError, FrameDecoder, encode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Transport failure.
#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    /// Socket error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Malformed or oversized frame; the connection must be closed.
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// Sends and receives whole [`Message`]s.
#[derive(Debug)]
pub struct FramedConn<S> {
    io: S,
    decoder: FrameDecoder,
    max_frame: usize,
    read_buf: Box<[u8]>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> FramedConn<S> {
    /// Wraps a stream with a frame-size limit.
    pub fn new(io: S, max_frame: usize) -> Self {
        Self {
            io,
            decoder: FrameDecoder::new(max_frame),
            max_frame,
            read_buf: vec![0; 16 * 1024].into(),
        }
    }

    /// Receives the next message; `Ok(None)` on a clean end of stream.
    ///
    /// # Errors
    /// I/O or codec errors.
    pub async fn recv(&mut self) -> Result<Option<Message>, ConnError> {
        loop {
            if let Some(msg) = self.decoder.next_message()? {
                return Ok(Some(msg));
            }
            let n = self.io.read(&mut self.read_buf).await?;
            if n == 0 {
                return if self.decoder.buffered() == 0 {
                    Ok(None)
                } else {
                    Err(
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "truncated frame")
                            .into(),
                    )
                };
            }
            self.decoder.push(&self.read_buf[..n]);
        }
    }

    /// Sends one message and flushes.
    ///
    /// # Errors
    /// I/O or codec errors.
    pub async fn send(&mut self, msg: &Message) -> Result<(), ConnError> {
        let frame = encode(msg, self.max_frame)?;
        self.io.write_all(&frame).await?;
        self.io.flush().await?;
        Ok(())
    }

    /// Returns the underlying stream.
    pub fn into_inner(self) -> S {
        self.io
    }
}
