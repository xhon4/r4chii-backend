//! Integration coverage for `Hub::publish_*`'s authorization-scoped fan-out
//! — the parts of `Hub` that need a real `domain::DomainService` (and so a
//! real Postgres) to exercise, unlike the registry-only unit tests in
//! `crates/realtime/src/hub.rs`. Same shared-Postgres pattern as
//! `crates/domain/tests/domain_service.rs`.

use std::time::Duration;

use auth::{AuthService, RegisterInput};
use domain::{
    channel_permissions, CreateChannelInput, CreateRoleInput, CreateServerInput, DomainService,
    SendMessageInput,
};
use realtime::{Hub, MemberLeaveReason};
use test_support::TestDb;
use tokio::time::timeout;

async fn test_services() -> (Hub, DomainService, AuthService, TestDb) {
    let test_db = test_support::test_db().await;
    let pool = test_db.pool();

    let domain = DomainService::new(pool.clone());
    let auth = AuthService::new(pool, std::sync::Arc::new(mailer::CaptureMailer::new()));
    let hub = Hub::new(domain.clone());

    (hub, domain, auth, test_db)
}

fn register_input(email: &str, username: &str) -> RegisterInput {
    RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: "Test User".to_string(),
    }
}

async fn register(auth: &AuthService, email: &str, username: &str) -> app_core::Uuid {
    auth.create_verified_account(register_input(email, username))
        .await
        .expect("registration succeeds")
        .id
}

/// Bounded wait so a failing assertion (a frame that never arrives) fails
/// fast instead of hanging the test suite.
async fn recv_within(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    duration: Duration,
) -> Option<String> {
    timeout(duration, rx.recv()).await.unwrap_or(None)
}

/// A server with one `text` channel owned by `owner`, returned as
/// `(server_id, invite_code, channel_id)` so a second account can join it.
async fn server_with_channel(
    domain: &DomainService,
    owner: app_core::Uuid,
) -> (app_core::Uuid, String, app_core::Uuid) {
    let server = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Alice's Place".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create_server succeeds");
    let invite_code = server
        .invite_code
        .clone()
        .expect("the owner's own view carries the invite code");
    let channel = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    (server.id, invite_code, channel.id)
}

/// Asserts one frame is exactly the `presence.update` the spec pins:
/// `{ account_id, status }`.
fn assert_presence(frame: &str, account_id: app_core::Uuid, status: &str) {
    let value: serde_json::Value = serde_json::from_str(frame).expect("frame is valid JSON");
    assert_eq!(value["type"], "presence.update");
    assert_eq!(value["data"]["account_id"], account_id.to_string());
    assert_eq!(value["data"]["status"], status);
}

#[tokio::test]
async fn publish_message_create_only_reaches_authorized_connections() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let outsider = register(&auth, "outsider@example.com", "outsider").await;

    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Alice's Place".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    // Alice is a member of the server that owns this channel; the outsider
    // never joined anything.
    let (_alice_handle, mut alice_rx) = hub.register(alice).await;
    let (_outsider_handle, mut outsider_rx) = hub.register(outsider).await;

    let message = domain
        .send_message(
            alice,
            channel.id,
            SendMessageInput {
                content: "hello".to_string(),
            },
        )
        .await
        .expect("send_message succeeds");

    hub.publish_message_create(channel.id, &message)
        .await
        .expect("publish succeeds");

    let alice_frame = recv_within(&mut alice_rx, Duration::from_secs(2))
        .await
        .expect("the authorized member's connection receives the event");
    assert!(alice_frame.contains("\"type\":\"message.create\""));
    assert!(alice_frame.contains("hello"));

    let outsider_frame = recv_within(&mut outsider_rx, Duration::from_millis(300)).await;
    assert!(
        outsider_frame.is_none(),
        "an account with no access to the channel must never receive its events"
    );
}

