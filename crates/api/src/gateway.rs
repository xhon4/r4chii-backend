//! `GET /api/v1/gateway` — the WebSocket upgrade route.
//! `api` owns this route only; the wire
//! protocol types live in `realtime` and the fan-out hub in `realtime::Hub`.

use std::time::Duration;

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use tokio::time::timeout;

use crate::{extract::ResolvedToken, AppState};

/// Close code for an unauthenticated socket — unauthenticated sockets are
/// rejected with close code 4001.
const CLOSE_UNAUTHENTICATED: u16 = 4001;

/// How long an unauthenticated socket may wait for an `identify` frame
/// before being closed. Not spec-pinned — the spec only requires *some*
/// bound so a silent Bearer-only client that never identifies can't hold a
/// socket open forever.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn gateway(
    State(state): State<AppState>,
    ResolvedToken(token): ResolvedToken,
    ws: WebSocketUpgrade,
) -> Response {
    // Auth verification happens post-upgrade (inside `handle_socket`), never
    // by rejecting the HTTP upgrade itself — an invalid/absent token is
    // reported via the WebSocket close code 4001,
    // not an HTTP error response.
    ws.on_upgrade(move |socket| handle_socket(socket, state, token))
}

async fn handle_socket(mut socket: WebSocket, state: AppState, token: Option<String>) {
    let context = match token {
        // A token was found at handshake time (cookie, or a Bearer header a
        // non-browser client CAN set on the upgrade request): verify now,
        // and if it's invalid, close 4001 immediately — never fall through
        // to waiting for an `identify` frame that isn't coming.
        Some(token) => match state.auth.verify_session(&token).await {
            Ok(context) => context,
            Err(_) => {
                close_unauthenticated(&mut socket).await;
                return;
            }
        },
        // No token found at handshake time (the common browser + Bearer
        // case, which can't set custom headers on a WS handshake) — upgrade
        // anyway and wait for the client's first frame to be `identify`.
        None => match wait_for_identify(&mut socket, &state).await {
            Some(context) => context,
            None => {
                close_unauthenticated(&mut socket).await;
                return;
            }
        },
    };

    let account_id = context.account_id;
    let (handle, mut hub_rx) = state.realtime.register(account_id).await;

    let channel_ids = state
        .domain
        .accessible_channel_ids(account_id)
        .await
        .unwrap_or_else(|err| {
            // Best-effort: a `ready` event with a possibly-incomplete
            // channel list is far less harmful than dropping the
            // connection outright over a transient lookup failure — the
            // per-publish authorization check in `Hub::publish_*` is the
            // actual security boundary, not this list.
            tracing::warn!(
                error = %err,
                "failed to resolve accessible channel ids for gateway ready event"
            );
            Vec::new()
        });

    let ready = realtime::ServerEvent::Ready {
        account_id,
        channel_ids,
    };
    if let Ok(payload) = serde_json::to_string(&ready) {
        if socket.send(Message::Text(payload.into())).await.is_err() {
            state.realtime.unregister(account_id, handle).await;
            return;
        }
    }

    loop {
        tokio::select! {
            event = hub_rx.recv() => {
                match event {
                    Some(payload) => {
                        if socket.send(Message::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    // The hub's sender side is gone — unreachable in
                    // practice while this connection stays registered, but
                    // treated as a clean disconnect rather than a panic.
                    None => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        // Unknown `type` (or a malformed frame) is ignored,
                        // never crashes the connection.
                        match realtime::parse_client_frame(text.as_str()) {
                            Some(realtime::ClientFrame::Ping) => {
                                let pong = serde_json::to_string(&realtime::ServerEvent::Pong {})
                                    .unwrap_or_default();
                                if !pong.is_empty()
                                    && socket.send(Message::Text(pong.into())).await.is_err()
                                {
                                    break;
                                }
                            }
                            Some(realtime::ClientFrame::VoiceJoin { channel_id }) => {
                                // Authorized via DomainService::can_join_voice:
                                // a voice channel's call is open to whoever may access the voice channel
                                // (enforcing membership, kind == "voice", and restricted channel VIEW_CHANNEL)
                                // and is not timed out. Reuses require_channel_access so voice permissions
                                // cannot drift from the channel permission model.
                                let authorized = state
                                    .domain
                                    .can_join_voice(account_id, channel_id)
                                    .await
                                    .unwrap_or(false);
                                if authorized {
                                    state
                                        .realtime
                                        .voice_join(channel_id, account_id, handle)
                                        .await;
                                }
                                // Silently ignored when unauthorized: replying
                                // "no" would confirm the channel exists to
                                // someone who cannot see it.
                            }
                            Some(realtime::ClientFrame::VoiceLeave) => {
                                state.realtime.voice_leave(account_id, None).await;
                            }
                            Some(realtime::ClientFrame::VoiceSignal {
                                to_account_id,
                                signal,
                            }) => {
                                // The hub refuses unless both accounts are in
                                // the same call, so this cannot be used to push
                                // JSON at an arbitrary account.
                                state
                                    .realtime
                                    .voice_relay(account_id, to_account_id, signal)
                                    .await;
                            }
                            // An `identify` frame after auth, an unknown type,
                            // or a malformed frame: ignored, never fatal.
                            Some(realtime::ClientFrame::Identify { .. }) | None => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    // Ping/Pong/Binary frames: axum answers WebSocket-level
                    // pings automatically; nothing in this protocol uses
                    // binary frames, so ignore them rather than treat them
                    // as a fatal error.
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    state.realtime.unregister(account_id, handle).await;
}

/// Waits (bounded) for the client's first frame to be a valid `identify`.
/// Returns `None` on timeout, a read error, a non-text frame, or any frame
/// that isn't a well-formed `identify` with a token that verifies.
async fn wait_for_identify(socket: &mut WebSocket, state: &AppState) -> Option<app_core::AuthContext> {
    let text = match timeout(IDENTIFY_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => text,
        _ => return None,
    };

    match realtime::parse_client_frame(text.as_str()) {
        Some(realtime::ClientFrame::Identify { token }) => {
            state.auth.verify_session(&token).await.ok()
        }
        _ => None,
    }
}

async fn close_unauthenticated(socket: &mut WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: CLOSE_UNAUTHENTICATED,
            reason: "unauthenticated".into(),
        })))
        .await;
}
