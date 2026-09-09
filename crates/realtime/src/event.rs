//! The wire protocol: the `{ "type": "event.name", "data": { } }` frame
//! envelope, the M0 server->client events, and parsing for client->server
//! frames. This crate owns the event protocol — `domain::MessageSummary`
//! itself carries no serde knowledge, so `MessagePayload` is this crate's
//! own serde-aware view of it, converted with `From`.

use app_core::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Wire shape of a message inside `message.create`/`message.update` events.
/// Kept separate from `domain::MessageSummary` so `domain` stays serde-free —
/// the same crate-boundary pattern as `api::dto`'s DTOs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MessagePayload {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_account_id: Uuid,
    pub content: Option<String>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// `null` = not pinned.
    pub pinned_at: Option<DateTime<Utc>>,
}

/// Wire shape of a role inside `role.create`/`role.update` events (M2).
/// Same reasoning as `MessagePayload` — `domain::RoleSummary` stays
/// serde-free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RolePayload {
    pub id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    pub color: Option<String>,
    pub permissions: i64,
    pub position: i32,
    pub is_default: bool,
    /// When `true`, ANY member may `@`-mention this role.
    pub mentionable: bool,
}

impl From<&domain::RoleSummary> for RolePayload {
    fn from(role: &domain::RoleSummary) -> Self {
        Self {
            id: role.id,
            server_id: role.server_id,
            name: role.name.clone(),
            color: role.color.clone(),
            permissions: role.permissions,
            position: role.position,
            is_default: role.is_default,
            mentionable: role.mentionable,
        }
    }
}

impl From<&domain::MessageSummary> for MessagePayload {
    fn from(message: &domain::MessageSummary) -> Self {
        Self {
            id: message.id,
            channel_id: message.channel_id,
            author_account_id: message.author_account_id,
            content: message.content.clone(),
            created_at: message.created_at,
            edited_at: message.edited_at,
            deleted_at: message.deleted_at,
            pinned_at: message.pinned_at,
        }
    }
}

/// Whether an account currently holds at least one live gateway socket.
/// Exactly the two states the spec pins for `presence.update` — there is
/// deliberately no idle/away/dnd state and no activity string, because
/// presence here is derived from socket connectivity alone, not from
/// anything a client asserts about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceStatus {
    Online,
    Offline,
}

/// The declared presence an account chooses for itself. Distinct from
/// `PresenceStatus` (socket connectivity): `online|offline` is whether you
/// hold a gateway connection, `online|idle|dnd|invisible` is what you tell
/// others you are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeclaredStatus {
    Online,
    Idle,
    Dnd,
    Invisible,
}

