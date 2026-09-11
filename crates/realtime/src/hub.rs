//! The in-process fan-out hub: a single WebSocket endpoint with an
//! in-process hub that fans messages out to connected clients. M0 fan-out
//! is single-node; the public surface here (`publish_*`) is written so a
//! later swap to Postgres LISTEN/NOTIFY or Redis pub/sub is a change behind
//! this interface, not a rewrite.

use std::collections::HashMap;
use std::sync::Arc;

use app_core::Uuid;
use tokio::sync::{mpsc, RwLock};

use crate::error::RealtimeError;
use crate::event::{
    ChannelPayload, DeclaredStatus, MemberLeaveReason, MessagePayload, PresenceStatus, RolePayload,
    ServerEvent,
};

/// Opaque per-connection identity, distinct from `account_id`: an account
/// can have more than one live connection (web + desktop open at once), so
/// `unregister` needs something more specific than the account id to target
/// exactly the one connection being removed. A fresh id per `register` call
/// — stable and immune to any `Vec` index shifting from concurrent
/// registration/unregistration on the same account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionHandle(Uuid);

struct Connection {
    handle: ConnectionHandle,
    sender: mpsc::UnboundedSender<String>,
}

struct Inner {
    connections: RwLock<HashMap<Uuid, Vec<Connection>>>,
    /// Who is in which voice channel, keyed by channel then by account.
    ///
    /// Deliberately in memory and never persisted. Being in a call is a
    /// property of a live socket, exactly like presence: if the process
    /// restarts nobody is in a call any more, and a row in Postgres claiming
    /// otherwise would be a lie that outlives the thing it described. The
    /// `ConnectionHandle` is stored so a second tab closing cannot evict the
    /// tab that is actually in the call.
    voice: RwLock<HashMap<Uuid, HashMap<Uuid, ConnectionHandle>>>,
    domain: domain::DomainService,
}

/// In-process WebSocket fan-out hub. Cheap to clone — wraps an `Arc`
/// internally, the same pattern every service in this codebase uses to be
/// handed around as `axum` state.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

impl Hub {
    pub fn new(domain: domain::DomainService) -> Self {
        Self {
            inner: Arc::new(Inner {
                connections: RwLock::new(HashMap::new()),
                voice: RwLock::new(HashMap::new()),
                domain,
            }),
        }
    }

    /// Registers a new live connection for `account_id`. The caller
    /// (`api`'s gateway handler) reads server->client JSON frames from the
    /// returned receiver and forwards them to the socket, and must call
    /// [`Hub::unregister`] with the returned handle on disconnect.
    ///
    /// Announces `presence.update` `online` only when this is the account's
    /// *first* connection — see [`Hub::publish_presence`].
    pub async fn register(
        &self,
        account_id: Uuid,
    ) -> (ConnectionHandle, mpsc::UnboundedReceiver<String>) {
        let handle = ConnectionHandle(app_core::new_id());
        let (sender, receiver) = mpsc::unbounded_channel();

        let came_online = {
            let mut connections = self.inner.connections.write().await;
            let list = connections.entry(account_id).or_default();
            list.push(Connection { handle, sender });
            list.len() == 1
        };

        // Deliberately outside the write lock: `publish_presence` takes the
        // read lock to fan out, and this `RwLock` is not reentrant.
        if came_online {
            self.publish_presence(account_id, PresenceStatus::Online)
                .await;
        }

        (handle, receiver)
    }

    /// Removes exactly the one connection identified by `handle` for
    /// `account_id`. Must actually drop the entry (not just leave an empty
    /// `Vec` behind) so reconnect/disconnect churn cannot leak memory over
    /// the process lifetime.
    ///
    /// Announces `presence.update` `offline` only when this was the account's
    /// *last* connection — see [`Hub::publish_presence`].
    pub async fn unregister(&self, account_id: Uuid, handle: ConnectionHandle) {
        // A dropped socket must leave the call. Without this, closing a tab
        // mid-call leaves a ghost participant that every other client keeps a
        // dead peer connection open for, and that nobody can remove.
        self.voice_leave(account_id, Some(handle)).await;

        let went_offline = {
            let mut connections = self.inner.connections.write().await;
            match connections.get_mut(&account_id) {
                Some(list) => {
                    list.retain(|conn| conn.handle != handle);
                    let empty = list.is_empty();
                    if empty {
                        connections.remove(&account_id);
                    }
                    empty
                }
                None => false,
            }
        };

        if went_offline {
            self.publish_presence(account_id, PresenceStatus::Offline)
                .await;
        }
    }

