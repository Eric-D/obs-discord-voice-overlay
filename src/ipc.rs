//! Cross-platform Discord RPC IPC dispatcher.
//!
//! - On Unix: connects to `$XDG_RUNTIME_DIR/discord-ipc-{0..9}` (with `snap.discord/`
//!   and `app/com.discordapp.Discord/` prefixes).
//! - On Windows: connects to `\\.\pipe\discord-ipc-{0..9}`.
//!
//! Frame format: 8-byte LE header (`op: u32`, `len: u32`) + UTF-8 JSON body.
//!
//! Public surface:
//! - [`IpcClient`]: send commands and await responses; subscribe to events.
//! - [`spawn_ipc_loop`]: reconnect loop that keeps an [`IpcClient`] alive.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot, watch, Mutex};
use uuid::Uuid;

pub const OP_HANDSHAKE: u32 = 0;
pub const OP_FRAME: u32 = 1;
#[allow(dead_code)]
pub const OP_CLOSE: u32 = 2;
#[allow(dead_code)]
pub const OP_PING: u32 = 3;
#[allow(dead_code)]
pub const OP_PONG: u32 = 4;

/// Hard cap on the declared body length of a single IPC frame (16 MiB).
/// Anything larger is treated as a protocol violation: we drop the connection
/// without attempting to allocate.
pub const MAX_FRAME_LEN: u32 = 1 << 24;

/// Errors emitted by the IPC layer.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ipc disconnected")]
    Disconnected,
    #[error("ipc returned error: {0}")]
    Remote(String),
    #[error("no Discord IPC endpoint found - is Discord running?")]
    NoEndpoint,
    #[error("frame too large: declared {0} bytes, max {1}")]
    FrameTooLarge(u32, u32),
}

/// Encode a frame: 8-byte LE header + JSON body.
#[allow(dead_code)]
pub fn encode_frame(op: u32, body: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let bytes = serde_json::to_vec(body)?;
    let mut out = Vec::with_capacity(8 + bytes.len());
    out.extend_from_slice(&op.to_le_bytes());
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
    Ok(out)
}

/// Parse a frame previously produced by [`encode_frame`]. Returns the op and
/// JSON body. Errors if the input is malformed.
#[allow(dead_code)]
pub fn decode_frame(buf: &[u8]) -> Result<(u32, Value), IpcError> {
    if buf.len() < 8 {
        return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "frame shorter than 8-byte header",
        )));
    }
    let op = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge(len, MAX_FRAME_LEN));
    }
    let len = len as usize;
    if buf.len() < 8 + len {
        return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "frame body shorter than declared length",
        )));
    }
    let body: Value = serde_json::from_slice(&buf[8..8 + len])?;
    Ok((op, body))
}

async fn write_frame<S>(s: &mut S, op: u32, v: &Value) -> Result<(), IpcError>
where
    S: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(v)?;
    s.write_all(&op.to_le_bytes()).await?;
    s.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    s.write_all(&bytes).await?;
    Ok(())
}

/// Outcome of [`read_frame_raw`]: a successful frame, or a frame body whose
/// JSON failed to parse — the latter is recoverable: the reader loop logs and
/// continues with the next frame.
enum RawFrame {
    Ok(u32, Value),
    Malformed { op: u32, body: Vec<u8>, err: serde_json::Error },
}

#[cfg(test)]
impl std::fmt::Debug for RawFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RawFrame::Ok(op, _) => write!(f, "RawFrame::Ok(op={op})"),
            RawFrame::Malformed { op, .. } => write!(f, "RawFrame::Malformed(op={op})"),
        }
    }
}

