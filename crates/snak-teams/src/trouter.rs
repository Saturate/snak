// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Trouter push connection derived from Squads (https://github.com/IanTerzo/Squads, src/websockets.rs, MPL-2.0).
// Delivery acknowledgements follow ost (https://github.com/eisbaw/ost, src/trouter/websocket.rs, MIT).

//! Trouter, Teams' push channel: a socket.io 0.9 style WebSocket that carries
//! chat service notifications as HTTP-like requests inside `3:::` frames.

use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use rustls_platform_verifier::BuilderVerifierExt;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::auth::scope;
use crate::client::{TeamsClient, decode};
use crate::error::{CoreError, Result};
use crate::models::Message;

const TROUTER: &str = "https://go.trouter.teams.microsoft.com";
const REGISTRAR: &str = "https://edge.skype.com/registrar/prod/v2/registrations";
// Placeholders the Teams web client sends verbatim; Trouter accepts them.
const CLIENT_INFO: &str =
    r#"{"cv":"TEAMS_TROUTER_TCCV","ua":"TeamsCDL","hr":"","v":"TEAMS_CLIENTINFO_VERSION"}"#;
// The Skype token is sent to hosts Trouter hands back, so only follow those
// that stay inside Microsoft's own domains.
const TRUSTED_DOMAINS: [&str; 3] = ["microsoft.com", "skype.com", "skype.net"];

// appId, templateKey, path under the session URL. The template decides the
// payload shape; these versions are the ones whose shape Squads parses.
// NextGenCalling (calls) is left out: Snak cannot answer a call, and a
// registration would route invites to an endpoint that never responds.
const REGISTRATIONS: [(&str, &str, &str); 2] = [
    ("TeamsCDLWebWorker", "TeamsCDLWebWorker_1.9", ""),
    ("SkypeSpacesWeb", "SkypeSpacesWeb_2.3", "SkypeSpacesWeb"),
];
const REGISTRATION_TTL_SECS: u64 = 86_400;

const EVENT_BUFFER: usize = 256;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const HEARTBEAT: Duration = Duration::from_secs(30);
// Rotating hourly keeps the Skype token and registrations fresh without
// tracking their individual expiries (ost does the same).
const SESSION_MAX_AGE: Duration = Duration::from_secs(3600);
// A session that lived this long counts as healthy, so the next failure
// starts backoff from the bottom again.
const STABLE_AFTER: Duration = Duration::from_secs(60);
const MAX_BACKOFF_SECS: u64 = 60;

/// Something Teams pushed, or a change in the connection itself.
#[derive(Debug, Clone)]
pub enum PushEvent {
    /// A session is up. Sent again after every reconnect: anything that
    /// happened while disconnected was missed, so refetch what is on screen
    /// and call [`TeamsClient::subscribe_presence`] again.
    Connected(PushSession),
    /// The connection dropped; the next attempt starts after `retry_in`.
    Reconnecting {
        attempt: u32,
        retry_in: Duration,
        reason: String,
    },
    /// The refresh token is no longer accepted. The channel closes after this.
    SessionExpired,
    /// A message was posted. Your own sends come back here too, with the
    /// `client_message_id` you sent. Teams sometimes re-delivers a message
    /// with the same `id`, so upsert by id rather than append.
    NewMessage {
        conversation_id: String,
        message: Message,
    },
    /// An existing message changed. Deletions arrive here as well;
    /// check [`Message::is_deleted`].
    MessageEdited {
        conversation_id: String,
        message: Message,
    },
    /// Someone started or stopped typing. The stop (`Control/ClearTyping`)
    /// doesn't always come, so expire a start after a few seconds too.
    Typing {
        conversation_id: String,
        user_mri: String,
        /// `Control/ClearTyping`: they stopped without sending.
        stopped: bool,
    },
    /// Conversation metadata (title, members, read state) changed.
    ConversationUpdated { conversation_id: String },
    /// Presence for users subscribed through [`TeamsClient::subscribe_presence`].
    Presence(Vec<Presence>),
}

/// Identifies one live Trouter session, for subscribing to extra feeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSession {
    endpoint_id: String,
    surl: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    /// e.g. `8:orgid:<object id>`.
    pub mri: String,
    /// e.g. `Available`, `Busy`, `Away`, `Offline`.
    pub availability: Option<String>,
    /// e.g. `InACall`, `InAMeeting`, `Presenting`.
    pub activity: Option<String>,
    pub last_active_time: Option<String>,
    pub out_of_office: bool,
}

impl TeamsClient {
    /// Opens the push channel and keeps it open, reconnecting with backoff,
    /// until the receiver is dropped or the login expires.
    pub async fn subscribe(self: Arc<Self>) -> Result<mpsc::Receiver<PushEvent>> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| CoreError::TaskFailed(e.to_string()))?;
        let link = Link::new()?;
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        runtime.spawn(run(self, link, tx));
        Ok(rx)
    }

    /// Asks the presence service to push updates for these users over `session`.
    pub async fn subscribe_presence(&self, session: &PushSession, mris: &[String]) -> Result<()> {
        if mris.is_empty() {
            return Ok(());
        }
        let url = format!(
            "https://teams.microsoft.com/ups/{}/v1/pubsub/subscriptions/{}",
            self.region(),
            session.endpoint_id
        );
        let subscriptions: Vec<Value> = mris
            .iter()
            .map(|mri| json!({ "mri": mri, "source": "ups" }))
            .collect();
        let body = json!({
            "trouterUri": format!("{}/unifiedPresenceService", session.surl.trim_end_matches('/')),
            "shouldPurgePreviousSubscriptions": false,
            "subscriptionsToAdd": subscriptions,
            "subscriptionsToRemove": [],
        });
        let _: Value = self
            .call(scope::PRESENCE, &url, |http, token| {
                http.post(&url)
                    .bearer_auth(token)
                    .header("x-ms-client-user-agent", "Teams-V2-Web")
                    .header("x-ms-client-version", "1415/25040319109")
                    .header("x-ms-endpoint-id", &session.endpoint_id)
                    .json(&body)
            })
            .await?;
        Ok(())
    }
}