/// M0 server->client events. Serializes to exactly
/// `{ "type": "...", "data": { ... } }` via adjacent tagging.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerEvent {
    #[serde(rename = "ready")]
    Ready {
        account_id: Uuid,
        channel_ids: Vec<Uuid>,
    },
    #[serde(rename = "message.create")]
    MessageCreate { message: MessagePayload },
    #[serde(rename = "message.update")]
    MessageUpdate { message: MessagePayload },
    #[serde(rename = "message.delete")]
    MessageDelete { channel_id: Uuid, message_id: Uuid },
    #[serde(rename = "presence.update")]
    PresenceUpdate {
        account_id: Uuid,
        status: PresenceStatus,
    },
    #[serde(rename = "presence.status_update")]
    PresenceStatusUpdate {
        account_id: Uuid,
        status: DeclaredStatus,
    },
    /// The full roster of a voice channel, sent to an account the moment it
    /// joins. A mesh client needs to know who is already there so it can open
    /// one peer connection per existing participant, and it needs that as a
    /// snapshot rather than as a replay of past joins.
    #[serde(rename = "voice.state")]
    VoiceState {
        channel_id: Uuid,
        account_ids: Vec<Uuid>,
    },
    /// Someone joined. Sent to everyone already in the channel, never to the
    /// joiner — they got `voice.state` instead, and receiving both would have
    /// them open a duplicate connection to themselves.
    #[serde(rename = "voice.join")]
    VoiceJoin { channel_id: Uuid, account_id: Uuid },
    #[serde(rename = "voice.leave")]
    VoiceLeave { channel_id: Uuid, account_id: Uuid },
    /// One peer's WebRTC signalling blob, relayed verbatim.
    ///
    /// `signal` is deliberately opaque: it is an SDP offer/answer or an ICE
    /// candidate, and the backend has no business parsing it. This server
    /// never touches media, and staying ignorant of the payload is how that
    /// stays true — a server that understands SDP is one refactor away from
    /// rewriting it.
    #[serde(rename = "voice.signal")]
    VoiceSignal {
        channel_id: Uuid,
        from_account_id: Uuid,
        signal: serde_json::Value,
    },
    #[serde(rename = "pong")]
    Pong {},
    // ---- M2: roles & permissions ----
    // Fanned server-wide (`server_member_account_ids`), never per-channel —
    // a role or the roster is a server-level concept.
    #[serde(rename = "role.create")]
    RoleCreate { server_id: Uuid, role: RolePayload },
    #[serde(rename = "role.update")]
    RoleUpdate { server_id: Uuid, role: RolePayload },
    #[serde(rename = "role.delete")]
    RoleDelete { server_id: Uuid, role_id: Uuid },
    #[serde(rename = "member.roles_update")]
    MemberRolesUpdate {
        server_id: Uuid,
        account_id: Uuid,
        role_ids: Vec<Uuid>,
    },
    /// Covers leave, kick, and ban — one roster update, not three, since
    /// there is no system-message channel in this project to render a
    /// distinct chat line for each (there is no such event type).
    #[serde(rename = "member.leave")]
    MemberLeave {
        server_id: Uuid,
        account_id: Uuid,
        reason: MemberLeaveReason,
    },
    /// The whole server is gone — every member's client removes it from
    /// their list. Sent to the same server-wide audience as the events
    /// above, including the account that just deleted it (simpler than
    /// special-casing the actor out, and harmless: their own client already
    /// knows and will just no-op on a server id it no longer has).
    #[serde(rename = "server.delete")]
    ServerDelete { server_id: Uuid },
    // ---- Role permissions v2 ----
    /// Channel-scoped (like `message.update`) — a pin is a per-channel
    /// concept, not a server-level one.
    #[serde(rename = "message.pin_update")]
    MessagePinUpdate { message: MessagePayload },
    /// Server-wide (like `member.roles_update`) — timeout status is visible
    /// in the member list to everyone, same as a role assignment.
    #[serde(rename = "member.timeout_update")]
    MemberTimeoutUpdate {
        server_id: Uuid,
        account_id: Uuid,
        timeout_until: Option<DateTime<Utc>>,
    },
    /// Server-wide — a nickname is already visible to every member via the
    /// roster, same privacy tier as a role assignment.
    #[serde(rename = "member.nickname_update")]
    MemberNicknameUpdate {
        server_id: Uuid,
        account_id: Uuid,
        nickname: Option<String>,
    },
}

/// Why a `member.leave` event fired — lets the client render "left" vs "was
/// removed" vs "was banned" differently in the member list, without the
/// gateway having to send three different event names for what is
/// structurally the same update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberLeaveReason {
    Left,
    Kicked,
    Banned,
}

/// M0 client->server frames the gateway understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    Identify {
        token: String,
    },
    Ping,
    /// Enter a voice channel. Carried on the socket rather than over HTTP
    /// because voice membership is scoped to one live connection: it must end
    /// when the socket does, exactly like presence, and an HTTP call has no
    /// connection to be scoped to. An account with two tabs open would leave
    /// the server guessing which one is in the call.
    VoiceJoin {
        channel_id: Uuid,
    },
    VoiceLeave,
    /// Relay `signal` to one other participant in the same voice channel.
    VoiceSignal {
        to_account_id: Uuid,
        signal: serde_json::Value,
    },
}

