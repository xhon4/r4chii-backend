//! ICE server configuration for peer-to-peer voice.
//!
//! This is the only voice-related HTTP surface. Everything else — joining,
//! leaving, signalling — is carried on the gateway socket, because being in a
//! call is scoped to one live connection and an HTTP request has no connection
//! to be scoped to.
//!
//! What an ICE server list is for: two browsers behind NAT cannot generally
//! reach each other by guessing. STUN tells each peer what its own public
//! address looks like from outside, which is enough for most home routers.
//! When it is not — symmetric NAT, restrictive corporate networks — TURN
//! relays the media through a third party. This design accepts that TURN is a
//! paid dependency and that some calls will need it.
//!
//! TURN USED TO BE OPTIONAL HERE, AND THAT WAS A MISTAKE. The original reasoning
//! was that STUN alone covers peers on cooperative NAT, so a deployment could
//! try voice without paying for a relay first. That held for two people on the
//! same kind of network and fell apart the moment a real call had four.
//!
//! The reason it fails is worth writing down, because the symptom does not look
//! like a missing relay. Whether any PAIR of peers can reach each other
//! directly depends on BOTH of their NATs. Two cone-NAT peers find each other;
//! anyone behind symmetric NAT or CGNAT — ordinary on mobile networks and on
//! plenty of consumer ISPs — reaches nobody. So the failure is per pair, not per
//! user, and it presents as a matrix: A hears everyone, B hears only A and C, D
//! hears nobody. It reads as flakiness or as a bug in the signalling, and it is
//! neither. It is the absence of a route that no amount of client code can
//! create.
//!
//! So a relay is configured for real deployments, and `has_relay` reports
//! honestly when one is not, so the client can say "calls may fail across some
//! networks" rather than leaving someone to guess why they were the only one
//! nobody could hear.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use app_core::Uuid;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};

use crate::error::ApiError;
use crate::extract::AuthenticatedUser;
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    /// `RTCIceServer.urls` is `DOMString or sequence<DOMString>` in the WebRTC
    /// IDL, so ONE url may legitimately arrive either as a bare string or as a
    /// one-element array. Pinning only the array shape meant a string entry
    /// failed BOTH `CloudflareIcePayload` variants below, and the whole
    /// response then degraded to "no relay configured" — the single most
    /// expensive confusion in this file, reintroduced through the field that
    /// every entry has.
    #[serde(deserialize_with = "deserialize_urls")]
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub credential: Option<String>,
}

/// Accepts `urls` as either one string or a list of them, per the WebRTC IDL,
/// and normalises both to the list the rest of this module works with.
fn deserialize_urls<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(url) => vec![url],
        OneOrMany::Many(urls) => urls,
    })
}