#[tokio::test]
async fn unregister_stops_delivery_to_a_disconnected_connection() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Alice's Place".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    let (handle, mut rx) = hub.register(alice).await;
    hub.unregister(alice, handle).await;

    let message = domain
        .send_message(
            alice,
            channel.id,
            SendMessageInput {
                content: "hello".to_string(),
            },
        )
        .await
        .expect("send_message succeeds");

    hub.publish_message_create(channel.id, &message)
        .await
        .expect("publish succeeds even with no live connections");

    let frame = recv_within(&mut rx, Duration::from_millis(300)).await;
    assert!(
        frame.is_none(),
        "a disconnected (unregistered) connection must never receive a delivery"
    );
}

#[tokio::test]
async fn a_first_connection_publishes_presence_online_to_a_server_peer() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (_server_id, invite_code, _channel_id) = server_with_channel(&domain, alice).await;
    domain
        .join_via_invite(bob, &invite_code)
        .await
        .expect("bob joins alice's server");

    // Bob connects first. He is never told about his own transition — the
    // only socket of his that could hear it is the one that just connected.
    let (_bob_handle, mut bob_rx) = hub.register(bob).await;

    let (_alice_handle, _alice_rx) = hub.register(alice).await;

    let frame = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("a server peer sees the first socket come online");
    assert_presence(&frame, alice, "online");
}

#[tokio::test]
async fn losing_the_last_connection_publishes_presence_offline() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (_server_id, invite_code, _channel_id) = server_with_channel(&domain, alice).await;
    domain
        .join_via_invite(bob, &invite_code)
        .await
        .expect("bob joins alice's server");

    let (_bob_handle, mut bob_rx) = hub.register(bob).await;

    let (alice_handle, _alice_rx) = hub.register(alice).await;
    let online = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("alice's online transition arrives");
    assert_presence(&online, alice, "online");

    hub.unregister(alice, alice_handle).await;

    let frame = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("a server peer sees the last socket go offline");
    assert_presence(&frame, alice, "offline");
}

/// The multi-tab trap: presence tracks the account's *socket count*
/// transitioning across zero, not individual socket lifecycles. Two open
/// tabs closing one must not make the account appear offline while it is
/// still very much connected — and the second tab opening must not re-announce
/// an account that was already online.
#[tokio::test]
async fn a_second_concurrent_connection_neither_re_announces_online_nor_flaps_offline() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (_server_id, invite_code, _channel_id) = server_with_channel(&domain, alice).await;
    domain
        .join_via_invite(bob, &invite_code)
        .await
        .expect("bob joins alice's server");

    let (_bob_handle, mut bob_rx) = hub.register(bob).await;

    // Tab one: 0 -> 1 sockets, the only transition that announces `online`.
    let (alice_first, _alice_first_rx) = hub.register(alice).await;
    let online = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("the first socket announces online");
    assert_presence(&online, alice, "online");

    // Tab two: 1 -> 2 sockets, no transition, no event.
    let (alice_second, _alice_second_rx) = hub.register(alice).await;
    assert!(
        recv_within(&mut bob_rx, Duration::from_millis(300))
            .await
            .is_none(),
        "a second concurrent socket must not re-announce an already-online account"
    );

    // Closing tab one: 2 -> 1 sockets, still online, no event.
    hub.unregister(alice, alice_first).await;
    assert!(
        recv_within(&mut bob_rx, Duration::from_millis(300))
            .await
            .is_none(),
        "closing one of two tabs must never make a still-connected account appear offline"
    );

    // Closing tab two: 1 -> 0 sockets, now genuinely gone.
    hub.unregister(alice, alice_second).await;
    let offline = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("the last socket closing announces offline");
    assert_presence(&offline, alice, "offline");
}

#[tokio::test]
async fn presence_never_reaches_an_account_sharing_nothing_with_the_subject() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let outsider = register(&auth, "outsider@example.com", "outsider").await;

    // Both have a server of their own, so both are legitimately connected
    // accounts with channels — they just share none with each other, which is
    // the same authorization boundary `message.create` fan-out enforces.
    let (_server_id, _invite_code, _channel_id) = server_with_channel(&domain, alice).await;
    let (_outsider_server, _outsider_invite, _outsider_channel) =
        server_with_channel(&domain, outsider).await;

    let (_outsider_handle, mut outsider_rx) = hub.register(outsider).await;
    let (alice_handle, _alice_rx) = hub.register(alice).await;
    hub.unregister(alice, alice_handle).await;

    let frame = recv_within(&mut outsider_rx, Duration::from_millis(300)).await;
    assert!(
        frame.is_none(),
        "an account with no shared channel must never learn whether someone is online"
    );
}

