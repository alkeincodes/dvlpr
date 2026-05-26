//! Wire protocol: framed bincode messages over the Unix socket.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bumped whenever the wire format changes. Client and server must match exactly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Reject oversized frames to bound memory.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerHello {
    Ok { protocol_version: u32 },
    Reject { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// A rendered frame. `full` is always true in Phase 1.
    Frame { data: Vec<u8>, full: bool },
    Closed { reason: String },
}

pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let (value, consumed) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if consumed != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes after decoded frame",
        ));
    }
    Ok(value)
}

/// Write a length-prefixed bincode frame: `u32` big-endian length + payload.
pub async fn write_msg<W, T>(w: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let payload = encode(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_BYTES",
        ));
    }
    w.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed bincode frame. Returns `Ok(None)` on clean EOF.
pub async fn read_msg<R, T>(r: &mut R) -> io::Result<Option<T>>
where
    R: AsyncReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    // Probe the first byte: 0 bytes => clean EOF at a frame boundary (Ok(None)).
    // A short read AFTER the first byte is a truncated header => error, not EOF.
    if r.read(&mut len_buf[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut len_buf[1..]).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incoming frame exceeds MAX_FRAME_BYTES",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(Some(decode(&payload)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_one() {
        // Freeze the version so a wire change is a conscious, reviewed edit.
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn client_msg_round_trips_through_bincode() {
        let msg = ClientMsg::Resize { cols: 120, rows: 40 };
        let bytes = encode(&msg).unwrap();
        let back: ClientMsg = decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn server_msg_round_trips_through_bincode() {
        let msg = ServerMsg::Frame {
            data: b"\x1b[2J\x1b[Hhi".to_vec(),
            full: true,
        };
        let bytes = encode(&msg).unwrap();
        let back: ServerMsg = decode(&bytes).unwrap();
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn write_then_read_msg_round_trips() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let sent = ClientMsg::Input(b"ls -la\n".to_vec());
        write_msg(&mut a, &sent).await.unwrap();
        let got: ClientMsg = read_msg(&mut b).await.unwrap().unwrap();
        assert_eq!(sent, got);
    }

    #[tokio::test]
    async fn read_msg_returns_none_on_clean_eof() {
        let (a, mut b) = tokio::io::duplex(1024);
        drop(a); // close the writer end
        let got: Option<ClientMsg> = read_msg(&mut b).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn read_msg_errors_on_truncated_header() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        // Send only 2 of the 4 header bytes, then close: a truncated frame,
        // which must surface as an error, NOT a clean EOF.
        a.write_all(&[0u8, 0u8]).await.unwrap();
        drop(a);
        let result: io::Result<Option<ClientMsg>> = read_msg(&mut b).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_msg_rejects_oversized_frame() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        // Header announces a length above MAX_FRAME_BYTES.
        let len = (MAX_FRAME_BYTES as u32) + 1;
        a.write_all(&len.to_be_bytes()).await.unwrap();
        let err = read_msg::<_, ClientMsg>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