    /// Live presence for `account_ids`, read from the same connection
    /// registry `presence.update` is derived from, so the REST snapshot
    /// (`api`'s member list) and the socket deltas can never disagree about
    /// what "online" means. Presence is not stored anywhere — an account is
    /// online exactly while it holds at least one live connection.
    ///
    /// Resolves the whole set under one read lock rather than one lock per
    /// account, and returns an entry for every id asked about.
    pub async fn presence_snapshot(&self, account_ids: &[Uuid]) -> HashMap<Uuid, PresenceStatus> {
        let connections = self.inner.connections.read().await;
        account_ids
            .iter()
            .map(|account_id| {
                let status = if connections.contains_key(account_id) {
                    PresenceStatus::Online
                } else {
                    PresenceStatus::Offline
                };
                (*account_id, status)
            })
            .collect()
    }

    pub async fn publish_message_create(
        &self,
        channel_id: Uuid,
        message: &domain::MessageSummary,
    ) -> Result<(), RealtimeError> {
        self.publish(
            channel_id,
            ServerEvent::MessageCreate {
                message: MessagePayload::from(message),
            },
        )
        .await
    }

    pub async fn publish_message_update(
        &self,
        channel_id: Uuid,
        message: &domain::MessageSummary,
    ) -> Result<(), RealtimeError> {
        self.publish(
            channel_id,
            ServerEvent::MessageUpdate {
                message: MessagePayload::from(message),
            },
        )
        .await
    }

    pub async fn publish_message_delete(
        &self,
        channel_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), RealtimeError> {
        self.publish(
            channel_id,
            ServerEvent::MessageDelete {
                channel_id,
                message_id,
            },
        )
        .await
    }

    /// Covers both pin and unpin — the client reads `pinned_at` on the
    /// payload to tell which happened, same shape `message.update` already
    /// uses for edits.
    pub async fn publish_message_pin_update(
        &self,
        channel_id: Uuid,
        message: &domain::MessageSummary,
    ) -> Result<(), RealtimeError> {
        self.publish(
            channel_id,
            ServerEvent::MessagePinUpdate {
                message: MessagePayload::from(message),
            },
        )
        .await
    }

    // ---- M2: roles & permissions ----
    // All server-wide (`server_member_account_ids`), never per-channel — a
    // role or the roster is a server-level concept, not a channel one.

    async fn publish_server_wide(
        &self,
        server_id: Uuid,
        event: ServerEvent,
    ) -> Result<(), RealtimeError> {
        let account_ids = self
            .inner
            .domain
            .server_member_account_ids(server_id)
            .await?;
        self.send_to(&account_ids, &event).await;
        Ok(())
    }

    pub async fn publish_role_create(
        &self,
        server_id: Uuid,
        role: &domain::RoleSummary,
    ) -> Result<(), RealtimeError> {
        self.publish_server_wide(
            server_id,
            ServerEvent::RoleCreate {
                server_id,
                role: RolePayload::from(role),
            },
        )
        .await
    }

    pub async fn publish_role_update(
        &self,
        server_id: Uuid,
        role: &domain::RoleSummary,
    ) -> Result<(), RealtimeError> {
        self.publish_server_wide(
            server_id,
            ServerEvent::RoleUpdate {
                server_id,
                role: RolePayload::from(role),
            },
        )
        .await
    }

    pub async fn publish_role_delete(
        &self,
        server_id: Uuid,
        role_id: Uuid,
    ) -> Result<(), RealtimeError> {
        self.publish_server_wide(server_id, ServerEvent::RoleDelete { server_id, role_id })
            .await
    }

    pub async fn publish_member_roles_update(
        &self,
        server_id: Uuid,
        account_id: Uuid,
        role_ids: Vec<Uuid>,
    ) -> Result<(), RealtimeError> {
        self.publish_server_wide(
            server_id,
            ServerEvent::MemberRolesUpdate {
                server_id,
                account_id,
                role_ids,
            },
        )
        .await
    }

    /// `timeout_until: None` covers an early-cleared timeout too.
    pub async fn publish_member_timeout_update(
        &self,
        server_id: Uuid,
        account_id: Uuid,
        timeout_until: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), RealtimeError> {
        if timeout_until.is_some_and(|until| until > chrono::Utc::now()) {
            self.voice_leave_if_in_server(account_id, server_id).await;
        }

        self.publish_server_wide(
            server_id,
            ServerEvent::MemberTimeoutUpdate {
                server_id,
                account_id,
                timeout_until,
            },
        )
        .await
    }

    /// `nickname: None` covers clearing it back to "no override".
    pub async fn publish_member_nickname_update(
        &self,
        server_id: Uuid,
        account_id: Uuid,
        nickname: Option<String>,
    ) -> Result<(), RealtimeError> {
        self.publish_server_wide(
            server_id,
            ServerEvent::MemberNicknameUpdate {
                server_id,
                account_id,
                nickname,
            },
        )
        .await
    }