enum SessionEnd {
    ReceiverGone,
    Rotate,
    Closed(String),
}

async fn run(client: Arc<TeamsClient>, link: Link, tx: mpsc::Sender<PushEvent>) {
    let mut attempt = 0u32;
    loop {
        let started = Instant::now();
        let reason = match link.session(&client, &tx).await {
            Ok(SessionEnd::ReceiverGone) => return,
            Ok(SessionEnd::Rotate) => {
                attempt = 0;
                continue;
            }
            Ok(SessionEnd::Closed(reason)) => reason,
            Err(CoreError::SessionExpired(_)) => {
                let _ = tx.send(PushEvent::SessionExpired).await;
                return;
            }
            Err(e) => {
                if matches!(
                    e,
                    CoreError::Http {
                        status: 401 | 403,
                        ..
                    }
                ) {
                    client.forget_skype_token().await;
                }
                e.to_string()
            }
        };

        if started.elapsed() >= STABLE_AFTER {
            attempt = 0;
        }
        let retry_in = backoff(attempt);
        attempt = attempt.saturating_add(1);
        tracing::warn!(attempt, retry_in_s = retry_in.as_secs(), %reason, "push disconnected");
        let notice = PushEvent::Reconnecting {
            attempt,
            retry_in,
            reason,
        };
        if tx.send(notice).await.is_err() {
            return;
        }
        tokio::select! {
            _ = sleep(retry_in) => {}
            _ = tx.closed() => return,
        }
    }
}

/// Doubles from 1 s up to a minute, plus up to 50% jitter so many clients
/// dropped by the same outage don't reconnect in lockstep.
fn backoff(attempt: u32) -> Duration {
    let base_ms = (1u64 << attempt.min(6)).min(MAX_BACKOFF_SECS) * 1000;
    let jitter_ms = random_u64() % (base_ms / 2 + 1);
    Duration::from_millis(base_ms + jitter_ms)
}

// Avoids a `rand` dependency: each RandomState is freshly keyed from OS
// randomness, which is plenty for jitter and endpoint IDs.
fn random_u64() -> u64 {
    std::collections::hash_map::RandomState::new().hash_one(0u8)
}