impl IceServer {
    /// Whether this entry can actually RELAY media, as opposed to merely
    /// telling a peer what its own address looks like from outside.
    ///
    /// The distinction is the entire reason `has_relay` exists. A Cloudflare
    /// answer legitimately mixes STUN and TURN entries — the fixture in this
    /// module's own tests carries a credential-less STUN entry — so "the list
    /// is not empty" is a different question from "there is a relay in it".
    /// Answering the first while reporting the second suppressed the client's
    /// "calls may fail across some networks" warning for a call that had no
    /// relay behind it, which is the failure the warning exists to name.
    fn is_relay(&self) -> bool {
        let relay_url = self.urls.iter().any(|url| match url.split_once(':') {
            // Schemes are case-insensitive, and these come from a third party
            // rather than from this codebase.
            Some((scheme, _)) => {
                scheme.eq_ignore_ascii_case("turn") || scheme.eq_ignore_ascii_case("turns")
            }
            None => false,
        });

        // Credentials are the second signal, for a URL scheme this does not
        // recognise: nothing issues a username and a credential for a STUN
        // server, so an entry carrying both is a relay whatever its URL says.
        relay_url || (self.username.is_some() && self.credential.is_some())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IceConfigResponse {
    pub ice_servers: Vec<IceServer>,
    /// False when no TURN server is configured, so the client can warn that
    /// calls will fail on networks STUN cannot traverse rather than presenting
    /// a mysterious failure.
    pub has_relay: bool,
}

/// Public STUN, used when nothing else is configured.
///
/// Google's is the conventional default and costs nothing, but it does mean a
/// third party learns that some IP is starting a call. It reveals no audio and
/// no identity, and the alternative is voice that does not work even on a LAN,
/// so it is the accepted trade for a deployment with no relay configured.
const DEFAULT_STUN: &str = "stun:stun.l.google.com:19302";

/// Cloudflare Realtime TURN, which is where this deployment's relay comes from.
///
/// `generate-ice-servers` rather than `generate`: it answers with a list already
/// shaped like the `iceServers` a browser wants, including Cloudflare's own STUN
/// and TURN over UDP, TCP and TLS. The TLS-on-443 entry is the one that matters
/// most — it is what gets through networks that block everything else.
const CLOUDFLARE_TURN_ENDPOINT: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";

/// How long the credentials this hands out stay valid.
///
/// Long enough that nothing has to think about renewal mid-call, short enough
/// that a leaked pair stops being useful within a day. Credentials necessarily
/// reach the browser of everyone in a call, so they are not a secret — the TTL
/// is the whole control, which is why it exists rather than a static password
/// in the environment that would be valid forever.
const TURN_CREDENTIAL_TTL_SECS: u32 = 86_400;

/// Long enough for a round trip to Cloudflare, short enough that a hung relay
/// API does not hold a caller on the "joining" spinner. Exceeding it falls back
/// to STUN rather than failing: a call that might not cross every network beats
/// no call at all.
const TURN_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a minted set of credentials is reused before a fresh one is asked
/// for.
///
/// The margin against the TTL above is the point. Handing out a credential that
/// expires in a minute produces a call that connects and then loses its relay
/// mid-sentence, which is far harder to recognise than one that never
/// connected. An hour of reuse is longer than any call this is scoped to, so
/// nothing that starts on a cached credential can outlive it.
///
/// It is ONE hour rather than the twenty-three the margin arithmetic used to
/// produce, and the reason is the clock. `Instant` is `CLOCK_MONOTONIC`: it does
/// not advance while the host is suspended, so a laptop closed overnight or a VM
/// paused by its hypervisor comes back believing far less time has passed than
/// the wall clock the TTL is actually measured against. A 23h reuse window can
/// therefore outlive a 24h credential after a long enough suspend. An hour
/// bounds that exposure to something no realistic suspend overruns, and minting
/// costs nothing now that it is cached at all.
const TURN_CACHE_LIFETIME: Duration = Duration::from_secs(3_600);

/// How long a FAILED mint is remembered before Cloudflare is asked again.
///
/// Nothing used to be written on failure, so every joiner arriving while
/// Cloudflare was unreachable took the write lock in turn and paid its own full
/// `TURN_REQUEST_TIMEOUT`. Serialised, not shared: N joiners cost N timeouts in
/// sequence, and the sixth person waited about thirty seconds to enter a call
/// that was going to fall back to STUN anyway. The client asks for this on every
/// join AND on every rejoin attempt, so it sits squarely on the path of joining.
///
/// 30s is picked against what the failure costs on each side. Much shorter and
/// an outage is effectively uncached again — the timeout alone is 5s, so a burst
/// would re-pay it. Much longer and a relay that came back stays unused while
/// calls quietly run relay-less. Thirty seconds is six timeouts wide and well
/// inside how long anyone tolerates a bad call.
const TURN_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// The one outbound HTTP client this module uses.
///
/// Process-wide rather than a field on `AppState`. A `reqwest::Client` IS a
/// connection pool, so building one per request opens a fresh pool every time —
/// the documented way to make it slow. It lives here instead of being threaded
/// through `AppState` because nineteen places construct that struct and not one
/// of them has an opinion about HTTP.
///
/// `Option` rather than a fallback client. Building one only fails if the TLS
/// backend cannot start, and the obvious `unwrap_or_default()` is a trap twice
/// over: `Client::default()` is `Client::new()`, which panics on exactly the
/// same failure — inside `OnceLock::get_or_init`, so the cell stays uninitialised
/// and every later request panics again — and even when it succeeds it silently
/// drops the timeout this module exists to guarantee. No relay is a survivable
/// outcome here; a panicking request handler is not.
static HTTP: OnceLock<Option<reqwest::Client>> = OnceLock::new();

fn http_client() -> Option<&'static reqwest::Client> {
    HTTP.get_or_init(|| {
        match reqwest::Client::builder()
            .timeout(TURN_REQUEST_TIMEOUT)
            .build()
        {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not build the http client for cloudflare turn; voice falls back to stun"
                );
                None
            }
        }
    })
    .as_ref()
}