    /// Takes an explicit recipient list, unlike the role events above — by
    /// the time this is called the target's `membership` row is already
    /// gone, so re-resolving here (like `publish_server_wide` does) would
    /// miss exactly the account that most needs to hear it. The caller is
    /// `domain::DomainService::leave_server`/`kick_member`/`ban_member`,
    /// which resolve the list before deleting the row — see
    /// `remove_member`'s doc comment.
    pub async fn announce_member_leave(
        &self,
        account_ids: &[Uuid],
        server_id: Uuid,
        account_id: Uuid,
        reason: MemberLeaveReason,
    ) {
        self.voice_leave_if_in_server(account_id, server_id).await;

        self.send_to(
            account_ids,
            &ServerEvent::MemberLeave {
                server_id,
                account_id,
                reason,
            },
        )
        .await;
    }

    /// Takes an explicit recipient list rather than resolving one, unlike
    /// every other M2 event above: by the time `DomainService::delete_server`
    /// returns, `membership` (and everything cascading from `server`) is
    /// already gone, so resolving recipients afterward would find nobody.
    /// The `api` handler resolves `server_member_account_ids` BEFORE calling
    /// `delete_server`, then calls this after it succeeds. Infallible like
    /// `send_to` itself — there's no recipient lookup left to fail here.
    pub async fn announce_server_delete(&self, account_ids: &[Uuid], server_id: Uuid) {
        self.send_to(account_ids, &ServerEvent::ServerDelete { server_id })
            .await;
    }

    // ---- channel lifecycle ----

    /// Broadcasts `channel.create` to every member that may view the channel.
    pub async fn publish_channel_create(
        &self,
        channel: &domain::ChannelSummary,
    ) -> Result<(), RealtimeError> {
        let account_ids = self
            .inner
            .domain
            .channel_viewer_account_ids(channel.id)
            .await?;
        self.send_to(
            &account_ids,
            &ServerEvent::ChannelCreate {
                channel: ChannelPayload::from(channel),
            },
        )
        .await;
        Ok(())
    }

    /// Announces a freshly-created `dm`/`group_dm` to its participants.
    ///
    /// Takes the roster explicitly rather than resolving one, unlike
    /// `publish_channel_create`: that path resolves viewers with
    /// `channel_viewer_account_ids`, which requires a `server_id`, and a
    /// dm/group_dm has none by schema CHECK. A DM's audience is exactly its
    /// `channel_member` rows, which the caller already holds — the same
    /// explicit-recipients shape `announce_server_delete` uses.
    ///
    /// Infallible: there is no recipient lookup left to fail, and a dropped
    /// frame must never fail the HTTP request that already created the row.
    pub async fn announce_dm_create(&self, channel: &domain::ChannelSummary) {
        self.send_to(
            &channel.participant_ids,
            &ServerEvent::ChannelCreate {
                channel: ChannelPayload::from(channel),
            },
        )
        .await;
    }

    /// Broadcasts `channel.update` (rename or reorder) to viewers of the
    /// affected channel.
    pub async fn publish_channel_update(
        &self,
        channel: &domain::ChannelSummary,
    ) -> Result<(), RealtimeError> {
        let account_ids = self
            .inner
            .domain
            .channel_viewer_account_ids(channel.id)
            .await?;
        self.send_to(
            &account_ids,
            &ServerEvent::ChannelUpdate {
                channel: ChannelPayload::from(channel),
            },
        )
        .await;
        Ok(())
    }

    /// Broadcasts `channel.delete` to viewers captured BEFORE the soft-delete.
    pub async fn announce_channel_delete(
        &self,
        account_ids: &[Uuid],
        server_id: Uuid,
        channel_id: Uuid,
    ) {
        self.send_to(
            account_ids,
            &ServerEvent::ChannelDelete {
                server_id,
                channel_id,
            },
        )
        .await;
    }