fn random_uuid() -> String {
    let hi = (random_u64() & 0xffff_ffff_ffff_0fff) | 0x4000;
    let lo = (random_u64() & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hi >> 32,
        (hi >> 16) & 0xffff,
        hi & 0xffff,
        lo >> 48,
        lo & 0xffff_ffff_ffff
    )
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// State that survives reconnects.
struct Link {
    // Separate from the app's client so a redirect can never carry the
    // X-Skypetoken header to another host.
    http: reqwest::Client,
    tls: Connector,
    // Kept across reconnects so each registration replaces the previous one
    // instead of piling up stale routes.
    endpoint_id: String,
    registration_ids: [String; REGISTRATIONS.len()],
}

#[derive(Deserialize)]
struct Allocation {
    socketio: String,
    surl: String,
    ccid: Option<String>,
    #[serde(default)]
    connectparams: Map<String, Value>,
}

impl Link {
    fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            tls: tls_connector()?,
            endpoint_id: random_uuid(),
            registration_ids: [random_uuid(), random_uuid()],
        })
    }

    async fn session(
        &self,
        client: &TeamsClient,
        tx: &mpsc::Sender<PushEvent>,
    ) -> Result<SessionEnd> {
        let skype = client.skype_token().await?;
        let alloc = self.allocate(&skype).await?;
        let con_num = format!("{}_1", chrono::Utc::now().timestamp_millis());
        let session_id = self.socket_session(&alloc, &skype, &con_num).await?;

        let mut ws_url = socket_url(
            &alloc,
            &["websocket", &session_id],
            &self.endpoint_id,
            &con_num,
        )?;
        ws_url
            .set_scheme("wss")
            .map_err(|()| CoreError::Push("cannot build WebSocket URL".into()))?;
        let connect = tokio_tungstenite::connect_async_tls_with_config(
            ws_url.as_str(),
            None,
            false,
            Some(self.tls.clone()),
        );
        let (mut ws, _) = timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| CoreError::Push("WebSocket connect timed out".into()))?
            .map_err(|e| CoreError::Push(format!("WebSocket connect: {e}")))?;

        // Trouter normally opens with `1::`; anything else is handled as a
        // regular frame once the session is announced.
        let first = match timeout(HANDSHAKE_TIMEOUT, ws.next()).await {
            Err(_) => return Err(CoreError::Push("no handshake from Trouter".into())),
            Ok(None) => return Ok(SessionEnd::Closed("closed during handshake".into())),
            Ok(Some(Err(e))) => return Err(CoreError::Push(format!("handshake: {e}"))),
            Ok(Some(Ok(frame))) => frame,
        };
        let pending = match first.to_text().map(parse_frame) {
            Ok(Frame::Connect) => None,
            Ok(Frame::Disconnect) => return Ok(SessionEnd::Closed("refused at handshake".into())),
            Ok(Frame::Error(reason)) => {
                return Ok(SessionEnd::Closed(format!(
                    "refused at handshake: {reason}"
                )));
            }
            _ => Some(first),
        };

        self.register(&skype, &alloc.surl).await?;

        let session = PushSession {
            endpoint_id: self.endpoint_id.clone(),
            surl: alloc.surl,
        };
        tracing::info!("push connected");
        if tx.send(PushEvent::Connected(session)).await.is_err() {
            let _ = ws.close(None).await;
            return Ok(SessionEnd::ReceiverGone);
        }
        if let Some(frame) = pending
            && let Some(end) = deliver(&mut ws, tx, frame).await?
        {
            return Ok(end);
        }

        let mut heartbeat = tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let max_age = sleep(SESSION_MAX_AGE);
        tokio::pin!(max_age);

        loop {
            tokio::select! {
                frame = ws.next() => {
                    let frame = match frame {
                        None => return Ok(SessionEnd::Closed("connection closed".into())),
                        Some(Err(e)) => return Err(CoreError::Push(format!("receive: {e}"))),
                        Some(Ok(frame)) => frame,
                    };
                    if let Some(end) = deliver(&mut ws, tx, frame).await? {
                        return Ok(end);
                    }
                }
                _ = heartbeat.tick() => send(&mut ws, "2::").await?,
                _ = &mut max_age => {
                    let _ = ws.close(None).await;
                    return Ok(SessionEnd::Rotate);
                }
                _ = tx.closed() => {
                    let _ = ws.close(None).await;
                    return Ok(SessionEnd::ReceiverGone);
                }
            }
        }
    }

    /// Asks Trouter for a socket host and the signed parameters to connect with.
    async fn allocate(&self, skype: &str) -> Result<Allocation> {
        let url = format!("{TROUTER}/v4/a?epid={}", self.endpoint_id);
        let res = self
            .http
            .post(&url)
            .header("X-Skypetoken", skype)
            .header("Content-Length", "0")
            .send()
            .await?;
        let alloc: Allocation = decode(res, &url).await?;
        // Issue #19: allocation carries the signature parameters whose names
        // Squads guessed; log the keys that came back, never their values.
        let keys: Vec<&str> = alloc.connectparams.keys().map(String::as_str).collect();
        tracing::debug!(has_ccid = alloc.ccid.is_some(), ?keys, "trouter allocation");
        Ok(alloc)
    }

    /// The socket.io handshake answers `<session id>:<heartbeat>:<close>:<transports>`.
    async fn socket_session(
        &self,
        alloc: &Allocation,
        skype: &str,
        con_num: &str,
    ) -> Result<String> {
        let url = socket_url(alloc, &[], &self.endpoint_id, con_num)?;
        let redacted = redact(&url);
        // The query carries the connect signature, so keep it out of errors.
        let res = self
            .http
            .get(url)
            .header("X-Skypetoken", skype)
            .send()
            .await
            .map_err(|e| CoreError::Request(e.without_url()))?;
        let status = res.status();
        let body = res
            .text()
            .await
            .map_err(|e| CoreError::Request(e.without_url()))?;
        if !status.is_success() {
            return Err(CoreError::Http {
                status: status.as_u16(),
                url: redacted,
                body,
            });
        }
        let session_id = parse_session_id(&body).ok_or(CoreError::MissingField {
            url: redacted,
            field: "socket.io session id",
        })?;
        // Issue #19: the handshake body says what socket.io dialect Trouter
        // speaks; log the timeouts and transports to check the `1::`
        // handshake guess. The session id goes into the socket URL, so it
        // stays out.
        let mut parts = body.splitn(4, ':').skip(1);
        let heartbeat = parts.next().unwrap_or_default();
        let close = parts.next().unwrap_or_default();
        let transports: String = parts.next().unwrap_or_default().chars().take(60).collect();
        tracing::debug!(heartbeat, close, %transports, "trouter handshake");
        Ok(session_id)
    }

    /// Tells the registrar which Trouter path should receive pushes for each app.
    async fn register(&self, skype: &str, surl: &str) -> Result<()> {
        for ((app_id, template_key, suffix), registration_id) in
            REGISTRATIONS.iter().zip(&self.registration_ids)
        {
            let path = if suffix.is_empty() {
                surl.to_string()
            } else {
                format!("{}/{suffix}", surl.trim_end_matches('/'))
            };
            let body = json!({
                "clientDescription": {
                    "appId": app_id,
                    "aesKey": "",
                    "languageId": "en-US",
                    "platform": "edge",
                    "templateKey": template_key,
                    "platformUIVersion": "49/24062722442",
                },
                "registrationId": registration_id,
                "nodeId": "",
                "transports": {
                    "TROUTER": [{
                        "context": "",
                        "path": path,
                        "ttl": REGISTRATION_TTL_SECS,
                    }]
                },
            });
            let res = self
                .http
                .post(REGISTRAR)
                .header("X-Skypetoken", skype)
                .json(&body)
                .send()
                .await?;
            // Issue #19: the templateKey and platformUIVersion are guesses
            // from Squads; log what each registration returns so a live run
            // shows whether Teams accepts them or nudges a version.
            let status = res.status();
            let _: Value = decode(res, REGISTRAR).await?;
            tracing::debug!(app_id, template_key, %status, "registered with the registrar");
        }
        Ok(())
    }
}