/// The outcome of the last mint, and the moment it stops being reused.
///
/// Without this, every single join spends one metered Cloudflare call to mint a
/// credential valid for a day and then throws it away — and `/voice/ice` is on
/// the router with no rate limiter in front of it, so the bill is bounded by how
/// fast a logged-in client can loop.
///
/// A FAILURE is cached as well as a success, which is why the servers are an
/// `Option` rather than a list. See `TURN_FAILURE_COOLDOWN` for what that buys.
struct CachedIce {
    /// `None` records a mint that failed: readers hand back STUN immediately
    /// instead of each paying their own timeout to rediscover the same outage.
    ice_servers: Option<Vec<IceServer>>,
    expires_at: Instant,
}

#[derive(Default)]
struct IceCacheState {
    cached: Option<CachedIce>,
    /// In-flight single-flight channel receiver for deduplicating concurrent requests.
    in_flight: Option<watch::Receiver<Option<Option<Vec<IceServer>>>>>,
}

/// `Arc` rather than a bare `&'static Mutex`, so the exact same cache state a
/// test constructs can be handed to `ice_servers_via_single_flight` and
/// driven from multiple spawned tasks (which need owned, `'static` data),
/// while the process-wide caller (`cloudflare_ice_servers`) just clones the
/// one behind `ICE_CACHE` — an `Arc` clone, not a fresh cache.
static ICE_CACHE: OnceLock<Arc<Mutex<IceCacheState>>> = OnceLock::new();

fn ice_cache() -> Arc<Mutex<IceCacheState>> {
    ICE_CACHE
        .get_or_init(|| Arc::new(Mutex::new(IceCacheState::default())))
        .clone()
}

/// Cloudflare's two credential endpoints answer with two different shapes, and
/// this accepts both.
///
/// `/credentials/generate` returns `iceServers` as ONE object;
/// `/credentials/generate-ice-servers`, which is what this module calls,
/// returns it as a LIST. Pinning only the list shape means that if the endpoint
/// ever changes, or a deployment is pointed at the other one, the failure looks
/// exactly like "no relay configured" — the single most expensive confusion in
/// this whole file. Accepting both costs one enum and removes that.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CloudflareIcePayload {
    List(Vec<IceServer>),
    Single(IceServer),
}

#[derive(Debug, Deserialize)]
struct CloudflareIceServers {
    #[serde(rename = "iceServers")]
    ice_servers: CloudflareIcePayload,
}

impl CloudflareIceServers {
    /// Normalises either shape to the list a browser wants, or `None` when the
    /// answer parsed but carried no relay.
    ///
    /// `None` for an empty list, and `None` for a NON-empty list with no TURN
    /// entry in it. The caller turns `Some` into `has_relay: true`, so both
    /// cases would otherwise report a relayed call while handing the browser
    /// nothing to relay through. An all-STUN answer is not a hypothetical
    /// shape: STUN entries are a normal part of a Cloudflare response, so "not
    /// empty" was never the question worth asking here.
    fn into_ice_servers(self) -> Option<Vec<IceServer>> {
        let servers = match self.ice_servers {
            CloudflareIcePayload::List(list) => list,
            CloudflareIcePayload::Single(server) => vec![server],
        };
        servers.iter().any(IceServer::is_relay).then_some(servers)
    }
}