    /// Resolves authorized recipients server-side at publish time (never
    /// trusting a client's claimed subscriptions) and hands them to
    /// [`Hub::send_to`].
    /// Puts `account_id` into `channel_id`'s call and returns who was already
    /// there. Announces the arrival to them, and hands the joiner the roster.
    ///
    /// The caller must have authorized the account for the channel first: this
    /// deliberately does not check, so that authorization stays in one place
    /// (`domain`) rather than being re-derived here and drifting.
    ///
    /// Rejoining a channel already joined is idempotent; joining a different
    /// one leaves the first, because a client cannot be in two calls at once
    /// and the alternative is a participant nobody can hear.
    pub async fn voice_join(
        &self,
        channel_id: Uuid,
        account_id: Uuid,
        handle: ConnectionHandle,
    ) -> Vec<Uuid> {
        self.voice_leave(account_id, None).await;

        let peers = {
            let mut voice = self.inner.voice.write().await;
            let room = voice.entry(channel_id).or_default();
            let peers: Vec<Uuid> = room
                .keys()
                .copied()
                .filter(|id| *id != account_id)
                .collect();
            room.insert(account_id, handle);
            peers
        };

        // Announced to EVERYONE who can see the channel, not only to the
        // people in the call. The sidebar shows who is talking before you
        // join — that is most of the reason to join — and it cannot do that
        // from events only participants receive.
        let _ = self
            .publish(
                channel_id,
                ServerEvent::VoiceJoin {
                    channel_id,
                    account_id,
                },
            )
            .await;

        self.send_to_account(
            account_id,
            &ServerEvent::VoiceState {
                channel_id,
                account_ids: peers.clone(),
            },
        )
        .await;

        peers
    }

    /// Removes `account_id` from whatever call it is in and tells the rest.
    ///
    /// When `only_handle` is set, the account leaves ONLY if the call is held
    /// by that exact connection — the disconnect path passes it so that
    /// closing a second tab cannot hang up the tab holding the call.
    pub async fn voice_leave(&self, account_id: Uuid, only_handle: Option<ConnectionHandle>) {
        let left = {
            let mut voice = self.inner.voice.write().await;
            let mut left = None;
            for (channel_id, room) in voice.iter_mut() {
                match room.get(&account_id) {
                    Some(held) if only_handle.is_none_or(|h| *held == h) => {
                        room.remove(&account_id);
                        left = Some(*channel_id);
                        break;
                    }
                    _ => {}
                }
            }
            voice.retain(|_, room| !room.is_empty());
            left
        };

        if let Some(channel_id) = left {
            self.announce_voice_leave(channel_id, account_id).await;
        }
    }

    /// Removes `account_id` from voice ONLY if it is still in exactly
    /// `channel_id`'s call, and does nothing otherwise.
    ///
    /// This exists because `voice_leave` is unscoped — it evicts an account
    /// from WHATEVER call it currently holds, not from a specific one. A
    /// caller that validated "`account_id` is in a voice channel belonging to
    /// server X" against a stale read, then released the voice-map lock to do
    /// that validation (an admin action like kick/ban/timeout has to await a
    /// DB round trip to confirm server membership), cannot safely call
    /// `voice_leave` afterward: between the read and the DB confirmation, the
    /// account could have left that call and joined an unrelated one on a
    /// different server, and `voice_leave` would evict it from THAT call
    /// instead — an admin action on server A silently dropping a call on
    /// server B. Taking the write lock and re-checking the exact channel here
    /// closes that window: the read (which channel is this account in NOW)
    /// and the removal happen under the same lock, so nothing can move the
    /// account between them.
    ///
    /// `pub` rather than a private helper of `voice_leave_if_in_server`
    /// alone: the actual TOCTOU race it closes needs a real, timing-dependent
    /// DB round trip in the middle to reproduce, which a unit test cannot
    /// force deterministically — so the atomicity property this function
    /// provides (no-op unless still in exactly `channel_id`) is instead
    /// pinned directly, against the public API, in
    /// `crates/realtime/tests/hub.rs`.
    pub async fn voice_leave_from_channel(&self, account_id: Uuid, channel_id: Uuid) {
        let left = {
            let mut voice = self.inner.voice.write().await;
            let removed = match voice.get_mut(&channel_id) {
                Some(room) => room.remove(&account_id).is_some(),
                None => false,
            };
            voice.retain(|_, room| !room.is_empty());
            removed
        };

        if left {
            self.announce_voice_leave(channel_id, account_id).await;
        }
    }

    /// Shared tail of `voice_leave` and `voice_leave_from_channel`: tells the
    /// rest of the channel that `account_id` left, channel-wide for the same
    /// reason `voice_join`'s announcement is.
    async fn announce_voice_leave(&self, channel_id: Uuid, account_id: Uuid) {
        let _ = self
            .publish(
                channel_id,
                ServerEvent::VoiceLeave {
                    channel_id,
                    account_id,
                },
            )
            .await;
    }