// The process may enable more than one rustls crypto backend (reqwest picks
// aws-lc-rs, other crates pull in ring), and then rustls' implicit default
// panics. Naming the provider explicitly sidesteps that.
fn tls_connector() -> Result<Connector> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| builder.with_platform_verifier())
        .map_err(|e| CoreError::Push(format!("TLS setup: {e}")))?
        .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}

fn socket_url(
    alloc: &Allocation,
    segments: &[&str],
    endpoint_id: &str,
    con_num: &str,
) -> Result<Url> {
    let base = format!("{}/socket.io/1/", alloc.socketio.trim_end_matches('/'));
    let mut url = trusted_url(&base)?;
    if !segments.is_empty() {
        url.path_segments_mut()
            .map_err(|()| CoreError::Push("Trouter socket URL has no path".into()))?
            .pop_if_empty()
            .extend(segments);
    }
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("v", "v4");
        for (key, value) in &alloc.connectparams {
            if let Some(value) = value.as_str() {
                query.append_pair(key, value);
            }
        }
        query
            .append_pair("tc", CLIENT_INFO)
            .append_pair("con_num", con_num)
            .append_pair("epid", endpoint_id);
        if let Some(ccid) = &alloc.ccid {
            query.append_pair("ccid", ccid);
        }
        query
            .append_pair("auth", "true")
            .append_pair("timeout", "40");
    }
    Ok(url)
}

fn trusted_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|e| CoreError::Push(format!("bad Trouter URL: {e}")))?;
    let host = url.host_str().unwrap_or_default();
    let trusted = url.scheme() == "https"
        && TRUSTED_DOMAINS
            .iter()
            .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
    if trusted {
        Ok(url)
    } else {
        Err(CoreError::Push(format!("refusing Trouter host {host}")))
    }
}

/// Frames carry skype tokens and message text, so a raw frame only reaches
/// logs as its socket.io kind, byte length and, for a JSON object payload,
/// the field names from [`field_names`].
fn frame_shape(text: &str) -> String {
    let mut parts = text.splitn(4, ':');
    let kind = parts.next().unwrap_or_default();
    let kind = if kind.len() == 1 && kind.bytes().all(|b| b.is_ascii_digit()) {
        kind
    } else {
        "?"
    };
    let data = parts.nth(2).unwrap_or_default();
    let fields = match serde_json::from_str::<Value>(data) {
        Ok(Value::Object(object)) => field_names(&object),
        Ok(_) => "non object".into(),
        Err(_) => "not json".into(),
    };
    format!("kind={kind} len={} fields={fields}", text.len())
}

/// Object keys are field names in most payloads, but some key maps by user
/// id or carry free text; only short identifier-like keys are named, the
/// rest are counted.
fn field_names(object: &Map<String, Value>) -> String {
    let named =
        |k: &&String| k.len() <= 32 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let mut names: Vec<&str> = object.keys().filter(named).map(String::as_str).collect();
    let other = object.len() - names.len();
    let other = (other > 0).then(|| format!("+{other} other"));
    names.extend(other.as_deref());
    names.join(",")
}

fn redact(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.to_string()
}

fn parse_session_id(body: &str) -> Option<String> {
    let id = body.trim().split(':').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

async fn send(ws: &mut Socket, text: &str) -> Result<()> {
    ws.send(WsMessage::text(text))
        .await
        .map_err(|e| CoreError::Push(format!("send: {e}")))
}

async fn deliver(
    ws: &mut Socket,
    tx: &mpsc::Sender<PushEvent>,
    frame: WsMessage,
) -> Result<Option<SessionEnd>> {
    let text = match frame {
        WsMessage::Text(text) => text,
        WsMessage::Close(_) => return Ok(Some(SessionEnd::Closed("server closed".into()))),
        // tungstenite answers pings itself.
        _ => return Ok(None),
    };
    let handled = handle_frame(&text);
    for reply in &handled.replies {
        send(ws, reply).await?;
    }
    for event in handled.events {
        if tx.send(event).await.is_err() {
            return Ok(Some(SessionEnd::ReceiverGone));
        }
    }
    Ok(handled.close.map(SessionEnd::Closed))
}

/// A socket.io 0.9 frame: `type:id:endpoint:data`.
#[derive(Debug, PartialEq, Eq)]
enum Frame<'a> {
    Disconnect,
    Connect,
    Heartbeat,
    Message(&'a str),
    Event {
        ack_id: Option<&'a str>,
        data: &'a str,
    },
    Ack,
    Error(&'a str),
    Noop,
    Unknown,
}

fn parse_frame(text: &str) -> Frame<'_> {
    let mut parts = text.splitn(4, ':');
    let kind = parts.next().unwrap_or_default();
    let id = parts.next().unwrap_or_default();
    let _endpoint = parts.next();
    let data = parts.next().unwrap_or_default();
    match kind {
        "0" => Frame::Disconnect,
        "1" => Frame::Connect,
        "2" => Frame::Heartbeat,
        "3" | "4" => Frame::Message(data),
        "5" => {
            // A trailing `+` asks for an ack carrying data; the id is the same.
            let id = id.trim_end_matches('+');
            Frame::Event {
                ack_id: (!id.is_empty()).then_some(id),
                data,
            }
        }
        "6" => Frame::Ack,
        "7" => Frame::Error(data),
        "8" => Frame::Noop,
        _ => Frame::Unknown,
    }
}

#[derive(Debug, Default)]
struct Handled {
    replies: Vec<String>,
    events: Vec<PushEvent>,
    close: Option<String>,
}

fn handle_frame(text: &str) -> Handled {
    let mut handled = Handled::default();
    match parse_frame(text) {
        Frame::Heartbeat => handled.replies.push("2::".into()),
        Frame::Disconnect => handled.close = Some("server disconnected".into()),
        Frame::Error(reason) => {
            tracing::debug!(%reason, "trouter error frame");
            handled.close = Some(format!("server error: {reason}"));
        }
        Frame::Event { ack_id, data } => {
            // Events Trouter itself sends (`trouter.connected` and friends);
            // the name is all we log, acked or not.
            if tracing::enabled!(tracing::Level::DEBUG) {
                let name = serde_json::from_str::<Value>(data)
                    .ok()
                    .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string));
                let name: Option<String> = name.map(|n| n.chars().take(60).collect());
                tracing::debug!(event = name.as_deref(), "trouter event frame");
            }
            // Unacked events are redelivered and hold back later ones (per ost).
            if let Some(id) = ack_id {
                tracing::debug!(%id, "acknowledging event");
                handled.replies.push(format!("6:{id}::"));
            }
        }
        Frame::Message(data) => {
            let Ok(request) = serde_json::from_str::<Value>(data) else {
                tracing::debug!(frame = %frame_shape(text), "unreadable push frame");
                return handled;
            };
            // Each `3:::` frame is an HTTP request relayed from a backend;
            // without a 200 the sender sees a 504 and retries (per ost).
            if let Some(id) = request.get("id").and_then(Value::as_i64) {
                let response = json!({ "id": id, "status": 200 });
                handled.replies.push(format!("3:::{response}"));
            }
            let body = request.get("body");
            if tracing::enabled!(tracing::Level::DEBUG) {
                log_push_shape(body.unwrap_or(&request));
            }
            if let Some(event) = body.and_then(parse_notification) {
                handled.events.push(event);
            }
        }
        // Ack frames can carry data; their raw shape is what the guess
        // verification needs, never their payload.
        Frame::Ack => {
            tracing::debug!(frame = %frame_shape(text), "trouter ack frame");
        }
        Frame::Connect => tracing::debug!("trouter connect frame"),
        Frame::Noop => tracing::debug!("trouter noop frame"),
        Frame::Unknown => {
            tracing::debug!(frame = %frame_shape(text), "unknown trouter frame");
        }
    }
    handled
}