#[tokio::test]
async fn kick_or_ban_evicts_user_from_voice_and_broadcasts_voice_leave() {
    let (hub, domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner_k@example.com", "owner_k").await;
    let bob = register(&auth, "bob_k@example.com", "bob_k").await;

    let server = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Voice Eviction Server".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create_server succeeds");
    let invite = server.invite_code.expect("invite code exists");
    domain
        .join_via_invite(bob, &invite)
        .await
        .expect("bob joins");

    let voice_channel = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "Voice Room".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create_channel succeeds");

    let (owner_handle, mut owner_rx) = hub.register(owner).await;
    let (bob_handle, mut _bob_rx) = hub.register(bob).await;

    // Both join voice
    hub.voice_join(voice_channel.id, owner, owner_handle).await;
    hub.voice_join(voice_channel.id, bob, bob_handle).await;

    // Drain join notifications from owner_rx
    while recv_within(&mut owner_rx, Duration::from_millis(50))
        .await
        .is_some()
    {}

    // Verify Bob is in voice
    assert_eq!(hub.voice_channel_of(bob).await, Some(voice_channel.id));
    let rosters = hub.voice_rosters(&[voice_channel.id]).await;
    let room = rosters.get(&voice_channel.id).expect("room exists");
    assert!(room.contains(&owner));
    assert!(room.contains(&bob));

    // Owner kicks Bob
    let recipients = domain
        .kick_member(owner, server.id, bob)
        .await
        .expect("kick succeeds");
    hub.announce_member_leave(&recipients, server.id, bob, MemberLeaveReason::Kicked)
        .await;

    // Bob must be evicted from voice
    assert_eq!(hub.voice_channel_of(bob).await, None);
    let rosters_after = hub.voice_rosters(&[voice_channel.id]).await;
    let room_after = rosters_after.get(&voice_channel.id).expect("room exists");
    assert!(room_after.contains(&owner));
    assert!(!room_after.contains(&bob));

    // Owner must receive ServerEvent::VoiceLeave for Bob
    let event_str = recv_within(&mut owner_rx, Duration::from_secs(2))
        .await
        .expect("owner receives voice leave event");
    let parsed: serde_json::Value = serde_json::from_str(&event_str).expect("json parses");
    assert_eq!(parsed["type"], "voice.leave");
    assert_eq!(parsed["data"]["account_id"], bob.to_string());
    assert_eq!(parsed["data"]["channel_id"], voice_channel.id.to_string());
}