    /// Everyone currently in a call, grouped by channel, restricted to the
    /// channels given. Used to seed a client's view on load: the socket only
    /// carries deltas, so without a snapshot a client that connects mid-call
    /// shows an empty channel until somebody happens to join or leave.
    pub async fn voice_rosters(&self, channel_ids: &[Uuid]) -> HashMap<Uuid, Vec<Uuid>> {
        let voice = self.inner.voice.read().await;
        channel_ids
            .iter()
            .filter_map(|channel_id| {
                voice
                    .get(channel_id)
                    .filter(|room| !room.is_empty())
                    .map(|room| (*channel_id, room.keys().copied().collect()))
            })
            .collect()
    }

    /// The channel `account_id` is currently in a call on, if any.
    pub async fn voice_channel_of(&self, account_id: Uuid) -> Option<Uuid> {
        let voice = self.inner.voice.read().await;
        voice
            .iter()
            .find(|(_, room)| room.contains_key(&account_id))
            .map(|(channel_id, _)| *channel_id)
    }

    /// If `account_id` is currently in a voice channel belonging to `server_id`,
    /// evicts them from the call and announces the leave to the remaining participants.
    ///
    /// `voice_channel_of` reads the account's CURRENT channel, releases the
    /// voice-map lock, then awaits `channel_belongs_to_server` — a DB round
    /// trip. The eviction itself must not use the unscoped `voice_leave`
    /// (which drops whatever call the account holds AT THE TIME IT RUNS): if
    /// the account left `channel_id` and joined an unrelated voice channel on
    /// a different server during that await, an unscoped eviction would land
    /// on the new, unrelated call instead of doing nothing — a kick/ban/
    /// timeout on server A silently dropping a call on server B.
    /// `voice_leave_from_channel` closes that window by re-checking, under
    /// the same lock as the removal, that the account is still in exactly
    /// `channel_id`.
    pub async fn voice_leave_if_in_server(&self, account_id: Uuid, server_id: Uuid) {
        if let Some(channel_id) = self.voice_channel_of(account_id).await {
            let in_server = match self
                .inner
                .domain
                .channel_belongs_to_server(channel_id, server_id)
                .await
            {
                Ok(in_server) => in_server,
                Err(err) => {
                    // Fails CLOSED: a caller of this function has already
                    // decided `account_id` is being kicked, banned, or timed
                    // out (or, for a voluntary leave, is at least leaving
                    // `server_id`), and this is the eviction that is supposed
                    // to enforce that decision against an active call. A
                    // transient DB error here used to read as
                    // "unwrap_or(false)" — indistinguishable from "not in
                    // this server" — which meant a kicked/banned/timed-out
                    // account silently stayed in its call with no log and no
                    // retry. That is the wrong side to fail on: assuming "in
                    // server" on an unproven check means the worst case is an
                    // account occasionally evicted from a call that turns out
                    // to belong to an unrelated server (self-limited — they
                    // can simply rejoin), which is far cheaper than a
                    // removed/silenced account staying in a call
                    // indefinitely and invisibly.
                    tracing::warn!(
                        error = %err,
                        %account_id,
                        %channel_id,
                        %server_id,
                        "could not confirm whether a voice channel belongs to the server an eviction targets; evicting anyway"
                    );
                    true
                }
            };
            if in_server {
                self.voice_leave_from_channel(account_id, channel_id).await;
            }
        }
    }

    /// Relays one signalling blob between two participants of the same call.
    ///
    /// Returns false, and sends nothing, unless both accounts are in the SAME
    /// channel's call. Delivers the frame directly to the specific connection
    /// handle that joined the call, preventing signal leakage to other tabs.
    pub async fn voice_relay(
        &self,
        from_account_id: Uuid,
        to_account_id: Uuid,
        signal: serde_json::Value,
    ) -> bool {
        let (channel_id, target_handle) = {
            let voice = self.inner.voice.read().await;
            match voice.iter().find_map(|(channel_id, room)| {
                if room.contains_key(&from_account_id) {
                    room.get(&to_account_id)
                        .map(|handle| (*channel_id, *handle))
                } else {
                    None
                }
            }) {
                Some((channel_id, target_handle)) => (channel_id, target_handle),
                None => return false,
            }
        };

        self.send_to_connection(
            to_account_id,
            target_handle,
            &ServerEvent::VoiceSignal {
                channel_id,
                from_account_id,
                signal,
            },
        )
        .await;
        true
    }

    async fn publish(&self, channel_id: Uuid, event: ServerEvent) -> Result<(), RealtimeError> {
        let account_ids = self.inner.domain.authorized_account_ids(channel_id).await?;
        self.send_to(&account_ids, &event).await;

        Ok(())
    }