/// Parses one client->server frame. Returns `None` for a malformed frame,
/// an unknown `type`, or an `identify` frame missing `data.token` — the
/// caller must treat `None` as "ignore, never crash the connection": an
/// unknown `type` from a client is ignored, not fatal.
///
/// Deliberately does NOT deserialize the whole frame through one
/// `#[serde(tag = "type", content = "data")]` enum: that would make an
/// absent `data` key on a unit-variant frame (e.g. a bare `{"type":"ping"}`
/// with no `data` at all) an open question of serde's adjacent-tagging
/// edge cases. Parsing `type` and `data` as two separate steps sidesteps
/// that entirely and is easy to reason about.
pub fn parse_client_frame(text: &str) -> Option<ClientFrame> {
    #[derive(Deserialize)]
    struct RawFrame {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        data: serde_json::Value,
    }

    #[derive(Deserialize)]
    struct IdentifyData {
        token: String,
    }

    let raw: RawFrame = serde_json::from_str(text).ok()?;

    match raw.kind.as_str() {
        "identify" => {
            let identify: IdentifyData = serde_json::from_value(raw.data).ok()?;
            Some(ClientFrame::Identify {
                token: identify.token,
            })
        }
        "ping" => Some(ClientFrame::Ping),
        "voice.join" => {
            #[derive(Deserialize)]
            struct JoinData {
                channel_id: Uuid,
            }
            let join: JoinData = serde_json::from_value(raw.data).ok()?;
            Some(ClientFrame::VoiceJoin {
                channel_id: join.channel_id,
            })
        }
        "voice.leave" => Some(ClientFrame::VoiceLeave),
        "voice.signal" => {
            #[derive(Deserialize)]
            struct SignalData {
                to_account_id: Uuid,
                signal: serde_json::Value,
            }
            let signal: SignalData = serde_json::from_value(raw.data).ok()?;
            Some(ClientFrame::VoiceSignal {
                to_account_id: signal.to_account_id,
                signal: signal.signal,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_event_serializes_to_the_envelope_shape() {
        let event = ServerEvent::Ready {
            account_id: Uuid::nil(),
            channel_ids: vec![Uuid::nil()],
        };
        let value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["type"], "ready");
        assert!(value["data"]["account_id"].is_string());
        assert!(value["data"]["channel_ids"].is_array());
    }

    #[test]
    fn message_delete_event_carries_no_content() {
        let event = ServerEvent::MessageDelete {
            channel_id: Uuid::nil(),
            message_id: Uuid::nil(),
        };
        let value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["type"], "message.delete");
        assert!(value["data"].get("content").is_none());
    }

    #[test]
    fn pong_event_serializes_to_an_empty_data_object() {
        let value = serde_json::to_value(ServerEvent::Pong {}).expect("serializes");
        assert_eq!(value["type"], "pong");
        assert_eq!(value["data"], serde_json::json!({}));
    }

    #[test]
    fn message_create_carries_null_content_for_a_deleted_message() {
        let payload = MessagePayload {
            id: Uuid::nil(),
            channel_id: Uuid::nil(),
            author_account_id: Uuid::nil(),
            content: None,
            created_at: Utc::now(),
            edited_at: None,
            deleted_at: Some(Utc::now()),
            pinned_at: None,
        };
        let value = serde_json::to_value(ServerEvent::MessageCreate { message: payload })
            .expect("serializes");
        assert!(value["data"]["message"]["content"].is_null());
        assert!(value["data"]["message"]["deleted_at"].is_string());
    }

    #[test]
    fn presence_update_serializes_to_the_spec_shape() {
        let event = ServerEvent::PresenceUpdate {
            account_id: Uuid::nil(),
            status: PresenceStatus::Online,
        };
        let value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["type"], "presence.update");
        assert!(value["data"]["account_id"].is_string());
        assert_eq!(value["data"]["status"], "online");
        // The spec pins `{ account_id, status }` and nothing else — no
        // activity string, no idle/dnd state, no last-seen timestamp.
        assert_eq!(value["data"].as_object().map(serde_json::Map::len), Some(2));
    }

    #[test]
    fn presence_status_offline_serializes_to_the_lowercase_spec_token() {
        let event = ServerEvent::PresenceUpdate {
            account_id: Uuid::nil(),
            status: PresenceStatus::Offline,
        };
        let value = serde_json::to_value(&event).expect("serializes");
        assert_eq!(value["data"]["status"], "offline");
    }

    #[test]
    fn parse_client_frame_parses_identify() {
        let frame = parse_client_frame(r#"{"type":"identify","data":{"token":"abc123"}}"#);
        assert_eq!(
            frame,
            Some(ClientFrame::Identify {
                token: "abc123".to_string()
            })
        );
    }

    #[test]
    fn parse_client_frame_parses_ping_with_no_data_key() {
        let frame = parse_client_frame(r#"{"type":"ping"}"#);
        assert_eq!(frame, Some(ClientFrame::Ping));
    }

    #[test]
    fn parse_client_frame_ignores_an_unknown_type() {
        assert_eq!(parse_client_frame(r#"{"type":"typing.start"}"#), None);
    }

    #[test]
    fn parse_client_frame_ignores_malformed_json() {
        assert_eq!(parse_client_frame("not json"), None);
    }

    #[test]
    fn parse_client_frame_ignores_an_identify_frame_missing_the_token() {
        assert_eq!(parse_client_frame(r#"{"type":"identify","data":{}}"#), None);
    }
}
