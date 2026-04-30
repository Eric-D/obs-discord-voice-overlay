//! Overlay state: the snapshot of "who's in the channel + who's speaking", plus
//! merge logic for incoming Discord RPC events.
//!
//! Two outbound channels are exposed:
//! - [`watch::Receiver`] for full snapshots (used by SSE on connect/full-resync).
//! - [`broadcast::Receiver`] for incremental [`StateDelta`] pushes.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};

use crate::events::{DiscordUser, VoiceFlags};

/// One participant card on the overlay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Participant {
    pub user_id: String,
    pub username: String,
    /// Display name to show on the card (nick > global_name > username).
    pub display_name: String,
    pub avatar_url: String,
    pub mute: bool,
    pub deaf: bool,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub speaking: bool,
}

/// Full overlay snapshot. SSE pushes this on connect and channel switch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlayState {
    pub channel_id: Option<String>,
    pub participants: Vec<Participant>,
    /// True when IPC + Discord client are both healthy.
    pub connected: bool,
    /// True when the binary is running without a usable persistent config
    /// (missing, decrypt failure, machine-id unavailable, etc.). When set,
    /// the tray icon goes amber and overrides the connected/channel mapping.
    /// Not serialized to the SSE wire — purely internal.
    #[serde(skip)]
    pub needs_setup: bool,
}

/// Coarse tray-icon state derived from `(connected, channel_id, needs_setup)`.
///
/// Mapping (frozen by the spec):
/// - `needs_setup=true`             -> `NeedsSetup` (overrides everything)
/// - `(connected=false, _)`         -> `DiscordOffline`
/// - `(connected=true, None)`       -> `Idle`
/// - `(connected=true, Some(_))`    -> `InVoice`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayState {
    DiscordOffline,
    Idle,
    InVoice,
    NeedsSetup,
}

impl TrayState {
    /// Pure helper used both by [`OverlayStore`] and unit tests.
    pub fn from_parts(connected: bool, channel_id: Option<&str>) -> Self {
        match (connected, channel_id) {
            (false, _) => TrayState::DiscordOffline,
            (true, None) => TrayState::Idle,
            (true, Some(_)) => TrayState::InVoice,
        }
    }

    /// Helper that honors the setup-mode override.
    pub fn from_parts_with_setup(
        needs_setup: bool,
        connected: bool,
        channel_id: Option<&str>,
    ) -> Self {
        if needs_setup {
            TrayState::NeedsSetup
        } else {
            TrayState::from_parts(connected, channel_id)
        }
    }
}

impl From<&OverlayState> for TrayState {
    fn from(s: &OverlayState) -> Self {
        TrayState::from_parts_with_setup(s.needs_setup, s.connected, s.channel_id.as_deref())
    }
}

/// Incremental delta sent to the browser between full snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateDelta {
    State(OverlayState),
    ParticipantJoin(Participant),
    ParticipantLeave { user_id: String },
    SpeakingStart { user_id: String },
    SpeakingStop { user_id: String },
    VoiceStateUpdate(Participant),
    Connection { connected: bool },
}

/// Inner state held by [`OverlayStore`].
#[derive(Debug, Default, Clone)]
struct Inner {
    channel_id: Option<String>,
    participants: HashMap<String, Participant>,
    speaking: HashSet<String>,
    connected: bool,
    needs_setup: bool,
}

impl Inner {
    fn snapshot(&self) -> OverlayState {
        let mut list: Vec<Participant> = self.participants.values().cloned().collect();
        // Stable order: primary by display_name, tiebreak by user_id so two
        // people with the same display name don't reshuffle between snapshots.
        list.sort_by(|a, b| {
            a.display_name
                .cmp(&b.display_name)
                .then_with(|| a.user_id.cmp(&b.user_id))
        });
        OverlayState {
            channel_id: self.channel_id.clone(),
            participants: list,
            connected: self.connected,
            needs_setup: self.needs_setup,
        }
    }
}

/// Mutable store with watch + broadcast outputs.
pub struct OverlayStore {
    inner: std::sync::Mutex<Inner>,
    snapshot_tx: watch::Sender<OverlayState>,
    deltas_tx: broadcast::Sender<StateDelta>,
    tray_tx: watch::Sender<TrayState>,
}

impl OverlayStore {
    pub fn new() -> Self {
        let (snapshot_tx, _) = watch::channel(OverlayState::default());
        let (deltas_tx, _) = broadcast::channel(128);
        let (tray_tx, _) = watch::channel(TrayState::DiscordOffline);
        Self {
            inner: std::sync::Mutex::new(Inner::default()),
            snapshot_tx,
            deltas_tx,
            tray_tx,
        }
    }

    #[allow(dead_code)]
    pub fn subscribe_snapshot(&self) -> watch::Receiver<OverlayState> {
        self.snapshot_tx.subscribe()
    }