    /// Fans a presence transition out to the accounts authorized to observe
    /// `account_id` — everyone sharing a server or dm with them, resolved by
    /// `domain::DomainService::presence_observer_account_ids`, which reuses
    /// the exact authorization message fan-out uses. Presence is never
    /// broadcast to every connected socket.
    ///
    /// Best-effort and infallible to the caller on purpose: this runs on the
    /// socket connect/disconnect path, and a failed recipient lookup must
    /// never stop a client connecting or a disconnect from being cleaned up.
    /// The protocol has no replay buffer anyway, so a dropped presence event
    /// is reconciled by the member list's `status` on the next fetch.
    ///
    /// Called only on the 0->1 and 1->0 socket-count transitions, never per
    /// socket: an account with two tabs open must not appear to go offline
    /// when it closes one. Because the registry lock is released before this
    /// runs, a register and an unregister racing on the same account can
    /// still publish out of order; the REST snapshot is the tiebreaker, which
    /// is why it reads the same registry rather than a cached copy.
    async fn publish_presence(&self, account_id: Uuid, status: PresenceStatus) {
        let mut account_ids = match self
            .inner
            .domain
            .presence_observer_account_ids(account_id)
            .await
        {
            Ok(account_ids) => account_ids,
            Err(err) => {
                tracing::warn!(error = %err, "failed to resolve presence.update recipients");
                return;
            }
        };

        // The subject is authorized to observe itself (it shares its own
        // channels), but telling it is always redundant: the only one of its
        // sockets that could receive this is the one that just caused the
        // transition. Dropping it keeps the frame stream free of news a
        // client already has.
        account_ids.retain(|recipient| *recipient != account_id);

        self.send_to(
            &account_ids,
            &ServerEvent::PresenceUpdate { account_id, status },
        )
        .await;
    }

    /// Fans a declared status change to the same observers as `publish_presence`.
    /// Reuses `presence_observer_account_ids` so the two presence axes share
    /// exactly one authorization set. Best-effort and infallible for the same
    /// reasons: a failed lookup must not fail the HTTP request that just wrote
    /// the new status (the row is already durably saved), and no replay buffer
    /// exists to recover a dropped frame — the next profile fetch reconciles it.
    pub async fn publish_declared_status(&self, account_id: Uuid, status: DeclaredStatus) {
        let mut account_ids = match self
            .inner
            .domain
            .presence_observer_account_ids(account_id)
            .await
        {
            Ok(account_ids) => account_ids,
            Err(err) => {
                tracing::warn!(error = %err, "failed to resolve presence.status_update recipients");
                return;
            }
        };

        account_ids.retain(|recipient| *recipient != account_id);

        self.send_to(
            &account_ids,
            &ServerEvent::PresenceStatusUpdate { account_id, status },
        )
        .await;
    }

    /// Serializes `event` once and pushes it to every currently-registered
    /// connection of each account in `account_ids`. A send failing (the
    /// connection is already gone) is silently skipped — one dead connection
    /// must never fail the whole publish.
    /// One recipient. Voice signalling is point-to-point by nature, so it
    /// never goes through the channel fan-out `publish` uses — sending an SDP
    /// offer to everyone authorized for the channel would hand a private
    /// negotiation to bystanders.
    async fn send_to_account(&self, account_id: Uuid, event: &ServerEvent) {
        self.send_to(std::slice::from_ref(&account_id), event).await;
    }

    async fn send_to_connection(
        &self,
        account_id: Uuid,
        handle: ConnectionHandle,
        event: &ServerEvent,
    ) {
        let payload = match serde_json::to_string(event) {
            Ok(payload) => payload,
            Err(_) => return,
        };

        let connections = self.inner.connections.read().await;
        if let Some(list) = connections.get(&account_id) {
            if let Some(conn) = list.iter().find(|c| c.handle == handle) {
                let _ = conn.sender.send(payload);
            }
        }
    }

