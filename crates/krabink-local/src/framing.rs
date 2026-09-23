//! Byte framing on a QUIC stream. Every stream opens with a one-byte
//! [`Lane`] tag written by the dialer; after that both directions carry
//! `u32` big-endian length-prefixed frames, each frame being exactly the
//! bytes [`krabink_core::ClientMsg::encode`] / [`krabink_core::ServerMsg::encode`]
//! produce (version byte + postcard).

use iroh::endpoint::{ReadExactError, RecvStream, SendStream};
use krabink_core::{ClientMsg, ServerMsg};

/// Upper bound on one frame: bounds a catch-up snapshot allocation.
pub const MAX_FRAME: usize = 64 << 20;

/// Which stream of a connection a frame travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Lane {
    /// Handshake, subscriptions, CRDT updates, catch-up: ordered, must arrive.
    Docs = 0,
    /// Wet ink and presence: its own stream so a large catch-up on the docs
    /// lane never delays live points.
    Ephemeral = 1,
}

impl Lane {
    pub fn tag(self) -> u8 {
        self as u8
    }

    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Docs),
            1 => Some(Self::Ephemeral),
            _ => None,
        }
    }

    pub fn of_client(msg: &ClientMsg) -> Self {
        match msg {
            ClientMsg::Ephemeral { .. } => Self::Ephemeral,
            _ => Self::Docs,
        }
    }

    pub fn of_server(msg: &ServerMsg) -> Self {
        match msg {
            ServerMsg::Ephemeral { .. } => Self::Ephemeral,
            _ => Self::Docs,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("stream ended mid-frame")]
    Truncated,
    #[error("stream closed: {0}")]
    Closed(String),
}

pub async fn write_frame(stream: &mut SendStream, frame: &[u8]) -> Result<(), FrameError> {
    if frame.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(frame.len()));
    }
    let len = (frame.len() as u32).to_be_bytes();
    stream
        .write_all(&len)
        .await
        .map_err(|err| FrameError::Closed(err.to_string()))?;
    stream
        .write_all(frame)
        .await
        .map_err(|err| FrameError::Closed(err.to_string()))
}

/// The next frame, or `None` when the peer finished the stream cleanly.
pub async fn read_frame(stream: &mut RecvStream) -> Result<Option<Vec<u8>>, FrameError> {
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len).await {
        Ok(()) => {}
        Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(ReadExactError::FinishedEarly(_)) => return Err(FrameError::Truncated),
        Err(ReadExactError::ReadError(err)) => return Err(FrameError::Closed(err.to_string())),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut frame = vec![0u8; len];
    match stream.read_exact(&mut frame).await {
        Ok(()) => Ok(Some(frame)),
        Err(ReadExactError::FinishedEarly(_)) => Err(FrameError::Truncated),
        Err(ReadExactError::ReadError(err)) => Err(FrameError::Closed(err.to_string())),
    }
}

/// Write the lane tag that opens a dialer-side stream.
pub async fn write_lane(stream: &mut SendStream, lane: Lane) -> Result<(), FrameError> {
    stream
        .write_all(&[lane.tag()])
        .await
        .map_err(|err| FrameError::Closed(err.to_string()))
}

/// Read the lane tag from a freshly accepted stream.
pub async fn read_lane(stream: &mut RecvStream) -> Result<Option<Lane>, FrameError> {
    let mut tag = [0u8; 1];
    match stream.read_exact(&mut tag).await {
        Ok(()) => Ok(Lane::from_tag(tag[0])),
        Err(ReadExactError::FinishedEarly(_)) => Err(FrameError::Truncated),
        Err(ReadExactError::ReadError(err)) => Err(FrameError::Closed(err.to_string())),
    }
}