/// Reads an environment variable, treating "set but empty" as unset.
///
/// Compose files and `.env` templates habitually carry empty placeholders, and
/// a half-configured relay is worse than no relay: the browser would be handed
/// a TURN URL with no credentials, fail during ICE, and look like a bug in the
/// call rather than a missing setting.
fn configured(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// Whether a just-finished mint attempt should overwrite the cache.
///
/// Pulled out of the single-flight loop below into its own pure function —
/// no lock, no I/O, just the three-way decision on what is already known —
/// so both the production path and its tests call the SAME decision instead
/// of the tests hand-copying these match arms and silently drifting from
/// whatever this function is changed to later.
fn should_write_cache(current: &Option<CachedIce>, minted: &Option<Vec<IceServer>>, now: Instant) -> bool {
    match (current, minted) {
        // Success always updates the cache with fresh credentials.
        (_, Some(_)) => true,
        // Failure only updates if there is no currently valid cache.
        (None, None) => true,
        (Some(current), None) => current.expires_at <= now,
    }
}

/// The single-flight leader/follower loop, generic over the cache instance
/// and the mint function so a test can drive this EXACT loop against an
/// injected `IceCacheState` and an injected async mint — instead of the
/// loop being reimplemented inside the test body, which is what let a
/// previous version of this file's tests pass while asserting nothing about
/// whether this loop still behaved this way.
///
/// Deduplicates concurrent requests (single-flight) to eliminate stampedes
/// when the cache expires. Safely resolves races: a slow or failed request
/// can never overwrite a valid cache written by a newer or faster attempt
/// (`should_write_cache` above).
async fn ice_servers_via_single_flight<F, Fut>(
    cache: Arc<Mutex<IceCacheState>>,
    mint: F,
) -> Option<Vec<IceServer>>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Option<Vec<IceServer>>>,
{
    loop {
        let mut in_flight_rx = {
            let mut state = cache.lock().await;

            // 1. Return valid cache immediately if present and not expired
            if let Some(ref cached) = state.cached {
                if cached.expires_at > Instant::now() {
                    return cached.ice_servers.clone();
                }
            }

            // 2. Check if a live mint is already in flight
            if let Some(ref rx) = state.in_flight {
                if rx.has_changed().is_err() {
                    state.in_flight = None;
                }
            }

            if let Some(ref rx) = state.in_flight {
                rx.clone()
            } else {
                // 3. No live in-flight request: this task becomes the single-flight leader
                let (tx, rx) = watch::channel(None);
                state.in_flight = Some(rx);

                // Release the mutex during the outbound network call
                drop(state);

                let minted = mint().await;

                // Re-acquire lock to commit the result and notify waiters
                let mut state = cache.lock().await;
                state.in_flight = None;

                let now = Instant::now();
                if should_write_cache(&state.cached, &minted, now) {
                    let lifetime = if minted.is_some() {
                        TURN_CACHE_LIFETIME
                    } else {
                        TURN_FAILURE_COOLDOWN
                    };
                    state.cached = Some(CachedIce {
                        ice_servers: minted.clone(),
                        expires_at: now + lifetime,
                    });
                }

                // Broadcast to all concurrent waiters
                let _ = tx.send(Some(minted.clone()));
                return minted;
            }
        };

        // 4. Follower: wait for the in-flight leader to finish
        if in_flight_rx.wait_for(|val| val.is_some()).await.is_ok() {
            let borrowed = in_flight_rx.borrow();
            if let Some(ref result) = *borrowed {
                return result.clone();
            }
        }
        // If wait_for errored (leader was dropped/cancelled), loop retries to check cache or become leader
    }
}

/// The ICE servers to hand this caller, minting a set only when the cached
/// outcome — success or failure — has run out.
///
/// Thin wrapper binding the process-wide cache and the real Cloudflare mint
/// to `ice_servers_via_single_flight` above — see that function for the
/// actual caching/dedup behaviour.
async fn cloudflare_ice_servers() -> Option<Vec<IceServer>> {
    ice_servers_via_single_flight(ice_cache(), mint_cloudflare_ice_servers).await
}

/// Asks Cloudflare for a fresh, short-lived set of ICE servers.
///
/// Split from the caching above so that "should we ask" and "what did asking
/// return" are two separate questions rather than one long function where the
/// early returns mean different things.
async fn mint_cloudflare_ice_servers() -> Option<Vec<IceServer>> {
    let key_id = configured("CLOUDFLARE_TURN_KEY_ID")?;
    let token = configured("CLOUDFLARE_TURN_API_TOKEN")?;

    let response = http_client()?
        .post(format!(
            "{CLOUDFLARE_TURN_ENDPOINT}/{key_id}/credentials/generate-ice-servers"
        ))
        .bearer_auth(token)
        .json(&serde_json::json!({ "ttl": TURN_CREDENTIAL_TTL_SECS }))
        .send()
        .await;

    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            // The status, never the body: a rejected credential request can
            // echo back the key id, and this line goes to the log.
            tracing::warn!(
                status = %response.status(),
                "cloudflare turn refused the credential request; falling back to stun"
            );
            return None;
        }
        Err(error) => {
            // `without_url` first, and this is not tidiness. `reqwest::Error`'s
            // Display appends " for url (...)", and the URL this module builds
            // carries CLOUDFLARE_TURN_KEY_ID in its path — so the plain error
            // would write half the relay credentials into the log the comment
            // above promises not to write them to.
            tracing::warn!(
                error = %error.without_url(),
                "could not reach cloudflare turn; falling back to stun"
            );
            return None;
        }
    };

    // Read as TEXT first, then decode from that text. `response.json()` consumes
    // the body and leaves nothing to say what actually arrived, and
    // `#[serde(untagged)]` collapses serde's field-level complaint into "data
    // did not match any variant of untagged enum CloudflareIcePayload" — so the
    // log line below could report that something changed and never what,
    // reintroducing in the diagnostics exactly the opacity the enum was added to
    // remove.
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            // `without_url` for the same reason as above: the request URL
            // carries CLOUDFLARE_TURN_KEY_ID in its path.
            tracing::warn!(
                error = %error.without_url(),
                "could not read the body of the cloudflare turn response"
            );
            return None;
        }
    };

    match serde_json::from_str::<CloudflareIceServers>(&body) {
        Ok(parsed) => match parsed.into_ice_servers() {
            Some(ice_servers) => Some(ice_servers),
            None => {
                tracing::warn!("cloudflare turn returned no relay in its ice server list");
                None
            }
        },
        Err(error) => {
            // `serde_json::Error` carries no URL, so it can be logged whole —
            // and it names the line and column the decode gave up at, which is
            // half of what the untagged enum eats. `describe_body` supplies the
            // other half, the shape that arrived, without carrying one value
            // with it.
            tracing::warn!(
                %error,
                body = %describe_body(&body),
                "cloudflare turn returned an unexpected body"
            );
            None
        }
    }
}