/// Debug-shapes for issue #19: verify the payload guesses against live
/// traffic by running with `SNAK_LOG=debug` (or the push example) and
/// reading what resourceTypes and body keys Teams actually sends. Values
/// are never logged, only field names, so message text stays out of logs.
fn log_push_shape(body: &Value) {
    let Some(object) = body.as_object() else {
        // Bodies arrive both as inline objects and as JSON strings, the
        // same way `parse_notification` takes them.
        if let Value::String(raw) = body
            && let Ok(parsed) = serde_json::from_str::<Value>(raw)
            && parsed.is_object()
        {
            return log_push_shape(&parsed);
        }
        tracing::debug!(kind = "non object", "push body ignored");
        return;
    };
    let keys = field_names(object);
    match body.get("presence") {
        Some(list) => {
            let count = list.as_array().map_or(0, Vec::len);
            tracing::debug!(%keys, count, "presence push");
        }
        None => {
            let resource_type = body.get("resourceType").and_then(Value::as_str);
            let Some(resource) = body.get("resource").and_then(Value::as_object) else {
                // Without a resource the keys are anyone's guess (presence
                // style maps key by user id), so only their count is safe.
                let key_count = object.len();
                tracing::debug!(key_count, ?resource_type, "push body without resource");
                return;
            };
            let resource_keys = field_names(resource);
            let message_type = resource.get("messagetype").and_then(Value::as_str);
            // A resourceType outside the handled set says exactly what to add
            // next; flag it so a shared debug log greps straight to it.
            let known = matches!(
                resource_type,
                Some("NewMessage" | "MessageUpdate" | "ConversationUpdate" | "ThreadUpdate")
            );
            if !known {
                tracing::debug!(%keys, ?resource_type, %resource_keys, ?message_type, "UNKNOWN push resource type");
            } else {
                tracing::debug!(%keys, ?resource_type, %resource_keys, ?message_type, "push notification");
            }
        }
    }
}

/// Turns the relayed request body into an event. Squads sends it as a JSON
/// string; an inline object is accepted too.
fn parse_notification(body: &Value) -> Option<PushEvent> {
    let parsed;
    let body = match body {
        Value::String(raw) => {
            parsed = serde_json::from_str::<Value>(raw).ok()?;
            &parsed
        }
        other => other,
    };

    if let Some(list) = body.get("presence").and_then(Value::as_array) {
        let presences: Vec<Presence> = list
            .iter()
            .filter_map(|p| serde_json::from_value::<RawPresence>(p.clone()).ok())
            .map(Presence::from)
            .collect();
        return (!presences.is_empty()).then_some(PushEvent::Presence(presences));
    }

    let resource = body.get("resource")?;
    let resource_type = body.get("resourceType").and_then(Value::as_str);
    let conversation_id = resource
        .get("conversationLink")
        .and_then(Value::as_str)
        .and_then(conversation_from_link)
        .or_else(|| {
            body.get("resourceLink")
                .and_then(Value::as_str)
                .and_then(conversation_from_link)
        });

    match resource_type {
        Some("ConversationUpdate" | "ThreadUpdate") => {
            let conversation_id = conversation_id.or_else(|| {
                resource
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })?;
            Some(PushEvent::ConversationUpdated { conversation_id })
        }
        Some("NewMessage" | "MessageUpdate") | None => {
            let conversation_id = conversation_id?;
            let message: Message = serde_json::from_value(resource.clone()).ok()?;
            let kind = message.message_type.as_deref().unwrap_or_default();
            if kind == "Control/Typing" || kind == "Control/ClearTyping" {
                let from = message.from.as_deref()?;
                let user_mri = from.rsplit('/').next().unwrap_or(from).to_string();
                return Some(PushEvent::Typing {
                    conversation_id,
                    user_mri,
                    stopped: kind == "Control/ClearTyping",
                });
            }
            // Live traffic also sends `ThreadActivity/...` markers (read
            // receipts, consumption horizons) with no message content; they
            // carry no message.
            if kind.starts_with("Control/") || kind.starts_with("ThreadActivity/") {
                return None;
            }
            let changed = resource_type == Some("MessageUpdate")
                || message.is_edited()
                || message.is_deleted();
            Some(if changed {
                PushEvent::MessageEdited {
                    conversation_id,
                    message,
                }
            } else {
                PushEvent::NewMessage {
                    conversation_id,
                    message,
                }
            })
        }
        Some(_) => None,
    }
}

