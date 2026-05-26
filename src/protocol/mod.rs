//! Wire protocol: framed bincode messages over the Unix socket.

use serde::{Deserialize, Serialize};

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

use std::io;

pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    let (value, _consumed) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(value)
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
}