    async fn send_to(&self, account_ids: &[Uuid], event: &ServerEvent) {
        let payload = match serde_json::to_string(event) {
            Ok(payload) => payload,
            // Unreachable in practice: every `ServerEvent` field is
            // trivially serializable. Never fail the publish over it.
            Err(_) => return,
        };

        let connections = self.inner.connections.read().await;
        for account_id in account_ids {
            if let Some(list) = connections.get(account_id) {
                for conn in list {
                    let _ = conn.sender.send(payload.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // `Hub::publish_*` needs `domain::DomainService::authorized_account_ids`
    // to resolve recipients, which needs a real Postgres — these unit tests
    // instead exercise only the parts of `Hub` that don't touch the domain
    // layer at all: connection registry bookkeeping. The
    // authorization-aware fan-out behavior itself is covered by the
    // testcontainers-backed integration test in
    // crates/realtime/tests/hub.rs.
    //
    // `Hub::new` requires a `domain::DomainService`, which requires a
    // `db::PgPool` — `connect_lazy` builds one synchronously without
    // actually opening a connection (it only connects lazily, on first
    // query), which is exactly what these registry-only tests need.
    //
    // `register`/`unregister` DO reach for the domain layer now, to resolve
    // presence recipients, but that is best-effort: the lookup fails against
    // this pool and is logged and dropped, leaving the registry bookkeeping
    // under test untouched. The short `acquire_timeout` is what keeps that
    // failure fast — sqlx otherwise retries a refused connection for its
    // 30-second default before giving up.
    fn hub_for_registry_test() -> Hub {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(50))
            .connect_lazy("postgres://user:pass@localhost/nonexistent")
            .expect("lazy pool construction never opens a connection");
        Hub::new(domain::DomainService::new(pool))
    }

    #[tokio::test]
    async fn register_then_unregister_removes_the_connection_entirely() {
        let hub = hub_for_registry_test();
        let account_id = Uuid::nil();

        let (handle, _rx) = hub.register(account_id).await;

        {
            let connections = hub.inner.connections.read().await;
            assert_eq!(connections.get(&account_id).map(Vec::len), Some(1));
        }

        hub.unregister(account_id, handle).await;

        let connections = hub.inner.connections.read().await;
        assert!(
            !connections.contains_key(&account_id),
            "unregistering the only connection must drop the account's entry entirely, not leave an empty Vec"
        );
    }

    #[tokio::test]
    async fn unregister_only_removes_the_targeted_connection() {
        let hub = hub_for_registry_test();
        let account_id = Uuid::nil();

        let (handle_a, _rx_a) = hub.register(account_id).await;
        let (handle_b, mut rx_b) = hub.register(account_id).await;

        hub.unregister(account_id, handle_a).await;

        {
            let connections = hub.inner.connections.read().await;
            let list = connections
                .get(&account_id)
                .expect("second connection remains");
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].handle, handle_b);
        }

        // The surviving connection's sender must still be live.
        let sender = {
            let connections = hub.inner.connections.read().await;
            connections.get(&account_id).unwrap()[0].sender.clone()
        };
        sender
            .send("still alive".to_string())
            .expect("send succeeds");
        let received = tokio::time::timeout(Duration::from_secs(1), rx_b.recv())
            .await
            .expect("does not hang")
            .expect("channel is still open");
        assert_eq!(received, "still alive");
    }

    /// Drains whatever is already queued for a connection, as parsed JSON.
    /// Non-blocking on purpose: every voice event under test is sent before
    /// the call under test returns, so anything not here by now is a bug
    /// rather than something worth waiting for.
    fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            if let Ok(value) = serde_json::from_str(&payload) {
                out.push(value);
            }
        }
        out
    }

    fn types_of(events: &[serde_json::Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| e["type"].as_str().map(str::to_string))
            .collect()
    }

    #[tokio::test]
    async fn joining_hands_the_joiner_the_roster_and_tells_everyone_else() {
        let hub = hub_for_registry_test();
        let channel = Uuid::from_u128(1);
        let alice = Uuid::from_u128(10);
        let bob = Uuid::from_u128(11);

        let (alice_handle, mut alice_rx) = hub.register(alice).await;
        let (bob_handle, mut bob_rx) = hub.register(bob).await;

        hub.voice_join(channel, alice, alice_handle).await;
        let _ = drain(&mut alice_rx);

        let peers = hub.voice_join(channel, bob, bob_handle).await;

        assert_eq!(peers, vec![alice], "bob must be told who was already there");

        // Bob gets the roster, not a replay of joins: a mesh client opens one
        // connection per existing peer and needs a snapshot to do it. This is
        // sent directly to the joiner, so it works without the domain layer.
        let bob_events = drain(&mut bob_rx);
        assert_eq!(types_of(&bob_events), vec!["voice.state"]);
        assert_eq!(bob_events[0]["data"]["account_ids"][0], alice.to_string());

        // Alice's `voice.join` announcement is NOT asserted here. It goes out
        // channel-wide through `publish`, which resolves recipients through
        // the domain layer and therefore needs a real Postgres — the same
        // reason `publish_message_create`'s fan-out is covered by the
        // testcontainers integration test rather than here. What IS checked is
        // that the joiner does not receive it: getting both `voice.state` and
        // a `voice.join` for themselves would have them build a duplicate
        // connection to a peer they already hold.
        let alice_events = drain(&mut alice_rx);
        assert!(
            !types_of(&alice_events).contains(&"voice.state".to_string()),
            "voice.state belongs to the joiner alone"
        );
    }