async fn read_frame_raw<S>(s: &mut S) -> Result<RawFrame, IpcError>
where
    S: AsyncRead + Unpin,
{
    let mut h = [0u8; 8];
    s.read_exact(&mut h).await?;
    let op = u32::from_le_bytes(h[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(h[4..8].try_into().unwrap());
    if len > MAX_FRAME_LEN {
        return Err(IpcError::FrameTooLarge(len, MAX_FRAME_LEN));
    }
    let mut buf = vec![0u8; len as usize];
    s.read_exact(&mut buf).await?;
    match serde_json::from_slice::<Value>(&buf) {
        Ok(body) => Ok(RawFrame::Ok(op, body)),
        Err(err) => Ok(RawFrame::Malformed { op, body: buf, err }),
    }
}

async fn read_frame<S>(s: &mut S) -> Result<(u32, Value), IpcError>
where
    S: AsyncRead + Unpin,
{
    match read_frame_raw(s).await? {
        RawFrame::Ok(op, body) => Ok((op, body)),
        RawFrame::Malformed { err, .. } => Err(IpcError::Json(err)),
    }
}

#[cfg(unix)]
async fn connect_ipc() -> Result<tokio::net::UnixStream, IpcError> {
    use std::path::PathBuf;
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .or_else(|| std::env::var("TMPDIR").ok())
        .unwrap_or_else(|| "/tmp".into());
    let prefixes = ["", "snap.discord/", "app/com.discordapp.Discord/"];
    for p in prefixes {
        for i in 0..10u8 {
            let path = PathBuf::from(format!("{}/{}discord-ipc-{}", dir, p, i));
            if let Ok(s) = tokio::net::UnixStream::connect(&path).await {
                tracing::info!("ipc: connected to {}", path.display());
                return Ok(s);
            }
        }
    }
    Err(IpcError::NoEndpoint)
}

#[cfg(windows)]
async fn connect_ipc() -> Result<tokio::net::windows::named_pipe::NamedPipeClient, IpcError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    for i in 0..10u8 {
        let path = format!(r"\\.\pipe\discord-ipc-{}", i);
        if let Ok(client) = ClientOptions::new().open(&path) {
            tracing::info!("ipc: connected to {}", path);
            return Ok(client);
        }
    }
    Err(IpcError::NoEndpoint)
}

/// Event broadcast by the IPC reader (Discord `DISPATCH` frames).
#[derive(Debug, Clone)]
pub struct IpcEvent {
    pub evt: String,
    pub data: Value,
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// Handle to a connected IPC session. Cheap to clone.
#[derive(Clone)]
pub struct IpcClient {
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    pending: PendingMap,
    events_tx: broadcast::Sender<IpcEvent>,
}

impl IpcClient {
    /// Subscribe to broadcast events from the reader task.
    pub fn subscribe(&self) -> broadcast::Receiver<IpcEvent> {
        self.events_tx.subscribe()
    }

    /// Register a oneshot under a fresh nonce, attempt to write the frame.
    /// If the write fails, the nonce is removed from the pending map so the
    /// entry doesn't leak when the connection is broken.
    async fn dispatch(&self, payload: Value, nonce: String) -> Result<oneshot::Receiver<Value>, IpcError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(nonce.clone(), tx);
        }
        let write_res = {
            let mut w = self.writer.lock().await;
            write_frame(&mut *w, OP_FRAME, &payload).await
        };
        if let Err(e) = write_res {
            // Drop the pending entry so the map doesn't grow without bound on
            // a broken pipe.
            let mut pending = self.pending.lock().await;
            pending.remove(&nonce);
            return Err(e);
        }
        Ok(rx)
    }

    /// Send a `cmd` frame with the given args; awaits the matching nonce reply.
    pub async fn command(&self, cmd: &str, args: Value) -> Result<Value, IpcError> {
        let nonce = Uuid::new_v4().to_string();
        let payload = json!({ "cmd": cmd, "args": args, "nonce": nonce });
        let rx = self.dispatch(payload, nonce).await?;
        let resp = rx.await.map_err(|_| IpcError::Disconnected)?;
        if resp.get("evt").and_then(|v| v.as_str()) == Some("ERROR") {
            let msg = resp
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(IpcError::Remote(msg));
        }
        Ok(resp)
    }

    /// Send a SUBSCRIBE for the given event with optional args.
    pub async fn subscribe_event(&self, evt: &str, args: Value) -> Result<Value, IpcError> {
        let nonce = Uuid::new_v4().to_string();
        let payload = json!({ "cmd": "SUBSCRIBE", "evt": evt, "args": args, "nonce": nonce });
        let rx = self.dispatch(payload, nonce).await?;
        let resp = rx.await.map_err(|_| IpcError::Disconnected)?;
        if resp.get("evt").and_then(|v| v.as_str()) == Some("ERROR") {
            let msg = resp
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(IpcError::Remote(msg));
        }
        Ok(resp)
    }

    /// Send an UNSUBSCRIBE for the given event.
    pub async fn unsubscribe_event(&self, evt: &str, args: Value) -> Result<Value, IpcError> {
        let nonce = Uuid::new_v4().to_string();
        let payload = json!({ "cmd": "UNSUBSCRIBE", "evt": evt, "args": args, "nonce": nonce });
        let rx = self.dispatch(payload, nonce).await?;
        let resp = rx.await.map_err(|_| IpcError::Disconnected)?;
        Ok(resp)
    }
}