    pub fn subscribe_deltas(&self) -> broadcast::Receiver<StateDelta> {
        self.deltas_tx.subscribe()
    }

    /// Subscribe to the coarse tray-icon state. The tray UI polls this
    /// channel to decide when to swap its icon and tooltip.
    pub fn subscribe_tray_state(&self) -> watch::Receiver<TrayState> {
        self.tray_tx.subscribe()
    }

    /// Internal helper: derive the tray state from an [`OverlayState`] and
    /// broadcast it. Cheap no-op when the value hasn't changed (watch
    /// channels coalesce identical sends to subscribers via `has_changed`).
    fn push_tray_state(&self, snap: &OverlayState) {
        let next = TrayState::from(snap);
        let _ = self.tray_tx.send_if_modified(|cur| {
            if *cur != next {
                *cur = next;
                true
            } else {
                false
            }
        });
    }

    pub fn snapshot(&self) -> OverlayState {
        self.inner.lock().unwrap().snapshot()
    }

    /// Mark setup-mode. Used when the persistent config is missing,
    /// decrypt-failed, or otherwise unusable — drives the tray icon to
    /// `NeedsSetup` regardless of IPC/voice state.
    pub fn set_needs_setup(&self, needs_setup: bool) {
        let snap = {
            let mut inner = self.inner.lock().unwrap();
            inner.needs_setup = needs_setup;
            inner.snapshot()
        };
        self.push_tray_state(&snap);
        let _ = self.snapshot_tx.send(snap);
    }

    /// Mark IPC connectivity. Triggers a Connection delta. The snapshot is
    /// taken inside the same critical section as the mutation so the
    /// `Connection` delta and the watch-channel snapshot can never disagree.
    pub fn set_connected(&self, connected: bool) {
        let snap = {
            let mut inner = self.inner.lock().unwrap();
            inner.connected = connected;
            if !connected {
                inner.speaking.clear();
                for p in inner.participants.values_mut() {
                    p.speaking = false;
                }
            }
            inner.snapshot()
        };
        self.push_tray_state(&snap);
        let _ = self.snapshot_tx.send(snap);
        let _ = self.deltas_tx.send(StateDelta::Connection { connected });
    }

    /// Replace the current channel. Pushes a fresh `State` snapshot.
    pub fn set_channel(&self, channel_id: Option<String>, participants: Vec<Participant>) {
        let snap = {
            let mut inner = self.inner.lock().unwrap();
            inner.channel_id = channel_id;
            inner.participants.clear();
            inner.speaking.clear();
            for p in participants {
                inner.participants.insert(p.user_id.clone(), p);
            }
            inner.snapshot()
        };
        self.push_tray_state(&snap);
        let _ = self.snapshot_tx.send(snap.clone());
        let _ = self.deltas_tx.send(StateDelta::State(snap));
    }

    /// Merge a participant create/update event.
    pub fn upsert_participant(&self, mut p: Participant) {
        let (delta, snap) = {
            let mut inner = self.inner.lock().unwrap();
            let is_join = !inner.participants.contains_key(&p.user_id);
            // Carry over existing speaking flag.
            p.speaking = inner.speaking.contains(&p.user_id);
            inner.participants.insert(p.user_id.clone(), p.clone());
            let snap = inner.snapshot();
            let delta = if is_join {
                StateDelta::ParticipantJoin(p)
            } else {
                StateDelta::VoiceStateUpdate(p)
            };
            (delta, snap)
        };
        let _ = self.snapshot_tx.send(snap);
        let _ = self.deltas_tx.send(delta);
    }

    /// Remove a participant by id.
    pub fn remove_participant(&self, user_id: &str) {
        let snap_and_delta = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner.participants.remove(user_id).is_some();
            inner.speaking.remove(user_id);
            if removed {
                Some(inner.snapshot())
            } else {
                None
            }
        };
        if let Some(snap) = snap_and_delta {
            let _ = self.snapshot_tx.send(snap);
            let _ = self.deltas_tx.send(StateDelta::ParticipantLeave {
                user_id: user_id.to_string(),
            });
        }
    }

    /// Set the speaking flag for a user. Returns true if state changed.
    ///
    /// If `user_id` isn't a known participant we return `false` immediately
    /// **without** mutating `inner.speaking` — otherwise stale entries from
    /// users who never join would accumulate ("ghost" speaking ids) and
    /// silently bias future `upsert_participant` calls.
    pub fn set_speaking(&self, user_id: &str, speaking: bool) -> bool {
        let snap_and_delta = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.participants.contains_key(user_id) {
                return false;
            }
            let changed = if speaking {
                inner.speaking.insert(user_id.to_string())
            } else {
                inner.speaking.remove(user_id)
            };
            if let Some(p) = inner.participants.get_mut(user_id) {
                p.speaking = speaking;
            }
            if changed {
                Some(inner.snapshot())
            } else {
                None
            }
        };
        if let Some(snap) = snap_and_delta {
            let _ = self.snapshot_tx.send(snap);
            let delta = if speaking {
                StateDelta::SpeakingStart {
                    user_id: user_id.to_string(),
                }
            } else {
                StateDelta::SpeakingStop {
                    user_id: user_id.to_string(),
                }
            };
            let _ = self.deltas_tx.send(delta);
            true
        } else {
            false
        }
    }
}