/// How deep into a JSON body the shape description below goes.
const BODY_SHAPE_MAX_DEPTH: usize = 4;

/// How many keys of one object it names before eliding the rest.
const BODY_SHAPE_MAX_KEYS: usize = 12;

/// How much of a body that is not JSON at all is worth putting in a log line.
const BODY_EXCERPT_LIMIT: usize = 200;

/// A bounded, VALUE-FREE description of a response body, for a decode failure.
///
/// Never the body itself. A credential response carries `username` and
/// `credential`, and this line goes to the log — the same rule the status-only
/// logging above follows. Key names and value TYPES answer the question a
/// decode failure actually raises, which is which field changed shape, and no
/// value can escape through them.
///
/// A body that is not JSON at all is a different thing entirely — a proxy error
/// page, an HTML challenge — and has no shape to describe, so that case gets a
/// hard-truncated excerpt instead. It cannot leak a minted credential because
/// nothing that fails to parse as JSON minted one.
fn describe_body(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => describe_json(&value, BODY_SHAPE_MAX_DEPTH),
        Err(_) => {
            // By chars, not bytes: slicing a UTF-8 string by byte offset panics
            // mid-codepoint, and this runs on a body from a third party.
            let excerpt: String = body.chars().take(BODY_EXCERPT_LIMIT).collect();
            format!("not json: {excerpt}")
        }
    }
}

/// Renders one JSON value as its shape: names of keys, types of values.
fn describe_json(value: &serde_json::Value, depth: usize) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "bool".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::String(_) => "string".to_string(),
        serde_json::Value::Array(items) if depth == 0 => format!("[..{} items]", items.len()),
        serde_json::Value::Array(items) => match items.first() {
            None => "[]".to_string(),
            // Every entry of an ice server list has the same shape, so one
            // description plus a count says what a longer one would.
            Some(first) => format!("[{}; {}]", describe_json(first, depth - 1), items.len()),
        },
        serde_json::Value::Object(fields) if depth == 0 => format!("{{..{} keys}}", fields.len()),
        serde_json::Value::Object(fields) => {
            let described: Vec<String> = fields
                .iter()
                .take(BODY_SHAPE_MAX_KEYS)
                .map(|(key, value)| format!("{key}: {}", describe_json(value, depth - 1)))
                .collect();
            let elided = fields.len().saturating_sub(described.len());
            if elided == 0 {
                format!("{{{}}}", described.join(", "))
            } else {
                format!("{{{}, ..{elided} more}}", described.join(", "))
            }
        }
    }
}

/// Builds the ICE server list for one caller, newest credentials first.
///
/// Three tiers, in descending order of how well a call will actually work:
/// Cloudflare's managed relay, a self-hosted TURN from the environment, and
/// bare STUN. Only the first two count as a relay.
pub async fn get_ice_config(
    State(state): State<AppState>,
    // Authenticated on purpose: an open ICE endpoint is a free STUN/TURN
    // lookup for anyone on the internet, and TURN credentials are a resource
    // with a bill attached.
    AuthenticatedUser(_context): AuthenticatedUser,
) -> Result<Json<IceConfigResponse>, ApiError> {
    let _ = &state;

    if let Some(ice_servers) = cloudflare_ice_servers().await {
        return Ok(Json(IceConfigResponse {
            ice_servers,
            has_relay: true,
        }));
    }

    Ok(Json(fallback_ice_config()))
}

