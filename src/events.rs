//! Typed event payloads for Discord RPC voice events,
//! plus thin wrappers over [`crate::ipc::IpcClient::subscribe_event`].

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ipc::{IpcClient, IpcError};

/// Subset of `User` returned in voice payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordUser {
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub discriminator: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
}

/// Voice flags payload (mute, deaf, suppress, etc).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VoiceFlags {
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub deaf: bool,
    #[serde(default)]
    pub self_mute: bool,
    #[serde(default)]
    pub self_deaf: bool,
    #[serde(default)]
    pub suppress: bool,
}

/// `VOICE_STATE_*` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceState {
    pub user: DiscordUser,
    #[serde(default)]
    pub voice_state: VoiceFlags,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub volume: Option<f64>,
}

/// `SPEAKING_START` / `SPEAKING_STOP`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Speaking {
    pub user_id: String,
    #[serde(default)]
    pub channel_id: Option<String>,
}

/// `VOICE_CHANNEL_SELECT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSelect {
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
}

/// Subscribe to `VOICE_CHANNEL_SELECT` (no args).
pub async fn subscribe_channel_select(ipc: &IpcClient) -> Result<(), IpcError> {
    ipc.subscribe_event("VOICE_CHANNEL_SELECT", json!({})).await?;
    Ok(())
}

/// Subscribe to all per-channel events for `channel_id`.
pub async fn subscribe_voice_channel(ipc: &IpcClient, channel_id: &str) -> Result<(), IpcError> {
    let args = json!({ "channel_id": channel_id });
    ipc.subscribe_event("VOICE_STATE_CREATE", args.clone()).await?;
    ipc.subscribe_event("VOICE_STATE_UPDATE", args.clone()).await?;
    ipc.subscribe_event("VOICE_STATE_DELETE", args.clone()).await?;
    ipc.subscribe_event("SPEAKING_START", args.clone()).await?;
    ipc.subscribe_event("SPEAKING_STOP", args).await?;
    Ok(())
}

/// Unsubscribe from per-channel events for `channel_id`.
pub async fn unsubscribe_voice_channel(ipc: &IpcClient, channel_id: &str) -> Result<(), IpcError> {
    let args = json!({ "channel_id": channel_id });
    let _ = ipc.unsubscribe_event("VOICE_STATE_CREATE", args.clone()).await;
    let _ = ipc.unsubscribe_event("VOICE_STATE_UPDATE", args.clone()).await;
    let _ = ipc.unsubscribe_event("VOICE_STATE_DELETE", args.clone()).await;
    let _ = ipc.unsubscribe_event("SPEAKING_START", args.clone()).await;
    let _ = ipc.unsubscribe_event("SPEAKING_STOP", args).await;
    Ok(())
}

/// Fetch the user's currently selected voice channel via `GET_SELECTED_VOICE_CHANNEL`.
pub async fn get_selected_voice_channel(ipc: &IpcClient) -> Result<Value, IpcError> {
    ipc.command("GET_SELECTED_VOICE_CHANNEL", json!({})).await
}

/// Fetch the full participant list for `channel_id` via `GET_CHANNEL`.
pub async fn get_channel(ipc: &IpcClient, channel_id: &str) -> Result<Value, IpcError> {
    ipc.command("GET_CHANNEL", json!({ "channel_id": channel_id }))
        .await
}