impl Default for OverlayStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `Participant` from typed inputs (used by both initial fetch + merges).
pub fn participant_from(
    user: &DiscordUser,
    nick: Option<&str>,
    flags: &VoiceFlags,
    speaking: bool,
) -> Participant {
    let display_name = nick
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            user.global_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| user.username.clone());
    Participant {
        user_id: user.id.clone(),
        username: user.username.clone(),
        display_name,
        avatar_url: avatar_url(&user.id, user.avatar.as_deref()),
        mute: flags.mute,
        deaf: flags.deaf,
        self_mute: flags.self_mute,
        self_deaf: flags.self_deaf,
        speaking,
    }
}

/// Compute the CDN URL for a user's avatar.
///
/// If `avatar_hash` is `None`, falls back to the Discord 2023+ default avatar
/// formula: `embed/avatars/{(user_id_u64 >> 22) % 6}.png`. The user id is a
/// snowflake string and **must** be parsed to `u64` before the bit shift.
pub fn avatar_url(user_id: &str, avatar_hash: Option<&str>) -> String {
    match avatar_hash {
        Some(hash) if !hash.is_empty() => format!(
            "https://cdn.discordapp.com/avatars/{}/{}.png?size=128",
            user_id, hash
        ),
        _ => {
            let id: u64 = user_id.parse().unwrap_or(0);
            let idx = (id >> 22) % 6;
            format!("https://cdn.discordapp.com/embed/avatars/{}.png", idx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, name: &str) -> Participant {
        Participant {
            user_id: id.into(),
            username: name.into(),
            display_name: name.into(),
            avatar_url: avatar_url(id, None),
            mute: false,
            deaf: false,
            self_mute: false,
            self_deaf: false,
            speaking: false,
        }
    }

    #[test]
    fn avatar_url_with_hash() {
        let url = avatar_url("123", Some("abc"));
        assert_eq!(url, "https://cdn.discordapp.com/avatars/123/abc.png?size=128");
    }

    #[test]
    fn avatar_url_default_pomelo_formula() {
        // Snowflake-ish id; must parse to u64 first.
        let url = avatar_url("80351110224678912", None);
        // (80351110224678912 >> 22) % 6 == ?
        let id: u64 = 80351110224678912;
        let idx = (id >> 22) % 6;
        assert_eq!(
            url,
            format!("https://cdn.discordapp.com/embed/avatars/{}.png", idx)
        );
    }

    #[test]
    fn avatar_url_default_unparseable_id_falls_back_to_zero() {
        let url = avatar_url("not-a-snowflake", None);
        assert_eq!(url, "https://cdn.discordapp.com/embed/avatars/0.png");
    }

    #[test]
    fn store_join_then_leave() {
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![]);
        let mut deltas = s.subscribe_deltas();

        s.set_channel(Some("c1".into()), vec![p("u1", "alice")]);
        assert!(matches!(
            deltas.try_recv(),
            Ok(StateDelta::State(_))
        ));

        let snap = s.snapshot();
        assert_eq!(snap.participants.len(), 1);

        s.remove_participant("u1");
        let snap = s.snapshot();
        assert_eq!(snap.participants.len(), 0);
    }

    #[test]
    fn speaking_for_unknown_user_is_dropped() {
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![]);
        // SPEAKING for a user we don't know about must not break.
        let changed = s.set_speaking("ghost", true);
        assert!(!changed);
        // And must not appear in the snapshot.
        let snap = s.snapshot();
        assert!(snap.participants.is_empty());
    }

    #[test]
    fn speaking_toggle_emits_start_and_stop() {
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![p("u1", "alice")]);
        let mut deltas = s.subscribe_deltas();

        assert!(s.set_speaking("u1", true));
        let evt = deltas.try_recv().unwrap();
        assert!(matches!(evt, StateDelta::SpeakingStart { .. }));

        // Idempotent: re-issuing the same flag is a no-op.
        assert!(!s.set_speaking("u1", true));

        assert!(s.set_speaking("u1", false));
        let evt = deltas.try_recv().unwrap();
        assert!(matches!(evt, StateDelta::SpeakingStop { .. }));
    }

    #[test]
    fn channel_switch_clears_speakers() {
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![p("u1", "alice")]);
        s.set_speaking("u1", true);
        let snap = s.snapshot();
        assert!(snap.participants[0].speaking);

        s.set_channel(Some("c2".into()), vec![p("u2", "bob")]);
        let snap = s.snapshot();
        assert_eq!(snap.channel_id.as_deref(), Some("c2"));
        assert_eq!(snap.participants.len(), 1);
        assert_eq!(snap.participants[0].user_id, "u2");
        assert!(!snap.participants[0].speaking);
    }

    #[test]
    fn leaving_voice_yields_empty_payload() {
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![p("u1", "alice"), p("u2", "bob")]);
        s.set_channel(None, vec![]);
        let snap = s.snapshot();
        assert!(snap.channel_id.is_none());
        assert!(snap.participants.is_empty());
    }

    #[test]
    fn snapshot_sort_tiebreaker_by_user_id() {
        // Two participants share the display name; sort must fall back to
        // user_id deterministically.
        let s = OverlayStore::new();
        s.set_channel(
            Some("c1".into()),
            vec![
                p("u_z", "alice"),
                p("u_a", "alice"),
                p("u_m", "alice"),
            ],
        );
        let snap = s.snapshot();
        let ids: Vec<&str> = snap.participants.iter().map(|p| p.user_id.as_str()).collect();
        assert_eq!(ids, vec!["u_a", "u_m", "u_z"]);
    }

    #[test]
    fn ghost_speaking_id_does_not_pollute_set() {
        // A SPEAKING for an unknown user must not poison `inner.speaking`,
        // i.e. the next time that user joins they should NOT come in already
        // marked as speaking.
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![]);
        assert!(!s.set_speaking("ghost", true));
        // Now the user actually joins.
        s.upsert_participant(p("ghost", "ghost"));
        let snap = s.snapshot();
        let joined = snap.participants.iter().find(|p| p.user_id == "ghost").unwrap();
        assert!(!joined.speaking, "ghost speaking flag must not survive into upsert");
    }

    #[test]
    fn tray_state_mapping() {
        // (connected=false, _) -> DiscordOffline
        assert_eq!(
            TrayState::from_parts(false, None),
            TrayState::DiscordOffline
        );
        assert_eq!(
            TrayState::from_parts(false, Some("c1")),
            TrayState::DiscordOffline,
            "stale channel id while disconnected must still map to DiscordOffline"
        );
        // (true, None) -> Idle
        assert_eq!(TrayState::from_parts(true, None), TrayState::Idle);
        // (true, Some(_)) -> InVoice
        assert_eq!(TrayState::from_parts(true, Some("c1")), TrayState::InVoice);

        // From<&OverlayState> agrees with from_parts.
        let mut s = OverlayState::default();
        assert_eq!(TrayState::from(&s), TrayState::DiscordOffline);
        s.connected = true;
        assert_eq!(TrayState::from(&s), TrayState::Idle);
        s.channel_id = Some("c1".into());
        assert_eq!(TrayState::from(&s), TrayState::InVoice);
    }

    #[test]
    fn tray_state_watch_emits_transitions() {
        let s = OverlayStore::new();
        let mut rx = s.subscribe_tray_state();
        // Seeded with DiscordOffline.
        assert_eq!(*rx.borrow_and_update(), TrayState::DiscordOffline);

        s.set_connected(true);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TrayState::Idle);

        s.set_channel(Some("c1".into()), vec![]);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TrayState::InVoice);

        s.set_channel(None, vec![]);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TrayState::Idle);

        s.set_connected(false);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TrayState::DiscordOffline);
    }

    #[test]
    fn needs_setup_overrides_connected_state() {
        // Even when fully connected and in a voice channel, set_needs_setup(true)
        // must drive the tray to NeedsSetup.
        let s = OverlayStore::new();
        s.set_connected(true);
        s.set_channel(Some("c1".into()), vec![p("u1", "alice")]);

        let mut rx = s.subscribe_tray_state();
        // Drain anything observed so far.
        let _ = rx.borrow_and_update();
        assert_eq!(*rx.borrow(), TrayState::InVoice);

        s.set_needs_setup(true);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TrayState::NeedsSetup);

        // Snapshot also reflects the flag.
        let snap = s.snapshot();
        assert!(snap.needs_setup);
        assert!(snap.connected);
        assert_eq!(snap.channel_id.as_deref(), Some("c1"));
        // Derived TrayState honors the override.
        assert_eq!(TrayState::from(&snap), TrayState::NeedsSetup);

        // Clearing returns to InVoice.
        s.set_needs_setup(false);
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), TrayState::InVoice);
    }

    #[test]
    fn set_connected_clears_speaking() {
        let s = OverlayStore::new();
        s.set_channel(Some("c1".into()), vec![p("u1", "alice")]);
        s.set_speaking("u1", true);
        s.set_connected(false);
        let snap = s.snapshot();
        assert!(!snap.participants[0].speaking);
    }
}
