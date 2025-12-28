//! Minimal binary protocol shared by daemon and client.
use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;

pub type SessionId = u64;
pub type WindowId = u64;
pub type PaneId = u64;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
pub enum Capability {
    SplitVertical,
    SplitHorizontal,
    Tabs,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub enum ClientToDaemon {
    Hello {
        version: u16,
        capabilities: Vec<Capability>,
    },
    CreateSession {
        name: String,
    },
    Attach {
        session: SessionId,
    },
    Detach,
    Stdin {
        pane: PaneId,
        data: Vec<u8>,
    },
    Resize {
        pane: PaneId,
        cols: u16,
        rows: u16,
    },
    Ping(u64),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub enum DaemonToClient {
    HelloAck { version: u16 },
    SessionCreated { session: SessionId },
    AttachOk { pane: PaneId },
    PaneData { pane: PaneId, data: Vec<u8> },
    Error { message: String },
    Pong(u64),
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("peer requested incompatible protocol version {0}")]
    VersionMismatch(u16),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("encode error: {0}")]
    Encode(String),
}

pub fn encode_msg<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    bincode::serialize(msg).map_err(|e| ProtocolError::Encode(e.to_string()))
}

pub fn decode_msg<T: DeserializeOwned>(buf: &[u8]) -> Result<T, ProtocolError> {
    bincode::deserialize(buf).map_err(|e| ProtocolError::Decode(e.to_string()))
}

/// Simple length-prefixed framing (big endian u32 length).
pub mod frame {
    use std::io::{self, IoSlice, Read, Write};
    use std::os::unix::net::UnixStream;

    pub fn write(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
        let len = (payload.len() as u32).to_be_bytes();
        let mut len_sent = 0;
        let mut payload_sent = 0;
        let total = len.len() + payload.len();
        let mut written = 0;
        while written < total {
            // Rebuild slices each loop to avoid aliasing/borrow issues.
            let mut bufs = if len_sent < len.len() {
                [
                    IoSlice::new(&len[len_sent..]),
                    IoSlice::new(&payload[payload_sent..]),
                ]
            } else {
                [IoSlice::new(&payload[payload_sent..]), IoSlice::new(&[])]
            };
            let n = stream.write_vectored(&mut bufs)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write length-prefixed frame",
                ));
            }
            written += n;
            // Advance counters.
            if len_sent < len.len() {
                let advance = n.min(len.len() - len_sent);
                len_sent += advance;
                payload_sent += n.saturating_sub(advance);
            } else {
                payload_sent += n;
            }
        }
        Ok(())
    }

    pub fn read(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload)?;
        Ok(payload)
    }
}
