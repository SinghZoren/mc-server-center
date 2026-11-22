//! Length-prefixed framing: `u32` big-endian length followed by that many bytes of UTF-8 JSON.
//!
//! [`FrameDecoder`] is transport-agnostic and incremental: feed it whatever bytes arrived and
//! pop complete frames. It enforces a maximum frame size *before* buffering a frame, so a peer
//! cannot make us allocate unbounded memory.

use crate::Message;

/// Default maximum frame size for control traffic (1 MiB).
pub const MAX_CONTROL_FRAME: usize = 1024 * 1024;

/// Framing or decoding failure. Any of these should close the connection.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The peer announced a frame larger than allowed.
    #[error("frame of {len} bytes exceeds limit of {max}")]
    TooLarge {
        /// Announced length.
        len: usize,
        /// Configured limit.
        max: usize,
    },
    /// A zero-length frame.
    #[error("empty frame")]
    Empty,
    /// The payload is not a valid message.
    #[error("invalid message: {0}")]
    Json(#[from] serde_json::Error),
}

/// Encodes one message as a frame.
///
/// # Errors
/// [`CodecError::TooLarge`] if the encoded message exceeds `max`.
pub fn encode(msg: &Message, max: usize) -> Result<Vec<u8>, CodecError> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > max || u32::try_from(body.len()).is_err() {
        return Err(CodecError::TooLarge {
            len: body.len(),
            max,
        });
    }
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Incremental frame decoder.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    max: usize,
}

impl FrameDecoder {
    /// New decoder with a frame-size limit.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    /// Appends received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete message, `Ok(None)` if more bytes are needed.
    ///
    /// # Errors
    /// Any [`CodecError`]; the connection should then be closed.
    pub fn next_message(&mut self) -> Result<Option<Message>, CodecError> {
        let Some(len_bytes) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]);
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len == 0 {
            return Err(CodecError::Empty);
        }
        if len > self.max {
            return Err(CodecError::TooLarge { len, max: self.max });
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = serde_json::from_slice(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        Ok(Some(msg?))
    }

    /// Bytes buffered but not yet decoded.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ping(n: u64) -> Message {
        Message::Ping { nonce: n }
    }

    #[test]
    fn round_trip_split_across_reads() {
        let mut wire = encode(&ping(1), MAX_CONTROL_FRAME).unwrap();
        wire.extend(encode(&ping(2), MAX_CONTROL_FRAME).unwrap());
        let mut d = FrameDecoder::new(MAX_CONTROL_FRAME);
        let mut got = Vec::new();
        for b in wire {
            d.push(&[b]);
            while let Some(m) = d.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, vec![ping(1), ping(2)]);
        assert_eq!(d.buffered(), 0);
    }

    #[test]
    fn oversized_frame_is_rejected_before_buffering_body() {
        let mut d = FrameDecoder::new(16);
        d.push(&1_000_000u32.to_be_bytes());
        assert!(matches!(
            d.next_message(),
            Err(CodecError::TooLarge {
                len: 1_000_000,
                max: 16
            })
        ));
        assert!(encode(&ping(1), 4).is_err());
    }

    #[test]
    fn empty_and_garbage_frames_error() {
        let mut d = FrameDecoder::new(64);
        d.push(&0u32.to_be_bytes());
        assert!(matches!(d.next_message(), Err(CodecError::Empty)));
        let mut d = FrameDecoder::new(64);
        d.push(&3u32.to_be_bytes());
        d.push(b"{x}");
        assert!(matches!(d.next_message(), Err(CodecError::Json(_))));
    }

    proptest! {
        /// Arbitrary bytes never panic the decoder.
        #[test]
        fn decoder_never_panics(chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..64), 0..16)) {
            let mut d = FrameDecoder::new(1024);
            for c in chunks {
                d.push(&c);
                while let Ok(Some(_)) = d.next_message() {}
            }
        }
    }
}