#[tokio::test]
async fn kick_from_one_server_does_not_evict_from_another_servers_voice_channel() {
    let (hub, domain, auth, _container) = test_services().await;
    let owner1 = register(&auth, "owner1@example.com", "owner1").await;
    let owner2 = register(&auth, "owner2@example.com", "owner2").await;
    let bob = register(&auth, "bob_multi@example.com", "bob_multi").await;

    let server1 = domain
        .create_server(
            owner1,
            CreateServerInput {
                name: "Server 1".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server 1");
    let invite1 = server1.invite_code.expect("invite 1");
    domain
        .join_via_invite(bob, &invite1)
        .await
        .expect("bob joins server 1");

    let server2 = domain
        .create_server(
            owner2,
            CreateServerInput {
                name: "Server 2".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server 2");
    let invite2 = server2.invite_code.expect("invite 2");
    domain
        .join_via_invite(bob, &invite2)
        .await
        .expect("bob joins server 2");

    let voice_channel2 = domain
        .create_channel(
            owner2,
            server2.id,
            CreateChannelInput {
                name: "Server 2 Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create voice channel in server 2");

    let (bob_handle, _bob_rx) = hub.register(bob).await;

    // Bob joins voice on Server 2
    hub.voice_join(voice_channel2.id, bob, bob_handle).await;
    assert_eq!(hub.voice_channel_of(bob).await, Some(voice_channel2.id));

    // Bob is kicked from Server 1
    let recipients = domain
        .kick_member(owner1, server1.id, bob)
        .await
        .expect("kick from server 1 succeeds");
    hub.announce_member_leave(&recipients, server1.id, bob, MemberLeaveReason::Kicked)
        .await;

    // Bob must STILL be in Server 2's voice channel
    assert_eq!(hub.voice_channel_of(bob).await, Some(voice_channel2.id));
}

#[tokio::test]
async fn voice_relay_delivers_only_to_call_holding_connection_and_isolates_channels() {
    let (hub, domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner_vr@example.com", "owner_vr").await;
    let bob = register(&auth, "bob_vr@example.com", "bob_vr").await;
    let charlie = register(&auth, "charlie_vr@example.com", "charlie_vr").await;

    let server = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Relay Server".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server");
    let invite = server.invite_code.expect("invite");
    domain
        .join_via_invite(bob, &invite)
        .await
        .expect("bob joins");
    domain
        .join_via_invite(charlie, &invite)
        .await
        .expect("charlie joins");

    let voice_channel1 = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "Voice 1".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("voice 1");

    let voice_channel2 = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "Voice 2".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("voice 2");

    let (owner_handle, _owner_rx) = hub.register(owner).await;
    let (bob_tab1, mut bob_tab1_rx) = hub.register(bob).await;
    let (_bob_tab2, mut bob_tab2_rx) = hub.register(bob).await;
    let (charlie_handle, mut charlie_rx) = hub.register(charlie).await;

    // Owner and Bob (Tab 1) join Voice Channel 1
    hub.voice_join(voice_channel1.id, owner, owner_handle).await;
    hub.voice_join(voice_channel1.id, bob, bob_tab1).await;

    // Charlie joins Voice Channel 2
    hub.voice_join(voice_channel2.id, charlie, charlie_handle)
        .await;

    // Drain initial messages
    while recv_within(&mut bob_tab1_rx, Duration::from_millis(50))
        .await
        .is_some()
    {}
    while recv_within(&mut bob_tab2_rx, Duration::from_millis(50))
        .await
        .is_some()
    {}
    while recv_within(&mut charlie_rx, Duration::from_millis(50))
        .await
        .is_some()
    {}

    // Owner sends voice signal to Bob
    let signal_payload = serde_json::json!({ "sdp": { "type": "offer", "sdp": "mock-sdp" } });
    let relayed = hub.voice_relay(owner, bob, signal_payload.clone()).await;
    assert!(
        relayed,
        "voice_relay must succeed for peers in the same room"
    );

    // Bob Tab 1 (in the call) receives the signal
    let event_str = recv_within(&mut bob_tab1_rx, Duration::from_secs(2))
        .await
        .expect("bob tab 1 receives signal");
    let parsed: serde_json::Value = serde_json::from_str(&event_str).expect("json parses");
    assert_eq!(parsed["type"], "voice.signal");
    assert_eq!(parsed["data"]["channel_id"], voice_channel1.id.to_string());
    assert_eq!(parsed["data"]["from_account_id"], owner.to_string());
    assert_eq!(parsed["data"]["signal"]["sdp"]["type"], "offer");

    // Bob Tab 2 (not in the call) receives NOTHING
    let tab2_frame = recv_within(&mut bob_tab2_rx, Duration::from_millis(200)).await;
    assert!(
        tab2_frame.is_none(),
        "signals must not leak to tabs not in the call"
    );

    // Owner attempts to send voice signal to Charlie (who is in Voice Channel 2, not 1)
    let cross_relayed = hub.voice_relay(owner, charlie, signal_payload).await;
    assert!(
        !cross_relayed,
        "voice_relay must refuse signals between different voice rooms"
    );
    let charlie_frame = recv_within(&mut charlie_rx, Duration::from_millis(200)).await;
    assert!(
        charlie_frame.is_none(),
        "charlie must receive no signal from another channel"
    );
}

/// Pins `Hub::voice_leave_from_channel`'s atomicity property directly rather
/// than forcing the actual TOCTOU race through `voice_leave_if_in_server`.
///
/// The real race needs a DB round trip (`channel_belongs_to_server`) to land
/// in the exact window between reading an account's current voice channel and
/// evicting it — a timing-dependent interleaving with a real Postgres round
/// trip in the middle that a test cannot force deterministically. What CAN be
/// pinned deterministically, and what `voice_leave_from_channel` exists to
/// guarantee, is the atomicity itself: eviction only happens if the account
/// is still in exactly the validated channel at the moment of removal. This
/// test drives that directly through the public `Hub` API — join the
/// channel that will be "validated", then move to an unrelated channel
/// (simulating the account moving during the DB await), then call
/// `voice_leave_from_channel` with the stale, validated channel id and assert
/// it is a no-op; finally call it with the account's real current channel and
/// assert it evicts.
#[tokio::test]
async fn voice_leave_from_channel_is_a_no_op_unless_still_in_that_exact_channel() {
    let (hub, domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner_toctou@example.com", "owner_toctou").await;
    let bob = register(&auth, "bob_toctou@example.com", "bob_toctou").await;

    let server_a = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Server A".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server a");
    let invite_a = server_a.invite_code.expect("invite a");
    domain
        .join_via_invite(bob, &invite_a)
        .await
        .expect("bob joins server a");
    let voice_a = domain
        .create_channel(
            owner,
            server_a.id,
            CreateChannelInput {
                name: "Voice A".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create voice channel a");

    let server_b = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Server B".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server b");
    let invite_b = server_b.invite_code.expect("invite b");
    domain
        .join_via_invite(bob, &invite_b)
        .await
        .expect("bob joins server b");
    let voice_b = domain
        .create_channel(
            owner,
            server_b.id,
            CreateChannelInput {
                name: "Voice B".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create voice channel b");

    let (bob_handle, _bob_rx) = hub.register(bob).await;

    // Bob was "validated" in server A's voice channel (this stands in for the
    // stale `voice_channel_of` read an eviction caller would have taken
    // before its DB await)...
    hub.voice_join(voice_a.id, bob, bob_handle).await;

    // ...but by the time the eviction is actually attempted, he has moved to
    // server B's voice channel — the exact interleaving F1 closes.
    hub.voice_join(voice_b.id, bob, bob_handle).await;
    assert_eq!(hub.voice_channel_of(bob).await, Some(voice_b.id));

    // Evicting from the STALE, validated channel (A) must do nothing: bob is
    // no longer there.
    hub.voice_leave_from_channel(bob, voice_a.id).await;
    assert_eq!(
        hub.voice_channel_of(bob).await,
        Some(voice_b.id),
        "an eviction scoped to a channel the account already left must not touch the call it moved to"
    );

    // Evicting from the channel he is ACTUALLY in must succeed.
    hub.voice_leave_from_channel(bob, voice_b.id).await;
    assert_eq!(
        hub.voice_channel_of(bob).await,
        None,
        "an eviction scoped to the account's real current channel must still evict"
    );
}

#[tokio::test]
async fn revoking_a_channel_role_stops_message_and_voice_fan_out() {
    let (hub, domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner_revoked@example.com", "owner_revoked").await;
    let member = register(&auth, "member_revoked@example.com", "member_revoked").await;

    let server = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Revoked Access".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server succeeds");
    domain
        .join_via_invite(member, server.invite_code.as_ref().unwrap())
        .await
        .expect("member joins");
    let channel = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "staff-voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create channel succeeds");
    domain
        .update_channel_restricted(owner, server.id, channel.id, true)
        .await
        .expect("restricting channel succeeds");
    let role = domain
        .create_role(
            owner,
            server.id,
            CreateRoleInput {
                name: "Staff".to_string(),
            },
        )
        .await
        .expect("create role succeeds");
    domain
        .set_member_roles(owner, server.id, member, vec![role.id])
        .await
        .expect("assign role succeeds");
    domain
        .set_channel_role_permission(
            owner,
            server.id,
            channel.id,
            role.id,
            channel_permissions::VIEW_CHANNEL,
        )
        .await
        .expect("grant channel view succeeds");

    let (owner_handle, _owner_rx) = hub.register(owner).await;
    let (_member_handle, mut member_rx) = hub.register(member).await;
    while recv_within(&mut member_rx, Duration::from_millis(50))
        .await
        .is_some()
    {}

    let first = domain
        .send_message(
            owner,
            channel.id,
            SendMessageInput {
                content: "before revocation".to_string(),
            },
        )
        .await
        .expect("send succeeds");
    hub.publish_message_create(channel.id, &first)
        .await
        .expect("publish succeeds");
    assert!(recv_within(&mut member_rx, Duration::from_secs(2))
        .await
        .expect("granted member receives the message")
        .contains("message.create"));

    hub.voice_join(channel.id, owner, owner_handle).await;
    assert!(recv_within(&mut member_rx, Duration::from_secs(2))
        .await
        .expect("granted member receives the voice event")
        .contains("voice.join"));

    domain
        .set_member_roles(owner, server.id, member, vec![])
        .await
        .expect("revoke role succeeds");
    hub.voice_leave(owner, None).await;

    let second = domain
        .send_message(
            owner,
            channel.id,
            SendMessageInput {
                content: "after revocation".to_string(),
            },
        )
        .await
        .expect("send succeeds");
    hub.publish_message_create(channel.id, &second)
        .await
        .expect("publish succeeds");

    assert!(
        recv_within(&mut member_rx, Duration::from_millis(300))
            .await
            .is_none(),
        "a revoked member must receive neither voice.leave nor message.create"
    );
}

// A dm/group_dm has no `server_id`, so `publish_channel_create`'s viewer
// lookup cannot resolve one. `announce_dm_create` takes the roster the
// caller already holds — without it a DM someone else opens never reaches
// that account until it reloads.
#[tokio::test]
async fn announce_dm_create_reaches_every_participant_and_nobody_else() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let outsider = register(&auth, "outsider@example.com", "outsider").await;

    let (_alice_handle, mut alice_rx) = hub.register(alice).await;
    let (_bob_handle, mut bob_rx) = hub.register(bob).await;
    let (_outsider_handle, mut outsider_rx) = hub.register(outsider).await;

    let (channel, created) = domain
        .create_dm(alice, bob)
        .await
        .expect("create_dm succeeds");
    assert!(created);

    hub.announce_dm_create(&channel).await;

    // Bob never touched this channel — the event is the only way he learns
    // it exists, and it must carry the roster he will label it with.
    let bob_frame = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("the other participant receives the event");
    assert!(bob_frame.contains("\"type\":\"channel.create\""));
    assert!(bob_frame.contains(&alice.to_string()));
    assert!(bob_frame.contains(&bob.to_string()));

    let alice_frame = recv_within(&mut alice_rx, Duration::from_secs(2))
        .await
        .expect("the creator receives the event too");
    assert!(alice_frame.contains("\"type\":\"channel.create\""));

    let outsider_frame = recv_within(&mut outsider_rx, Duration::from_millis(300)).await;
    assert!(
        outsider_frame.is_none(),
        "a private conversation must never reach a non-participant"
    );
}

#[tokio::test]
async fn announce_dm_create_reaches_every_member_of_a_group_dm() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let (_bob_handle, mut bob_rx) = hub.register(bob).await;
    let (_carol_handle, mut carol_rx) = hub.register(carol).await;

    let channel = domain
        .create_group_dm(
            alice,
            domain::CreateGroupDmInput {
                account_ids: vec![bob, carol],
            },
        )
        .await
        .expect("create_group_dm succeeds");

    hub.announce_dm_create(&channel).await;

    for (name, rx) in [("bob", &mut bob_rx), ("carol", &mut carol_rx)] {
        let frame = recv_within(rx, Duration::from_secs(2))
            .await
            .unwrap_or_else(|| panic!("{name} receives the event"));
        assert!(frame.contains("\"type\":\"channel.create\""));
        assert!(frame.contains("group_dm"));
    }
}

// A friendship row is stored order-agnostic and projected per viewer:
// `account_id` always names the OTHER party. Both sides therefore get the
// same row with that one field flipped — without this event, accepting a
// request is invisible to the person who sent it until they reload.
#[tokio::test]
async fn publish_friendship_update_gives_each_side_its_own_projection() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let outsider = register(&auth, "outsider@example.com", "outsider").await;

    let (_alice_handle, mut alice_rx) = hub.register(alice).await;
    let (_bob_handle, mut bob_rx) = hub.register(bob).await;
    let (_outsider_handle, mut outsider_rx) = hub.register(outsider).await;

    let (friendship, changed) = domain
        .send_friend_request(alice, bob)
        .await
        .expect("send_friend_request succeeds");
    assert!(changed);

    hub.publish_friendship_update(&friendship, alice).await;

    let bob_frame = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("the recipient of the request is told about it");
    let bob_event: serde_json::Value =
        serde_json::from_str(&bob_frame).expect("a json frame");
    assert_eq!(bob_event["type"], "friendship.update");
    // Bob's view names alice, not himself.
    assert_eq!(bob_event["data"]["friendship"]["account_id"], alice.to_string());
    assert_eq!(bob_event["data"]["friendship"]["status"], "pending");
    assert_eq!(
        bob_event["data"]["friendship"]["requested_by"],
        alice.to_string()
    );

    let alice_frame = recv_within(&mut alice_rx, Duration::from_secs(2))
        .await
        .expect("the sender's other tabs are told too");
    let alice_event: serde_json::Value =
        serde_json::from_str(&alice_frame).expect("a json frame");
    assert_eq!(alice_event["type"], "friendship.update");
    // Alice's view names bob — the same row, flipped.
    assert_eq!(alice_event["data"]["friendship"]["account_id"], bob.to_string());

    let outsider_frame = recv_within(&mut outsider_rx, Duration::from_millis(300)).await;
    assert!(
        outsider_frame.is_none(),
        "a friendship is private to its two parties"
    );
}

// Accepting is the same endpoint as requesting, and is the leg that was
// reported broken: alice must learn that bob accepted without reloading.
#[tokio::test]
async fn accepting_a_request_reaches_the_account_that_sent_it() {
    let (hub, domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain
        .send_friend_request(alice, bob)
        .await
        .expect("send_friend_request succeeds");

    let (_alice_handle, mut alice_rx) = hub.register(alice).await;

    let (accepted, changed) = domain
        .send_friend_request(bob, alice)
        .await
        .expect("accepting succeeds");
    assert!(changed);
    assert_eq!(accepted.status, "accepted");

    hub.publish_friendship_update(&accepted, bob).await;

    let frame = recv_within(&mut alice_rx, Duration::from_secs(2))
        .await
        .expect("the original requester is told the request was accepted");
    let event: serde_json::Value = serde_json::from_str(&frame).expect("a json frame");
    assert_eq!(event["type"], "friendship.update");
    assert_eq!(event["data"]["friendship"]["status"], "accepted");
    assert_eq!(event["data"]["friendship"]["account_id"], bob.to_string());
}

#[tokio::test]
async fn publish_friendship_remove_reaches_both_parties_and_nobody_else() {
    let (hub, _domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let outsider = register(&auth, "outsider@example.com", "outsider").await;

    let (_alice_handle, mut alice_rx) = hub.register(alice).await;
    let (_bob_handle, mut bob_rx) = hub.register(bob).await;
    let (_outsider_handle, mut outsider_rx) = hub.register(outsider).await;

    hub.publish_friendship_remove(alice, bob).await;

    let bob_frame = recv_within(&mut bob_rx, Duration::from_secs(2))
        .await
        .expect("the removed party is told");
    let bob_event: serde_json::Value = serde_json::from_str(&bob_frame).expect("a json frame");
    assert_eq!(bob_event["type"], "friendship.remove");
    assert_eq!(bob_event["data"]["account_id"], alice.to_string());

    let alice_frame = recv_within(&mut alice_rx, Duration::from_secs(2))
        .await
        .expect("the actor's other tabs are told");
    let alice_event: serde_json::Value =
        serde_json::from_str(&alice_frame).expect("a json frame");
    assert_eq!(alice_event["type"], "friendship.remove");
    assert_eq!(alice_event["data"]["account_id"], bob.to_string());

    let outsider_frame = recv_within(&mut outsider_rx, Duration::from_millis(300)).await;
    assert!(outsider_frame.is_none());
}