/// Connection lifecycle signal forwarded to upstream tasks.
#[derive(Clone)]
pub enum IpcStatus {
    Connected(IpcClient),
    Disconnected,
}

impl std::fmt::Debug for IpcStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcStatus::Connected(_) => f.write_str("IpcStatus::Connected"),
            IpcStatus::Disconnected => f.write_str("IpcStatus::Disconnected"),
        }
    }
}

/// Spawn a background task that connects to Discord IPC, performs the
/// handshake, and re-establishes the connection on drop.
///
/// Each fresh [`IpcClient`] is published via [`watch`] so subscribers always
/// see the latest state and can never lag past a `Connected`/`Disconnected`
/// transition.
pub fn spawn_ipc_loop(client_id: String) -> watch::Receiver<IpcStatus> {
    let (status_tx, status_rx) = watch::channel::<IpcStatus>(IpcStatus::Disconnected);
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(2);
        let max_backoff = Duration::from_secs(30);
        loop {
            match connect_and_handshake(&client_id, status_tx.clone()).await {
                Ok(()) => {
                    backoff = Duration::from_secs(2);
                }
                Err(e) => {
                    tracing::warn!("ipc: connection error: {e}");
                }
            }
            let _ = status_tx.send(IpcStatus::Disconnected);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    });
    status_rx
}

#[cfg(unix)]
async fn connect_and_handshake(
    client_id: &str,
    status_tx: watch::Sender<IpcStatus>,
) -> Result<(), IpcError> {
    let stream = connect_ipc().await?;
    let (read_half, write_half) = stream.into_split();
    run_session(client_id, status_tx, Box::new(read_half), Box::new(write_half)).await
}

#[cfg(windows)]
async fn connect_and_handshake(
    client_id: &str,
    status_tx: watch::Sender<IpcStatus>,
) -> Result<(), IpcError> {
    use tokio::io::split;
    let stream = connect_ipc().await?;
    let (read_half, write_half) = split(stream);
    run_session(client_id, status_tx, Box::new(read_half), Box::new(write_half)).await
}