    #[tokio::test]
    async fn a_dropped_socket_leaves_the_call() {
        let hub = hub_for_registry_test();
        let channel = Uuid::from_u128(1);
        let alice = Uuid::from_u128(10);
        let bob = Uuid::from_u128(11);

        let (alice_handle, mut alice_rx) = hub.register(alice).await;
        let (bob_handle, _bob_rx) = hub.register(bob).await;
        hub.voice_join(channel, alice, alice_handle).await;
        hub.voice_join(channel, bob, bob_handle).await;
        let _ = drain(&mut alice_rx);

        // Closing the tab, not pressing "leave". Without this, the call keeps
        // a participant nobody can hear and nobody can remove.
        hub.unregister(bob, bob_handle).await;

        // The state change is what matters and is what this level can see; the
        // `voice.leave` announcement is channel-wide through `publish` and so
        // needs the domain layer, like the join above.
        assert!(hub.voice_channel_of(bob).await.is_none());
        // Alice is still in it, so the room survives with just her — the
        // roster is the observable proof that bob is gone from it.
        assert_eq!(
            hub.voice_rosters(&[channel]).await.get(&channel),
            Some(&vec![alice])
        );
        let _ = drain(&mut alice_rx);
    }

    #[tokio::test]
    async fn closing_a_second_tab_does_not_hang_up_the_call() {
        let hub = hub_for_registry_test();
        let channel = Uuid::from_u128(1);
        let alice = Uuid::from_u128(10);

        let (call_handle, _call_rx) = hub.register(alice).await;
        let (other_tab, _other_rx) = hub.register(alice).await;
        hub.voice_join(channel, alice, call_handle).await;

        hub.unregister(alice, other_tab).await;

        assert_eq!(
            hub.voice_channel_of(alice).await,
            Some(channel),
            "the call is held by one connection; a different tab closing must not end it"
        );
    }

    #[tokio::test]
    async fn joining_a_second_channel_leaves_the_first() {
        let hub = hub_for_registry_test();
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let alice = Uuid::from_u128(10);

        let (handle, _rx) = hub.register(alice).await;
        hub.voice_join(first, alice, handle).await;
        hub.voice_join(second, alice, handle).await;

        assert_eq!(hub.voice_channel_of(alice).await, Some(second));
    }

    #[tokio::test]
    async fn signalling_is_refused_between_accounts_not_in_the_same_call() {
        let hub = hub_for_registry_test();
        let channel = Uuid::from_u128(1);
        let elsewhere = Uuid::from_u128(2);
        let alice = Uuid::from_u128(10);
        let stranger = Uuid::from_u128(12);

        let (alice_handle, _alice_rx) = hub.register(alice).await;
        let (stranger_handle, mut stranger_rx) = hub.register(stranger).await;
        hub.voice_join(channel, alice, alice_handle).await;
        let _ = drain(&mut stranger_rx);

        // Not in any call at all.
        assert!(
            !hub.voice_relay(alice, stranger, serde_json::json!({"sdp": "x"}))
                .await,
            "relaying to an account that is not in a call must be refused"
        );

        // In a call, but a different one. This is the case that matters: it is
        // the difference between voice signalling and an authenticated way to
        // push arbitrary JSON at any account on the service.
        hub.voice_join(elsewhere, stranger, stranger_handle).await;
        let _ = drain(&mut stranger_rx);
        assert!(
            !hub.voice_relay(alice, stranger, serde_json::json!({"sdp": "x"}))
                .await,
            "relaying across two different calls must be refused"
        );

        assert!(
            drain(&mut stranger_rx).is_empty(),
            "a refused relay must send nothing at all"
        );
    }

    #[tokio::test]
    async fn signalling_reaches_the_other_participant_of_the_same_call() {
        let hub = hub_for_registry_test();
        let channel = Uuid::from_u128(1);
        let alice = Uuid::from_u128(10);
        let bob = Uuid::from_u128(11);

        let (alice_handle, _alice_rx) = hub.register(alice).await;
        let (bob_handle, mut bob_rx) = hub.register(bob).await;
        hub.voice_join(channel, alice, alice_handle).await;
        hub.voice_join(channel, bob, bob_handle).await;
        let _ = drain(&mut bob_rx);

        assert!(
            hub.voice_relay(alice, bob, serde_json::json!({"sdp": "offer"}))
                .await
        );

        let events = drain(&mut bob_rx);
        assert_eq!(types_of(&events), vec!["voice.signal"]);
        assert_eq!(events[0]["data"]["from_account_id"], alice.to_string());
        // Relayed verbatim: the server has no business parsing SDP, and a
        // test that accepted a rewritten payload would let that change.
        assert_eq!(events[0]["data"]["signal"]["sdp"], "offer");
    }
}