/// The ICE list for a deployment with no Cloudflare relay: STUN, plus a
/// self-hosted TURN if one is configured.
///
/// Split out from the handler so it can be tested for what it actually
/// decides. It reads only the environment and touches no network, which is the
/// whole reason the tiers are worth pinning down here rather than behind an
/// HTTP round trip.
fn fallback_ice_config() -> IceConfigResponse {
    let mut ice_servers = vec![IceServer {
        urls: vec![configured("STUN_URL").unwrap_or_else(|| DEFAULT_STUN.to_string())],
        username: None,
        credential: None,
    }];

    // A static TURN credential is what a self-hoster running coturn can set up
    // in an afternoon, and it stays supported for exactly that reason. It is
    // strictly worse than the Cloudflare path above — the credential is handed
    // to every browser in a call and never expires — so it is the fallback, not
    // the default.
    let has_relay = match (
        configured("TURN_URL"),
        configured("TURN_USERNAME"),
        configured("TURN_CREDENTIAL"),
    ) {
        (Some(url), Some(username), Some(credential)) => {
            ice_servers.push(IceServer {
                urls: vec![url],
                username: Some(username),
                credential: Some(credential),
            });
            true
        }
        _ => false,
    };

    IceConfigResponse {
        ice_servers,
        has_relay,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VoiceRostersResponse {
    /// Channel id -> the accounts currently in that channel's call. Channels
    /// with nobody in them are absent rather than present-and-empty, so the
    /// client can treat presence in this map as "there is a call happening".
    pub rosters: HashMap<Uuid, Vec<Uuid>>,
}

/// Who is currently in a call, for every voice channel of one server.
///
/// The socket only carries deltas — join and leave — which is right for a
/// live view and useless for a client that connects while a call is already
/// running. Without this, opening the app during a call shows an empty
/// channel until somebody happens to join or leave, which reads as the
/// feature being broken.
pub async fn get_server_voice_rosters(
    State(state): State<AppState>,
    Path(server_id): Path<Uuid>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<VoiceRostersResponse>, ApiError> {
    // Listing channels is already authorization-checked, and reusing it here
    // means voice cannot accidentally expose a channel the caller could not
    // otherwise see.
    let channels = state
        .domain
        .list_channels(context.account_id, server_id)
        .await?;
    let voice_channel_ids: Vec<Uuid> = channels.iter().map(|c| c.id).collect();

    Ok(Json(VoiceRostersResponse {
        rosters: state.realtime.voice_rosters(&voice_channel_ids).await,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys these tests write. Read straight from the process environment
    /// by the code under test, so every case has to start from a known state
    /// and every case has to clean up after itself.
    const KEYS: [&str; 4] = ["STUN_URL", "TURN_URL", "TURN_USERNAME", "TURN_CREDENTIAL"];

    fn clear() {
        for key in KEYS {
            std::env::remove_var(key);
        }
    }

    /// One test rather than four.
    ///
    /// The environment is per-process and Rust runs tests in parallel threads,
    /// so four tests writing these same four keys would race each other and
    /// fail for reasons that have nothing to do with the code. Keeping the
    /// cases in one sequential body is the honest fix; splitting them and
    /// hoping is not.
    #[test]
    fn fallback_tiers() {
        clear();

        // Nothing configured: public STUN, and honest about having no relay.
        let config = fallback_ice_config();
        assert_eq!(config.ice_servers.len(), 1);
        assert_eq!(config.ice_servers[0].urls, vec![DEFAULT_STUN.to_string()]);
        assert!(!config.has_relay);

        // A deployment may point STUN somewhere else without gaining a relay.
        std::env::set_var("STUN_URL", "stun:stun.example.com:3478");
        let config = fallback_ice_config();
        assert_eq!(
            config.ice_servers[0].urls,
            vec!["stun:stun.example.com:3478".to_string()]
        );
        assert!(!config.has_relay);

        // A complete self-hosted TURN counts as a relay and is appended after
        // STUN, so a browser still tries the cheap path first.
        std::env::set_var("TURN_URL", "turn:turn.example.com:3478");
        std::env::set_var("TURN_USERNAME", "someone");
        std::env::set_var("TURN_CREDENTIAL", "secret");
        let config = fallback_ice_config();
        assert_eq!(config.ice_servers.len(), 2);
        assert_eq!(config.ice_servers[1].username.as_deref(), Some("someone"));
        assert!(config.has_relay);

        // Half a TURN is no TURN. Handing the browser a TURN URL with no
        // credentials would fail during ICE and look like a broken call rather
        // than a missing setting.
        std::env::remove_var("TURN_CREDENTIAL");
        let config = fallback_ice_config();
        assert_eq!(config.ice_servers.len(), 1);
        assert!(!config.has_relay);

        // An empty value is a placeholder, not a setting — compose files and
        // `.env` templates are full of them.
        std::env::set_var("TURN_CREDENTIAL", "");
        let config = fallback_ice_config();
        assert!(!config.has_relay);

        clear();
    }

    /// The list shape, which is what `/credentials/generate-ice-servers`
    /// answers with and therefore what this module normally sees.
    ///
    /// Pure serde, no network. That is the point: the Cloudflare path had no
    /// coverage at all, and every one of its failure modes ends in the same
    /// `None` that an unconfigured deployment produces — so a body this code
    /// silently stopped understanding would present as "no relay configured"
    /// and nobody would go looking for a parser bug.
    #[test]
    fn parses_the_ice_server_list_shape() {
        let body = r#"{
            "iceServers": [
                { "urls": ["stun:stun.cloudflare.com:3478"] },
                {
                    "urls": ["turns:turn.cloudflare.com:5349?transport=tcp"],
                    "username": "minted",
                    "credential": "secret"
                }
            ]
        }"#;

        let parsed: CloudflareIceServers =
            serde_json::from_str(body).expect("the list shape must parse");
        let servers = parsed
            .into_ice_servers()
            .expect("the turns entry makes this list a usable relay");

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].username, None);
        assert_eq!(servers[1].username.as_deref(), Some("minted"));
        assert_eq!(servers[1].credential.as_deref(), Some("secret"));
    }

    /// The single-object shape, which `/credentials/generate` answers with.
    ///
    /// This module does not call that endpoint today. It parses the shape
    /// anyway so that pointing a deployment at the other endpoint, or
    /// Cloudflare changing which one it serves, degrades into a working relay
    /// rather than into an indistinguishable "no relay" fallback.
    #[test]
    fn parses_the_single_ice_server_shape() {
        let body = r#"{
            "iceServers": {
                "urls": ["turn:turn.cloudflare.com:3478"],
                "username": "minted",
                "credential": "secret"
            }
        }"#;

        let parsed: CloudflareIceServers =
            serde_json::from_str(body).expect("the single-object shape must parse");
        let servers = parsed
            .into_ice_servers()
            .expect("one server is still a relay");

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, vec!["turn:turn.cloudflare.com:3478"]);
        assert_eq!(servers[0].username.as_deref(), Some("minted"));
    }

    /// A well-formed answer carrying nothing is not a relay.
    ///
    /// It has to be `None` rather than an empty list, because the caller turns
    /// `Some` into `has_relay: true` — which would tell the client its calls
    /// are relayed while handing the browser no server to relay through.
    #[test]
    fn an_empty_ice_server_list_is_not_a_relay() {
        let parsed: CloudflareIceServers =
            serde_json::from_str(r#"{ "iceServers": [] }"#).expect("an empty list still parses");

        assert!(parsed.into_ice_servers().is_none());
    }

    /// An answer full of STUN and nothing else is not a relay either.
    ///
    /// This is the case "not empty" got wrong, and it is not exotic: STUN
    /// entries are a normal part of a Cloudflare response — the list fixture
    /// above contains one — so an answer that lost only its TURN entries still
    /// parses, still has a length, and used to report `has_relay: true`. The
    /// client then suppressed its "calls may fail across some networks" warning
    /// for a call with no relay behind it.
    #[test]
    fn an_all_stun_ice_server_list_is_not_a_relay() {
        let body = r#"{
            "iceServers": [
                { "urls": ["stun:stun.cloudflare.com:3478"] },
                { "urls": ["stun:stun.l.google.com:19302"] }
            ]
        }"#;

        let parsed: CloudflareIceServers =
            serde_json::from_str(body).expect("an all-stun list still parses");

        assert!(parsed.into_ice_servers().is_none());
    }

    /// `urls` as a bare string, which the WebRTC IDL allows and which pinning
    /// `Vec<String>` rejected.
    ///
    /// A rejection here is invisible: the entry fails both untagged variants,
    /// the whole body fails to decode, and the deployment reads as "no relay
    /// configured" while a perfectly good relay was on the wire.
    #[test]
    fn accepts_urls_as_a_bare_string() {
        let body = r#"{
            "iceServers": [
                { "urls": "stun:stun.cloudflare.com:3478" },
                {
                    "urls": "turns:turn.cloudflare.com:5349?transport=tcp",
                    "username": "minted",
                    "credential": "secret"
                }
            ]
        }"#;

        let parsed: CloudflareIceServers =
            serde_json::from_str(body).expect("a string url must parse like a one-element list");
        let servers = parsed
            .into_ice_servers()
            .expect("the turns entry is a relay");

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, vec!["stun:stun.cloudflare.com:3478"]);
        assert_eq!(
            servers[1].urls,
            vec!["turns:turn.cloudflare.com:5349?transport=tcp"]
        );
    }

    /// The decode diagnostic names the shape and never a value.
    ///
    /// Worth pinning rather than trusting: this string goes to the log on every
    /// unexpected body, and the body it describes is a credential document. A
    /// change that started rendering values would be a credential leak that no
    /// test failure would otherwise catch.
    #[test]
    fn the_body_description_carries_shape_but_no_values() {
        let body = r#"{
            "iceServers": [
                {
                    "urls": ["turns:turn.cloudflare.com:5349"],
                    "username": "leaky-username",
                    "credential": "leaky-credential"
                }
            ]
        }"#;

        let described = describe_body(body);

        assert!(!described.contains("leaky-username"));
        assert!(!described.contains("leaky-credential"));
        assert!(!described.contains("turns:"));
        // The key names ARE the diagnosis: they say which field changed shape.
        assert!(described.contains("iceServers"));
        assert!(described.contains("username: string"));

        // A body that is not JSON has no shape, so it falls back to a bounded
        // excerpt rather than to nothing.
        let described = describe_body("<html>502 Bad Gateway</html>");
        assert!(described.starts_with("not json: "));
        assert!(described.contains("502 Bad Gateway"));
    }

    /// Exercises the REAL decision function, not a copy of its match arms —
    /// the whole point being that a future edit to `should_write_cache`
    /// cannot silently drift out of sync with what this test asserts.
    #[test]
    fn cache_state_failure_does_not_overwrite_valid_cache() {
        let valid_servers = vec![IceServer {
            urls: vec!["turn:turn.example.com:3478".to_string()],
            username: Some("user".to_string()),
            credential: Some("pass".to_string()),
        }];

        // Valid cache expiring in 1 hour.
        let cached = Some(CachedIce {
            ice_servers: Some(valid_servers),
            expires_at: Instant::now() + Duration::from_secs(3600),
        });

        // A failed mint (minted = None) must NOT overwrite it.
        let minted: Option<Vec<IceServer>> = None;
        assert!(!should_write_cache(&cached, &minted, Instant::now()));
    }

    #[test]
    fn cache_state_expired_cache_allows_failure_cooldown() {
        // Already-expired cache.
        let cached = Some(CachedIce {
            ice_servers: Some(vec![]),
            expires_at: Instant::now() - Duration::from_secs(10),
        });

        // A failed mint is free to write the failure-cooldown entry once the
        // cache it would replace has nothing valid left in it.
        let minted: Option<Vec<IceServer>> = None;
        assert!(should_write_cache(&cached, &minted, Instant::now()));
    }

    #[test]
    fn cache_state_success_always_overwrites_even_a_valid_cache() {
        // A still-valid cache does not block a SUCCESSFUL mint from
        // refreshing it — only a failed one is held back by
        // `cache_state_failure_does_not_overwrite_valid_cache` above.
        let cached = Some(CachedIce {
            ice_servers: Some(vec![]),
            expires_at: Instant::now() + Duration::from_secs(3600),
        });

        let minted = Some(vec![IceServer {
            urls: vec!["turn:fresh.example.com:3478".to_string()],
            username: Some("user".to_string()),
            credential: Some("pass".to_string()),
        }]);
        assert!(should_write_cache(&cached, &minted, Instant::now()));
    }

    /// Drives the REAL single-flight loop (`ice_servers_via_single_flight`),
    /// not a reimplementation of it, against an injected cache and an
    /// injected mint function that counts its own invocations. A previous
    /// version of this test hand-copied the leader/follower loop into the
    /// test body, which meant it could stay green after a change that broke
    /// the real loop's deduplication.
    #[tokio::test]
    async fn single_flight_deduplicates_concurrent_calls() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cache = Arc::new(Mutex::new(IceCacheState::default()));
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..20 {
            let cache = Arc::clone(&cache);
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                ice_servers_via_single_flight(cache, move || {
                    let counter = Arc::clone(&counter);
                    async move {
                        // Simulate a slow network fetch, so every task that
                        // is going to race for the leader slot has time to
                        // arrive before the leader finishes.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        counter.fetch_add(1, Ordering::SeqCst);
                        Some(vec![IceServer {
                            urls: vec!["turn:mock.cloudflare.com".to_string()],
                            username: Some("u".to_string()),
                            credential: Some("c".to_string()),
                        }])
                    }
                })
                .await
            }));
        }

        for h in handles {
            let res = h.await.unwrap();
            assert!(res.is_some());
            assert_eq!(res.unwrap()[0].urls[0], "turn:mock.cloudflare.com");
        }

        // Exactly 1 network fetch should have executed despite 20 concurrent tasks!
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