async fn run_session(
    client_id: &str,
    status_tx: watch::Sender<IpcStatus>,
    mut reader: Box<dyn AsyncRead + Send + Unpin>,
    mut writer: Box<dyn AsyncWrite + Send + Unpin>,
) -> Result<(), IpcError> {
    write_frame(
        &mut writer,
        OP_HANDSHAKE,
        &json!({ "v": 1, "client_id": client_id }),
    )
    .await?;
    let (_, ready) = read_frame(&mut reader).await?;
    tracing::info!("ipc: handshake ready evt={:?}", ready.get("evt"));

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let (events_tx, _events_rx) = broadcast::channel::<IpcEvent>(64);
    let writer_arc: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>> = Arc::new(Mutex::new(writer));

    let client = IpcClient {
        writer: writer_arc,
        pending: pending.clone(),
        events_tx: events_tx.clone(),
    };

    let _ = status_tx.send(IpcStatus::Connected(client));

    // Reader loop: dispatch frames to pending (by nonce) or broadcast as events.
    loop {
        match read_frame_raw(&mut reader).await {
            Ok(RawFrame::Ok(_op, body)) => {
                if let Some(nonce) = body.get("nonce").and_then(|v| v.as_str()) {
                    let nonce = nonce.to_string();
                    let mut pending = pending.lock().await;
                    if let Some(tx) = pending.remove(&nonce) {
                        let _ = tx.send(body);
                        continue;
                    }
                }
                if let Some(evt) = body.get("evt").and_then(|v| v.as_str()) {
                    let event = IpcEvent {
                        evt: evt.to_string(),
                        data: body.get("data").cloned().unwrap_or(Value::Null),
                    };
                    let _ = events_tx.send(event);
                }
            }
            Ok(RawFrame::Malformed { op, body, err }) => {
                // A single bad frame must not tear down the session — Discord
                // could send something we don't yet model. Log and keep reading.
                let preview = String::from_utf8_lossy(&body);
                let truncated: String = preview.chars().take(256).collect();
                tracing::warn!(
                    "ipc: skipping malformed frame op={op} err={err} body={truncated}"
                );
            }
            Err(e) => {
                tracing::warn!("ipc: reader exit: {e}");
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frame_roundtrip_simple() {
        let body = json!({ "cmd": "AUTHORIZE", "args": { "client_id": "abc" }, "nonce": "n1" });
        let bytes = encode_frame(OP_FRAME, &body).unwrap();
        let (op, parsed) = decode_frame(&bytes).unwrap();
        assert_eq!(op, OP_FRAME);
        assert_eq!(parsed, body);
    }

    #[test]
    fn frame_roundtrip_empty_object() {
        let body = json!({});
        let bytes = encode_frame(OP_HANDSHAKE, &body).unwrap();
        let (op, parsed) = decode_frame(&bytes).unwrap();
        assert_eq!(op, OP_HANDSHAKE);
        assert_eq!(parsed, body);
    }

    #[test]
    fn frame_header_le_layout() {
        let body = json!({ "x": 1 });
        let bytes = encode_frame(OP_FRAME, &body).unwrap();
        // First 4 bytes must be op LE.
        assert_eq!(&bytes[0..4], &OP_FRAME.to_le_bytes());
        let body_bytes = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            &bytes[4..8],
            &(body_bytes.len() as u32).to_le_bytes(),
            "len header must be LE"
        );
        assert_eq!(&bytes[8..], body_bytes.as_slice());
    }

    #[test]
    fn frame_decode_rejects_short_header() {
        let r = decode_frame(&[0u8; 4]);
        assert!(r.is_err());
    }

    #[test]
    fn frame_decode_rejects_truncated_body() {
        // Header says 100 bytes, but body is empty.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&OP_FRAME.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes());
        let r = decode_frame(&bytes);
        assert!(r.is_err());
    }

    #[test]
    fn frame_decode_rejects_oversize_len() {
        // Header declares > MAX_FRAME_LEN bytes — must error before allocation.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&OP_FRAME.to_le_bytes());
        bytes.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let r = decode_frame(&bytes);
        assert!(matches!(r, Err(IpcError::FrameTooLarge(_, _))));
    }

    #[tokio::test]
    async fn read_frame_rejects_oversize_len() {
        // Build a frame whose header declares MAX+1 bytes; we don't even need
        // to follow it with data — the cap must trigger before the body read.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&OP_FRAME.to_le_bytes());
        bytes.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let mut cursor = std::io::Cursor::new(bytes);
        let res = read_frame(&mut cursor).await;
        assert!(matches!(res, Err(IpcError::FrameTooLarge(_, _))));
    }

    #[tokio::test]
    async fn read_frame_raw_returns_malformed_for_bad_json() {
        // Encode a frame whose body is not JSON. The raw reader should hand
        // the bytes back as `Malformed` so the session loop can skip + log.
        let body = b"not json";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&OP_FRAME.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        let mut cursor = std::io::Cursor::new(bytes);
        match read_frame_raw(&mut cursor).await {
            Ok(RawFrame::Malformed { op, body: b, .. }) => {
                assert_eq!(op, OP_FRAME);
                assert_eq!(b, body);
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