// Links look like `https://notifications.skype.net/v1/users/ME/conversations/<id>[/messages/<n>]`.
fn conversation_from_link(link: &str) -> Option<String> {
    let (_, rest) = link.split_once("/conversations/")?;
    let id = rest.split('/').next()?;
    (!id.is_empty()).then(|| id.to_string())
}

#[derive(Deserialize)]
struct RawPresence {
    mri: String,
    #[serde(default)]
    presence: RawPresenceInfo,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPresenceInfo {
    availability: Option<String>,
    activity: Option<String>,
    last_active_time: Option<String>,
    calendar_data: Option<RawCalendarData>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCalendarData {
    #[serde(default)]
    is_out_of_office: bool,
}

impl From<RawPresence> for Presence {
    fn from(raw: RawPresence) -> Self {
        Self {
            mri: raw.mri,
            availability: raw.presence.availability,
            activity: raw.presence.activity,
            last_active_time: raw.presence.last_active_time,
            out_of_office: raw
                .presence
                .calendar_data
                .is_some_and(|c| c.is_out_of_office),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "19:abc123@thread.v2";
    const LINK: &str =
        "https://notifications.skype.net/v1/users/ME/conversations/19:abc123@thread.v2";

    fn relayed(id: i64, body: &Value) -> String {
        let request = json!({
            "id": id,
            "method": "POST",
            "url": "/v4/f/example/messaging",
            "headers": { "Content-Type": "application/json" },
            "body": body.to_string(),
        });
        format!("3:::{request}")
    }

    fn message_body(resource_type: &str, message_type: &str, properties: Value) -> Value {
        json!({
            "time": "2026-01-01T10:00:00Z",
            "type": "EventMessage",
            "resourceType": resource_type,
            "resourceLink": format!("{LINK}/messages/1767261600000"),
            "resource": {
                "id": "1767261600000",
                "clientmessageid": "42",
                "conversationLink": LINK,
                "content": "<p>hi</p>",
                "messagetype": message_type,
                "from": "https://notifications.skype.net/v1/users/ME/contacts/8:orgid:00000000-0000-0000-0000-000000000001",
                "imdisplayname": "Test User",
                "originalarrivaltime": "2026-01-01T10:00:00.000Z",
                "properties": properties,
            },
            "isactive": true,
        })
    }

    #[test]
    fn parses_socketio_frame_kinds() {
        assert_eq!(parse_frame("1::"), Frame::Connect);
        assert_eq!(parse_frame("0::"), Frame::Disconnect);
        assert_eq!(parse_frame("2::"), Frame::Heartbeat);
        assert_eq!(parse_frame("8::"), Frame::Noop);
        assert_eq!(parse_frame("6:::1"), Frame::Ack);
        assert_eq!(parse_frame("7:::1+0"), Frame::Error("1+0"));
        assert_eq!(parse_frame(""), Frame::Unknown);
        assert_eq!(parse_frame("garbage"), Frame::Unknown);
        assert_eq!(
            parse_frame(r#"3:::{"a":"b:c"}"#),
            Frame::Message(r#"{"a":"b:c"}"#)
        );
        assert_eq!(
            parse_frame(r#"5:3+::{"name":"trouter.connected"}"#),
            Frame::Event {
                ack_id: Some("3"),
                data: r#"{"name":"trouter.connected"}"#
            }
        );
        assert_eq!(
            parse_frame(r#"5:::{"name":"x"}"#),
            Frame::Event {
                ack_id: None,
                data: r#"{"name":"x"}"#
            }
        );
    }

    #[test]
    fn answers_heartbeats_and_acks_events() {
        assert_eq!(handle_frame("2::").replies, ["2::"]);
        assert_eq!(handle_frame(r#"5:7+::{"name":"x"}"#).replies, ["6:7::"]);
        assert!(handle_frame(r#"5:::{"name":"x"}"#).replies.is_empty());
        assert!(handle_frame("1::").replies.is_empty());
    }

    #[test]
    fn closes_on_disconnect_and_error() {
        assert!(handle_frame("0::").close.is_some());
        assert!(handle_frame("7:::boom").close.is_some());
        assert!(handle_frame("8::").close.is_none());
    }

    #[test]
    fn acknowledges_relayed_requests() {
        let handled = handle_frame(&relayed(17, &json!({})));
        assert_eq!(handled.replies, [r#"3:::{"id":17,"status":200}"#]);
        assert!(handled.events.is_empty());
    }

    #[test]
    fn ignores_malformed_message_frames() {
        let handled = handle_frame("3:::not json");
        assert!(handled.replies.is_empty());
        assert!(handled.events.is_empty());
        assert!(
            handle_frame(r#"3:::{"id":1,"body":"{broken"}"#)
                .events
                .is_empty()
        );
    }

    #[test]
    fn new_message() {
        let handled = handle_frame(&relayed(
            1,
            &message_body("NewMessage", "RichText/Html", json!({})),
        ));
        let [
            PushEvent::NewMessage {
                conversation_id,
                message,
            },
        ] = handled.events.as_slice()
        else {
            panic!("expected one NewMessage, got {:?}", handled.events);
        };
        assert_eq!(conversation_id, CHAT);
        assert_eq!(message.id.as_deref(), Some("1767261600000"));
        assert_eq!(message.client_message_id.as_deref(), Some("42"));
        assert_eq!(
            message.sender_id(),
            Some("00000000-0000-0000-0000-000000000001")
        );
    }

    #[test]
    fn edits_and_deletes() {
        for (resource_type, properties) in [
            ("MessageUpdate", json!({})),
            ("NewMessage", json!({ "edittime": "1767261700000" })),
            ("NewMessage", json!({ "deletetime": 1767261700000_i64 })),
        ] {
            let body = message_body(resource_type, "RichText/Html", properties);
            let event = parse_notification(&body);
            assert!(
                matches!(event, Some(PushEvent::MessageEdited { ref conversation_id, .. }) if conversation_id == CHAT),
                "{resource_type}: {event:?}"
            );
        }
    }

    #[test]
    fn typing() {
        let body = message_body("NewMessage", "Control/Typing", json!({}));
        let Some(PushEvent::Typing {
            conversation_id,
            user_mri,
            stopped: false,
        }) = parse_notification(&body)
        else {
            panic!("expected Typing");
        };
        assert_eq!(conversation_id, CHAT);
        assert_eq!(user_mri, "8:orgid:00000000-0000-0000-0000-000000000001");

        let clear = message_body("NewMessage", "Control/ClearTyping", json!({}));
        assert!(matches!(
            parse_notification(&clear),
            Some(PushEvent::Typing { stopped: true, .. })
        ));

        let horizon = message_body(
            "NewMessage",
            "ThreadActivity/MemberConsumptionHorizonUpdate",
            json!({}),
        );
        assert!(parse_notification(&horizon).is_none());
    }

    #[test]
    fn conversation_update() {
        let body = json!({
            "resourceType": "ThreadUpdate",
            "resourceLink": "https://notifications.skype.net/v1/threads/19:abc123@thread.v2",
            "resource": { "id": CHAT, "type": "Thread" },
        });
        assert!(matches!(
            parse_notification(&body),
            Some(PushEvent::ConversationUpdated { conversation_id }) if conversation_id == CHAT
        ));

        let body = json!({
            "resourceType": "ConversationUpdate",
            "resourceLink": LINK,
            "resource": { "id": CHAT },
        });
        assert!(matches!(
            parse_notification(&body),
            Some(PushEvent::ConversationUpdated { conversation_id }) if conversation_id == CHAT
        ));
    }

    #[test]
    fn body_as_inline_object() {
        let body = message_body("NewMessage", "Text", json!({}));
        let frame = format!("3:::{}", json!({ "id": 2, "body": body }));
        assert!(matches!(
            handle_frame(&frame).events.as_slice(),
            [PushEvent::NewMessage { .. }]
        ));
    }

    #[test]
    fn unknown_resource_types_are_ignored() {
        let body = json!({
            "resourceType": "EndpointPresence",
            "resourceLink": LINK,
            "resource": { "id": "x" },
        });
        assert!(parse_notification(&body).is_none());
        assert!(parse_notification(&json!({ "nothing": true })).is_none());
    }

    #[test]
    fn push_shapes_log_without_panicking() {
        // Odd bodies must reach the shape log without unwinding; a live day
        // of traffic sends shapes we have no structs for.
        for body in [
            json!("a string"),
            json!(42),
            json!({}),
            json!({ "presence": "not a list" }),
            json!({ "resourceType": "NewMessage", "resource": "not an object" }),
            json!({ "resourceType": 7, "resource": { "id": "x" } }),
            message_body("NewMessage", "RichText/Html", json!({})),
        ] {
            log_push_shape(&body);
        }
    }

    const TOKEN: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJSUzI1NiJ9.secret";
    const BODY: &str = "meet me at the usual place";

    fn frames_with_secrets() -> Vec<String> {
        let payload = json!({
            "skypetoken": TOKEN,
            "8:orgid:00000000-0000-0000-0000-000000000001": BODY,
            "content": BODY,
        });
        vec![
            format!("6:::{payload}"),
            format!("9:::{payload}"),
            format!("3:::{{not json {TOKEN} {BODY}"),
            format!("{TOKEN}:{BODY}"),
            format!(
                "3:::{}",
                json!({ "id": 1, "body": { "resourceType": "Odd", "skypetoken": TOKEN, "text": BODY } })
            ),
            format!(
                "3:::{}",
                json!({ "id": 1, "body": { "x-token": TOKEN, "content": BODY } })
            ),
        ]
    }

    #[test]
    fn frame_shape_names_fields_not_values() {
        let shape = frame_shape(&frames_with_secrets()[0]);
        assert!(shape.starts_with("kind=6 len="), "{shape}");
        // Key order follows serde_json's `preserve_order` feature, which the
        // workspace may or may not unify in.
        for part in ["content", "skypetoken", "+1 other"] {
            assert!(shape.contains(part), "{shape}");
        }
        assert_eq!(frame_shape("2::"), "kind=2 len=3 fields=not json");
        assert!(frame_shape("€€€:x").starts_with("kind=? len=11"));
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn frame_logs_carry_no_tokens_or_text() {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        // With a single live dispatcher, tracing computes a callsite's
        // interest from whichever thread registers it first, and parallel
        // tests register these with no subscriber. A second dispatcher makes
        // registration consult every live one, this capture included.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        tracing::subscriber::with_default(subscriber, || {
            for frame in frames_with_secrets() {
                handle_frame(&frame);
            }
        });
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("trouter ack frame"), "{log}");
        assert!(log.contains("unknown trouter frame"), "{log}");
        assert!(log.contains("unreadable push frame"), "{log}");
        assert!(log.contains("push body without resource"), "{log}");
        for secret in [TOKEN, BODY, "eyJ0eXAi", "usual place", "00000000-0000"] {
            assert!(!log.contains(secret), "{secret} leaked: {log}");
        }
    }

    #[test]
    fn presence() {
        let body = json!({
            "presence": [
                {
                    "mri": "8:orgid:00000000-0000-0000-0000-000000000002",
                    "etag": "A1",
                    "source": "ups",
                    "presence": {
                        "availability": "Busy",
                        "activity": "InAMeeting",
                        "lastActiveTime": "2026-01-01T09:00:00Z",
                        "calendarData": { "isOutOfOffice": true }
                    }
                },
                { "mri": "8:orgid:00000000-0000-0000-0000-000000000003" },
                { "no_mri": true }
            ],
            "isSnapshot": false
        });
        let Some(PushEvent::Presence(list)) = parse_notification(&body) else {
            panic!("expected Presence");
        };
        assert_eq!(
            list,
            [
                Presence {
                    mri: "8:orgid:00000000-0000-0000-0000-000000000002".into(),
                    availability: Some("Busy".into()),
                    activity: Some("InAMeeting".into()),
                    last_active_time: Some("2026-01-01T09:00:00Z".into()),
                    out_of_office: true,
                },
                Presence {
                    mri: "8:orgid:00000000-0000-0000-0000-000000000003".into(),
                    availability: None,
                    activity: None,
                    last_active_time: None,
                    out_of_office: false,
                },
            ]
        );
    }

    #[test]
    fn conversation_links() {
        assert_eq!(conversation_from_link(LINK).as_deref(), Some(CHAT));
        assert_eq!(
            conversation_from_link(&format!("{LINK}/messages/1")).as_deref(),
            Some(CHAT)
        );
        assert_eq!(conversation_from_link("https://example.com/x"), None);
        assert_eq!(conversation_from_link("https://x/conversations/"), None);
    }

    #[test]
    fn session_id_from_handshake() {
        assert_eq!(
            parse_session_id("abc-123:180:180:websocket,xhr-polling").as_deref(),
            Some("abc-123")
        );
        assert_eq!(parse_session_id(""), None);
        assert_eq!(parse_session_id(":180"), None);
    }

    fn allocation(socketio: &str) -> Allocation {
        let mut connectparams = Map::new();
        connectparams.insert("sig".into(), json!("a+b/c="));
        connectparams.insert("se".into(), json!("1767261600000"));
        connectparams.insert("ignored".into(), json!(5));
        Allocation {
            socketio: socketio.into(),
            surl: "https://pub.trouter.teams.microsoft.com:3443/v4/f/x/".into(),
            ccid: Some("cc1".into()),
            connectparams,
        }
    }

    #[test]
    fn builds_socket_urls() {
        let alloc = allocation("https://pub.trouter.teams.microsoft.com:443/");
        let url = socket_url(&alloc, &[], "ep-1", "5_1").expect("session url");
        assert_eq!(url.path(), "/socket.io/1/");
        let query: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert!(query.contains(&("sig".into(), "a+b/c=".into())));
        assert!(query.contains(&("epid".into(), "ep-1".into())));
        assert!(query.contains(&("ccid".into(), "cc1".into())));
        assert!(query.contains(&("con_num".into(), "5_1".into())));
        assert!(!query.iter().any(|(k, _)| k == "ignored"));
        assert!(!redact(&url).contains("sig"));

        let ws = socket_url(&alloc, &["websocket", "s/id"], "ep-1", "5_1").expect("ws url");
        assert_eq!(ws.path(), "/socket.io/1/websocket/s%2Fid");
    }

    #[test]
    fn refuses_foreign_socket_hosts() {
        for host in [
            "https://evil.example.com/",
            "https://notmicrosoft.com/",
            "http://pub.trouter.teams.microsoft.com/",
        ] {
            assert!(
                socket_url(&allocation(host), &[], "ep", "1_1").is_err(),
                "{host}"
            );
        }
        assert!(
            socket_url(
                &allocation("https://go.trouter.skype.com"),
                &[],
                "ep",
                "1_1"
            )
            .is_ok()
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        for attempt in 0..20 {
            let delay = backoff(attempt);
            let base_ms = (1u64 << attempt.min(6)).min(MAX_BACKOFF_SECS) * 1000;
            assert!(
                delay >= Duration::from_millis(base_ms),
                "{attempt}: {delay:?}"
            );
            assert!(
                delay <= Duration::from_millis(base_ms * 3 / 2),
                "{attempt}: {delay:?}"
            );
        }
    }

    #[test]
    fn uuid_shape() {
        let id = random_uuid();
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(groups[2].starts_with('4'));
        assert_ne!(id, random_uuid());
    }
}
