//! End-to-end smoke test against a *running* Aurix server (PostgreSQL + Redis required).
//!
//! Skipped unless `AURIX_E2E_API_KEY` is set; see the `e2e` job in `.github/workflows/ci.yml`
//! which boots the stack, bootstraps an admin/app and then runs this test:
//!
//! ```text
//! AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 \
//! AURIX_E2E_API_KEY=aurx_... cargo test -p aurix-server --test e2e_live -- --ignored
//! ```
//!
//! Flow covered: API key -> player tokens -> WebSocket SessionInit/ChannelJoin ->
//! authenticated AURX SessionBind -> audio routed Alice -> Bob over UDP ->
//! TURN credentials from the API accepted by the TURN server -> leave/close persisted.

use aurix_common::crypto::MediaKeys;
use aurix_common::protocol::{
    channel_id_hash, encode_volume_byte, AurixPacket, ControlMessage, PacketFlags, PacketType,
    TransmissionMode, UserPosition,
};
use aurix_common::types::{
    AudioCodec, AudioPolicy, ChannelId, Direction, MediaTransportKind, OpusBandwidth, OpusSignal,
    Orientation3D, Position3D, RecordingConsent, SessionId, UserId,
};
use aurix_turn::stun::{StunAttributeType, StunMessage, StunMessageType};
use base64::Engine;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
struct Env {
    api: String,
    ws: String,
    api_key: String,
}

fn env() -> Option<Env> {
    let api_key = std::env::var("AURIX_E2E_API_KEY").ok()?;
    Some(Env {
        api: std::env::var("AURIX_E2E_API").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        ws: std::env::var("AURIX_E2E_WS").unwrap_or_else(|_| "ws://127.0.0.1:8081".into()),
        api_key,
    })
}

/// A dedicated application for one test, so that app-wide observers — the `/v1/events` SSE
/// stream, webhooks, moderation / safety listings, quality alerts, app quotas — never see the
/// traffic of tests running in parallel. Returns the tenant's key and app id; without
/// `AURIX_E2E_ADMIN_TOKEN` the shared application is used and the test tolerates neighbours as
/// far as it can.
async fn isolated_env(base: &Env, http: &reqwest::Client, test: &str) -> (Env, Option<String>) {
    let Ok(admin) = std::env::var("AURIX_E2E_ADMIN_TOKEN") else {
        eprintln!(
            "AURIX_E2E_ADMIN_TOKEN not set; `{test}` shares its application with other tests"
        );
        return (base.clone(), None);
    };
    let app: serde_json::Value = http
        .post(format!("{}/v1/apps", base.api))
        .bearer_auth(&admin)
        .json(&serde_json::json!({"name": format!("e2e-{test}-{}", uuid::Uuid::now_v7().simple())}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .expect("create the test's own application")
        .json()
        .await
        .unwrap();
    let env = Env {
        api_key: app["api_key"].as_str().expect("api_key").to_string(),
        ..base.clone()
    };
    (env, Some(app["id"].as_str().expect("app id").to_string()))
}

struct Player {
    name: &'static str,
    token: String,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    session_id: SessionId,
    ssrc: u32,
    keys: MediaKeys,
    media_addr: SocketAddr,
    media_addrs: Vec<String>,
    udp: UdpSocket,
    media_key: Vec<u8>,
    resume_token: String,
    resume_grace: Duration,
    resumed: bool,
    media_tunnel: bool,
    migrated: bool,
    failover: Vec<String>,
    translation: Option<aurix_common::protocol::TranslationInfo>,
}

impl Player {
    async fn send(&mut self, msg: &ControlMessage) {
        self.ws
            .send(Message::Text(serde_json::to_string(msg).unwrap()))
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> ControlMessage {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .unwrap_or_else(|_| panic!("{}: timed out waiting for a WS message", self.name))
                .expect("ws closed")
                .expect("ws error");
            match m {
                Message::Text(t) => {
                    return serde_json::from_str(&t)
                        .unwrap_or_else(|e| panic!("{}: bad control message {t}: {e}", self.name))
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("{}: unexpected frame {other:?}", self.name),
            }
        }
    }

    /// Next message within `wait`, or `None` when the connection stays silent.
    async fn try_recv(&mut self, wait: Duration) -> Option<ControlMessage> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let m = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .ok()??
                .ok()?;
            match m {
                Message::Text(t) => return serde_json::from_str(&t).ok(),
                Message::Ping(_) | Message::Pong(_) => continue,
                _ => return None,
            }
        }
    }

    /// Wait for a message matching `pred`, skipping others (participant/speaking events).
    async fn expect<F: Fn(&ControlMessage) -> bool>(
        &mut self,
        what: &str,
        pred: F,
    ) -> ControlMessage {
        for _ in 0..20 {
            let m = self.recv().await;
            if pred(&m) {
                return m;
            }
            if let ControlMessage::Error { code, message, .. } = &m {
                panic!(
                    "{}: server error while waiting for {what}: {code} {message}",
                    self.name
                );
            }
        }
        panic!("{}: never received {what}", self.name);
    }

    async fn recv_udp(&self) -> Option<AurixPacket> {
        let mut buf = vec![0u8; 2048];
        match tokio::time::timeout(Duration::from_secs(3), self.udp.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
                assert!(
                    p.header.has_flag(PacketFlags::Encrypted),
                    "{}: downlink must be encrypted",
                    self.name
                );
                assert!(
                    p.open(&self.keys),
                    "{}: downlink must be sealed with this session's key",
                    self.name
                );
                Some(p)
            }
            _ => None,
        }
    }
}

async fn issue_token(
    env: &Env,
    http: &reqwest::Client,
    external_id: &str,
    name: &str,
    ch: ChannelId,
) -> (String, String) {
    issue_token_for(env, http, external_id, name, &[ch]).await
}

async fn issue_token_for(
    env: &Env,
    http: &reqwest::Client,
    external_id: &str,
    name: &str,
    channels: &[ChannelId],
) -> (String, String) {
    let grants: Vec<serde_json::Value> = channels
        .iter()
        .map(|ch| {
            serde_json::json!({"channel_id": ch, "join": true, "speak": true, "receive": true, "moderate": false})
        })
        .collect();
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": external_id,
            "display_name": name,
            "channels": grants,
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        r["token"].as_str().unwrap().to_string(),
        r["user_id"].as_str().unwrap().to_string(),
    )
}

async fn connect(env: &Env, name: &'static str, token: String) -> Player {
    connect_with(env, name, token, None).await
}

/// Connects, optionally presenting `<session_id>.<resume_token>` to reattach to a session.
async fn connect_with(
    env: &Env,
    name: &'static str,
    token: String,
    resume: Option<(SessionId, &str)>,
) -> Player {
    let mut req = format!("{}/ws", env.ws).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    if let Some((sid, tok)) = resume {
        req.headers_mut()
            .insert("x-aurix-resume", format!("{sid}.{tok}").parse().unwrap());
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws connect");
    // The server authenticates on upgrade and assigns the session id itself.
    let ack = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("SessionInitAck timeout")
        .unwrap()
        .unwrap();
    let Message::Text(t) = ack else {
        panic!("expected text")
    };
    let msg: ControlMessage = serde_json::from_str(&t).unwrap();
    let ControlMessage::SessionInitAck {
        session_id,
        ssrc,
        media_addr,
        media_addrs,
        media_key,
        resume_token,
        resume_grace_ms,
        resumed,
        media_tunnel,
        downlink_mix,
        migrated,
        failover,
        translation,
        ..
    } = msg
    else {
        panic!("{name}: expected SessionInitAck, got {t}");
    };
    assert!(
        downlink_mix,
        "{name}: dev nodes run with media.downlink_mix enabled"
    );
    assert!(
        media_addrs.is_empty() || media_addrs[0] == media_addr,
        "{name}: media_addrs must start with the legacy media_addr ({media_addr} vs {media_addrs:?})"
    );
    let media_key = base64::engine::general_purpose::STANDARD
        .decode(media_key)
        .unwrap();
    assert_eq!(media_key.len(), 32);
    assert!(
        !resume_token.is_empty(),
        "{name}: ack must carry a resume token"
    );
    let udp = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    Player {
        name,
        token,
        ws,
        session_id,
        ssrc,
        keys: MediaKeys::derive(&media_key),
        media_addr: media_addr.parse().unwrap(),
        media_addrs,
        udp,
        media_key,
        resume_token,
        resume_grace: Duration::from_millis(resume_grace_ms),
        resumed,
        media_tunnel,
        migrated,
        failover,
        translation,
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn bind_media(p: &mut Player) {
    let pkt = AurixPacket::session_bind(&p.session_id, p.ssrc, now_ms(), rand::random());
    p.udp
        .send_to(&pkt.encode_authenticated(&p.keys), p.media_addr)
        .await
        .unwrap();
    let ack = p.recv_udp().await.expect("no SessionBindAck");
    assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
    assert_eq!(ack.payload.len(), 8, "bind ack carries the server unix_ms");
    let m = p
        .expect("MediaBound", |m| {
            matches!(m, ControlMessage::MediaBound { .. })
        })
        .await;
    assert!(
        matches!(m, ControlMessage::MediaBound { session_id, .. } if session_id == p.session_id)
    );
}

#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn full_stack_two_players_udp_audio_and_turn() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();

    // Channel owned by this app.
    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"name": format!("e2e-{}", uuid::Uuid::now_v7())}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let channel_id = ChannelId::from_uuid(ch["id"].as_str().unwrap().parse().unwrap());

    let (tok_a, uid_a) = issue_token(&env, &http, "e2e:alice", "Alice", channel_id).await;
    let (tok_b, _uid_b) = issue_token(&env, &http, "e2e:bob", "Bob", channel_id).await;

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    assert_ne!(alice.ssrc, bob.ssrc);

    // Unauthenticated / unbound audio must be dropped: Bob is not bound yet, nothing arrives.
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;

    for p in [&mut alice, &mut bob] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id,
            token: tok,
        })
        .await;
        let ack = p
            .expect("ChannelJoinAck", |m| {
                matches!(m, ControlMessage::ChannelJoinAck { .. })
            })
            .await;
        if let ControlMessage::ChannelJoinAck { channel_id: c, .. } = ack {
            assert_eq!(c, channel_id);
        }
    }
    // Alice learns about Bob joining.
    alice
        .expect("ParticipantJoined", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Bob")
        })
        .await;

    // Participants visible through the server API.
    let parts: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{}/participants",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        parts["memberships"].as_array().map(|a| a.len()),
        Some(2),
        "{parts}"
    );
    assert_eq!(
        parts["live_on_this_node"].as_array().map(|a| a.len()),
        Some(2),
        "{parts}"
    );

    // Alice -> Bob audio over AURX.
    let hash = channel_id_hash(&channel_id);
    let payload = Bytes::from_static(&[0xFC, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let mut got = 0;
    for i in 0..10u32 {
        let pkt = AurixPacket::audio(i + 1, (i + 1) * 960, alice.ssrc, hash, payload.clone());
        alice
            .udp
            .send_to(&pkt.seal(&alice.keys), alice.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Some(p) = bob.recv_udp().await {
        if p.header.packet_type == PacketType::Audio {
            assert_eq!(p.header.ssrc, alice.ssrc, "downlink must carry sender SSRC");
            assert_eq!(&p.payload[..], &payload[..]);
            got += 1;
        }
        if got >= 5 {
            break;
        }
    }
    assert!(got >= 5, "Bob received only {got} audio packets from Alice");

    // Labelled audio (-6 dBov): the level byte is stripped from the downlink, Bob is told
    // Alice is speaking and receives her level over the control plane.
    for i in 0..10u32 {
        let seq = 20 + i;
        let pkt = AurixPacket::audio_with_level(seq, seq * 960, alice.ssrc, hash, 6, &payload);
        alice
            .udp
            .send_to(&pkt.seal(&alice.keys), alice.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut labelled = 0;
    while let Some(p) = bob.recv_udp().await {
        // The downlink carries the server's per-sender audio sequence (0-based), so the
        // labelled batch is frames 10..20 regardless of the uplink numbering.
        if p.header.packet_type == PacketType::Audio && p.header.sequence >= 10 {
            assert!(
                !p.header.has_flag(PacketFlags::Energy),
                "level metadata must not be forwarded to listeners"
            );
            assert_eq!(&p.payload[..], &payload[..]);
            labelled += 1;
        }
        if labelled >= 5 {
            break;
        }
    }
    assert!(
        labelled >= 5,
        "Bob received only {labelled} labelled packets"
    );
    let alice_uid = UserId::from_uuid(uid_a.parse().unwrap());
    bob.expect("SpeakingStateChanged(alice)", |m| {
        matches!(
            m,
            ControlMessage::SpeakingStateChanged { channel_id: c, user_id, speaking: true }
                if *c == channel_id && *user_id == alice_uid
        )
    })
    .await;
    let energy = bob
        .expect("ChannelEnergy(alice)", |m| {
            matches!(
                m,
                ControlMessage::ChannelEnergy { channel_id: c, levels }
                    if *c == channel_id && levels.iter().any(|l| l.user_id == alice_uid && l.energy > 0.0)
            )
        })
        .await;
    if let ControlMessage::ChannelEnergy { levels, .. } = energy {
        let alice_level = levels.iter().find(|l| l.user_id == alice_uid).unwrap();
        assert!(
            (alice_level.energy - 0.501).abs() < 0.01,
            "-6 dBov must decode to ~0.5, got {}",
            alice_level.energy
        );
    }

    // Forged audio (wrong key) or a spoofed SSRC must not be forwarded.
    let forged = AurixPacket::audio(
        100,
        100 * 960,
        alice.ssrc,
        hash,
        Bytes::from_static(b"forged"),
    );
    bob.udp
        .send_to(&forged.seal(&bob.keys), bob.media_addr)
        .await
        .unwrap();
    let stray = AurixPacket::audio(
        101,
        101 * 960,
        bob.ssrc,
        hash,
        Bytes::from_static(b"unauth"),
    );
    bob.udp
        .send_to(&stray.encode(), bob.media_addr)
        .await
        .unwrap();
    let mut leaked = false;
    while let Some(p) = alice.recv_udp().await {
        if p.header.packet_type == PacketType::Audio {
            leaked = true;
        }
    }
    assert!(!leaked, "forged/unauthenticated audio reached Alice");

    // Recording: real Opus packets from Alice must end up in a downloadable Ogg file.
    let rec: serde_json::Value = http
        .post(format!("{}/v1/recordings/start", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "user_id": uid_a}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let recording_id: uuid::Uuid = rec["id"].as_str().unwrap().parse().unwrap();
    // Both participants are told a recording is in progress.
    for p in [&mut alice, &mut bob] {
        p.expect("RecordingNotification", |m| {
            matches!(m, ControlMessage::RecordingNotification { active: true, recording_id: r, .. } if *r == recording_id)
        })
        .await;
    }
    alice
        .send(&ControlMessage::RecordingConsentResponse {
            recording_id,
            consent: RecordingConsent::Accepted,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    for i in 20..40u32 {
        let pkt = AurixPacket::audio(i + 1, (i + 1) * 960, alice.ssrc, hash, payload.clone());
        alice
            .udp
            .send_to(&pkt.seal(&alice.keys), alice.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while bob.recv_udp().await.is_some() {}
    http.post(format!("{}/v1/recordings/{}/stop", env.api, recording_id))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let ogg = http
        .get(format!(
            "{}/v1/recordings/{}/download",
            env.api, recording_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(ogg.starts_with(b"OggS"), "download is not an Ogg stream");
    assert!(
        ogg.windows(8).any(|w| w == b"OpusHead"),
        "Ogg stream lacks OpusHead"
    );
    let pages = ogg.windows(4).filter(|w| *w == b"OggS").count();
    assert!(
        pages >= 3,
        "expected audio pages beyond OpusHead/OpusTags, got {pages} pages"
    );
    assert!(
        ogg.windows(payload.len()).any(|w| w == &payload[..]),
        "recorded Opus payload not found in Ogg output"
    );

    // TURN: credentials minted by the API must be accepted by the TURN server.
    let turn: serde_json::Value = http
        .get(format!("{}/v1/me/turn-credentials", env.api))
        .bearer_auth(&alice.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let uri = turn["uris"][0].as_str().unwrap();
    let hostport = uri.trim_start_matches("turn:").split('?').next().unwrap();
    let turn_addr: SocketAddr = tokio::net::lookup_host(hostport)
        .await
        .unwrap()
        .next()
        .unwrap();
    let username = turn["username"].as_str().unwrap();
    let password = turn["password"].as_str().unwrap();
    let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let mut req = StunMessage::new(StunMessageType::AllocateRequest, rand::random());
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    sock.send_to(&req.encode(), turn_addr).await.unwrap();
    let mut buf = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .expect("TURN did not answer")
        .unwrap();
    let challenge = StunMessage::decode(&buf[..n]).unwrap();
    assert_eq!(challenge.msg_type, StunMessageType::AllocateErrorResponse);
    let realm = challenge.get_string(StunAttributeType::Realm).unwrap();
    let nonce = challenge.get_string(StunAttributeType::Nonce).unwrap();
    let key = aurix_common::crypto::stun_long_term_key(username, &realm, password);
    let mut req = StunMessage::new(StunMessageType::AllocateRequest, rand::random());
    req.add_attribute(StunAttributeType::Username, username.as_bytes().to_vec());
    req.add_attribute(StunAttributeType::Realm, realm.into_bytes());
    req.add_attribute(StunAttributeType::Nonce, nonce.into_bytes());
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    sock.send_to(&req.encode_with_integrity(&key), turn_addr)
        .await
        .unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .expect("TURN did not answer allocate")
        .unwrap();
    let resp = StunMessage::decode(&buf[..n]).unwrap();
    assert_eq!(resp.msg_type, StunMessageType::AllocateResponse, "{resp:?}");
    assert!(StunMessage::verify_integrity(&buf[..n], &key));
    assert!(resp
        .get_xor_address(StunAttributeType::XorRelayedAddress)
        .is_some());

    // Leave + close; server state must reflect it.
    bob.send(&ControlMessage::ChannelLeave { channel_id }).await;
    alice
        .expect("ParticipantLeft", |m| {
            matches!(m, ControlMessage::ParticipantLeft { .. })
        })
        .await;
    bob.ws.close(None).await.unwrap();
    alice.ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let parts: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{}/participants",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        parts["memberships"].as_array().map(|a| a.len()),
        Some(0),
        "{parts}"
    );
    assert_eq!(
        parts["live_on_this_node"].as_array().map(|a| a.len()),
        Some(0),
        "{parts}"
    );

    // Persisted user with a stable external id.
    let user: serde_json::Value = http
        .get(format!("{}/v1/users/{uid_a}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(user["user"]["external_id"], "e2e:alice");
    assert_eq!(
        user["active_sessions"].as_array().map(|a| a.len()),
        Some(0),
        "session must be closed in the database after WS disconnect: {user}"
    );
}

/// One TURN allocation over `sock`, asking for `family` (`0x01` IPv4, `0x02` IPv6); returns
/// the relayed address, or the error code the server answered with.
async fn turn_allocate(
    sock: &UdpSocket,
    turn_addr: SocketAddr,
    username: &str,
    password: &str,
    family: u8,
) -> Result<SocketAddr, u16> {
    let mut buf = vec![0u8; 2048];
    let mut req = StunMessage::new(StunMessageType::AllocateRequest, rand::random());
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    sock.send_to(&req.encode(), turn_addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .expect("TURN did not answer")
        .unwrap();
    let challenge = StunMessage::decode(&buf[..n]).unwrap();
    assert_eq!(challenge.msg_type, StunMessageType::AllocateErrorResponse);
    let realm = challenge.get_string(StunAttributeType::Realm).unwrap();
    let nonce = challenge.get_string(StunAttributeType::Nonce).unwrap();
    let key = aurix_common::crypto::stun_long_term_key(username, &realm, password);
    let mut req = StunMessage::new(StunMessageType::AllocateRequest, rand::random());
    req.add_attribute(StunAttributeType::Username, username.as_bytes().to_vec());
    req.add_attribute(StunAttributeType::Realm, realm.into_bytes());
    req.add_attribute(StunAttributeType::Nonce, nonce.into_bytes());
    req.add_attribute(StunAttributeType::RequestedTransport, vec![17, 0, 0, 0]);
    req.add_attribute(
        StunAttributeType::RequestedAddressFamily,
        vec![family, 0, 0, 0],
    );
    sock.send_to(&req.encode_with_integrity(&key), turn_addr)
        .await
        .unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .expect("TURN did not answer allocate")
        .unwrap();
    let resp = StunMessage::decode(&buf[..n]).unwrap();
    assert!(StunMessage::verify_integrity(&buf[..n], &key));
    match resp.msg_type {
        StunMessageType::AllocateResponse => Ok(resp
            .get_xor_address(StunAttributeType::XorRelayedAddress)
            .expect("relayed address")),
        StunMessageType::AllocateErrorResponse => Err(resp
            .get_attribute(StunAttributeType::ErrorCode)
            .map(|a| a.value[2] as u16 * 100 + a.value[3] as u16)
            .expect("error code")),
        other => panic!("unexpected TURN answer {other:?}"),
    }
}

/// A node started with `media.host = turn.host = "::"`, `media.external_ip = 127.0.0.1` and
/// `media.external_ipv6 = ::1` serves IPv4 and IPv6 players on the same sockets: the ack lists
/// both endpoints (IPv4 first), an IPv4 player and an IPv6 player hear each other, the TURN
/// credentials carry bracketed IPv6 URIs, and TURN relays in whichever family the client asks
/// for. Loopback only — this proves the plumbing, not public IPv6 reachability. Needs
/// `AURIX_E2E_IPV6=1`.
#[tokio::test]
#[ignore = "requires a dual-stack Aurix server (AURIX_E2E_IPV6=1); see docs/operations/deployment.md"]
async fn dual_stack_node_serves_ipv4_and_ipv6_players_and_turn() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    if std::env::var("AURIX_E2E_IPV6").ok().as_deref() != Some("1") {
        eprintln!("AURIX_E2E_IPV6 not set; skipping");
        return;
    }
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let (tok_a, _) = issue_token(&env, &http, "e2e:v6-alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env, &http, "e2e:v6-bob", "Bob", channel_id).await;

    // Alice stays on the legacy IPv4 endpoint; Bob binds the IPv6 candidate.
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let ack_addrs = alice.media_addrs.clone();
    assert_eq!(
        ack_addrs.len(),
        2,
        "dual-stack node advertises both families: {ack_addrs:?}"
    );
    let v4: SocketAddr = ack_addrs[0].parse().unwrap();
    let v6: SocketAddr = ack_addrs[1].parse().unwrap();
    assert!(v4.is_ipv4() && v6.is_ipv6(), "{ack_addrs:?}");
    assert!(
        ack_addrs[1].starts_with('['),
        "IPv6 endpoint must be bracketed: {ack_addrs:?}"
    );
    assert_eq!(alice.media_addr, v4);
    bob.media_addr = v6;
    bob.udp = UdpSocket::bind("[::]:0").await.unwrap();

    bind_media(&mut alice).await;
    bind_media(&mut bob).await;
    join(&mut alice, channel_id).await;
    join(&mut bob, channel_id).await;
    alice
        .expect("ParticipantJoined(bob)", |m| {
            matches!(m, ControlMessage::ParticipantJoined { .. })
        })
        .await;

    let payload = Bytes::from_static(b"dual-stack-opus");
    send_audio(&alice, channel_id, 1, &payload).await;
    assert!(
        count_audio_from(&bob, alice.ssrc, &payload).await >= 5,
        "IPv6 Bob must hear IPv4 Alice"
    );
    send_audio(&bob, channel_id, 1, &payload).await;
    assert!(
        count_audio_from(&alice, bob.ssrc, &payload).await >= 5,
        "IPv4 Alice must hear IPv6 Bob"
    );

    // TURN credentials: URIs for both families, IPv6 bracketed.
    let turn: serde_json::Value = http
        .get(format!("{}/v1/me/turn-credentials", env.api))
        .bearer_auth(&alice.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let uris: Vec<&str> = turn["uris"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap())
        .collect();
    let udp_v4 = uris
        .iter()
        .find(|u| u.starts_with("turn:127.0.0.1:") && u.ends_with("transport=udp"))
        .unwrap_or_else(|| panic!("no IPv4 TURN UDP URI in {uris:?}"));
    let udp_v6 = uris
        .iter()
        .find(|u| u.starts_with("turn:[::1]:") && u.ends_with("transport=udp"))
        .unwrap_or_else(|| panic!("no bracketed IPv6 TURN UDP URI in {uris:?}"));
    assert!(
        uris.iter().any(|u| u.starts_with("stun:[::1]:")),
        "{uris:?}"
    );
    let addr_of = |uri: &str| -> SocketAddr {
        uri.trim_start_matches("turn:")
            .split('?')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let (turn_v4, turn_v6) = (addr_of(udp_v4), addr_of(udp_v6));
    assert_eq!(turn_v4.port(), turn_v6.port());
    let username = turn["username"].as_str().unwrap();
    let password = turn["password"].as_str().unwrap();

    // IPv4 client → IPv4 relay (default) and IPv6 relay on request; IPv6 client likewise.
    let sock4 = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let relay = turn_allocate(&sock4, turn_v4, username, password, 0x01)
        .await
        .expect("IPv4 relay for IPv4 client");
    assert_eq!(
        relay.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    let sock4b = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let relay = turn_allocate(&sock4b, turn_v4, username, password, 0x02)
        .await
        .expect("IPv6 relay for IPv4 client");
    assert_eq!(
        relay.ip(),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
    );
    let sock6 = UdpSocket::bind("[::]:0").await.unwrap();
    let relay = turn_allocate(&sock6, turn_v6, username, password, 0x02)
        .await
        .expect("IPv6 relay for IPv6 client");
    assert_eq!(
        relay.ip(),
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
    );
    let sock6b = UdpSocket::bind("[::]:0").await.unwrap();
    assert_eq!(
        turn_allocate(&sock6b, turn_v6, username, password, 0x07).await,
        Err(440),
        "unknown address family is refused"
    );

    bob.ws.close(None).await.unwrap();
    alice.ws.close(None).await.unwrap();
}

async fn create_channel(env: &Env, http: &reqwest::Client) -> ChannelId {
    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"name": format!("e2e-{}", uuid::Uuid::now_v7())}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    ChannelId::from_uuid(ch["id"].as_str().unwrap().parse().unwrap())
}

async fn membership_count(env: &Env, http: &reqwest::Client, channel_id: ChannelId) -> usize {
    let parts: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{}/participants",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    parts["memberships"].as_array().map(|a| a.len()).unwrap()
}

async fn join(p: &mut Player, channel_id: ChannelId) {
    let tok = p.token.clone();
    p.send(&ControlMessage::ChannelJoin {
        channel_id,
        token: tok,
    })
    .await;
    p.expect(
        "ChannelJoinAck",
        |m| matches!(m, ControlMessage::ChannelJoinAck { channel_id: c, .. } if *c == channel_id),
    )
    .await;
}

async fn send_audio(from: &Player, channel_id: ChannelId, first_seq: u32, payload: &Bytes) {
    let hash = channel_id_hash(&channel_id);
    for i in 0..10u32 {
        let seq = first_seq + i;
        let pkt = AurixPacket::audio(seq, seq * 960, from.ssrc, hash, payload.clone());
        from.udp
            .send_to(&pkt.seal(&from.keys), from.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn count_audio_from(to: &Player, ssrc: u32, payload: &Bytes) -> usize {
    let mut got = 0;
    while let Some(p) = to.recv_udp().await {
        if p.header.packet_type == PacketType::Audio && p.header.ssrc == ssrc {
            assert_eq!(&p.payload[..], &payload[..]);
            got += 1;
        }
        if got >= 5 {
            break;
        }
    }
    got
}

/// A WebSocket that dies without a Close frame keeps the session alive for the grace period;
/// reconnecting with the resume token restores the same session/SSRC/key and the channel
/// membership without peers seeing a leave. Wrong tokens fall back to a fresh session, and
/// an expired grace period closes the session for real.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn session_resume_after_ws_drop() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let (tok_a, _) = issue_token(&env, &http, "e2e:resume-alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env, &http, "e2e:resume-bob", "Bob", channel_id).await;

    let alice = connect(&env, "alice", tok_a.clone()).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    assert!(!alice.resumed && !bob.resumed);
    assert!(
        alice.resume_grace >= Duration::from_secs(1),
        "server must advertise a resume grace period (got {:?})",
        alice.resume_grace
    );
    let mut alice = alice;
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;
    join(&mut alice, channel_id).await;
    join(&mut bob, channel_id).await;
    alice
        .expect("ParticipantJoined", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Bob")
        })
        .await;

    // Alice's socket dies without a Close frame (network blip).
    let Player {
        session_id: sid_a,
        ssrc: ssrc_a,
        media_key: key_a,
        resume_token: first_token,
        ws: dead_ws,
        ..
    } = alice;
    drop(dead_ws);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Bob sees nothing about Alice (his own periodic quality report is not a peer event),
    // the membership is still persisted.
    let leaked = bob
        .try_recv(Duration::from_millis(700))
        .await
        .filter(|m| !matches!(m, ControlMessage::NetworkQuality { .. }));
    assert!(
        leaked.is_none(),
        "peers must not be notified while the session is detached: {leaked:?}"
    );
    assert_eq!(membership_count(&env, &http, channel_id).await, 2);

    // Another user's JWT cannot resume Alice's session even with the right resume token.
    let (tok_c, _) = issue_token(&env, &http, "e2e:resume-carol", "Carol", channel_id).await;
    let mut hijack = connect_with(
        &env,
        "carol-as-alice",
        tok_c.clone(),
        Some((sid_a, &first_token)),
    )
    .await;
    assert!(!hijack.resumed);
    assert_ne!(hijack.session_id, sid_a);
    hijack.ws.close(None).await.unwrap();

    // The real resume: same session, SSRC and key; channel state replayed; token rotated.
    let mut alice = connect_with(&env, "alice", tok_a.clone(), Some((sid_a, &first_token))).await;
    assert!(alice.resumed, "expected resumed session");
    assert_eq!(alice.session_id, sid_a);
    assert_eq!(alice.ssrc, ssrc_a);
    assert_eq!(alice.media_key, key_a);
    assert_ne!(alice.resume_token, first_token, "resume token must rotate");
    let ack = alice
        .expect("ChannelJoinAck (replayed)", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    let ControlMessage::ChannelJoinAck {
        channel_id: c,
        participants,
        ..
    } = ack
    else {
        unreachable!()
    };
    assert_eq!(c, channel_id);
    assert_eq!(participants.len(), 1, "{participants:?}");
    assert_eq!(participants[0].display_name, "Bob");
    assert_eq!(participants[0].ssrc, bob.ssrc);

    // Media re-binds from a new port and audio flows with the old SSRC.
    bind_media(&mut alice).await;
    let payload = Bytes::from_static(&[0xFC, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
    send_audio(&alice, channel_id, 1000, &payload).await;
    let got = count_audio_from(&bob, ssrc_a, &payload).await;
    assert!(got >= 5, "Bob received only {got} packets after resume");
    assert_eq!(membership_count(&env, &http, channel_id).await, 2);

    // The used token is dead and the session is attached: Carol can claim nothing with either.
    for tok in [&first_token, &alice.resume_token.clone()] {
        let mut c = connect_with(&env, "carol-replay", tok_c.clone(), Some((sid_a, tok))).await;
        assert!(!c.resumed);
        c.ws.close(None).await.unwrap();
    }
    let second_token = alice.resume_token.clone();

    // Grace expiry closes the session for real (only exercised with a short configured grace).
    if alice.resume_grace <= Duration::from_secs(10) {
        let grace = alice.resume_grace;
        drop(alice.ws);
        tokio::time::timeout(grace + Duration::from_secs(3), async {
            loop {
                if let ControlMessage::ParticipantLeft { .. } = bob.recv().await {
                    break;
                }
            }
        })
        .await
        .expect("Bob never saw Alice leave after the grace period");
        assert_eq!(membership_count(&env, &http, channel_id).await, 1);
        let mut late = connect_with(
            &env,
            "alice-late",
            tok_a.clone(),
            Some((sid_a, &second_token)),
        )
        .await;
        assert!(!late.resumed, "expired sessions must not be resumable");
        assert_ne!(late.session_id, sid_a);
        late.ws.close(None).await.unwrap();
    } else {
        eprintln!(
            "resume grace is {:?}; set AURIX__SERVER__SESSION_RESUME_GRACE_SECS<=10 to test expiry",
            alice.resume_grace
        );
        alice.ws.close(None).await.unwrap();
        bob.expect("ParticipantLeft", |m| {
            matches!(m, ControlMessage::ParticipantLeft { .. })
        })
        .await;
    }

    // A stale/wrong token gives the same user a fresh session, which replaces the detached one
    // right away instead of leaving it to the grace timer.
    let mut alice = connect(&env, "alice", tok_a.clone()).await;
    let sid_a2 = alice.session_id;
    join(&mut alice, channel_id).await;
    bob.expect("ParticipantJoined", |m| {
        matches!(m, ControlMessage::ParticipantJoined { .. })
    })
    .await;
    drop(alice.ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut fresh = connect_with(&env, "alice-wrong-token", tok_a, Some((sid_a2, "AAAA"))).await;
    assert!(!fresh.resumed);
    assert_ne!(fresh.session_id, sid_a2);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let ControlMessage::ParticipantLeft { .. } = bob.recv().await {
                break;
            }
        }
    })
    .await
    .expect("replaced session must leave its channels immediately");
    assert_eq!(membership_count(&env, &http, channel_id).await, 1);
    fresh.ws.close(None).await.unwrap();
    bob.ws.close(None).await.unwrap();
}

/// Two Aurix nodes sharing one PostgreSQL/Redis, no `cascade_peers` configured: the second node
/// is given by `AURIX_E2E_API2` / `AURIX_E2E_WS2`. Alice joins on node 1, Bob on node 2, and
/// Alice's audio must reach Bob through the automatically discovered cascade.
#[tokio::test]
#[ignore = "requires two running Aurix nodes; see README (Scaling)"]
async fn two_nodes_auto_cascade_relays_audio() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let Ok(ws2) = std::env::var("AURIX_E2E_WS2") else {
        eprintln!("AURIX_E2E_WS2 not set; skipping");
        return;
    };
    let env2 = Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws: ws2,
        api_key: env.api_key.clone(),
    };
    let http = reqwest::Client::new();

    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"name": format!("cascade-{}", uuid::Uuid::now_v7())}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let channel_id = ChannelId::from_uuid(ch["id"].as_str().unwrap().parse().unwrap());

    let (tok_a, alice_uid) = issue_token(&env, &http, "cascade:alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env2, &http, "cascade:bob", "Bob", channel_id).await;
    let alice_uid = UserId::from_uuid(alice_uid.parse().unwrap());
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env2, "bob", tok_b).await;
    assert_ne!(
        alice.media_addr.port(),
        bob.media_addr.port(),
        "players must land on different nodes"
    );
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;

    for p in [&mut alice, &mut bob] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id,
            token: tok,
        })
        .await;
        p.expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    }
    // Cross-node presence replicates through Redis.
    alice
        .expect("ParticipantJoined(Bob) from other node", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Bob")
        })
        .await;

    // Topology reconciles on the join event; give both nodes a moment and then stream.
    let hash = channel_id_hash(&channel_id);
    let payload = Bytes::from_static(&[0xFC, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
    let mut got = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    let mut seq = 0u32;
    while got < 5 && tokio::time::Instant::now() < deadline {
        for _ in 0..5 {
            seq += 1;
            let pkt = AurixPacket::audio(seq, seq * 960, alice.ssrc, hash, payload.clone());
            alice
                .udp
                .send_to(&pkt.seal(&alice.keys), alice.media_addr)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut buf = vec![0u8; 2048];
        while let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(300), bob.udp.recv_from(&mut buf)).await
        {
            let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
            assert!(
                p.open(&bob.keys),
                "relayed downlink must be re-sealed with Bob's key"
            );
            if p.header.packet_type == PacketType::Audio {
                assert_eq!(p.header.ssrc, alice.ssrc);
                assert_eq!(&p.payload[..], &payload[..]);
                got += 1;
            }
        }
    }
    assert!(
        got >= 5,
        "Bob (node 2) received only {got} audio packets from Alice (node 1) via cascade"
    );

    // Receiver-local preferences must hold across the cascade: Bob's node knows Alice only as
    // a relayed SSRC, and Alice's node learns about Bob's block through the event bus.
    let bob_payload = Bytes::from_static(&[0xFC, 1, 1, 2, 3, 5, 8, 13]);
    let mut bob_seq = 1000u32;
    while bob.recv_udp().await.is_some() {}
    while alice.recv_udp().await.is_some() {}
    send_audio(&bob, channel_id, bob_seq, &bob_payload).await;
    bob_seq += 10;
    let (_, n) = audio_from(&alice, bob.ssrc, &bob_payload).await;
    assert!(n >= 8, "baseline: Alice got {n} relayed packets from Bob");

    bob.send(&ControlMessage::SetParticipantMute {
        user_id: alice_uid,
        channel_id: None,
        muted: true,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_audio(&alice, channel_id, seq + 1, &payload).await;
    seq += 10;
    let (_, n) = audio_from(&bob, alice.ssrc, &payload).await;
    assert_eq!(n, 0, "local mute must drop relayed audio from Alice");

    bob.send(&ControlMessage::SetParticipantMute {
        user_id: alice_uid,
        channel_id: None,
        muted: false,
    })
    .await;
    bob.send(&ControlMessage::SetParticipantVolume {
        user_id: alice_uid,
        volume: 0.5,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_audio(&alice, channel_id, seq + 1, &payload).await;
    seq += 10;
    let (gain, n) = audio_from(&bob, alice.ssrc, &payload).await;
    assert!(n >= 8, "unmuted relayed audio came back ({n} packets)");
    assert_eq!(
        gain,
        Some(encode_volume_byte(0.5)),
        "relayed audio carries Bob's local gain"
    );

    bob.send(&ControlMessage::SetUserBlock {
        user_id: alice_uid,
        blocked: true,
    })
    .await;
    bob.expect("UserBlockChanged", |m| {
        matches!(m, ControlMessage::UserBlockChanged { user_id, blocked: true } if *user_id == alice_uid)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    send_audio(&alice, channel_id, seq + 1, &payload).await;
    seq += 10;
    send_audio(&bob, channel_id, bob_seq, &bob_payload).await;
    bob_seq += 10;
    let (_, to_bob) = audio_from(&bob, alice.ssrc, &payload).await;
    let (_, to_alice) = audio_from(&alice, bob.ssrc, &bob_payload).await;
    assert_eq!(to_bob, 0, "blocked Alice must not reach Bob across nodes");
    assert_eq!(
        to_alice, 0,
        "cross-mute is mutual: Bob must not reach Alice on the other node"
    );
    bob.send(&ControlMessage::SetUserBlock {
        user_id: alice_uid,
        blocked: false,
    })
    .await;
    bob.expect("UserBlockChanged(unblocked)", |m| {
        matches!(m, ControlMessage::UserBlockChanged { user_id, blocked: false } if *user_id == alice_uid)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    send_audio(&bob, channel_id, bob_seq, &bob_payload).await;
    let (_, n) = audio_from(&alice, bob.ssrc, &bob_payload).await;
    assert!(n >= 8, "unblock restores relayed audio ({n} packets)");

    // Bob leaves: node 1 must stop relaying this channel to node 2.
    bob.send(&ControlMessage::ChannelLeave { channel_id }).await;
    alice
        .expect("ParticipantLeft(Bob)", |m| {
            matches!(m, ControlMessage::ParticipantLeft { .. })
        })
        .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mut leaked = 0;
    for _ in 0..10 {
        seq += 1;
        let pkt = AurixPacket::audio(seq, seq * 960, alice.ssrc, hash, payload.clone());
        alice
            .udp
            .send_to(&pkt.seal(&alice.keys), alice.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Some(p) = bob.recv_udp().await {
        if p.header.packet_type == PacketType::Audio {
            leaked += 1;
        }
    }
    assert_eq!(leaked, 0, "audio kept flowing to node 2 after Bob left");

    let _ = alice.ws.close(None).await;
    let _ = bob.ws.close(None).await;
}

/// Collects distinct audio sequence numbers from `ssrc` arriving at `to` until the link is
/// quiet for 500 ms; panics on a duplicate (a relay tree must deliver every packet once).
async fn audio_seqs_from(to: &Player, ssrc: u32, payload: &Bytes) -> Vec<u32> {
    let mut seqs = Vec::new();
    let mut buf = vec![0u8; 2048];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(500), to.udp.recv_from(&mut buf)).await
    {
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: downlink must verify", to.name);
        if p.header.packet_type != PacketType::Audio || p.header.ssrc != ssrc {
            continue;
        }
        assert_eq!(&p.payload[..], &payload[..], "{}: payload intact", to.name);
        assert!(
            !seqs.contains(&p.header.sequence),
            "{}: sequence {} from {ssrc} delivered twice (relay tree duplicated a packet)",
            to.name,
            p.header.sequence
        );
        seqs.push(p.header.sequence);
    }
    seqs
}

/// `seqs` is one burst of `expected` consecutive per-sender sequence numbers (the origin node
/// renumbers a sender's audio once; the cascade preserves it), allowing two lost packets.
fn assert_burst(seqs: &[u32], expected: usize, what: &str) {
    let (min, max) = (
        seqs.iter().copied().min().unwrap_or(0),
        seqs.iter().copied().max().unwrap_or(0),
    );
    assert!(
        seqs.len() + 2 >= expected && (max - min) < expected as u32,
        "{what}: {} of {expected} packets, sequences {min}..={max}",
        seqs.len()
    );
}

fn metric_sum(body: &str, name: &str, label: &str) -> f64 {
    body.lines()
        .filter(|l| l.starts_with(name) && l.contains(label))
        .filter_map(|l| l.split_whitespace().last()?.parse::<f64>().ok())
        .sum()
}

/// Region-tree cascade over four nodes: nodes 1 and 2 (`us_east`) and node 3 (`eu_west`) host
/// players, node 4 (`eu_west`) is a `cascade_relay_only` hub. Given by `AURIX_E2E_API3` /
/// `AURIX_E2E_WS3`, `AURIX_E2E_WS4` and `AURIX_E2E_METRICS4`. Alice (node 1) talks to Carol
/// (node 2, same region — direct) and Bob (node 3 — via the `us_east` hub and the relay-only
/// `eu_west` hub, 3 hops); every packet arrives exactly once, the hub is never offered to
/// clients, and — with `AURIX_E2E_NODE4_STOP` — losing the hub re-elects node 3 as the
/// `eu_west` hub and cross-region audio recovers.
#[tokio::test]
#[ignore = "requires four Aurix nodes in two regions with a relay-only hub; see docs/operations/scaling.md"]
async fn region_tree_relays_through_hub_exactly_once_and_survives_hub_loss() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let (Ok(ws2), Ok(ws3), Ok(ws4)) = (
        std::env::var("AURIX_E2E_WS2"),
        std::env::var("AURIX_E2E_WS3"),
        std::env::var("AURIX_E2E_WS4"),
    ) else {
        eprintln!("AURIX_E2E_WS2/WS3/WS4 not set; skipping");
        return;
    };
    let env2 = Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws: ws2,
        api_key: env.api_key.clone(),
    };
    let env3 = Env {
        api: std::env::var("AURIX_E2E_API3").unwrap_or_else(|_| "http://127.0.0.1:8100".into()),
        ws: ws3,
        api_key: env.api_key.clone(),
    };
    let http = reqwest::Client::new();

    // The relay-only hub is registered (admin registry) but never offered to clients.
    let mut hub_id = None;
    if let Ok(admin) = std::env::var("AURIX_E2E_ADMIN_TOKEN") {
        let nodes: Vec<serde_json::Value> = http
            .get(format!("{}/v1/nodes", env.api))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let hubs: Vec<&serde_json::Value> = nodes
            .iter()
            .filter(|n| n["relay_only"].as_bool() == Some(true))
            .collect();
        assert_eq!(
            hubs.len(),
            1,
            "exactly one relay-only node registered: {nodes:?}"
        );
        assert!(
            hubs[0]["ws_url"].is_null(),
            "relay-only hub advertises no ws_url"
        );
        assert_eq!(hubs[0]["capacity"].as_u64(), Some(0));
        assert_eq!(hubs[0]["region"].as_str(), Some("eu_west"));
        hub_id = Some(hubs[0]["id"].as_str().unwrap().to_string());
    } else {
        eprintln!("AURIX_E2E_ADMIN_TOKEN not set; skipping the node-registry check");
    }
    let regions: serde_json::Value = http
        .get(format!("{}/v1/regions", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let regions = regions["regions"].as_array().expect("regions array");
    assert!(
        regions.iter().any(|r| r["region"] == "eu_west"),
        "eu_west is still offered through its hosting node: {regions:?}"
    );
    for r in regions {
        assert!(
            hub_id.is_none() || r["node_id"].as_str() != hub_id.as_deref(),
            "region discovery must not hand out the relay-only hub: {r}"
        );
        let hub_hostport = ws4.trim_start_matches("ws://");
        assert!(
            r["ws_url"]
                .as_str()
                .is_some_and(|u| !u.contains(hub_hostport)),
            "region discovery must not hand out the relay-only hub: {r}"
        );
    }
    let mut req = format!("{ws4}/ws").into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer bogus".parse().unwrap());
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => assert_eq!(
            resp.status(),
            503,
            "a relay-only node refuses client sessions before authentication"
        ),
        other => panic!("relay-only /ws must answer 503, got {other:?}"),
    }

    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"name": format!("tree-{}", uuid::Uuid::now_v7())}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let channel_id = ChannelId::from_uuid(ch["id"].as_str().unwrap().parse().unwrap());

    let (tok_a, _) = issue_token(&env, &http, "tree:alice", "Alice", channel_id).await;
    let (tok_c, _) = issue_token(&env2, &http, "tree:carol", "Carol", channel_id).await;
    let (tok_b, _) = issue_token(&env3, &http, "tree:bob", "Bob", channel_id).await;
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut carol = connect(&env2, "carol", tok_c).await;
    let mut bob = connect(&env3, "bob", tok_b).await;
    let ports: std::collections::HashSet<u16> = [&alice, &carol, &bob]
        .iter()
        .map(|p| p.media_addr.port())
        .collect();
    assert_eq!(ports.len(), 3, "players must land on three different nodes");
    for p in [&mut alice, &mut carol, &mut bob] {
        bind_media(p).await;
    }
    for p in [&mut alice, &mut carol, &mut bob] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id,
            token: tok,
        })
        .await;
        p.expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    }
    // Presence replicates through Redis: everyone sees the players that joined after them.
    for (p, later) in [
        (&mut alice, vec!["Carol", "Bob"]),
        (&mut carol, vec!["Bob"]),
    ] {
        for other in later {
            p.expect("ParticipantJoined from other nodes", |m| {
                matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == other)
            })
            .await;
        }
    }

    // Every node re-plans on the join events; wait until Alice reaches both regions.
    let payload = Bytes::from_static(&[0xFC, 3, 1, 4, 1, 5, 9, 2, 6]);
    let mut seq = 1u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        send_audio(&alice, channel_id, seq, &payload).await;
        seq += 10;
        let to_carol = audio_seqs_from(&carol, alice.ssrc, &payload).await;
        let to_bob = audio_seqs_from(&bob, alice.ssrc, &payload).await;
        if to_carol.len() >= 8 && to_bob.len() >= 8 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Alice's audio did not reach both regions (Carol {} / Bob {} packets)",
            to_carol.len(),
            to_bob.len()
        );
    }

    // Steady state: 30 packets from Alice, each delivered exactly once in both regions, and
    // Bob's audio takes the tree back (eu_west hub → us_east hub → nodes 1 and 2).
    while alice.recv_udp().await.is_some() {}
    while carol.recv_udp().await.is_some() {}
    while bob.recv_udp().await.is_some() {}
    for _ in 0..3 {
        send_audio(&alice, channel_id, seq, &payload).await;
        seq += 10;
    }
    let to_carol = audio_seqs_from(&carol, alice.ssrc, &payload).await;
    let to_bob = audio_seqs_from(&bob, alice.ssrc, &payload).await;
    assert_burst(&to_carol, 30, "Carol (same region, direct)");
    assert_burst(&to_bob, 30, "Bob (other region, via two hubs)");
    let bob_payload = Bytes::from_static(&[0xFC, 2, 7, 1, 8, 2, 8]);
    let mut bob_seq = 5000u32;
    send_audio(&bob, channel_id, bob_seq, &bob_payload).await;
    bob_seq += 10;
    let back_a = audio_seqs_from(&alice, bob.ssrc, &bob_payload).await;
    let back_c = audio_seqs_from(&carol, bob.ssrc, &bob_payload).await;
    assert!(
        back_a.len() >= 8,
        "Alice got {} packets from Bob",
        back_a.len()
    );
    assert!(
        back_c.len() >= 8,
        "Carol got {} packets from Bob",
        back_c.len()
    );

    // The relay-only node really is the eu_west hub: it re-forwarded envelopes and hubs the
    // channel, and never hit the hop cap.
    if let Ok(metrics4) = std::env::var("AURIX_E2E_METRICS4") {
        let body = http
            .get(&metrics4)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let forwarded = metric_sum(&body, "aurix_cascade_forwarded_total", "role=\"hub\"");
        assert!(
            forwarded >= 40.0,
            "relay-only hub forwarded only {forwarded} envelopes:\n{body}"
        );
        assert_eq!(
            metric_sum(&body, "aurix_cascade_forwarded_total", "role=\"origin\""),
            0.0,
            "a relay-only node hosts nobody and originates nothing"
        );
        assert_eq!(
            metric_sum(&body, "aurix_cascade_forwarded_total", "role=\"hop_limit\""),
            0.0,
            "hop cap must never be reached on a 3-hop path"
        );
        let hub_channels = metric_sum(&body, "aurix_cascade_hub_channels", "");
        assert!(hub_channels >= 1.0, "hub gauge {hub_channels}");
    } else {
        eprintln!("AURIX_E2E_METRICS4 not set; skipping hub metric checks");
    }

    // Hub loss: kill node 4. Node 3 becomes its own region's hub once the registry marks node
    // 4 unhealthy, and Alice ↔ Bob audio recovers without anyone re-joining.
    if let Ok(stop) = std::env::var("AURIX_E2E_NODE4_STOP") {
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&stop)
            .status()
            .await
            .expect("run AURIX_E2E_NODE4_STOP");
        assert!(status.success(), "AURIX_E2E_NODE4_STOP failed");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        let mut recovered = false;
        while tokio::time::Instant::now() < deadline {
            while bob.recv_udp().await.is_some() {}
            send_audio(&alice, channel_id, seq, &payload).await;
            seq += 10;
            let to_bob = audio_seqs_from(&bob, alice.ssrc, &payload).await;
            if to_bob.len() >= 8 {
                recovered = true;
                break;
            }
        }
        assert!(
            recovered,
            "cross-region audio did not recover after losing the hub"
        );
        // Same-region delivery was never affected and the tree still delivers once.
        while carol.recv_udp().await.is_some() {}
        while bob.recv_udp().await.is_some() {}
        for _ in 0..2 {
            send_audio(&alice, channel_id, seq, &payload).await;
            seq += 10;
        }
        let to_carol = audio_seqs_from(&carol, alice.ssrc, &payload).await;
        let to_bob = audio_seqs_from(&bob, alice.ssrc, &payload).await;
        assert_burst(&to_carol, 20, "Carol after hub loss");
        assert_burst(&to_bob, 20, "Bob after hub loss");
        send_audio(&bob, channel_id, bob_seq, &bob_payload).await;
        let back_a = audio_seqs_from(&alice, bob.ssrc, &bob_payload).await;
        assert!(
            back_a.len() >= 8,
            "Alice got {} packets from Bob after hub loss",
            back_a.len()
        );
        if let Ok(start) = std::env::var("AURIX_E2E_NODE4_START") {
            let status = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&start)
                .status()
                .await
                .expect("run AURIX_E2E_NODE4_START");
            assert!(status.success(), "AURIX_E2E_NODE4_START failed");
        }
    } else {
        eprintln!("AURIX_E2E_NODE4_STOP not set; skipping the hub-loss scenario");
    }

    let _ = alice.ws.close(None).await;
    let _ = carol.ws.close(None).await;
    let _ = bob.ws.close(None).await;
}

/// Drains every audio packet from `ssrc` arriving at `to` until the link is quiet for 400 ms,
/// returning the gain byte of the first one (`None` when not attenuated) and the packet count.
async fn audio_from(to: &Player, ssrc: u32, payload: &Bytes) -> (Option<u8>, usize) {
    let mut got = 0;
    let mut gain = None;
    let mut buf = vec![0u8; 2048];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(400), to.udp.recv_from(&mut buf)).await
    {
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: downlink must verify", to.name);
        if p.header.packet_type != PacketType::Audio || p.header.ssrc != ssrc {
            continue;
        }
        if p.header.has_flag(PacketFlags::VolumeAttenuated) {
            gain.get_or_insert(p.payload[0]);
            assert_eq!(&p.payload[1..], &payload[..]);
        } else {
            assert_eq!(&p.payload[..], &payload[..]);
        }
        got += 1;
    }
    (gain, got)
}

/// Receiver-side controls over the control plane and REST: a local mute and a per-participant
/// volume shape only the caller's downlink; a cross-mute is mutual, persists in the database,
/// is applied to a brand-new session at login and shows up in `ReceiverPreferences`.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn local_mute_volume_and_persistent_cross_mute() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let (tok_a, uid_a) = issue_token(&env, &http, "e2e:mute-alice", "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "e2e:mute-bob", "Bob", channel_id).await;
    let (tok_c, _) = issue_token(&env, &http, "e2e:mute-carol", "Carol", channel_id).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a.clone()).await;
    let mut bob = connect(&env, "bob", tok_b.clone()).await;
    let mut carol = connect(&env, "carol", tok_c).await;
    for p in [&mut alice, &mut bob, &mut carol] {
        let prefs = p
            .expect("ReceiverPreferences", |m| {
                matches!(m, ControlMessage::ReceiverPreferences { .. })
            })
            .await;
        if let ControlMessage::ReceiverPreferences {
            blocked_users,
            local_mutes,
            volumes,
            ..
        } = prefs
        {
            assert!(blocked_users.is_empty() && local_mutes.is_empty() && volumes.is_empty());
        }
        bind_media(p).await;
        join(p, channel_id).await;
    }
    let hello = Bytes::from_static(b"hello");

    // Baseline.
    send_audio(&alice, channel_id, 1, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await, (None, 10));
    assert_eq!(audio_from(&carol, alice.ssrc, &hello).await, (None, 10));

    // Bob mutes Alice for himself only: no MuteStateChanged is broadcast, Carol still hears her.
    bob.send(&ControlMessage::SetParticipantMute {
        user_id: uid_a,
        channel_id: Some(channel_id),
        muted: true,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_audio(&alice, channel_id, 100, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 0);
    assert_eq!(audio_from(&carol, alice.ssrc, &hello).await.1, 10);
    while let Some(m) = alice.try_recv(Duration::from_millis(300)).await {
        assert!(
            !matches!(
                m,
                ControlMessage::MuteStateChanged { .. } | ControlMessage::UserBlockChanged { .. }
            ),
            "the sender must not learn about a local mute: {m:?}"
        );
    }
    bob.send(&ControlMessage::SetParticipantMute {
        user_id: uid_a,
        channel_id: None,
        muted: false,
    })
    .await;

    // Muting a channel one is not a member of is rejected.
    bob.send(&ControlMessage::SetParticipantMute {
        user_id: uid_a,
        channel_id: Some(ChannelId::new()),
        muted: true,
    })
    .await;
    bob.expect(
        "NOT_IN_CHANNEL",
        |m| matches!(m, ControlMessage::Error { code, .. } if code == "NOT_IN_CHANNEL"),
    )
    .await;

    // Per-participant volume: Bob hears Alice at 0.5, Carol at full.
    bob.send(&ControlMessage::SetParticipantVolume {
        user_id: uid_a,
        volume: 0.5,
    })
    .await;
    bob.send(&ControlMessage::SetParticipantVolume {
        user_id: uid_a,
        volume: 3.0,
    })
    .await;
    bob.expect(
        "VALIDATION_ERROR",
        |m| matches!(m, ControlMessage::Error { code, .. } if code == "VALIDATION_ERROR"),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_audio(&alice, channel_id, 200, &hello).await;
    assert_eq!(
        audio_from(&bob, alice.ssrc, &hello).await,
        (Some(encode_volume_byte(0.5)), 10)
    );
    assert_eq!(audio_from(&carol, alice.ssrc, &hello).await, (None, 10));
    bob.send(&ControlMessage::SetParticipantVolume {
        user_id: uid_a,
        volume: 1.0,
    })
    .await;

    // Cross-mute over the control plane: acked, mutual, invisible to Alice.
    bob.send(&ControlMessage::SetUserBlock {
        user_id: uid_a,
        blocked: true,
    })
    .await;
    bob.expect("UserBlockChanged", |m| {
        matches!(m, ControlMessage::UserBlockChanged { user_id, blocked: true } if *user_id == uid_a)
    })
    .await;
    send_audio(&alice, channel_id, 300, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 0);
    assert_eq!(audio_from(&carol, alice.ssrc, &hello).await.1, 10);
    send_audio(&bob, channel_id, 1, &hello).await;
    assert_eq!(audio_from(&alice, bob.ssrc, &hello).await.1, 0);
    assert_eq!(audio_from(&carol, bob.ssrc, &hello).await.1, 10);
    while let Some(m) = alice.try_recv(Duration::from_millis(300)).await {
        assert!(
            !matches!(
                m,
                ControlMessage::MuteStateChanged { .. } | ControlMessage::UserBlockChanged { .. }
            ),
            "the blocked side must not be notified: {m:?}"
        );
    }

    // Listed over REST, and persisted: a fresh session for Bob starts with the block loaded.
    let list: serde_json::Value = http
        .get(format!("{}/v1/users/{}/blocks", env.api, uid_b))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["blocked_users"], serde_json::json!([uid_a]));

    // Tenant isolation: an API key of another application cannot see, create or delete blocks
    // that involve users of this application, and the same external ids map to different users
    // (with their own, empty block lists) over there.
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let env2 = Env {
            api_key: api_key2,
            ..env.clone()
        };
        let r = http
            .get(format!("{}/v1/users/{}/blocks", env2.api, uid_b))
            .header("x-api-key", &env2.api_key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "foreign tenant must not list blocks");
        let r = http
            .post(format!("{}/v1/users/{}/blocks", env2.api, uid_b))
            .header("x-api-key", &env2.api_key)
            .json(&serde_json::json!({"blocked_user_id": uid_a}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "foreign tenant must not create blocks");
        let r = http
            .delete(format!("{}/v1/users/{}/blocks/{}", env2.api, uid_b, uid_a))
            .header("x-api-key", &env2.api_key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "foreign tenant must not delete blocks");

        let channel2 = create_channel(&env2, &http).await;
        let (tok_b2, uid_b2) = issue_token(&env2, &http, "e2e:mute-bob", "Bob", channel2).await;
        assert_ne!(
            uid_b2,
            uid_b.to_string(),
            "users are scoped per application"
        );
        let list2: serde_json::Value = http
            .get(format!("{}/v1/users/{}/blocks", env2.api, uid_b2))
            .header("x-api-key", &env2.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list2["blocked_users"], serde_json::json!([]));
        let mut bob2 = connect(&env2, "bob-tenant2", tok_b2).await;
        let prefs = bob2
            .expect("ReceiverPreferences", |m| {
                matches!(m, ControlMessage::ReceiverPreferences { .. })
            })
            .await;
        assert!(
            matches!(&prefs, ControlMessage::ReceiverPreferences { blocked_users, .. } if blocked_users.is_empty()),
            "blocks must not leak across tenants: {prefs:?}"
        );
        let _ = bob2.ws.close(None).await;
        // The original tenant's block is untouched by the foreign attempts above.
        let list: serde_json::Value = http
            .get(format!("{}/v1/users/{}/blocks", env.api, uid_b))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["blocked_users"], serde_json::json!([uid_a]));
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
    }

    bob.ws.close(None).await.unwrap();
    let mut bob = connect(&env, "bob2", tok_b).await;
    let prefs = bob
        .expect("ReceiverPreferences", |m| {
            matches!(m, ControlMessage::ReceiverPreferences { .. })
        })
        .await;
    assert!(
        matches!(&prefs, ControlMessage::ReceiverPreferences { blocked_users, .. } if blocked_users == &vec![uid_a]),
        "block list must be loaded at login: {prefs:?}"
    );
    bind_media(&mut bob).await;
    join(&mut bob, channel_id).await;
    send_audio(&alice, channel_id, 400, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 0);

    // Unblock through REST: Bob's live session gets the ack and both hear each other again.
    http.delete(format!("{}/v1/users/{}/blocks/{}", env.api, uid_b, uid_a))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    bob.expect("UserBlockChanged(false)", |m| {
        matches!(m, ControlMessage::UserBlockChanged { user_id, blocked: false } if *user_id == uid_a)
    })
    .await;
    send_audio(&alice, channel_id, 500, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 10);
    send_audio(&bob, channel_id, 100, &hello).await;
    assert_eq!(audio_from(&alice, bob.ssrc, &hello).await.1, 10);

    // Blocking yourself is refused on both surfaces.
    bob.send(&ControlMessage::SetUserBlock {
        user_id: uid_b,
        blocked: true,
    })
    .await;
    bob.expect(
        "VALIDATION_ERROR",
        |m| matches!(m, ControlMessage::Error { code, .. } if code == "VALIDATION_ERROR"),
    )
    .await;
    let r = http
        .post(format!("{}/v1/users/{}/blocks", env.api, uid_b))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"blocked_user_id": uid_b}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    for mut p in [alice, bob, carol] {
        let _ = p.ws.close(None).await;
    }
}

async fn action_token(
    env: &Env,
    http: &reqwest::Client,
    body: serde_json::Value,
) -> Result<serde_json::Value, u16> {
    let r = http
        .post(format!("{}/v1/tokens/action", env.api))
        .header("x-api-key", &env.api_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    if !r.status().is_success() {
        return Err(r.status().as_u16());
    }
    Ok(r.json().await.unwrap())
}

/// WebSocket connect that surfaces the HTTP status of a refused upgrade.
async fn try_connect(
    env: &Env,
    token: &str,
    resume: Option<(SessionId, &str)>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u16,
> {
    let mut req = format!("{}/ws", env.ws).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    if let Some((sid, tok)) = resume {
        req.headers_mut()
            .insert("x-aurix-resume", format!("{sid}.{tok}").parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("ws connect failed unexpectedly: {e}"),
    }
}

async fn expect_error(p: &mut Player, what: &str, code: &str) {
    let m = p
        .expect(what, |m| matches!(m, ControlMessage::Error { .. }))
        .await;
    let ControlMessage::Error {
        code: got, message, ..
    } = m
    else {
        unreachable!()
    };
    assert_eq!(got, code, "{}: {what}: {message}", p.name);
}

/// One-time action tokens: `login` opens exactly one fresh session (a resume of that session
/// is not a second use), `join` grants one channel entry to one user, `kick`/`mute`/`unmute`
/// let a player moderate one target once. Every replay is refused with TOKEN_REUSED and
/// every mismatch (other user, channel, target or action) with AUTH_DENIED.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn action_tokens_are_single_use() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let other_channel = create_channel(&env, &http).await;

    // ── issuance validation ──
    let login_a = action_token(
        &env,
        &http,
        serde_json::json!({"action": "login", "external_id": "e2e:act-alice", "display_name": "Alice"}),
    )
    .await
    .unwrap();
    let uid_a = UserId::from_uuid(login_a["user_id"].as_str().unwrap().parse().unwrap());
    assert_eq!(login_a["action"], "login");
    assert!(!login_a["jti"].as_str().unwrap().is_empty());
    let login_b = action_token(
        &env,
        &http,
        serde_json::json!({"action": "login", "external_id": "e2e:act-bob", "display_name": "Bob"}),
    )
    .await
    .unwrap();
    let uid_b = UserId::from_uuid(login_b["user_id"].as_str().unwrap().parse().unwrap());
    assert_eq!(
        action_token(
            &env,
            &http,
            serde_json::json!({"action": "join", "user_id": uid_a})
        )
        .await,
        Err(400),
        "join needs channel_id"
    );
    assert_eq!(
        action_token(
            &env,
            &http,
            serde_json::json!({"action": "kick", "user_id": uid_a, "channel_id": channel_id})
        )
        .await,
        Err(400),
        "kick needs target_user_id"
    );
    assert_eq!(
        action_token(
            &env,
            &http,
            serde_json::json!({"action": "login", "user_id": uid_a, "ttl_secs": 100000})
        )
        .await,
        Err(400),
        "ttl above the configured maximum"
    );
    assert_eq!(
        action_token(
            &env,
            &http,
            serde_json::json!({"action": "join", "user_id": uid_a, "channel_id": uuid::Uuid::now_v7()})
        )
        .await,
        Err(404),
        "unknown channel"
    );
    assert_eq!(
        action_token(
            &env,
            &http,
            serde_json::json!({"action": "login", "user_id": uuid::Uuid::now_v7()})
        )
        .await,
        Err(404),
        "unknown user"
    );
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let env2 = Env {
            api_key: api_key2,
            ..env.clone()
        };
        assert_eq!(
            action_token(
                &env2,
                &http,
                serde_json::json!({"action": "join", "user_id": uid_a, "channel_id": channel_id})
            )
            .await,
            Err(404),
            "foreign tenant must not mint tokens for our users/channels"
        );
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation check");
    }

    // ── login: exactly one fresh session ──
    let login_tok_a = login_a["token"].as_str().unwrap().to_string();
    let mut alice = connect(&env, "alice", login_tok_a.clone()).await;
    assert!(!alice.resumed);
    assert_eq!(
        try_connect(&env, &login_tok_a, None).await.err(),
        Some(401),
        "second login with the same token must be refused"
    );
    let mut bob = connect(&env, "bob", login_b["token"].as_str().unwrap().to_string()).await;
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;

    // The login token carries no channel rights and a join token is bound to its channel.
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: login_tok_a.clone(),
        })
        .await;
    expect_error(&mut alice, "join without rights", "AUTH_DENIED").await;
    let join_wrong = action_token(
        &env,
        &http,
        serde_json::json!({"action": "join", "user_id": uid_a, "channel_id": other_channel}),
    )
    .await
    .unwrap();
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: join_wrong["token"].as_str().unwrap().to_string(),
        })
        .await;
    expect_error(&mut alice, "join with other channel's token", "AUTH_DENIED").await;
    // Moderation tokens cannot be used to join.
    let kick_tok = action_token(
        &env,
        &http,
        serde_json::json!({"action": "kick", "user_id": uid_a, "channel_id": channel_id, "target_user_id": uid_b}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: kick_tok.clone(),
        })
        .await;
    expect_error(&mut alice, "join with kick token", "AUTH_DENIED").await;

    // ── join: one entry, bound to the user ──
    let join_a = action_token(
        &env,
        &http,
        serde_json::json!({"action": "join", "user_id": uid_a, "channel_id": channel_id, "moderate": true}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    bob.send(&ControlMessage::ChannelJoin {
        channel_id,
        token: join_a.clone(),
    })
    .await;
    expect_error(&mut bob, "bob using alice's join token", "AUTH_DENIED").await;
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: join_a.clone(),
        })
        .await;
    alice
        .expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { channel_id: c, .. } if *c == channel_id)
        })
        .await;
    alice
        .send(&ControlMessage::ChannelLeave { channel_id })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: join_a.clone(),
        })
        .await;
    expect_error(&mut alice, "replayed join token", "TOKEN_REUSED").await;
    let join_a2 = action_token(
        &env,
        &http,
        serde_json::json!({"action": "join", "user_id": uid_a, "channel_id": channel_id, "moderate": true}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: join_a2,
        })
        .await;
    alice
        .expect("ChannelJoinAck (fresh token)", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    let join_b = action_token(
        &env,
        &http,
        serde_json::json!({"action": "join", "user_id": uid_b, "channel_id": channel_id}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    bob.send(&ControlMessage::ChannelJoin {
        channel_id,
        token: join_b,
    })
    .await;
    bob.expect("ChannelJoinAck", |m| {
        matches!(m, ControlMessage::ChannelJoinAck { .. })
    })
    .await;
    alice
        .expect(
            "ParticipantJoined",
            |m| matches!(m, ControlMessage::ParticipantJoined { user_id, .. } if *user_id == uid_b),
        )
        .await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 2);

    // ── mute / unmute via one-time tokens ──
    let mute_tok = action_token(
        &env,
        &http,
        serde_json::json!({"action": "mute", "user_id": uid_a, "channel_id": channel_id, "target_user_id": uid_b}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    // Wrong action for the token, wrong target, wrong actor: all refused, token untouched.
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_b,
            action: aurix_common::types::ActionKind::Unmute,
            token: mute_tok.clone(),
            reason: None,
        })
        .await;
    expect_error(&mut alice, "mute token used for unmute", "AUTH_DENIED").await;
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_a,
            action: aurix_common::types::ActionKind::Mute,
            token: mute_tok.clone(),
            reason: None,
        })
        .await;
    expect_error(
        &mut alice,
        "mute token used on another target",
        "AUTH_DENIED",
    )
    .await;
    bob.send(&ControlMessage::ModerateParticipant {
        channel_id,
        user_id: uid_b,
        action: aurix_common::types::ActionKind::Mute,
        token: mute_tok.clone(),
        reason: None,
    })
    .await;
    expect_error(&mut bob, "bob using alice's mute token", "AUTH_DENIED").await;
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_b,
            action: aurix_common::types::ActionKind::Mute,
            token: mute_tok.clone(),
            reason: None,
        })
        .await;
    alice
        .expect("ModerateParticipantAck", |m| {
            matches!(
                m,
                ControlMessage::ModerateParticipantAck { action: aurix_common::types::ActionKind::Mute, user_id, .. } if *user_id == uid_b
            )
        })
        .await;
    bob.expect("MuteStateChanged", |m| {
        matches!(
            m,
            ControlMessage::MuteStateChanged { user_id, muted: true, server_muted: true, .. } if *user_id == uid_b
        )
    })
    .await;
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_b,
            action: aurix_common::types::ActionKind::Mute,
            token: mute_tok,
            reason: None,
        })
        .await;
    expect_error(&mut alice, "replayed mute token", "TOKEN_REUSED").await;
    let unmute_tok = action_token(
        &env,
        &http,
        serde_json::json!({"action": "unmute", "user_id": uid_a, "channel_id": channel_id, "target_user_id": uid_b}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_b,
            action: aurix_common::types::ActionKind::Unmute,
            token: unmute_tok,
            reason: None,
        })
        .await;
    alice
        .expect("ModerateParticipantAck (unmute)", |m| {
            matches!(
                m,
                ControlMessage::ModerateParticipantAck {
                    action: aurix_common::types::ActionKind::Unmute,
                    ..
                }
            )
        })
        .await;
    bob.expect("MuteStateChanged (unmuted)", |m| {
        matches!(
            m,
            ControlMessage::MuteStateChanged { user_id, muted: false, .. } if *user_id == uid_b
        )
    })
    .await;

    // ── kick via one-time token ──
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_b,
            action: aurix_common::types::ActionKind::Kick,
            token: kick_tok.clone(),
            reason: Some("e2e".into()),
        })
        .await;
    alice
        .expect("ModerateParticipantAck (kick)", |m| {
            matches!(
                m,
                ControlMessage::ModerateParticipantAck {
                    action: aurix_common::types::ActionKind::Kick,
                    ..
                }
            )
        })
        .await;
    bob.expect("Kick", |m| {
        matches!(m, ControlMessage::Kick { user_id, reason, .. } if *user_id == uid_b && reason == "e2e")
    })
    .await;
    alice
        .expect(
            "ParticipantLeft",
            |m| matches!(m, ControlMessage::ParticipantLeft { user_id, .. } if *user_id == uid_b),
        )
        .await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 1);
    alice
        .send(&ControlMessage::ModerateParticipant {
            channel_id,
            user_id: uid_b,
            action: aurix_common::types::ActionKind::Kick,
            token: kick_tok,
            reason: None,
        })
        .await;
    expect_error(&mut alice, "replayed kick token", "TOKEN_REUSED").await;

    // ── login token + resume: reattaching the session it opened is not a second use ──
    let Player {
        session_id: sid_a,
        resume_token,
        ws: dead_ws,
        ..
    } = alice;
    drop(dead_ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alice = connect_with(
        &env,
        "alice",
        login_tok_a.clone(),
        Some((sid_a, &resume_token)),
    )
    .await;
    assert!(
        alice.resumed,
        "login token must still resume its own session"
    );
    assert_eq!(alice.session_id, sid_a);
    // A newly issued login token may resume too (the old one may have expired by then) and
    // is spent by doing so.
    let Player {
        resume_token,
        ws: dead_ws,
        ..
    } = alice;
    drop(dead_ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let login_tok_a2 = action_token(
        &env,
        &http,
        serde_json::json!({"action": "login", "user_id": uid_a}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut alice = connect_with(
        &env,
        "alice",
        login_tok_a2.clone(),
        Some((sid_a, &resume_token)),
    )
    .await;
    assert!(alice.resumed);
    assert_eq!(alice.session_id, sid_a);
    alice.ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        try_connect(&env, &login_tok_a, None).await.err(),
        Some(401),
        "after the session is gone the login token stays consumed"
    );
    assert_eq!(
        try_connect(&env, &login_tok_a2, None).await.err(),
        Some(401),
        "a login token spent on a resume cannot open a fresh session"
    );

    // Login action tokens still authenticate end-user REST reads for the session's lifetime.
    let me = http
        .get(format!("{}/v1/me/turn-credentials", env.api))
        .header("authorization", format!("Bearer {login_tok_a}"))
        .send()
        .await
        .unwrap();
    assert!(
        me.status().is_success(),
        "login token must be accepted by end-user REST (got {})",
        me.status()
    );
    // ...but a moderation token is not an API credential.
    let denied = http
        .get(format!("{}/v1/me/turn-credentials", env.api))
        .header(
            "authorization",
            format!("Bearer {}", join_wrong["token"].as_str().unwrap()),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 403);
    bob.ws.close(None).await.unwrap();
}

/// With `auth.require_action_tokens = true` the reusable session JWT is refused everywhere a
/// one-time token exists: as a WebSocket login and as a join credential. Run against a node
/// started with `AURIX__AUTH__REQUIRE_ACTION_TOKENS=true` and `AURIX_E2E_STRICT=1`.
#[tokio::test]
#[ignore = "requires a running Aurix server in strict mode (AURIX_E2E_STRICT=1)"]
async fn strict_mode_requires_action_tokens() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    if std::env::var("AURIX_E2E_STRICT").is_err() {
        eprintln!("AURIX_E2E_STRICT not set; skipping");
        return;
    }
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let (session_jwt, uid) =
        issue_token(&env, &http, "e2e:strict-alice", "Alice", channel_id).await;
    assert_eq!(
        try_connect(&env, &session_jwt, None).await.err(),
        Some(403),
        "session JWT must not open a WebSocket session in strict mode"
    );
    let login = action_token(
        &env,
        &http,
        serde_json::json!({"action": "login", "user_id": uid}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let mut alice = connect(&env, "alice", login).await;
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: session_jwt,
        })
        .await;
    expect_error(&mut alice, "join with session JWT", "ACTION_TOKEN_REQUIRED").await;
    let join = action_token(
        &env,
        &http,
        serde_json::json!({"action": "join", "user_id": uid, "channel_id": channel_id}),
    )
    .await
    .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    alice
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: join,
        })
        .await;
    alice
        .expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    alice.ws.close(None).await.unwrap();
}

async fn expect_chat(p: &mut Player, what: &str) -> aurix_common::protocol::ChatMessage {
    let m = p
        .expect(what, |m| {
            matches!(m, ControlMessage::ChatMessageReceived { .. })
        })
        .await;
    let ControlMessage::ChatMessageReceived { message } = m else {
        unreachable!()
    };
    message
}

async fn assert_no_chat(p: &mut Player, why: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while let Some(m) = p
        .try_recv(deadline.saturating_duration_since(tokio::time::Instant::now()))
        .await
    {
        assert!(
            !matches!(
                m,
                ControlMessage::ChatMessageReceived { .. }
                    | ControlMessage::ParticipantTyping { .. }
            ),
            "{}: must not receive chat while {why}: {m:?}",
            p.name
        );
    }
}

/// Text chat lite: channel messages reach every member (sender echo carries `client_ref`),
/// directed messages reach one online user, typing indicators are throttled, non-members,
/// blocked pairs, server-muted and flooding senders are refused, REST can inject operator
/// messages and (when `chat.persist` is on) read tenant-scoped history.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn text_chat_channel_direct_typing_and_history() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let other_channel = create_channel(&env, &http).await;
    let persist = chat_persists(&env, &http, channel_id).await;
    let (tok_a, uid_a) = issue_token(&env, &http, "e2e:chat-alice", "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "e2e:chat-bob", "Bob", channel_id).await;
    let (tok_c, uid_c) = issue_token(&env, &http, "e2e:chat-carol", "Carol", channel_id).await;
    let (tok_d, _) = issue_token(&env, &http, "e2e:chat-dave", "Dave", other_channel).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());
    let uid_c = UserId::from_uuid(uid_c.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut carol = connect(&env, "carol", tok_c).await;
    let mut dave = connect(&env, "dave", tok_d).await;
    for p in [&mut alice, &mut bob, &mut carol] {
        join(p, channel_id).await;
    }
    join(&mut dave, other_channel).await;

    // ── channel message: echo with client_ref, members without, non-member nothing ──
    alice
        .send(&ControlMessage::ChatSend {
            channel_id,
            text: "gg wp".into(),
            metadata: Some(serde_json::json!({"kind": "say"})),
            client_ref: Some("ref-1".into()),
        })
        .await;
    let echo = expect_chat(&mut alice, "own echo").await;
    assert_eq!(echo.client_ref.as_deref(), Some("ref-1"));
    assert_eq!(echo.text, "gg wp");
    assert_eq!(echo.from_user_id, uid_a);
    assert_eq!(echo.display_name, "Alice");
    assert_eq!(echo.channel_id, Some(channel_id));
    assert_eq!(echo.metadata, Some(serde_json::json!({"kind": "say"})));
    for p in [&mut bob, &mut carol] {
        let m = expect_chat(p, "alice's channel message").await;
        assert_eq!(m.id, echo.id);
        assert_eq!(m.text, "gg wp");
        assert!(m.client_ref.is_none(), "client_ref is for the sender only");
    }
    assert_no_chat(&mut dave, "not a member").await;

    dave.send(&ControlMessage::ChatSend {
        channel_id,
        text: "let me in".into(),
        metadata: None,
        client_ref: Some("ref-dave".into()),
    })
    .await;
    let m = dave
        .expect("non-member ChatSend error", |m| {
            matches!(m, ControlMessage::Error { .. })
        })
        .await;
    assert!(
        matches!(&m, ControlMessage::Error { code, client_ref, .. }
            if code == "AUTH_DENIED" && client_ref.as_deref() == Some("ref-dave")),
        "rejections carry the client_ref: {m:?}"
    );

    // ── validation ──
    alice
        .send(&ControlMessage::ChatSend {
            channel_id,
            text: "   ".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_error(&mut alice, "empty text", "VALIDATION_ERROR").await;
    alice
        .send(&ControlMessage::ChatSend {
            channel_id,
            text: "x".repeat(4000),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_error(&mut alice, "oversize text", "VALIDATION_ERROR").await;

    // ── directed message: recipient + sender echo only ──
    bob.send(&ControlMessage::ChatSendDirect {
        user_id: uid_a,
        text: "/invite".into(),
        metadata: None,
        client_ref: Some("ref-2".into()),
    })
    .await;
    let echo = expect_chat(&mut bob, "own direct echo").await;
    assert_eq!(echo.client_ref.as_deref(), Some("ref-2"));
    assert_eq!(echo.to_user_id, Some(uid_a));
    assert!(echo.channel_id.is_none());
    let m = expect_chat(&mut alice, "bob's directed message").await;
    assert_eq!(m.id, echo.id);
    assert_eq!(m.from_user_id, uid_b);
    assert!(m.client_ref.is_none());
    assert_no_chat(&mut carol, "not the target").await;

    bob.send(&ControlMessage::ChatSendDirect {
        user_id: UserId::new(),
        text: "hello?".into(),
        metadata: None,
        client_ref: None,
    })
    .await;
    // With offline delivery (chat.persist) a missing user is NOT_FOUND, otherwise every
    // target without a session is USER_OFFLINE.
    expect_error(
        &mut bob,
        "direct to unknown user",
        if persist { "NOT_FOUND" } else { "USER_OFFLINE" },
    )
    .await;
    bob.send(&ControlMessage::ChatSendDirect {
        user_id: uid_b,
        text: "me".into(),
        metadata: None,
        client_ref: None,
    })
    .await;
    expect_error(&mut bob, "direct to self", "VALIDATION_ERROR").await;

    // ── typing: fan-out to other members, throttled per channel, stop always passes ──
    alice
        .send(&ControlMessage::ChatTyping {
            channel_id,
            typing: true,
        })
        .await;
    for p in [&mut bob, &mut carol] {
        let m = p
            .expect("ParticipantTyping", |m| {
                matches!(m, ControlMessage::ParticipantTyping { .. })
            })
            .await;
        assert!(matches!(
            m,
            ControlMessage::ParticipantTyping { channel_id: c, user_id, typing: true }
                if c == channel_id && user_id == uid_a
        ));
    }
    alice
        .send(&ControlMessage::ChatTyping {
            channel_id,
            typing: true,
        })
        .await;
    assert_no_chat(&mut bob, "typing is throttled").await;
    assert_no_chat(&mut alice, "own typing is never echoed").await;
    alice
        .send(&ControlMessage::ChatTyping {
            channel_id,
            typing: false,
        })
        .await;
    let m = bob
        .expect("ParticipantTyping(false)", |m| {
            matches!(m, ControlMessage::ParticipantTyping { .. })
        })
        .await;
    assert!(matches!(
        m,
        ControlMessage::ParticipantTyping { typing: false, .. }
    ));
    dave.send(&ControlMessage::ChatTyping {
        channel_id,
        typing: true,
    })
    .await;
    assert_no_chat(&mut bob, "non-member typing is dropped").await;

    // ── persistent block: no text either way, same as media ──
    alice
        .send(&ControlMessage::SetUserBlock {
            user_id: uid_c,
            blocked: true,
        })
        .await;
    alice
        .expect("UserBlockChanged", |m| {
            matches!(m, ControlMessage::UserBlockChanged { blocked: true, .. })
        })
        .await;
    carol
        .send(&ControlMessage::ChatSend {
            channel_id,
            text: "can you hear me".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_chat(&mut carol, "own echo").await;
    expect_chat(&mut bob, "carol's message").await;
    assert_no_chat(&mut alice, "alice blocked carol").await;
    carol
        .send(&ControlMessage::ChatSendDirect {
            user_id: uid_a,
            text: "psst".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_error(&mut carol, "direct to a user who blocked me", "AUTH_DENIED").await;
    alice
        .send(&ControlMessage::ChatSend {
            channel_id,
            text: "still here".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_chat(&mut alice, "own echo").await;
    expect_chat(&mut bob, "alice's message").await;
    assert_no_chat(&mut carol, "blocked by alice").await;
    alice
        .send(&ControlMessage::SetUserBlock {
            user_id: uid_c,
            blocked: false,
        })
        .await;
    alice
        .expect("UserBlockChanged", |m| {
            matches!(m, ControlMessage::UserBlockChanged { blocked: false, .. })
        })
        .await;

    // ── server mute (moderation) also silences text ──
    http.post(format!("{}/v1/moderation/mute", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"user_id": uid_c, "channel_id": channel_id, "muted": true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    carol
        .expect("MuteStateChanged", |m| {
            matches!(m, ControlMessage::MuteStateChanged { muted: true, .. })
        })
        .await;
    carol
        .send(&ControlMessage::ChatSend {
            channel_id,
            text: "mmmph".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_error(&mut carol, "muted ChatSend", "USER_MUTED").await;
    http.post(format!("{}/v1/moderation/mute", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"user_id": uid_c, "channel_id": channel_id, "muted": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // ── anti-flood: default bucket is 10 burst / 2 per second ──
    for i in 0..20 {
        bob.send(&ControlMessage::ChatSend {
            channel_id,
            text: format!("spam {i}"),
            metadata: None,
            client_ref: None,
        })
        .await;
    }
    let (mut echoes, mut limited) = (0, 0);
    while let Some(m) = bob.try_recv(Duration::from_millis(500)).await {
        match m {
            ControlMessage::ChatMessageReceived { .. } => echoes += 1,
            ControlMessage::Error { code, .. } if code == "RATE_LIMIT_EXCEEDED" => limited += 1,
            ControlMessage::MuteStateChanged { .. } => {}
            other => panic!("bob: unexpected {other:?}"),
        }
    }
    assert!(
        (10..=12).contains(&echoes) && echoes + limited == 20,
        "flood control: {echoes} delivered, {limited} limited"
    );
    // Drain what the others received.
    for p in [&mut alice, &mut carol] {
        while p.try_recv(Duration::from_millis(300)).await.is_some() {}
    }

    // ── operator messages via REST ──
    let sys: serde_json::Value = http
        .post(format!("{}/v1/channels/{}/messages", env.api, channel_id))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"text": "Match starts in 10s", "metadata": {"kind": "announce"}}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sys["display_name"], "Server");
    for p in [&mut alice, &mut bob, &mut carol] {
        let m = expect_chat(p, "system channel message").await;
        assert_eq!(m.id.to_string(), sys["id"].as_str().unwrap());
        assert!(m.from_user_id.0.is_nil());
        assert_eq!(m.text, "Match starts in 10s");
    }
    assert_no_chat(&mut dave, "system message to another channel").await;
    http.post(format!("{}/v1/users/{}/messages", env.api, uid_a))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"text": "You have been invited", "display_name": "Matchmaker"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let m = expect_chat(&mut alice, "system direct message").await;
    assert_eq!(m.display_name, "Matchmaker");
    assert_eq!(m.to_user_id, Some(uid_a));
    assert_no_chat(&mut bob, "directed system message to alice").await;
    let r = http
        .post(format!("{}/v1/users/{}/messages", env.api, UserId::new()))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"text": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // ── history (only when the server runs with chat.persist = true) ──
    if !persist {
        eprintln!("chat.persist is off on this server; skipping history checks");
    } else {
        let body: serde_json::Value = http
            .get(format!(
                "{}/v1/channels/{}/messages?limit=100",
                env.api, channel_id
            ))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        let texts: Vec<&str> = msgs.iter().map(|m| m["text"].as_str().unwrap()).collect();
        assert_eq!(texts[0], "Match starts in 10s", "newest first: {texts:?}");
        assert!(texts.contains(&"gg wp") && texts.contains(&"still here"));
        assert!(
            !texts.contains(&"/invite") && !texts.contains(&"You have been invited"),
            "directed messages are not channel history"
        );
        assert!(msgs.iter().all(|m| m.get("client_ref").is_none()));
        assert!(
            texts.iter().filter(|t| t.starts_with("spam ")).count() <= 12,
            "rate-limited messages must not be stored"
        );

        let body: serde_json::Value = http
            .get(format!("{}/v1/users/{}/messages", env.api, uid_a))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let texts: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["text"].as_str().unwrap())
            .collect();
        assert!(texts.contains(&"/invite") && texts.contains(&"You have been invited"));
        assert!(texts.contains(&"gg wp"), "own channel messages: {texts:?}");
        assert!(
            !texts.contains(&"spam 0"),
            "other people's channel messages are not included"
        );

        if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
            for url in [
                format!("{}/v1/channels/{}/messages", env.api, channel_id),
                format!("{}/v1/users/{}/messages", env.api, uid_a),
            ] {
                let r = http
                    .get(&url)
                    .header("x-api-key", &api_key2)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    r.status(),
                    404,
                    "foreign tenant must not read history: {url}"
                );
                let r = http
                    .post(&url)
                    .header("x-api-key", &api_key2)
                    .json(&serde_json::json!({"text": "hi"}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    r.status(),
                    404,
                    "foreign tenant must not inject messages: {url}"
                );
            }
        } else {
            eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
        }
    }

    for p in [&mut alice, &mut bob, &mut carol, &mut dave] {
        p.ws.close(None).await.unwrap();
    }
}

/// Whether the server stores chat (`chat.persist`): history endpoints answer 404 otherwise.
async fn chat_persists(env: &Env, http: &reqwest::Client, channel_id: ChannelId) -> bool {
    let r = http
        .get(format!("{}/v1/channels/{}/messages", env.api, channel_id))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    r.status() != 404
}

struct HistoryPage {
    messages: Vec<aurix_common::protocol::ChatMessage>,
    next_before: Option<String>,
    next_after: Option<String>,
}

async fn expect_history(p: &mut Player, client_ref: &str) -> HistoryPage {
    let m = p
        .expect("ChatHistoryResult", |m| {
            matches!(m, ControlMessage::ChatHistoryResult { client_ref: r, .. }
                if r.as_deref() == Some(client_ref))
        })
        .await;
    let ControlMessage::ChatHistoryResult {
        messages,
        next_before,
        next_after,
        ..
    } = m
    else {
        unreachable!()
    };
    HistoryPage {
        messages,
        next_before,
        next_after,
    }
}

async fn expect_marker(p: &mut Player, what: &str) -> aurix_common::protocol::ChatReadMarker {
    let m = p
        .expect(what, |m| matches!(m, ControlMessage::ChatReadMarker { .. }))
        .await;
    let ControlMessage::ChatReadMarker { marker } = m else {
        unreachable!()
    };
    marker
}

/// Drains the offline replay after `SessionInitAck`: the queued messages (all `offline`) in
/// arrival order, then `ChatInboxSynced`.
async fn expect_inbox(p: &mut Player) -> (Vec<aurix_common::protocol::ChatMessage>, u32, bool) {
    let mut messages = Vec::new();
    loop {
        match p.recv().await {
            ControlMessage::ChatMessageReceived { message } => {
                assert!(message.offline, "{}: replayed messages are flagged", p.name);
                messages.push(message);
            }
            ControlMessage::ChatInboxSynced {
                delivered,
                truncated,
            } => return (messages, delivered, truncated),
            ControlMessage::NetworkQuality { .. } | ControlMessage::ReceiverPreferences { .. } => {}
            other => panic!("{}: unexpected during inbox replay: {other:?}", p.name),
        }
    }
}

async fn assert_silent(p: &mut Player, why: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while let Some(m) = p
        .try_recv(deadline.saturating_duration_since(tokio::time::Instant::now()))
        .await
    {
        assert!(
            matches!(m, ControlMessage::NetworkQuality { .. }),
            "{}: must stay silent while {why}: {m:?}",
            p.name
        );
    }
}

/// Stored chat (`chat.persist`): keyset pagination over WS and REST that visits every
/// message exactly once in both directions, directed messages queued for an offline user and
/// replayed on every device (across nodes) until read, read markers that only move forward,
/// unread counts, receipts fanned out to the peer / channel, conversation-scoped
/// authorization, tenant isolation and the deletion cascade.
#[tokio::test]
#[ignore = "requires a running Aurix server with chat.persist = true; see the e2e job in .github/workflows/ci.yml"]
async fn chat_history_pagination_offline_delivery_and_read_markers() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    if !chat_persists(&env, &http, channel_id).await {
        eprintln!("chat.persist is off on this server; skipping");
        return;
    }
    // Carol connects to the second node when there is one: offline replay, receipts and
    // markers then cross nodes.
    let two_nodes = std::env::var("AURIX_E2E_WS2").is_ok();
    let env2 = match std::env::var("AURIX_E2E_WS2") {
        Ok(ws2) => Env {
            api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
            ws: ws2,
            api_key: env.api_key.clone(),
        },
        Err(_) => {
            eprintln!("AURIX_E2E_WS2 not set; running on one node");
            Env {
                api: env.api.clone(),
                ws: env.ws.clone(),
                api_key: env.api_key.clone(),
            }
        }
    };
    let other_channel = create_channel(&env, &http).await;
    // Fresh users per run: inboxes and markers are durable and would leak between runs.
    let run = uuid::Uuid::new_v4().simple().to_string();
    let ext = |who: &str| format!("e2e:hist-{who}-{run}");
    let (tok_a, uid_a) = issue_token(&env, &http, &ext("alice"), "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, &ext("bob"), "Bob", channel_id).await;
    let (tok_c, uid_c) = issue_token(&env, &http, &ext("carol"), "Carol", channel_id).await;
    let (tok_d, _) = issue_token(&env, &http, &ext("dave"), "Dave", other_channel).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());
    let uid_c = UserId::from_uuid(uid_c.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut dave = connect(&env, "dave", tok_d).await;
    for p in [&mut alice, &mut bob] {
        let (inbox, delivered, _) = expect_inbox(p).await;
        assert!(
            inbox.is_empty() && delivered == 0,
            "{}: fresh user has no inbox",
            p.name
        );
        join(p, channel_id).await;
    }
    expect_inbox(&mut dave).await;
    join(&mut dave, other_channel).await;

    // ── 9 channel messages: alice ×7 (rate limit is 10 burst), bob ×2 in the middle ──
    let mut ids = Vec::new();
    for i in 0..9 {
        let (sender, other) = if i == 3 || i == 4 {
            (&mut bob, &mut alice)
        } else {
            (&mut alice, &mut bob)
        };
        sender
            .send(&ControlMessage::ChatSend {
                channel_id,
                text: format!("msg {i}"),
                metadata: None,
                client_ref: Some(format!("m{i}")),
            })
            .await;
        let echo = expect_chat(sender, "own echo").await;
        assert_eq!(echo.client_ref.as_deref(), Some(format!("m{i}").as_str()));
        assert!(!echo.offline, "live channel messages are not offline");
        let m = expect_chat(other, "channel message").await;
        assert_eq!(m.id, echo.id);
        ids.push(echo.id);
    }

    // ── WS pagination backwards: 4 + 4 + 1, newest first ──
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: None,
        after: None,
        limit: Some(4),
        client_ref: Some("p1".into()),
    })
    .await;
    let p1 = expect_history(&mut bob, "p1").await;
    let texts: Vec<&str> = p1.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["msg 8", "msg 7", "msg 6", "msg 5"], "newest first");
    assert!(p1.next_before.is_some() && p1.next_after.is_none());
    assert_eq!(
        p1.next_before.as_deref(),
        Some(p1.messages.last().unwrap().cursor().as_str()),
        "next_before is the oldest message's cursor"
    );
    assert!(p1
        .messages
        .iter()
        .all(|m| m.client_ref.is_none() && !m.offline));
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: p1.next_before.clone(),
        after: None,
        limit: Some(4),
        client_ref: Some("p2".into()),
    })
    .await;
    let p2 = expect_history(&mut bob, "p2").await;
    let texts: Vec<&str> = p2.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["msg 4", "msg 3", "msg 2", "msg 1"]);
    assert!(p2.next_before.is_some() && p2.next_after.is_some());
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: p2.next_before.clone(),
        after: None,
        limit: Some(4),
        client_ref: Some("p3".into()),
    })
    .await;
    let p3 = expect_history(&mut bob, "p3").await;
    assert_eq!(p3.messages.len(), 1);
    assert_eq!(p3.messages[0].text, "msg 0");
    assert!(p3.next_before.is_none(), "the oldest message ends the walk");
    assert!(p3.next_after.is_some());
    let mut walked: Vec<uuid::Uuid> = p1
        .messages
        .iter()
        .chain(&p2.messages)
        .chain(&p3.messages)
        .map(|m| m.id)
        .collect();
    let mut expected = ids.clone();
    expected.reverse();
    assert_eq!(walked, expected, "every message exactly once, no gaps");

    // ── forwards from the oldest with `after`: 4 + 4, still newest first inside a page ──
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: None,
        after: Some(p3.messages[0].cursor()),
        limit: Some(4),
        client_ref: Some("f1".into()),
    })
    .await;
    let f1 = expect_history(&mut bob, "f1").await;
    let texts: Vec<&str> = f1.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["msg 4", "msg 3", "msg 2", "msg 1"]);
    assert!(f1.next_after.is_some() && f1.next_before.is_some());
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: None,
        after: f1.next_after.clone(),
        limit: Some(4),
        client_ref: Some("f2".into()),
    })
    .await;
    let f2 = expect_history(&mut bob, "f2").await;
    let texts: Vec<&str> = f2.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["msg 8", "msg 7", "msg 6", "msg 5"]);
    assert!(f2.next_after.is_none(), "caught up with the present");
    walked = f2
        .messages
        .iter()
        .chain(&f1.messages)
        .map(|m| m.id)
        .collect();
    assert_eq!(
        walked,
        expected[..8].to_vec(),
        "forward pages tile the same set"
    );

    // ── validation and authorization of history requests ──
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: Some("not-a-cursor".into()),
        after: None,
        limit: None,
        client_ref: Some("bad".into()),
    })
    .await;
    let m = bob
        .expect("invalid cursor error", |m| {
            matches!(m, ControlMessage::Error { .. })
        })
        .await;
    assert!(
        matches!(&m, ControlMessage::Error { code, client_ref, .. }
            if code == "VALIDATION_ERROR" && client_ref.as_deref() == Some("bad")),
        "{m:?}"
    );
    bob.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: None,
        after: None,
        limit: Some(100_000),
        client_ref: Some("big".into()),
    })
    .await;
    let big = expect_history(&mut bob, "big").await;
    assert_eq!(
        big.messages.len(),
        9,
        "oversize limits are clamped, not refused"
    );
    bob.send(&ControlMessage::ChatHistory {
        channel_id: None,
        user_id: None,
        before: None,
        after: None,
        limit: None,
        client_ref: None,
    })
    .await;
    expect_error(
        &mut bob,
        "history without a conversation",
        "VALIDATION_ERROR",
    )
    .await;
    dave.send(&ControlMessage::ChatHistory {
        channel_id: Some(channel_id),
        user_id: None,
        before: None,
        after: None,
        limit: None,
        client_ref: None,
    })
    .await;
    expect_error(&mut dave, "non-member history", "AUTH_DENIED").await;
    dave.send(&ControlMessage::ChatReadMarkers {
        channel_id: Some(channel_id),
        user_id: None,
    })
    .await;
    expect_error(&mut dave, "non-member read markers", "AUTH_DENIED").await;

    // ── REST pagination agrees with WS ──
    let page = |query: String| {
        let http = http.clone();
        let url = format!("{}/v1/channels/{}/messages?{query}", env.api, channel_id);
        let key = env.api_key.clone();
        async move {
            let v: serde_json::Value = http
                .get(url)
                .header("x-api-key", key)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            v
        }
    };
    let r1 = page("limit=4".into()).await;
    let rest_texts: Vec<&str> = r1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(rest_texts, ["msg 8", "msg 7", "msg 6", "msg 5"]);
    assert_eq!(r1["next_before"].as_str(), p1.next_before.as_deref());
    assert!(r1["next_after"].is_null());
    assert!(
        r1["messages"][0].get("offline").is_none(),
        "false is omitted on the wire"
    );
    let r2 = page(format!(
        "limit=4&before={}",
        r1["next_before"].as_str().unwrap()
    ))
    .await;
    let rest_ids: Vec<uuid::Uuid> = r2["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        rest_ids,
        p2.messages.iter().map(|m| m.id).collect::<Vec<_>>()
    );
    let r = http
        .get(format!(
            "{}/v1/channels/{}/messages?before=%2A%2A",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "invalid cursor over REST");

    // ── channel read markers: bob reads msg 5, alice (a member) gets the receipt ──
    bob.send(&ControlMessage::ChatMarkRead {
        channel_id: Some(channel_id),
        user_id: None,
        message_id: ids[5],
    })
    .await;
    let own = expect_marker(&mut bob, "own channel marker").await;
    assert_eq!(own.user_id, uid_b);
    assert_eq!(own.channel_id, Some(channel_id));
    assert!(own.peer_user_id.is_none());
    assert_eq!(own.message_id, ids[5]);
    let receipt = expect_marker(&mut alice, "bob's receipt").await;
    assert_eq!(receipt.message_id, ids[5]);
    assert_eq!(receipt.user_id, uid_b);
    assert_silent(&mut dave, "receipts stay inside the channel").await;
    alice
        .send(&ControlMessage::ChatReadMarkers {
            channel_id: Some(channel_id),
            user_id: None,
        })
        .await;
    let m = alice
        .expect("ChatReadMarkersResult", |m| {
            matches!(m, ControlMessage::ChatReadMarkersResult { .. })
        })
        .await;
    let ControlMessage::ChatReadMarkersResult {
        markers,
        unread_count,
        channel_id: c,
        ..
    } = m
    else {
        unreachable!()
    };
    assert_eq!(c, Some(channel_id));
    assert_eq!(markers.len(), 1, "only bob has read so far: {markers:?}");
    assert_eq!(markers[0].user_id, uid_b);
    assert_eq!(
        unread_count, 2,
        "alice never marked: bob's two messages are unread"
    );
    // Backwards / repeated marks do not move and produce no event.
    for id in [ids[2], ids[5]] {
        bob.send(&ControlMessage::ChatMarkRead {
            channel_id: Some(channel_id),
            user_id: None,
            message_id: id,
        })
        .await;
    }
    assert_silent(&mut bob, "markers only move forward").await;
    assert_silent(&mut alice, "no receipt for a marker that did not move").await;
    bob.send(&ControlMessage::ChatMarkRead {
        channel_id: Some(channel_id),
        user_id: None,
        message_id: uuid::Uuid::new_v4(),
    })
    .await;
    expect_error(&mut bob, "mark an unknown message", "NOT_FOUND").await;
    bob.send(&ControlMessage::ChatReadMarkers {
        channel_id: Some(channel_id),
        user_id: None,
    })
    .await;
    let m = bob
        .expect("ChatReadMarkersResult", |m| {
            matches!(m, ControlMessage::ChatReadMarkersResult { .. })
        })
        .await;
    assert!(
        matches!(
            m,
            ControlMessage::ChatReadMarkersResult {
                unread_count: 3,
                ..
            }
        ),
        "msg 6..8 from alice are after bob's marker: {m:?}"
    );

    // ── REST markers: dashboards see bob; alice is moved by the operator; tenant scoping ──
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{}/read-markers",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["markers"].as_array().unwrap().len(), 1);
    assert_eq!(v["markers"][0]["user_id"], serde_json::json!(uid_b));
    let v: serde_json::Value = http
        .put(format!("{}/v1/users/{}/read-markers", env.api, uid_a))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "message_id": ids[8]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["moved"], true);
    assert_eq!(v["unread_count"], 0);
    assert_eq!(v["marker"]["message_id"], serde_json::json!(ids[8]));
    let pushed = expect_marker(&mut alice, "operator-set marker reaches alice's device").await;
    assert_eq!(pushed.message_id, ids[8]);
    let receipt = expect_marker(&mut bob, "alice's receipt").await;
    assert_eq!(receipt.user_id, uid_a);
    let v: serde_json::Value = http
        .put(format!("{}/v1/users/{}/read-markers", env.api, uid_a))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "message_id": ids[8]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["moved"], false, "idempotent");
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/users/{}/read-markers?channel_id={}",
            env.api, uid_a, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["marker"]["message_id"], serde_json::json!(ids[8]));
    assert_eq!(v["unread_count"], 0);
    for (body, status) in [
        (
            serde_json::json!({"channel_id": channel_id, "peer_user_id": uid_b, "message_id": ids[8]}),
            400,
        ),
        (serde_json::json!({"message_id": ids[8]}), 400),
        (
            serde_json::json!({"channel_id": other_channel, "message_id": ids[8]}),
            404,
        ),
    ] {
        let r = http
            .put(format!("{}/v1/users/{}/read-markers", env.api, uid_a))
            .header("x-api-key", &env.api_key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), status, "{body}");
    }

    // ── offline delivery: bob writes to carol before she ever connects ──
    let mut queued = Vec::new();
    for i in 0..3 {
        bob.send(&ControlMessage::ChatSendDirect {
            user_id: uid_c,
            text: format!("dm {i}"),
            metadata: None,
            client_ref: Some(format!("dm{i}")),
        })
        .await;
        let echo = expect_chat(&mut bob, "queued echo").await;
        assert_eq!(echo.client_ref.as_deref(), Some(format!("dm{i}").as_str()));
        assert!(
            echo.offline,
            "the echo says the message was queued, not delivered"
        );
        assert_eq!(echo.to_user_id, Some(uid_c));
        queued.push(echo);
    }
    bob.send(&ControlMessage::ChatSendDirect {
        user_id: uid_c,
        text: "x".repeat(4000),
        metadata: None,
        client_ref: None,
    })
    .await;
    expect_error(&mut bob, "oversize offline message", "VALIDATION_ERROR").await;
    assert_silent(&mut alice, "directed messages are private").await;

    // Carol's first device (other node): the backlog replays oldest first, then the sync marker.
    let mut carol = connect(&env2, "carol", tok_c.clone()).await;
    let (inbox, delivered, truncated) = expect_inbox(&mut carol).await;
    assert_eq!(
        inbox.iter().map(|m| m.id).collect::<Vec<_>>(),
        queued.iter().map(|m| m.id).collect::<Vec<_>>(),
        "oldest first, exactly the accepted ones"
    );
    assert!(inbox
        .iter()
        .all(|m| m.client_ref.is_none() && m.from_user_id == uid_b));
    assert_eq!((delivered, truncated), (3, false));
    // A second device replays the same unread messages (the client dedupes by id). Aurix keeps
    // one live session per user and node, so on a single node this replaces the first device.
    let mut carol2 = connect(&env, "carol2", tok_c.clone()).await;
    let (inbox2, delivered2, _) = expect_inbox(&mut carol2).await;
    assert_eq!(delivered2, 3);
    assert_eq!(
        inbox2.iter().map(|m| m.id).collect::<Vec<_>>(),
        inbox.iter().map(|m| m.id).collect::<Vec<_>>()
    );
    let mut second = two_nodes.then_some(carol);

    // Carol reads dm 1 on one device: every device and bob learn about it.
    carol2
        .send(&ControlMessage::ChatMarkRead {
            channel_id: None,
            user_id: Some(uid_b),
            message_id: queued[1].id,
        })
        .await;
    for p in [&mut carol2, &mut bob].into_iter().chain(second.iter_mut()) {
        let m = expect_marker(p, "carol's direct marker").await;
        assert_eq!(m.user_id, uid_c);
        assert_eq!(m.peer_user_id, Some(uid_b));
        assert!(m.channel_id.is_none());
        assert_eq!(m.message_id, queued[1].id);
    }
    assert_silent(&mut alice, "direct receipts go to the peer only").await;
    // A reconnect (replacing that node's session) replays only what is still unread.
    let mut carol = connect(&env2, "carol3", tok_c.clone()).await;
    let (inbox3, delivered3, _) = expect_inbox(&mut carol).await;
    assert_eq!(delivered3, 1);
    assert_eq!(inbox3[0].id, queued[2].id);
    second = two_nodes.then_some(carol2);

    carol
        .send(&ControlMessage::ChatReadMarkers {
            channel_id: None,
            user_id: Some(uid_b),
        })
        .await;
    let m = carol
        .expect("ChatReadMarkersResult", |m| {
            matches!(m, ControlMessage::ChatReadMarkersResult { .. })
        })
        .await;
    let ControlMessage::ChatReadMarkersResult {
        markers,
        unread_count,
        user_id: peer,
        ..
    } = m
    else {
        unreachable!()
    };
    assert_eq!(peer, Some(uid_b));
    assert_eq!(unread_count, 1);
    assert_eq!(
        markers.len(),
        1,
        "bob has not read anything from carol: {markers:?}"
    );
    assert_eq!(markers[0].user_id, uid_c);
    // Wrong conversation for that message: a channel message is not part of the DM thread.
    carol
        .send(&ControlMessage::ChatMarkRead {
            channel_id: None,
            user_id: Some(uid_b),
            message_id: ids[0],
        })
        .await;
    expect_error(&mut carol, "channel message in a DM thread", "NOT_FOUND").await;
    // Dave may not mark somebody else's conversation with that message either.
    dave.send(&ControlMessage::ChatMarkRead {
        channel_id: None,
        user_id: Some(uid_b),
        message_id: queued[0].id,
    })
    .await;
    expect_error(&mut dave, "foreign DM", "NOT_FOUND").await;
    carol
        .send(&ControlMessage::ChatMarkRead {
            channel_id: None,
            user_id: Some(uid_c),
            message_id: queued[0].id,
        })
        .await;
    expect_error(&mut carol, "DM thread with myself", "VALIDATION_ERROR").await;

    // Now that carol is online, bob's messages are live on both devices and not flagged.
    bob.send(&ControlMessage::ChatSendDirect {
        user_id: uid_c,
        text: "dm live".into(),
        metadata: None,
        client_ref: None,
    })
    .await;
    let live = expect_chat(&mut bob, "live echo").await;
    assert!(!live.offline);
    for p in [&mut carol].into_iter().chain(second.iter_mut()) {
        let m = expect_chat(p, "live DM").await;
        assert_eq!(m.id, live.id);
        assert!(!m.offline);
    }
    // Carol answers; bob's unread count in that thread becomes 1.
    carol
        .send(&ControlMessage::ChatSendDirect {
            user_id: uid_b,
            text: "dm reply".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    expect_chat(&mut carol, "own echo").await;
    if let Some(p) = second.as_mut() {
        expect_chat(p, "echo on the other device").await;
    }
    expect_chat(&mut bob, "carol's reply").await;

    // Direct history from either side, paged; the replayed ones keep their offline flag.
    carol
        .send(&ControlMessage::ChatHistory {
            channel_id: None,
            user_id: Some(uid_b),
            before: None,
            after: None,
            limit: Some(3),
            client_ref: Some("d1".into()),
        })
        .await;
    let d1 = expect_history(&mut carol, "d1").await;
    let texts: Vec<&str> = d1.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["dm reply", "dm live", "dm 2"]);
    assert_eq!(d1.messages[0].from_user_id, uid_c);
    assert!(!d1.messages[1].offline && d1.messages[2].offline);
    bob.send(&ControlMessage::ChatHistory {
        channel_id: None,
        user_id: Some(uid_c),
        before: d1.next_before.clone(),
        after: None,
        limit: Some(3),
        client_ref: Some("d2".into()),
    })
    .await;
    let d2 = expect_history(&mut bob, "d2").await;
    let texts: Vec<&str> = d2.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["dm 1", "dm 0"], "the same thread from bob's side");
    assert!(d2.next_before.is_none());
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/users/{}/messages?peer={}&limit=2",
            env.api, uid_c, uid_b
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let rest: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(rest, ["dm reply", "dm live"]);
    assert_eq!(v["messages"][1]["offline"], serde_json::Value::Null);
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/users/{}/messages?peer={}&before={}",
            env.api,
            uid_c,
            uid_b,
            v["next_before"].as_str().unwrap()
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let rest: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap())
        .collect();
    assert_eq!(rest, ["dm 2", "dm 1", "dm 0"]);
    assert_eq!(v["messages"][0]["offline"], true);
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/users/{}/read-markers?peer_user_id={}",
            env.api, uid_b, uid_c
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["marker"].is_null(), "bob never read carol's thread");
    assert_eq!(v["unread_count"], 1);
    let v: serde_json::Value = http
        .get(format!("{}/v1/users/{}/read-markers", env.api, uid_c))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["markers"].as_array().unwrap().len(), 1);
    assert_eq!(v["markers"][0]["peer_user_id"], serde_json::json!(uid_b));

    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        for url in [
            format!("{}/v1/channels/{}/read-markers", env.api, channel_id),
            format!("{}/v1/users/{}/read-markers", env.api, uid_c),
            format!("{}/v1/users/{}/messages?peer={}", env.api, uid_c, uid_b),
        ] {
            let r = http
                .get(&url)
                .header("x-api-key", &api_key2)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "foreign tenant: {url}");
        }
        let r = http
            .put(format!("{}/v1/users/{}/read-markers", env.api, uid_a))
            .header("x-api-key", &api_key2)
            .json(&serde_json::json!({"channel_id": channel_id, "message_id": ids[8]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "foreign tenant must not move markers");
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
    }

    // ── erasing carol removes her thread and markers; bob's channel history stays ──
    http.delete(format!("{}/v1/users/{}", env.api, uid_c))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/users/{}/messages?peer={}",
            env.api, uid_b, uid_c
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["messages"].as_array().unwrap().is_empty(), "{v}");
    let r = http
        .get(format!("{}/v1/users/{}/read-markers", env.api, uid_c))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let v: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{}/read-markers",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["markers"].as_array().unwrap().len(),
        2,
        "alice + bob: {v}"
    );

    // Carol's sockets were closed by the erasure.
    for p in [&mut alice, &mut bob, &mut dave] {
        p.ws.close(None).await.unwrap();
    }
}

/// Transmission policy and channel focus over the control plane: a `Single` policy drops the
/// sender's audio in every other joined channel and `None` everywhere, focus attenuates the
/// listener's other channels by the configured gain, both reset when their channel is left,
/// survive a session resume via `ReceiverPreferences`, and the per-session channel limit is
/// enforced at join with `CHANNEL_LIMIT_EXCEEDED`.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn transmission_mode_channel_focus_and_channel_limit() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let team = create_channel(&env, &http).await;
    let party = create_channel(&env, &http).await;
    let mut spare = Vec::new();
    for _ in 0..12 {
        spare.push(create_channel(&env, &http).await);
    }
    let mut all = vec![team, party];
    all.extend(spare.iter().copied());
    let (tok_a, _) = issue_token_for(&env, &http, "e2e:tx-alice", "Alice", &all).await;
    let (tok_b, _) = issue_token_for(&env, &http, "e2e:tx-bob", "Bob", &all).await;

    let mut alice = connect(&env, "alice", tok_a.clone()).await;
    let mut bob = connect(&env, "bob", tok_b.clone()).await;
    for p in [&mut alice, &mut bob] {
        let prefs = p
            .expect("ReceiverPreferences", |m| {
                matches!(m, ControlMessage::ReceiverPreferences { .. })
            })
            .await;
        if let ControlMessage::ReceiverPreferences {
            transmission,
            focus_channel,
            ..
        } = prefs
        {
            assert_eq!(transmission, TransmissionMode::All);
            assert_eq!(focus_channel, None);
        }
        bind_media(p).await;
        join(p, team).await;
        join(p, party).await;
    }
    let hello = Bytes::from_static(b"hello");

    // Baseline: `All` — both channels reach Bob at full gain.
    send_audio(&alice, team, 1, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await, (None, 10));
    send_audio(&alice, party, 100, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await, (None, 10));

    // `Single(team)`: party audio is dropped at the server, team still flows.
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::Single { channel_id: team },
        })
        .await;
    alice
        .expect("TransmissionChanged", |m| {
            matches!(
                m,
                ControlMessage::TransmissionChanged {
                    mode: TransmissionMode::Single { channel_id }
                } if *channel_id == team
            )
        })
        .await;
    send_audio(&alice, party, 200, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 0);
    send_audio(&alice, team, 300, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 10);

    // Targets must be joined channels.
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::Single {
                channel_id: ChannelId::new(),
            },
        })
        .await;
    expect_error(&mut alice, "SetTransmission(unknown)", "NOT_IN_CHANNEL").await;
    bob.send(&ControlMessage::SetChannelFocus {
        channel_id: Some(ChannelId::new()),
    })
    .await;
    expect_error(&mut bob, "SetChannelFocus(unknown)", "NOT_IN_CHANNEL").await;

    // `None`: nothing leaves the session.
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::None,
        })
        .await;
    alice
        .expect("TransmissionChanged(None)", |m| {
            matches!(
                m,
                ControlMessage::TransmissionChanged {
                    mode: TransmissionMode::None
                }
            )
        })
        .await;
    send_audio(&alice, team, 400, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await.1, 0);
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::All,
        })
        .await;
    alice
        .expect("TransmissionChanged(All)", |m| {
            matches!(
                m,
                ControlMessage::TransmissionChanged {
                    mode: TransmissionMode::All
                }
            )
        })
        .await;

    // Bob focuses team: party arrives attenuated (default unfocused gain 0.5), team at full.
    bob.send(&ControlMessage::SetChannelFocus {
        channel_id: Some(team),
    })
    .await;
    bob.expect(
        "ChannelFocusChanged",
        |m| matches!(m, ControlMessage::ChannelFocusChanged { channel_id: Some(c) } if *c == team),
    )
    .await;
    send_audio(&alice, party, 500, &hello).await;
    let (gain, n) = audio_from(&bob, alice.ssrc, &hello).await;
    assert_eq!(n, 10);
    assert!(
        gain.is_some() && gain != Some(encode_volume_byte(1.0)),
        "unfocused channel must be attenuated, got {gain:?}"
    );
    send_audio(&alice, team, 600, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await, (None, 10));

    // Focus and a `Single` target survive a session resume through ReceiverPreferences.
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::Single { channel_id: team },
        })
        .await;
    alice
        .expect("TransmissionChanged", |m| {
            matches!(m, ControlMessage::TransmissionChanged { .. })
        })
        .await;
    let sid_a = alice.session_id;
    let Player {
        resume_token: tok_resume,
        ws: dead_ws,
        ..
    } = alice;
    drop(dead_ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut alice = connect_with(&env, "alice", tok_a.clone(), Some((sid_a, &tok_resume))).await;
    assert!(alice.resumed);
    let prefs = alice
        .expect("ReceiverPreferences (resumed)", |m| {
            matches!(m, ControlMessage::ReceiverPreferences { .. })
        })
        .await;
    let ControlMessage::ReceiverPreferences { transmission, .. } = prefs else {
        unreachable!()
    };
    assert_eq!(transmission, TransmissionMode::Single { channel_id: team });
    bind_media(&mut alice).await;

    // Leaving the targeted / focused channel resets the state and tells the client.
    alice
        .send(&ControlMessage::ChannelLeave { channel_id: team })
        .await;
    alice
        .expect("TransmissionChanged(None) after leave", |m| {
            matches!(
                m,
                ControlMessage::TransmissionChanged {
                    mode: TransmissionMode::None
                }
            )
        })
        .await;
    bob.send(&ControlMessage::ChannelLeave { channel_id: team })
        .await;
    bob.expect("ChannelFocusChanged(None) after leave", |m| {
        matches!(m, ControlMessage::ChannelFocusChanged { channel_id: None })
    })
    .await;
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::All,
        })
        .await;
    alice
        .expect("TransmissionChanged(All)", |m| {
            matches!(
                m,
                ControlMessage::TransmissionChanged {
                    mode: TransmissionMode::All
                }
            )
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_audio(&alice, party, 700, &hello).await;
    assert_eq!(audio_from(&bob, alice.ssrc, &hello).await, (None, 10));

    // Per-session channel limit (media.max_channels_per_session): the server is expected to
    // run with a limit small enough to hit here (CI sets 4).
    match std::env::var("AURIX_E2E_MAX_CHANNELS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(limit) => {
            // Alice is in `party` only; fill up to the limit, then one more must be refused.
            let mut joined = 1;
            let mut it = spare.iter();
            while joined < limit {
                join(&mut alice, *it.next().expect("not enough spare channels")).await;
                joined += 1;
            }
            let extra = *it.next().expect("not enough spare channels");
            alice
                .send(&ControlMessage::ChannelJoin {
                    channel_id: extra,
                    token: tok_a.clone(),
                })
                .await;
            expect_error(
                &mut alice,
                "ChannelJoin over limit",
                "CHANNEL_LIMIT_EXCEEDED",
            )
            .await;
            // Leaving frees the slot again.
            alice
                .send(&ControlMessage::ChannelLeave { channel_id: party })
                .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            join(&mut alice, extra).await;
        }
        None => eprintln!("AURIX_E2E_MAX_CHANNELS not set; skipping channel-limit checks"),
    }

    let _ = alice.ws.close(None).await;
    let _ = bob.ws.close(None).await;
}

async fn create_channel_with(
    env: &Env,
    http: &reqwest::Client,
    config: serde_json::Value,
) -> ChannelId {
    let ch: serde_json::Value = http
        .post(format!("{}/v1/channels", env.api))
        .header("x-api-key", &env.api_key)
        .json(
            &serde_json::json!({"name": format!("e2e-{}", uuid::Uuid::now_v7()), "config": config}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    ChannelId::from_uuid(ch["id"].as_str().unwrap().parse().unwrap())
}

/// Publishes `who`'s pose and waits until `peer` saw the broadcast, so the SFU has it.
async fn move_to(
    who: &mut Player,
    peer: &mut Player,
    channel_id: ChannelId,
    user_id: UserId,
    position: Position3D,
    orientation: Orientation3D,
) {
    who.send(&ControlMessage::PositionUpdate {
        channel_id,
        positions: vec![UserPosition {
            user_id,
            position,
            orientation,
        }],
    })
    .await;
    peer.expect("PositionUpdate", |m| {
        matches!(m, ControlMessage::PositionUpdate { positions, .. }
            if positions.iter().any(|p| p.user_id == user_id))
    })
    .await;
}

/// Collects the downlink metadata of Alice's frames at `to`: `(volume, direction)` of the first
/// frame plus the count.
async fn directional_audio_from(
    to: &Player,
    ssrc: u32,
    payload: &Bytes,
) -> (Option<(f32, Option<Direction>)>, usize) {
    let mut got = 0;
    let mut meta = None;
    let mut buf = vec![0u8; 2048];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(400), to.udp.recv_from(&mut buf)).await
    {
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: downlink must verify", to.name);
        if p.header.packet_type != PacketType::Audio || p.header.ssrc != ssrc {
            continue;
        }
        let m = p.take_downlink_meta();
        assert_eq!(&p.payload[..], &payload[..]);
        meta.get_or_insert(m);
        got += 1;
    }
    (meta, got)
}

fn facing(fx: f32, fy: f32, fz: f32) -> Orientation3D {
    Orientation3D {
        forward_x: fx,
        forward_y: fy,
        forward_z: fz,
        up_x: 0.0,
        up_y: 1.0,
        up_z: 0.0,
    }
}

fn at(x: f32, y: f32, z: f32) -> Position3D {
    Position3D { x, y, z }
}

/// Directional positional channel over native AURX: the server prefixes each downlink frame
/// with the source's azimuth/elevation *relative to the listener's orientation* (and the
/// distance gain when attenuated); turning the listener rotates the sound, distance and a
/// receiver-local volume still combine into the gain byte, and nothing is delivered before
/// both poses are known or beyond `max_radius`.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn directional_positional_audio_follows_listener_orientation() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let arena = create_channel_with(
        &env,
        &http,
        serde_json::json!({
            "channel_type": "positional",
            "positional_config": {
                "near_distance": 2.0, "far_distance": 22.0, "rolloff": "linear",
                "max_radius": 30.0, "directional": true, "coordinate_system": "left_handed"
            }
        }),
    )
    .await;
    let (tok_a, uid_a) = issue_token(&env, &http, "e2e:dir-alice", "Alice", arena).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "e2e:dir-bob", "Bob", arena).await;
    let uid_a: UserId = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b: UserId = UserId::from_uuid(uid_b.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    for p in [&mut alice, &mut bob] {
        bind_media(p).await;
        join(p, arena).await;
    }
    let hello = Bytes::from_static(b"hello");

    // No poses yet: positional routing has nothing to place, nothing is forwarded.
    send_audio(&alice, arena, 1, &hello).await;
    assert_eq!(directional_audio_from(&bob, alice.ssrc, &hello).await.1, 0);

    // Bob at the origin facing +Z (Unity forward), Alice 1 m to his right: azimuth +π/2, unity gain.
    move_to(
        &mut bob,
        &mut alice,
        arena,
        uid_b,
        at(0.0, 0.0, 0.0),
        facing(0.0, 0.0, 1.0),
    )
    .await;
    move_to(
        &mut alice,
        &mut bob,
        arena,
        uid_a,
        at(1.0, 0.0, 0.0),
        facing(0.0, 0.0, 1.0),
    )
    .await;
    send_audio(&alice, arena, 100, &hello).await;
    let (meta, n) = directional_audio_from(&bob, alice.ssrc, &hello).await;
    assert_eq!(n, 10);
    let (volume, direction) = meta.unwrap();
    assert_eq!(volume, 1.0, "within near_distance: no gain byte");
    let d = direction.expect("directional channel must carry a direction");
    assert!(
        (d.azimuth - std::f32::consts::FRAC_PI_2).abs() < 0.03,
        "{d:?}"
    );
    assert!(d.elevation.abs() < 0.03, "{d:?}");

    // Bob turns to face +X: Alice is now straight ahead. The sender did not move.
    move_to(
        &mut bob,
        &mut alice,
        arena,
        uid_b,
        at(0.0, 0.0, 0.0),
        facing(1.0, 0.0, 0.0),
    )
    .await;
    send_audio(&alice, arena, 200, &hello).await;
    let (meta, n) = directional_audio_from(&bob, alice.ssrc, &hello).await;
    assert_eq!(n, 10);
    let d = meta.unwrap().1.unwrap();
    assert!(d.azimuth.abs() < 0.03, "{d:?}");

    // Bob turns back and Alice walks 12 m behind-left: azimuth ≈ -3π/4, linear gain 0.5, and a
    // receiver-local volume of 0.5 multiplies in.
    move_to(
        &mut bob,
        &mut alice,
        arena,
        uid_b,
        at(0.0, 0.0, 0.0),
        facing(0.0, 0.0, 1.0),
    )
    .await;
    let back_left = 12.0 / std::f32::consts::SQRT_2;
    move_to(
        &mut alice,
        &mut bob,
        arena,
        uid_a,
        at(-back_left, 0.0, -back_left),
        facing(0.0, 0.0, 1.0),
    )
    .await;
    bob.send(&ControlMessage::SetParticipantVolume {
        user_id: uid_a,
        volume: 0.5,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send_audio(&alice, arena, 300, &hello).await;
    let (meta, n) = directional_audio_from(&bob, alice.ssrc, &hello).await;
    assert_eq!(n, 10);
    let (volume, direction) = meta.unwrap();
    let d = direction.unwrap();
    assert!(
        (d.azimuth + 3.0 * std::f32::consts::FRAC_PI_4).abs() < 0.03,
        "{d:?}"
    );
    assert!(
        (volume - 0.25).abs() < 0.02,
        "distance 0.5 × local 0.5, got {volume}"
    );

    // Alice hears Bob too, mirrored: Bob is ahead-right of her, at the same distance gain.
    send_audio(&bob, arena, 400, &hello).await;
    let (meta, n) = directional_audio_from(&alice, bob.ssrc, &hello).await;
    assert_eq!(n, 10);
    let (volume, direction) = meta.unwrap();
    let d = direction.unwrap();
    assert!(
        (d.azimuth - std::f32::consts::FRAC_PI_4).abs() < 0.03,
        "{d:?}"
    );
    assert!((volume - 0.5).abs() < 0.02, "distance only, got {volume}");

    // Beyond max_radius nothing is forwarded at all.
    move_to(
        &mut alice,
        &mut bob,
        arena,
        uid_a,
        at(0.0, 0.0, 40.0),
        facing(0.0, 0.0, 1.0),
    )
    .await;
    send_audio(&alice, arena, 500, &hello).await;
    assert_eq!(directional_audio_from(&bob, alice.ssrc, &hello).await.1, 0);

    for p in [&mut alice, &mut bob] {
        p.send(&ControlMessage::ChannelLeave { channel_id: arena })
            .await;
    }
}

/// Echo channel over the real stack: Alice's frames come back only to Alice (her own SSRC,
/// re-sealed with her key, no metadata), Bob in the same echo channel receives nothing — also
/// when he sits on another node (`AURIX_E2E_WS2`), because echo channels are never relayed
/// through the cascade.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn echo_channel_loops_audio_back_to_the_sender_only() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let bob_env = env2.as_ref().unwrap_or(&env);
    let http = reqwest::Client::new();
    let echo = create_channel_with(&env, &http, serde_json::json!({"channel_type": "echo"})).await;

    let (tok_a, _) = issue_token(&env, &http, "echo:alice", "Alice", echo).await;
    let (tok_b, _) = issue_token(bob_env, &http, "echo:bob", "Bob", echo).await;
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(bob_env, "bob", tok_b).await;
    if env2.is_some() {
        assert_ne!(
            alice.media_addr.port(),
            bob.media_addr.port(),
            "players must land on different nodes"
        );
    }
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;
    join(&mut alice, echo).await;
    join(&mut bob, echo).await;
    // Presence is still shared (the roster is real), only the audio is private.
    alice
        .expect("ParticipantJoined(Bob)", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Bob")
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let hello = Bytes::from_static(&[0xFC, 0xEC, 0x40, 1, 2, 3, 4, 5, 6, 7]);
    send_audio(&alice, echo, 1, &hello).await;
    let (gain, n) = audio_from(&alice, alice.ssrc, &hello).await;
    assert!(n >= 8, "Alice got only {n} of her 10 frames back");
    assert_eq!(gain, None, "echo carries no volume metadata");
    let (_, n) = audio_from(&bob, alice.ssrc, &hello).await;
    assert_eq!(n, 0, "Bob must not hear Alice's echo");

    // Symmetric: Bob hears himself, Alice hears nothing of him.
    let world = Bytes::from_static(&[0xFC, 0xEC, 0x40, 9, 9, 9]);
    send_audio(&bob, echo, 100, &world).await;
    assert!(audio_from(&bob, bob.ssrc, &world).await.1 >= 8);
    assert_eq!(audio_from(&alice, bob.ssrc, &world).await.1, 0);

    for p in [&mut alice, &mut bob] {
        p.send(&ControlMessage::ChannelLeave { channel_id: echo })
            .await;
    }
}

// ── PCMU fallback ──

/// Audio from `ssrc` collected at `to`, decoded per the packet codec (Opus at 48 kHz, μ-law at
/// 8 kHz); returns (frames, PCMU frames, rms, first volume byte).
async fn levels_from(to: &Player, ssrc: u32) -> (usize, usize, f32, Option<u8>) {
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut pcm48 = vec![0i16; 960];
    let mut sum = 0.0f64;
    let mut count = 0usize;
    let (mut frames, mut pcmu) = (0, 0);
    let mut gain = None;
    let mut buf = vec![0u8; 2048];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(400), to.udp.recv_from(&mut buf)).await
    {
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: downlink must verify", to.name);
        if p.header.packet_type != PacketType::Audio || p.header.ssrc != ssrc {
            continue;
        }
        let body = if p.header.has_flag(PacketFlags::VolumeAttenuated) {
            gain.get_or_insert(p.payload[0]);
            &p.payload[1..]
        } else {
            &p.payload[..]
        };
        frames += 1;
        if p.header.has_flag(PacketFlags::Pcmu) {
            pcmu += 1;
            assert_eq!(body.len(), aurix_common::g711::PCMU_FRAME_SAMPLES);
            let mut pcm8 = Vec::new();
            aurix_common::g711::decode(body, &mut pcm8);
            sum += pcm8
                .iter()
                .map(|&s| (s as f64 / 32768.0).powi(2))
                .sum::<f64>();
            count += pcm8.len();
        } else {
            let n = dec.decode(body, &mut pcm48, false).expect("opus downlink");
            sum += pcm48[..n]
                .iter()
                .map(|&s| (s as f64 / 32768.0).powi(2))
                .sum::<f64>();
            count += n;
        }
    }
    let rms = if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt() as f32
    };
    (frames, pcmu, rms, gain)
}

/// 20 ms μ-law frames of a mono sine at 8 kHz (0.35 amplitude, like `opus_tone`).
fn ulaw_tone(hz: f32, ms: u32) -> Vec<Bytes> {
    use aurix_common::g711::{PCMU_FRAME_SAMPLES, PCMU_SAMPLE_RATE};
    (0..ms / 20)
        .map(|f| {
            let pcm: Vec<i16> = (0..PCMU_FRAME_SAMPLES)
                .map(|i| {
                    let t = (f as usize * PCMU_FRAME_SAMPLES + i) as f32 / PCMU_SAMPLE_RATE as f32;
                    ((t * hz * std::f32::consts::TAU).sin() * 0.35 * 32767.0) as i16
                })
                .collect();
            let mut out = Vec::new();
            aurix_common::g711::encode(&pcm, &mut out);
            Bytes::from(out)
        })
        .collect()
}

async fn stream_pcmu(
    from: &Player,
    channel_id: ChannelId,
    first_seq: u32,
    frames: &[Bytes],
    e2ee: bool,
) {
    let hash = channel_id_hash(&channel_id);
    for (i, payload) in frames.iter().enumerate() {
        let seq = first_seq + i as u32;
        let mut pkt = AurixPacket::audio(seq, seq * 960, from.ssrc, hash, payload.clone());
        pkt.header.set_flag(PacketFlags::Pcmu);
        if e2ee {
            pkt.header.set_flag(PacketFlags::E2ee);
        }
        from.udp
            .send_to(&pkt.seal(&from.keys), from.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A native session negotiates PCMU over the control plane and from then on speaks and hears
/// G.711 while the channel (and every other participant, on this node or a cascaded one)
/// stays Opus: the server transcodes at the edge, keeps per-receiver volume metadata, refuses
/// μ-law from sessions that did not negotiate it, and the preference survives a resume.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn pcmu_fallback_is_negotiated_per_session_and_transcoded() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let bob_env = env2.as_ref().unwrap_or(&env);
    let http = reqwest::Client::new();
    let team = create_channel(&env, &http).await;
    let (tok_a, _) = issue_token(&env, &http, "pcmu:alice", "Alice", team).await;
    let (tok_b, uid_b) = issue_token(bob_env, &http, "pcmu:bob", "Bob", team).await;
    let bob_uid = UserId::from_uuid(uid_b.parse().unwrap());
    let mut alice = connect(&env, "alice", tok_a.clone()).await;
    let mut bob = connect(bob_env, "bob", tok_b).await;
    for p in [&mut alice, &mut bob] {
        let prefs = p
            .expect("ReceiverPreferences", |m| {
                matches!(m, ControlMessage::ReceiverPreferences { .. })
            })
            .await;
        if let ControlMessage::ReceiverPreferences { codec, .. } = prefs {
            assert_eq!(codec, AudioCodec::Opus);
        }
        bind_media(p).await;
        join(p, team).await;
    }
    alice
        .expect("ParticipantJoined(Bob)", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Bob")
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // μ-law before negotiating is dropped at the server.
    let ulaw = ulaw_tone(440.0, 200);
    stream_pcmu(&alice, team, 1, &ulaw, false).await;
    assert_eq!(
        levels_from(&bob, alice.ssrc).await.0,
        0,
        "unnegotiated PCMU"
    );

    alice
        .send(&ControlMessage::SetAudioCodec {
            codec: AudioCodec::Pcmu,
        })
        .await;
    alice
        .expect("AudioCodecChanged(pcmu)", |m| {
            matches!(
                m,
                ControlMessage::AudioCodecChanged {
                    codec: AudioCodec::Pcmu
                }
            )
        })
        .await;
    bob.send(&ControlMessage::SetAudioCodec {
        codec: AudioCodec::Pcmu,
    })
    .await;
    bob.expect("AudioCodecChanged(pcmu)", |m| {
        matches!(
            m,
            ControlMessage::AudioCodecChanged {
                codec: AudioCodec::Pcmu
            }
        )
    })
    .await;
    bob.send(&ControlMessage::SetAudioCodec {
        codec: AudioCodec::Opus,
    })
    .await;
    bob.expect("AudioCodecChanged(opus)", |m| {
        matches!(
            m,
            ControlMessage::AudioCodecChanged {
                codec: AudioCodec::Opus
            }
        )
    })
    .await;

    // Alice (PCMU) → Bob (Opus): transcoded uplink, the tone survives at level.
    stream_pcmu(&alice, team, 100, &ulaw, false).await;
    let (frames, pcmu, rms, _) = levels_from(&bob, alice.ssrc).await;
    assert!(frames >= 8, "Bob got {frames} frames");
    assert_eq!(pcmu, 0, "Opus receiver never sees μ-law");
    assert!((rms - 0.247).abs() < 0.06, "Bob hears rms {rms}");

    // Bob (Opus) → Alice (PCMU): μ-law downlink with Alice's per-participant gain.
    alice
        .send(&ControlMessage::SetParticipantVolume {
            user_id: bob_uid,
            volume: 0.5,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let opus = opus_tone(440.0, 200);
    stream_frames(&bob, team, 100, &opus, false).await;
    let (frames, pcmu, rms, gain) = levels_from(&alice, bob.ssrc).await;
    assert!(frames >= 8, "Alice got {frames} frames");
    assert_eq!(pcmu, frames, "PCMU receiver only sees μ-law");
    assert!((rms - 0.247).abs() < 0.06, "Alice hears rms {rms}");
    assert_eq!(
        gain,
        Some(encode_volume_byte(0.5)),
        "gain rides in the volume byte"
    );

    // μ-law cannot be end-to-end encrypted (the server could not transcode it): dropped.
    stream_pcmu(&alice, team, 200, &ulaw, true).await;
    assert_eq!(levels_from(&bob, alice.ssrc).await.0, 0, "e2ee PCMU");

    // The negotiated codec survives a resume of the same session.
    let Player {
        session_id: sid_a,
        resume_token,
        ws: dead_ws,
        ..
    } = alice;
    drop(dead_ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut alice = connect_with(&env, "alice", tok_a, Some((sid_a, &resume_token))).await;
    assert!(alice.resumed, "expected a resumed session");
    alice
        .expect("ReceiverPreferences(pcmu)", |m| {
            matches!(
                m,
                ControlMessage::ReceiverPreferences {
                    codec: AudioCodec::Pcmu,
                    ..
                }
            )
        })
        .await;
    bind_media(&mut alice).await;
    stream_frames(&bob, team, 300, &opus, false).await;
    let (frames, pcmu, _, _) = levels_from(&alice, bob.ssrc).await;
    assert!(
        frames >= 8 && pcmu == frames,
        "after resume: {frames}/{pcmu}"
    );

    // Back to Opus: the same downlink arrives as Opus again.
    alice
        .send(&ControlMessage::SetAudioCodec {
            codec: AudioCodec::Opus,
        })
        .await;
    alice
        .expect("AudioCodecChanged(opus)", |m| {
            matches!(
                m,
                ControlMessage::AudioCodecChanged {
                    codec: AudioCodec::Opus
                }
            )
        })
        .await;
    stream_frames(&bob, team, 400, &opus, false).await;
    let (frames, pcmu, _, _) = levels_from(&alice, bob.ssrc).await;
    assert!(frames >= 8 && pcmu == 0, "after opus: {frames}/{pcmu}");

    for p in [&mut alice, &mut bob] {
        p.send(&ControlMessage::ChannelLeave { channel_id: team })
            .await;
    }
}

// ── Webhooks + SSE ──

struct Hook {
    delivery_id: String,
    event: String,
    webhook_id: String,
    attempt: u32,
    signature: String,
    body: Vec<u8>,
}

/// Minimal webhook receiver: records every POST and answers 500 to the first delivery of
/// `fail_once` (an event type), 200 to everything else.
async fn start_receiver(fail_once: Option<&'static str>) -> (String, HookRx) {
    use axum::extract::State as AxState;
    use axum::http::{HeaderMap, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Hook>();
    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(
            move |AxState((tx, hits)): AxState<(
                tokio::sync::mpsc::UnboundedSender<Hook>,
                Arc<AtomicUsize>,
            )>,
                  headers: HeaderMap,
                  body: axum::body::Bytes| async move {
                let h = |n: &str| {
                    headers
                        .get(n)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                };
                let event = h("x-aurix-event");
                let fail = fail_once == Some(event.as_str())
                    && hits
                        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok();
                let _ = tx.send(Hook {
                    delivery_id: h("x-aurix-delivery-id"),
                    event,
                    webhook_id: h("x-aurix-webhook-id"),
                    attempt: h("x-aurix-attempt").parse().unwrap_or(0),
                    signature: h("x-aurix-signature"),
                    body: body.to_vec(),
                });
                if fail {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app.with_state((tx, hits)))
            .await
            .unwrap();
    });
    (
        url,
        HookRx {
            rx,
            pending: Vec::new(),
        },
    )
}

fn verify(secret: &str, h: &Hook) -> bool {
    aurix_control::webhooks::verify_signature(
        secret,
        &h.signature,
        &h.body,
        chrono::Utc::now(),
        Duration::from_secs(300),
    )
}

/// Next `event` hook; other events that arrive meanwhile are kept in `rx.pending` so a
/// concurrent delivery order does not lose them.
async fn next_hook(rx: &mut HookRx, event: &str, wait: Duration) -> Hook {
    if let Some(i) = rx.pending.iter().position(|h| h.event == event) {
        return rx.pending.remove(i);
    }
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let h = tokio::time::timeout_at(deadline, rx.rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no `{event}` webhook within {wait:?}"))
            .expect("receiver closed");
        if h.event == event {
            return h;
        }
        rx.pending.push(h);
    }
}

struct HookRx {
    rx: tokio::sync::mpsc::UnboundedReceiver<Hook>,
    pending: Vec<Hook>,
}

impl HookRx {
    /// `true` when nothing about `channel_id` arrives within `wait`. Other tests share the
    /// tenant and may produce unrelated deliveries; those are buffered, not counted.
    async fn silent_for_channel(&mut self, channel_id: ChannelId, wait: Duration) -> bool {
        let about = |h: &Hook| {
            serde_json::from_slice::<serde_json::Value>(&h.body)
                .map(|b| b["data"]["channel_id"] == channel_id.to_string())
                .unwrap_or(false)
        };
        if self.pending.iter().any(about) {
            return false;
        }
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(h)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            if about(&h) {
                self.pending.push(h);
                return false;
            }
            self.pending.push(h);
        }
        true
    }
}

/// Reads SSE frames (`event:` + `data:` lines) from a streaming response until `pred` matches
/// an event name/JSON payload or `wait` elapses.
struct SseClient {
    resp: reqwest::Response,
    buf: String,
}

impl SseClient {
    async fn open(env: &Env, api_key: &str, types: Option<&str>) -> reqwest::Result<Self> {
        let mut url = format!("{}/v1/events", env.api);
        if let Some(t) = types {
            url.push_str(&format!("?types={t}"));
        }
        let resp = reqwest::Client::new()
            .get(url)
            .header("x-api-key", api_key)
            .header("accept", "text/event-stream")
            .send()
            .await?;
        Ok(Self {
            resp,
            buf: String::new(),
        })
    }

    /// Next `(event, data)` frame; comments (keepalives) are skipped.
    async fn next(&mut self, wait: Duration) -> Option<(String, serde_json::Value)> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(pos) = self.buf.find("\n\n") {
                let frame = self.buf[..pos].to_string();
                self.buf.drain(..pos + 2);
                let mut event = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim());
                    }
                }
                if event.is_empty() {
                    continue; // comment-only frame (keepalive)
                }
                let json = serde_json::from_str(&data).unwrap_or(serde_json::Value::Null);
                return Some((event, json));
            }
            let chunk = tokio::time::timeout_at(deadline, self.resp.chunk())
                .await
                .ok()?
                .ok()??;
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    async fn expect(&mut self, event: &str, wait: Duration) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let (ev, data) = self
                .next(remaining)
                .await
                .unwrap_or_else(|| panic!("no `{event}` SSE frame within {wait:?}"));
            if ev == event {
                return data;
            }
        }
    }

    /// Like `expect`, but skips frames of the same type about other streams (other tests of the
    /// tenant may run concurrently).
    async fn expect_stream(
        &mut self,
        event: &str,
        stream_id: uuid::Uuid,
        wait: Duration,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + wait;
        let want = stream_id.to_string();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let data = self.expect(event, remaining).await;
            if data["data"]["stream_id"].as_str() == Some(want.as_str()) {
                return data;
            }
        }
    }
}

/// Webhook subscriptions get signed, retried, tenant-scoped deliveries for channel/participant
/// lifecycle events (also when the player sits on another node), `webhook.test` / `webhook.resync`
/// on demand, and the same events flow over the `GET /v1/events` SSE stream.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn webhooks_and_sse_deliver_signed_tenant_scoped_events() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let (env, _app_id) = isolated_env(&base, &http, "webhooks").await;
    let api = |path: &str| format!("{}{path}", env.api);

    // Leftovers of an aborted run would eat the per-app subscription quota.
    let existing: serde_json::Value = http
        .get(api("/v1/webhooks"))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    for w in existing["webhooks"].as_array().unwrap() {
        if w["description"] == "e2e" {
            http.delete(api(&format!("/v1/webhooks/{}", w["id"].as_str().unwrap())))
                .header("x-api-key", &env.api_key)
                .send()
                .await
                .unwrap();
        }
    }

    // Event catalogue: public names only, noise flagged.
    let catalogue: serde_json::Value = http
        .get(api("/v1/webhooks/events"))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = catalogue["webhook"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    let stream: Vec<&str> = catalogue["stream"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap())
        .collect();
    for n in [
        "participant.joined",
        "participant.left",
        "channel.activated",
        "channel.deactivated",
        "recording.started",
        "moderation.event",
    ] {
        assert!(names.contains(&n), "catalogue lacks {n}: {names:?}");
    }
    assert!(
        !names.contains(&"participant.speaking"),
        "noise is not webhook-able"
    );
    assert!(
        stream.contains(&"participant.speaking"),
        "but it can be streamed on request"
    );
    assert!(!stream.iter().any(|n| n.starts_with("node.")));

    // Validation: bad URL, unknown event, noisy event.
    for (body, why) in [
        (
            serde_json::json!({"url": "ftp://example.com/x", "events": ["*"]}),
            "scheme",
        ),
        (
            serde_json::json!({"url": "http://127.0.0.1:9/x", "events": ["nope.nope"]}),
            "unknown event",
        ),
        (
            serde_json::json!({"url": "http://127.0.0.1:9/x", "events": ["participant.speaking"]}),
            "noisy event",
        ),
        (
            serde_json::json!({"url": "http://127.0.0.1:9/x", "events": []}),
            "empty events",
        ),
    ] {
        let r = http
            .post(api("/v1/webhooks"))
            .header("x-api-key", &env.api_key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{why} must be rejected");
    }

    // Subscription 1: everything; the first `channel.activated` delivery fails once -> retry.
    let (url1, mut rx1) = start_receiver(Some("channel.activated")).await;
    let created: serde_json::Value = http
        .post(api("/v1/webhooks"))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"url": url1, "events": ["*"], "description": "e2e"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let hook1 = created["id"].as_str().unwrap().to_string();
    let secret1 = created["secret"].as_str().unwrap().to_string();
    assert!(secret1.starts_with("whsec_"), "secret shown once on create");

    // The secret is never returned again.
    let fetched: serde_json::Value = http
        .get(api(&format!("/v1/webhooks/{hook1}")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        fetched.get("secret").is_none(),
        "GET must not leak the secret: {fetched}"
    );
    assert_eq!(fetched["events"], serde_json::json!(["*"]));

    // Subscription 2: only participant.left.
    let (url2, mut rx2) = start_receiver(None).await;
    let created2: serde_json::Value = http
        .post(api("/v1/webhooks"))
        .header("x-api-key", &env.api_key)
        .json(
            &serde_json::json!({"url": url2, "events": ["participant.left"], "description": "e2e"}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let hook2 = created2["id"].as_str().unwrap().to_string();
    let secret2 = created2["secret"].as_str().unwrap().to_string();

    // SSE consumers: default filter (no noise) and an explicit one.
    let mut sse = SseClient::open(&env, &env.api_key, None).await.unwrap();
    assert_eq!(sse.resp.status(), 200);
    assert!(sse.resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let hello = sse.expect("stream.open", Duration::from_secs(5)).await;
    assert_eq!(hello["filter"], serde_json::Value::Null);
    let mut sse_left = SseClient::open(&env, &env.api_key, Some("participant.left"))
        .await
        .unwrap();
    sse_left.expect("stream.open", Duration::from_secs(5)).await;
    let bad = SseClient::open(&env, &env.api_key, Some("nope.nope"))
        .await
        .unwrap();
    assert_eq!(bad.resp.status(), 400, "unknown SSE filter type");

    // A player joins (on node 2 when available: events must cross the cluster).
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let player_env = env2.as_ref().unwrap_or(&env);
    let channel_id = create_channel(&env, &http).await;
    let (tok_a, uid_a) = issue_token(&env, &http, "wh:alice", "Alice", channel_id).await;
    let mut alice = connect(player_env, "alice", tok_a).await;
    join(&mut alice, channel_id).await;

    // Webhook 1 fails the first delivery and receives the retry with attempt=2 and the same
    // delivery id; the signature verifies against the secret shown at creation.
    let first = next_hook(&mut rx1, "channel.activated", Duration::from_secs(10)).await;
    assert_eq!(first.attempt, 1);
    assert_eq!(first.webhook_id, hook1);
    assert!(
        verify(&secret1, &first),
        "signature must verify: {}",
        first.signature
    );
    assert!(!verify(&secret2, &first), "another secret must not verify");
    let body: serde_json::Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(body["type"], "channel.activated");
    assert_eq!(body["data"]["channel_id"], channel_id.to_string());
    assert!(
        body["data"].get("app_id").is_none(),
        "tenant id lives in the envelope only"
    );
    assert!(body["app_id"].is_string());
    let event_id = body["id"].as_str().unwrap().to_string();

    let retry = next_hook(&mut rx1, "channel.activated", Duration::from_secs(15)).await;
    assert_eq!(
        retry.delivery_id, first.delivery_id,
        "retry keeps the delivery id"
    );
    assert_eq!(retry.attempt, 2);
    let retry_body: serde_json::Value = serde_json::from_slice(&retry.body).unwrap();
    assert_eq!(
        retry_body["id"], event_id,
        "event id is stable across attempts"
    );

    let joined = next_hook(&mut rx1, "participant.joined", Duration::from_secs(15)).await;
    let joined_body: serde_json::Value = serde_json::from_slice(&joined.body).unwrap();
    assert_eq!(joined_body["data"]["user_id"], uid_a);
    assert_eq!(joined_body["data"]["channel_id"], channel_id.to_string());

    // SSE saw the same events (same ids) live.
    let sse_act = sse
        .expect("channel.activated", Duration::from_secs(5))
        .await;
    assert_eq!(
        sse_act["id"], event_id,
        "SSE and webhook share the event id"
    );
    let sse_join = sse
        .expect("participant.joined", Duration::from_secs(5))
        .await;
    assert_eq!(sse_join["data"]["user_id"], uid_a);

    // Webhook 2 is filtered: nothing about this channel yet.
    assert!(
        rx2.silent_for_channel(channel_id, Duration::from_secs(2))
            .await,
        "participant.left-only webhook must not get join events"
    );

    // Snapshot lists the live channel with Alice.
    let snap: serde_json::Value = http
        .get(api("/v1/events/snapshot"))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let ch = snap["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channel_id"] == channel_id.to_string())
        .unwrap_or_else(|| panic!("snapshot lacks the channel: {snap}"));
    assert_eq!(ch["participants"][0]["user_id"], uid_a);

    // Leave: participant.left everywhere, channel.deactivated, and webhook 2 finally fires.
    alice
        .send(&ControlMessage::ChannelLeave { channel_id })
        .await;
    let left2 = next_hook(&mut rx2, "participant.left", Duration::from_secs(10)).await;
    assert_eq!(left2.webhook_id, hook2);
    assert!(verify(&secret2, &left2));
    next_hook(&mut rx1, "participant.left", Duration::from_secs(10)).await;
    next_hook(&mut rx1, "channel.deactivated", Duration::from_secs(10)).await;
    sse.expect("participant.left", Duration::from_secs(5)).await;
    sse.expect("channel.deactivated", Duration::from_secs(5))
        .await;
    let (ev, _) = sse_left.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        ev, "participant.left",
        "explicit filter passes only its types"
    );

    // Delivery log: the retried delivery is listed (filter by status, since the tenant is shared
    // with concurrently running tests) and shows 2 attempts and `delivered`.
    let deliveries: serde_json::Value = http
        .get(api(&format!(
            "/v1/webhooks/{hook1}/deliveries?status=delivered&limit=200"
        )))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let d = deliveries["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == first.delivery_id)
        .unwrap_or_else(|| panic!("delivery log lacks {}: {deliveries}", first.delivery_id));
    assert_eq!(d["status"], "delivered");
    assert_eq!(d["attempts"], 2);
    assert_eq!(d["last_status"], 200);
    let sub: serde_json::Value = http
        .get(api(&format!("/v1/webhooks/{hook1}")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sub["consecutive_failures"], 0);
    assert_eq!(sub["last_status"], 200);

    // Manual redelivery re-sends the finished delivery with a fresh attempt counter.
    http.post(api(&format!(
        "/v1/webhooks/{hook1}/deliveries/{}/retry",
        first.delivery_id
    )))
    .header("x-api-key", &env.api_key)
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    let again = next_hook(&mut rx1, "channel.activated", Duration::from_secs(10)).await;
    assert_eq!(again.delivery_id, first.delivery_id);
    assert_eq!(again.attempt, 1);

    // Test + resync on demand.
    http.post(api(&format!("/v1/webhooks/{hook2}/test")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let t = next_hook(&mut rx2, "webhook.test", Duration::from_secs(10)).await;
    let t_body: serde_json::Value = serde_json::from_slice(&t.body).unwrap();
    assert_eq!(t_body["data"]["webhook_id"], hook2);
    http.post(api(&format!("/v1/webhooks/{hook2}/resync")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let r = next_hook(&mut rx2, "webhook.resync", Duration::from_secs(10)).await;
    let r_body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert!(r_body["data"]["channels"].is_array());

    // Rotate: old secret stops verifying, the new one is shown once.
    let rotated: serde_json::Value = http
        .post(api(&format!("/v1/webhooks/{hook2}/rotate-secret")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret2b = rotated["secret"].as_str().unwrap().to_string();
    assert_ne!(secret2b, secret2);
    http.post(api(&format!("/v1/webhooks/{hook2}/test")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let t2 = next_hook(&mut rx2, "webhook.test", Duration::from_secs(10)).await;
    assert!(verify(&secret2b, &t2));
    assert!(!verify(&secret2, &t2));

    // Disable: lifecycle events stop, PATCH is honoured.
    let patched: serde_json::Value = http
        .patch(api(&format!("/v1/webhooks/{hook2}")))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"enabled": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(patched["enabled"], false);

    // Tenant isolation: another app cannot see or touch these webhooks, and its SSE stream
    // never carries this app's events.
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let list2: serde_json::Value = http
            .get(api("/v1/webhooks"))
            .header("x-api-key", &api_key2)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !list2["webhooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == hook1 || w["id"] == hook2),
            "foreign tenant lists our webhooks: {list2}"
        );
        for (method, path) in [
            (reqwest::Method::GET, format!("/v1/webhooks/{hook1}")),
            (reqwest::Method::DELETE, format!("/v1/webhooks/{hook1}")),
            (reqwest::Method::POST, format!("/v1/webhooks/{hook1}/test")),
            (
                reqwest::Method::POST,
                format!("/v1/webhooks/{hook1}/rotate-secret"),
            ),
            (
                reqwest::Method::GET,
                format!("/v1/webhooks/{hook1}/deliveries"),
            ),
            (
                reqwest::Method::GET,
                format!("/v1/webhooks/{hook1}/deliveries/{}", first.delivery_id),
            ),
        ] {
            let r = http
                .request(method.clone(), api(&path))
                .header("x-api-key", &api_key2)
                .send()
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                404,
                "{method} {path} must be hidden from another tenant"
            );
        }
        let mut sse2 = SseClient::open(&env, &api_key2, None).await.unwrap();
        sse2.expect("stream.open", Duration::from_secs(5)).await;
        let (tok_b, _) = issue_token(&env, &http, "wh:bob", "Bob", channel_id).await;
        let mut bob = connect(&env, "bob", tok_b).await;
        join(&mut bob, channel_id).await;
        sse.expect("participant.joined", Duration::from_secs(5))
            .await;
        assert!(
            sse2.next(Duration::from_secs(2)).await.is_none(),
            "foreign tenant's SSE stream must stay silent"
        );
        bob.send(&ControlMessage::ChannelLeave { channel_id }).await;
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
    }

    // No key / wrong permission: nothing about webhooks or the stream is reachable.
    for path in [
        "/v1/webhooks",
        "/v1/webhooks/events",
        "/v1/events",
        "/v1/events/snapshot",
    ] {
        let r = http.get(api(path)).send().await.unwrap();
        assert_eq!(r.status(), 401, "{path} without a key");
        let r = http
            .get(api(path))
            .header("x-api-key", "ak_not_a_real_key")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{path} with a bogus key");
    }

    // Closing an SSE client is noticed by the server (gauge drops back, keepalives stop).
    if let Ok(metrics) = std::env::var("AURIX_E2E_METRICS") {
        let gauge = |body: &str| -> i64 {
            body.lines()
                .find(|l| l.starts_with("aurix_event_stream_clients "))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| v as i64)
                .unwrap_or_else(|| panic!("gauge missing in {body}"))
        };
        let open = gauge(
            &http
                .get(&metrics)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        );
        assert!(open >= 2, "our streams are counted: {open}");
        drop(sse);
        drop(sse_left);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let now = gauge(
                &http
                    .get(&metrics)
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
            );
            if now <= open - 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "SSE gauge stuck at {now} (was {open}) after clients disconnected"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    } else {
        eprintln!("AURIX_E2E_METRICS not set; skipping SSE disconnect gauge check");
    }

    // Cleanup; deleting a webhook drops its delivery log.
    for id in [&hook1, &hook2] {
        let r = http
            .delete(api(&format!("/v1/webhooks/{id}")))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
    }
    let r = http
        .get(api(&format!("/v1/webhooks/{hook1}/deliveries")))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

async fn channel_status(env: &Env, http: &reqwest::Client, channel_id: ChannelId) -> u16 {
    http.get(format!("{}/v1/channels/{}", env.api, channel_id))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn moderation_call(
    env: &Env,
    http: &reqwest::Client,
    api_key: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let r = http
        .post(format!("{}/v1/moderation/{path}", env.api))
        .header("x-api-key", api_key)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body = r.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn id_list(v: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = v
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// Ad-hoc channels: a token grant with an `ad_hoc` template creates the channel on first join
/// (id derived from the name, so the game server knows it up front), the channel disappears
/// when the last participant leaves and comes back on the next join. `mute-all` / `kick-all`
/// apply the single-user moderation semantics to everyone present minus an exclusion list.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn ad_hoc_channels_and_channel_wide_moderation() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let (env, _app_id) = isolated_env(&base, &http, "moderation").await;
    let name = format!("e2e-match-{}", uuid::Uuid::now_v7().simple());

    // Bad grants are refused when the token is issued, not when the player joins.
    for (body, why) in [
        (
            serde_json::json!({"ad_hoc": {"name": name, "channel_type": "team"}, "channel_id": ChannelId::new()}),
            "channel_id that does not match the derived one",
        ),
        (
            serde_json::json!({"ad_hoc": {"name": "", "channel_type": "team"}}),
            "empty ad-hoc name",
        ),
        (
            serde_json::json!({"ad_hoc": {"name": "x", "channel_type": "team", "max_participants": 0}}),
            "zero max_participants",
        ),
        (
            serde_json::json!({"join": true}),
            "grant without channel_id or ad_hoc",
        ),
    ] {
        let r = http
            .post(format!("{}/v1/tokens", env.api))
            .header("x-api-key", &env.api_key)
            .json(&serde_json::json!({
                "external_id": "e2e-adhoc-bad",
                "display_name": "Bad",
                "channels": [body],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400, "{why} must be rejected");
    }

    // Alice: session JWT with an ad-hoc grant. The response resolves the channel id.
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": "e2e-adhoc-alice",
            "display_name": "Alice",
            "channels": [{
                "ad_hoc": {"name": name, "channel_type": "team", "max_participants": 100000},
                "moderate": true,
            }],
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let alice_token = r["token"].as_str().unwrap().to_string();
    let uid_a = r["user_id"].as_str().unwrap().to_string();
    let channel_id: ChannelId =
        serde_json::from_value(r["channels"][0]["channel_id"].clone()).unwrap();
    assert_eq!(r["channels"][0]["ad_hoc"]["name"], name);
    assert!(
        r["channels"][0]["ad_hoc"]["max_participants"]
            .as_u64()
            .unwrap()
            < 100000,
        "max_participants is clamped to the app limit: {r}"
    );
    assert_eq!(
        channel_status(&env, &http, channel_id).await,
        404,
        "ad-hoc channel must not exist before the first join"
    );

    // Bob: one-time join action token with the same template and no channel_id.
    let r = action_token(
        &env,
        &http,
        serde_json::json!({
            "action": "join",
            "external_id": "e2e-adhoc-bob",
            "display_name": "Bob",
            "ad_hoc": {"name": name, "channel_type": "team"},
        }),
    )
    .await
    .expect("join action token with ad_hoc");
    let bob_join_tok = r["token"].as_str().unwrap().to_string();
    let uid_b = r["user_id"].as_str().unwrap().to_string();
    assert_eq!(
        serde_json::from_value::<ChannelId>(r["channel_id"].clone()).unwrap(),
        channel_id,
        "action token derives the same id from the same name"
    );
    assert_eq!(
        action_token(
            &env,
            &http,
            serde_json::json!({
                "action": "kick",
                "external_id": "e2e-adhoc-bob",
                "display_name": "Bob",
                "target_user_id": uid_a,
                "ad_hoc": {"name": name},
            }),
        )
        .await
        .err(),
        Some(400),
        "ad_hoc is only meaningful for join"
    );
    // Carol: session token for the same channel id but WITHOUT the template - she can join
    // only once the channel exists.
    let (carol_token, uid_c) =
        issue_token(&env, &http, "e2e-adhoc-carol", "Carol", channel_id).await;
    let mut carol = connect(&env, "carol", carol_token).await;
    carol
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: carol.token.clone(),
        })
        .await;
    expect_error(
        &mut carol,
        "join of a not-yet-created ad-hoc channel",
        "CHANNEL_NOT_FOUND",
    )
    .await;

    // First join creates the channel.
    let mut alice = connect(&env, "alice", alice_token).await;
    join(&mut alice, channel_id).await;
    assert_eq!(channel_status(&env, &http, channel_id).await, 200);
    let ch: serde_json::Value = http
        .get(format!("{}/v1/channels/{}", env.api, channel_id))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ch["ad_hoc"], true, "{ch}");
    assert_eq!(ch["name"], name, "{ch}");
    assert_eq!(ch["channel_type"], "team", "{ch}");

    // Bob's session is opened with an ordinary token (no channel grants); the ad-hoc join
    // token alone authorizes the channel entry.
    let (bob_session, _) = issue_token_for(&env, &http, "e2e-adhoc-bob", "Bob", &[]).await;
    let mut bob = connect(&env, "bob", bob_session).await;
    bob.send(&ControlMessage::ChannelJoin {
        channel_id,
        token: bob_join_tok,
    })
    .await;
    bob.expect(
        "ChannelJoinAck",
        |m| matches!(m, ControlMessage::ChannelJoinAck { channel_id: c, .. } if *c == channel_id),
    )
    .await;
    alice
        .expect(
            "ParticipantJoined(bob)",
            |m| matches!(m, ControlMessage::ParticipantJoined { user_id, .. } if user_id.to_string() == uid_b),
        )
        .await;
    join(&mut carol, channel_id).await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 3);

    // mute-all except Alice: Bob and Carol are server-muted, Alice untouched.
    let (status, body) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "mute-all",
        serde_json::json!({"channel_id": channel_id, "muted": true, "except": [uid_a]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let mut expected = vec![uid_b.clone(), uid_c.clone()];
    expected.sort();
    assert_eq!(id_list(&body["affected"]), expected, "{body}");
    assert_eq!(id_list(&body["skipped"]), vec![uid_a.clone()], "{body}");
    assert_eq!(
        body["failed"].as_array().map(|a| a.len()),
        Some(0),
        "{body}"
    );
    for target in [&uid_b, &uid_c] {
        alice
            .expect("MuteStateChanged(server mute)", |m| {
                matches!(m, ControlMessage::MuteStateChanged { user_id, muted: true, server_muted: true, .. } if user_id.to_string() == *target)
            })
            .await;
    }
    let parts: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{}/participants",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let muted: Vec<(String, bool)> = parts["memberships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["user_id"].as_str().unwrap().to_string(),
                m["is_server_muted"].as_bool().unwrap(),
            )
        })
        .collect();
    for (uid, is_muted) in &muted {
        assert_eq!(*is_muted, *uid != uid_a, "{uid}: {parts}");
    }

    // Unmute everyone (no exclusions).
    let (status, body) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "mute-all",
        serde_json::json!({"channel_id": channel_id, "muted": false}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["affected"].as_array().map(|a| a.len()),
        Some(3),
        "{body}"
    );

    // Tenant isolation and validation.
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let (status, _) = moderation_call(
            &env,
            &http,
            &api_key2,
            "kick-all",
            serde_json::json!({"channel_id": channel_id, "reason": "nope"}),
        )
        .await;
        assert_eq!(status, 404, "another tenant cannot moderate this channel");
    }
    let (status, _) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "kick-all",
        serde_json::json!({"channel_id": channel_id, "reason": ""}),
    )
    .await;
    assert_eq!(status, 400, "empty reason");
    let (status, _) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "kick-all",
        serde_json::json!({"channel_id": channel_id, "reason": "x", "except": ["not-a-uuid"]}),
    )
    .await;
    assert_eq!(status, 400, "malformed except entry");

    // kick-all except Alice: Bob and Carol receive Kick, the channel survives (Alice stays).
    let (status, body) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "kick-all",
        serde_json::json!({"channel_id": channel_id, "reason": "round over", "except": [uid_a]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kicked"], 2, "{body}");
    assert_eq!(id_list(&body["affected"]), expected, "{body}");
    bob.expect("Kick", |m| {
        matches!(m, ControlMessage::Kick { user_id, reason, .. } if user_id.to_string() == uid_b && reason == "round over")
    })
    .await;
    carol
        .expect(
            "Kick",
            |m| matches!(m, ControlMessage::Kick { user_id, .. } if user_id.to_string() == uid_c),
        )
        .await;
    for _ in 0..2 {
        alice
            .expect("ParticipantLeft", |m| {
                matches!(m, ControlMessage::ParticipantLeft { .. })
            })
            .await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 1);
    assert_eq!(
        channel_status(&env, &http, channel_id).await,
        200,
        "channel stays while a participant remains"
    );

    // Last participant leaves: the ad-hoc channel is destroyed...
    alice
        .send(&ControlMessage::ChannelLeave { channel_id })
        .await;
    let mut gone = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if channel_status(&env, &http, channel_id).await == 404 {
            gone = true;
            break;
        }
    }
    assert!(gone, "ad-hoc channel must be destroyed when it empties");
    let (status, _) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "mute-all",
        serde_json::json!({"channel_id": channel_id, "muted": true}),
    )
    .await;
    assert_eq!(status, 404, "destroyed channel is gone for moderation too");

    // ...and re-created by the next join with the same name and id.
    join(&mut alice, channel_id).await;
    assert_eq!(channel_status(&env, &http, channel_id).await, 200);
    assert_eq!(membership_count(&env, &http, channel_id).await, 1);
    alice.ws.close(None).await.unwrap();
    bob.ws.close(None).await.unwrap();
    carol.ws.close(None).await.unwrap();
    let mut gone = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if channel_status(&env, &http, channel_id).await == 404 {
            gone = true;
            break;
        }
    }
    assert!(
        gone,
        "disconnect of the last participant destroys the channel"
    );

    // Cross-node: the channel is created from node 1, Bob joins it on node 2 with his own
    // ad-hoc grant, and channel-wide moderation issued via node 1 reaches him.
    let Ok(ws2) = std::env::var("AURIX_E2E_WS2") else {
        eprintln!("AURIX_E2E_WS2 not set; skipping cross-node ad-hoc checks");
        return;
    };
    let env2 = Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws: ws2,
        api_key: env.api_key.clone(),
    };
    let mut alice = connect(&env, "alice", alice.token.clone()).await;
    join(&mut alice, channel_id).await;
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": "e2e-adhoc-bob",
            "display_name": "Bob",
            "channels": [{"ad_hoc": {"name": name, "channel_type": "team"}}],
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut bob = connect(&env2, "bob@node2", r["token"].as_str().unwrap().to_string()).await;
    join(&mut bob, channel_id).await;
    alice
        .expect(
            "ParticipantJoined(bob)",
            |m| matches!(m, ControlMessage::ParticipantJoined { user_id, .. } if user_id.to_string() == uid_b),
        )
        .await;
    assert_eq!(channel_status(&env2, &http, channel_id).await, 200);
    let (status, body) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "mute-all",
        serde_json::json!({"channel_id": channel_id, "muted": true, "except": [uid_a]}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(id_list(&body["affected"]), vec![uid_b.clone()], "{body}");
    bob.expect("MuteStateChanged on node 2", |m| {
        matches!(m, ControlMessage::MuteStateChanged { user_id, muted: true, server_muted: true, .. } if user_id.to_string() == uid_b)
    })
    .await;
    let (status, body) = moderation_call(
        &env,
        &http,
        &env.api_key,
        "kick-all",
        serde_json::json!({"channel_id": channel_id, "reason": "server restart"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["kicked"], 2, "{body}");
    bob.expect("Kick on node 2", |m| {
        matches!(m, ControlMessage::Kick { user_id, reason, .. } if user_id.to_string() == uid_b && reason == "server restart")
    })
    .await;
    alice
        .expect(
            "Kick",
            |m| matches!(m, ControlMessage::Kick { user_id, .. } if user_id.to_string() == uid_a),
        )
        .await;
    let mut gone = false;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if channel_status(&env, &http, channel_id).await == 404
            && channel_status(&env2, &http, channel_id).await == 404
        {
            gone = true;
            break;
        }
    }
    assert!(
        gone,
        "kick-all of everyone destroys the ad-hoc channel on both nodes"
    );
    alice.ws.close(None).await.unwrap();
    bob.ws.close(None).await.unwrap();
}

async fn status_of(env: &Env, http: &reqwest::Client, path: &str) -> u16 {
    http.get(format!("{}{path}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

/// `DELETE /v1/users/:id` erases everything keyed to the player — sessions and memberships
/// (live ones are closed on every node), blocks in both directions, chat, recordings
/// (including one still in progress, whose file must not survive) — and leaves a tombstone so
/// credentials minted before the deletion are refused. `GET /v1/users/:id/export` shows the
/// same data beforehand; both are tenant-scoped and 404 for unknown users.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn user_erasure_export_and_stale_token_rejection() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let ext_a = format!("e2e:erase-{}", uuid::Uuid::now_v7().simple());
    let (tok_a, uid_a) = issue_token(&env, &http, &ext_a, "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "e2e:erase-bob", "Bob", channel_id).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a.clone()).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    bind_media(&mut alice).await;
    join(&mut alice, channel_id).await;
    join(&mut bob, channel_id).await;
    // A second session of Alice on the other node, when the cluster has one.
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let mut alice2 = match &env2 {
        Some(e2) => {
            let mut p = connect(e2, "alice@node2", tok_a.clone()).await;
            join(&mut p, channel_id).await;
            Some(p)
        }
        None => None,
    };
    let expected_sessions = if alice2.is_some() { 2 } else { 1 };
    let expected_left = expected_sessions + 1; // + bob
    assert_eq!(
        membership_count(&env, &http, channel_id).await,
        expected_left
    );

    // Bob blocks Alice; a recording of Alice is running when she is erased.
    http.post(format!("{}/v1/users/{}/blocks", env.api, uid_b))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"blocked_user_id": uid_a}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let rec: serde_json::Value = http
        .post(format!("{}/v1/recordings/start", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "user_id": uid_a}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let recording_id: uuid::Uuid = rec["id"].as_str().unwrap().parse().unwrap();
    alice
        .expect("RecordingNotification", |m| {
            matches!(
                m,
                ControlMessage::RecordingNotification { active: true, .. }
            )
        })
        .await;
    alice
        .send(&ControlMessage::RecordingConsentResponse {
            recording_id,
            consent: RecordingConsent::Accepted,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let hash = channel_id_hash(&channel_id);
    let payload = Bytes::from_static(&[0xfc, 0xff, 0xfe, 0x01, 0x02, 0x03]);
    for i in 0..10u32 {
        let pkt = AurixPacket::audio(i + 1, (i + 1) * 960, alice.ssrc, hash, payload.clone());
        alice
            .udp
            .send_to(&pkt.seal(&alice.keys), alice.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{recording_id}")).await,
        200
    );

    // ── export ──
    let export: serde_json::Value = http
        .get(format!("{}/v1/users/{}/export", env.api, uid_a))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["format"], "aurix.user_export.v1");
    assert_eq!(export["user"]["id"], uid_a.to_string());
    assert_eq!(export["user"]["external_id"], ext_a);
    assert_eq!(
        export["sessions"].as_array().map(|a| a.len()),
        Some(expected_sessions),
        "{export}"
    );
    assert_eq!(
        export["channel_memberships"].as_array().map(|a| a.len()),
        Some(expected_sessions),
        "{export}"
    );
    assert_eq!(
        id_list(&export["blocks"]["blocked_by"]),
        vec![uid_b.to_string()],
        "{export}"
    );
    assert!(
        export["recordings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == recording_id.to_string()),
        "{export}"
    );
    assert_eq!(export["truncated"], serde_json::json!([]));

    // ── tenant isolation: another app sees neither export nor delete ──
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let other = Env {
            api: env.api.clone(),
            ws: env.ws.clone(),
            api_key: api_key2,
        };
        assert_eq!(
            status_of(&other, &http, &format!("/v1/users/{uid_a}/export")).await,
            404
        );
        let r = http
            .delete(format!("{}/v1/users/{}", other.api, uid_a))
            .header("x-api-key", &other.api_key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(
            status_of(&env, &http, &format!("/v1/users/{uid_a}")).await,
            200,
            "foreign tenant must not erase the user"
        );
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
    }

    // ── erase ──
    let deleted: serde_json::Value = http
        .delete(format!("{}/v1/users/{}", env.api, uid_a))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["user_id"], uid_a.to_string());
    assert_eq!(deleted["recordings_removed"], 1, "{deleted}");
    let rows = &deleted["rows_removed"];
    assert_eq!(rows["users"], 1, "{deleted}");
    assert_eq!(rows["user_blocks"], 1, "{deleted}");
    assert_eq!(
        rows["recordings"], 0,
        "recording rows go with their files, not with the transaction: {deleted}"
    );
    assert_eq!(rows["sessions"], expected_sessions, "{deleted}");
    assert_eq!(rows["channel_memberships"], expected_sessions, "{deleted}");

    // Every live session of Alice is closed, on both nodes; Bob sees her leave.
    for p in std::iter::once(&mut alice).chain(alice2.iter_mut()) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, p.ws.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) => continue,
                Err(_) => panic!("{}: session survived the erasure", p.name),
            }
        }
    }
    for _ in 0..expected_sessions {
        bob.expect(
            "ParticipantLeft for alice",
            |m| matches!(m, ControlMessage::ParticipantLeft { user_id, .. } if *user_id == uid_a),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 1);

    // Nothing about Alice remains.
    assert_eq!(
        status_of(&env, &http, &format!("/v1/users/{uid_a}")).await,
        404
    );
    assert_eq!(
        status_of(&env, &http, &format!("/v1/users/{uid_a}/export")).await,
        404
    );
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{recording_id}")).await,
        404
    );
    assert_eq!(
        status_of(
            &env,
            &http,
            &format!("/v1/recordings/{recording_id}/download")
        )
        .await,
        404
    );
    let blocks: serde_json::Value = http
        .get(format!("{}/v1/users/{}/blocks", env.api, uid_b))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !id_list(&blocks["blocked_users"]).contains(&uid_a.to_string()),
        "{blocks}"
    );
    let r = http
        .delete(format!("{}/v1/users/{}", env.api, uid_a))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "second erasure is a clean not-found");

    // Credentials minted before the erasure are dead: fresh connect, resume and REST.
    assert_eq!(try_connect(&env, &tok_a, None).await.err(), Some(401));
    assert_eq!(
        try_connect(&env, &tok_a, Some((alice.session_id, &alice.resume_token)))
            .await
            .err(),
        Some(401)
    );
    let r = http
        .get(format!("{}/v1/me/turn-credentials", env.api))
        .bearer_auth(&tok_a)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // The same player can come back: a new token means a new user with a new id.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (tok_a2, uid_a2) = issue_token(&env, &http, &ext_a, "Alice", channel_id).await;
    assert_ne!(uid_a2, uid_a.to_string());
    let mut alice_again = connect(&env, "alice-again", tok_a2).await;
    join(&mut alice_again, channel_id).await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 2);

    alice_again.ws.close(None).await.unwrap();
    bob.ws.close(None).await.unwrap();
    if let Some(mut p) = alice2.take() {
        let _ = p.ws.close(None).await;
    }
    let _ = alice.ws.close(None).await;
}

/// 20 ms Opus frames of a mono sine at 48 kHz — real audio the server-side STT can decode.
fn opus_tone(hz: f32, ms: u32) -> Vec<Bytes> {
    let mut enc = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip)
        .expect("opus encoder");
    let frames = ms / 20;
    let mut out = Vec::with_capacity(frames as usize);
    let mut pcm = vec![0i16; 960];
    let mut buf = vec![0u8; 1500];
    for f in 0..frames {
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = (f as f32 * 960.0 + i as f32) / 48_000.0;
            *s = ((t * hz * std::f32::consts::TAU).sin() * 0.35 * 32767.0) as i16;
        }
        let n = enc.encode(&pcm, &mut buf).unwrap();
        out.push(Bytes::copy_from_slice(&buf[..n]));
    }
    out
}

/// Streams `frames` as this player's microphone into `channel_id`, paced at 20 ms.
async fn stream_frames(
    from: &Player,
    channel_id: ChannelId,
    first_seq: u32,
    frames: &[Bytes],
    e2ee: bool,
) {
    let hash = channel_id_hash(&channel_id);
    for (i, payload) in frames.iter().enumerate() {
        let seq = first_seq + i as u32;
        let mut pkt = AurixPacket::audio(seq, seq * 960, from.ssrc, hash, payload.clone());
        if e2ee {
            pkt.header.set_flag(PacketFlags::E2ee);
        }
        from.udp
            .send_to(&pkt.seal(&from.keys), from.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits up to `wait` for a message matching `pred`, skipping everything else.
async fn expect_within<F: Fn(&ControlMessage) -> bool>(
    p: &mut Player,
    what: &str,
    wait: Duration,
    pred: F,
) -> ControlMessage {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            panic!("{}: never received {what}", p.name);
        }
        match p.try_recv(left).await {
            Some(m) if pred(&m) => return m,
            Some(ControlMessage::Error { code, message, .. }) => {
                panic!(
                    "{}: server error while waiting for {what}: {code} {message}",
                    p.name
                )
            }
            Some(_) => continue,
            None => panic!("{}: never received {what}", p.name),
        }
    }
}

/// Asserts no message matching `pred` reaches `p` within `wait` (other traffic is ignored).
async fn assert_none_matching<F: Fn(&ControlMessage) -> bool>(
    p: &mut Player,
    wait: Duration,
    why: &str,
    pred: F,
) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return;
        }
        match p.try_recv(left).await {
            Some(m) if pred(&m) => panic!("{}: unexpected message ({why}): {m:?}", p.name),
            Some(_) => continue,
            None => return,
        }
    }
}

async fn assert_no_transcript(p: &mut Player, wait: Duration, why: &str) {
    assert_none_matching(p, wait, why, |m| {
        matches!(m, ControlMessage::Transcript { .. })
    })
    .await;
}

/// Parses the frequency out of the mock provider's `tone <N>hz ...` transcript text.
fn tone_hz(text: &str) -> Option<f32> {
    text.strip_prefix("tone ")?
        .split_once("hz")?
        .0
        .parse::<f32>()
        .ok()
}

/// Discards whatever is pending on the control connection.
async fn drain_ws(p: &mut Player) {
    while p.try_recv(Duration::from_millis(300)).await.is_some() {}
}

async fn expect_transcript(
    p: &mut Player,
    channel_id: ChannelId,
    speaker: UserId,
) -> aurix_common::protocol::Transcript {
    let m = expect_within(p, "Transcript", Duration::from_secs(12), |m| {
        matches!(m, ControlMessage::Transcript { transcript }
            if transcript.channel_id == channel_id && transcript.user_id == speaker)
    })
    .await;
    match m {
        ControlMessage::Transcript { transcript } => transcript,
        _ => unreachable!(),
    }
}

async fn expect_tts_status(
    p: &mut Player,
    client_ref: &str,
    state: aurix_common::protocol::TtsState,
) -> (uuid::Uuid, Option<u64>, Option<String>) {
    let m = expect_within(
        p,
        &format!("TtsStatus {state:?} for {client_ref}"),
        Duration::from_secs(10),
        |m| {
            matches!(m, ControlMessage::TtsStatus { client_ref: Some(r), state: s, .. }
                if r == client_ref && *s == state)
        },
    )
    .await;
    match m {
        ControlMessage::TtsStatus {
            request_id,
            duration_ms,
            message,
            ..
        } => (request_id, duration_ms, message),
        _ => unreachable!(),
    }
}

/// Counts audio packets from `ssrc` arriving within `wait`; returns the count and whether
/// every payload was a decodable Opus frame.
async fn synth_audio_from(to: &Player, ssrc: u32, wait: Duration) -> (usize, bool) {
    let deadline = tokio::time::Instant::now() + wait;
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut pcm = vec![0i16; 5760];
    let mut got = 0;
    let mut decodable = true;
    let mut buf = vec![0u8; 2048];
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let Ok(Ok((n, _))) = tokio::time::timeout(left, to.udp.recv_from(&mut buf)).await else {
            break;
        };
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: downlink not sealed for us", to.name);
        if p.header.packet_type == PacketType::Audio && p.header.ssrc == ssrc {
            got += 1;
            decodable &= dec.decode(&p.payload, &mut pcm, false).is_ok();
        }
    }
    (got, decodable)
}

async fn drain_udp(p: &Player) {
    let mut buf = vec![0u8; 2048];
    while tokio::time::timeout(Duration::from_millis(300), p.udp.recv_from(&mut buf))
        .await
        .is_ok()
    {}
}

/// Speech features against the mock provider (`examples/mock_speech.rs`): transcripts are
/// produced only for channels with `transcription: true` and never for E2EE frames, reach the
/// speaker and same-tenant listeners on every node but not sessions that opted out; TTS plays
/// as the participant's synthetic SSRC to the chosen destination, reports its lifecycle to the
/// requester only, enforces voice/text/queue/rate limits, hides provider errors, and can be
/// cancelled; operator announcements come from a per-channel system SSRC and are tenant-scoped.
#[tokio::test]
#[ignore = "requires a running Aurix server with STT/TTS pointed at examples/mock_speech.rs"]
async fn speech_transcripts_and_text_to_speech() {
    use aurix_common::protocol::{TtsDestination, TtsState};
    use aurix_media::tts::{participant_voice_ssrc, system_voice_ssrc};

    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let voices: serde_json::Value = http
        .get(format!("{}/v1/tts/voices", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    if voices["enabled"] != true {
        eprintln!("TTS not enabled on the node; skipping (start examples/mock_speech.rs)");
        return;
    }
    assert_eq!(voices["client_requests"], true);
    assert_eq!(
        voices["voices"][0], "alloy",
        "default voice first: {voices}"
    );
    assert!(
        voices.get("api_key").is_none() && voices.get("endpoint").is_none(),
        "provider settings must not leak: {voices}"
    );
    let max_text = voices["max_text_chars"].as_u64().unwrap() as usize;
    let env2 = match std::env::var("AURIX_E2E_WS2") {
        Ok(ws) => Env {
            api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
            ws,
            api_key: env.api_key.clone(),
        },
        Err(_) => {
            eprintln!("AURIX_E2E_WS2 not set; running the listener on the same node");
            env.clone()
        }
    };

    let spoken = create_channel_with(&env, &http, serde_json::json!({"transcription": true})).await;
    let quiet = create_channel(&env, &http).await;
    let (tok_a, uid_a) =
        issue_token_for(&env, &http, "speech:alice", "Alice", &[spoken, quiet]).await;
    let (tok_b, _) = issue_token_for(&env, &http, "speech:bob", "Bob", &[spoken, quiet]).await;
    let (tok_c, _) = issue_token(&env2, &http, "speech:carol", "Carol", spoken).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut carol = connect(&env2, "carol", tok_c).await;
    for p in [&mut alice, &mut bob, &mut carol] {
        bind_media(p).await;
    }

    // The join ack tells the client whether the channel is transcribed.
    async fn join_expecting(p: &mut Player, ch: ChannelId, transcribed: bool) {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id: ch,
            token: tok,
        })
        .await;
        let ack = p
            .expect("ChannelJoinAck", |m| {
                matches!(m, ControlMessage::ChannelJoinAck { channel_id, .. } if *channel_id == ch)
            })
            .await;
        let ControlMessage::ChannelJoinAck { transcription, .. } = ack else {
            unreachable!()
        };
        assert_eq!(transcription, transcribed, "{}: join ack for {ch}", p.name);
    }
    join_expecting(&mut alice, spoken, true).await;
    join_expecting(&mut alice, quiet, false).await;
    join_expecting(&mut bob, spoken, true).await;
    join_expecting(&mut carol, spoken, true).await;
    // Alice transmits to both channels; the TTS request below must then name its channel.
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::All,
        })
        .await;
    alice
        .expect("TransmissionChanged", |m| {
            matches!(m, ControlMessage::TransmissionChanged { .. })
        })
        .await;

    // ── STT: speaker + same-tenant listeners (cross-node), not the opted-out session ──
    bob.send(&ControlMessage::SetTranscripts { enabled: false })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let tone = opus_tone(440.0, 1_600);
    stream_frames(&alice, spoken, 1, &tone, false).await;
    let t = expect_transcript(&mut alice, spoken, uid_a).await;
    assert!(
        tone_hz(&t.text).is_some_and(|hz| (hz - 440.0).abs() < 10.0) && t.text.contains("dur="),
        "speaker gets their own captions: {t:?}"
    );
    assert_eq!(t.language.as_deref(), Some("en"));
    assert!(t.duration_ms >= 1_000, "segment length: {t:?}");
    assert_eq!(t.words.len(), 2, "stt.include_words on the node: {t:?}");
    assert!(t.words[1].start_ms > 0 && t.words[1].end_ms >= t.words[1].start_ms);
    let tc = expect_transcript(&mut carol, spoken, uid_a).await;
    assert_eq!(tc.id, t.id, "the listener sees the same segment");
    assert_eq!(tc.text, t.text);
    // Let the tail of the stream (below `min_segment_ms`) be discarded before asserting silence.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_no_transcript(&mut bob, Duration::from_millis(500), "opted out").await;

    bob.send(&ControlMessage::SetTranscripts { enabled: true })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    stream_frames(&alice, spoken, 1_000, &opus_tone(880.0, 1_600), false).await;
    let tb = expect_transcript(&mut bob, spoken, uid_a).await;
    assert!(
        tone_hz(&tb.text).is_some_and(|hz| (hz - 880.0).abs() < 15.0),
        "opted back in: {tb:?}"
    );
    for p in [&mut alice, &mut carol] {
        expect_transcript(p, spoken, uid_a).await;
    }
    for p in [&mut alice, &mut bob, &mut carol] {
        drain_ws(p).await;
    }

    // A channel without `transcription` is never sent to STT; neither are E2EE frames.
    stream_frames(&alice, quiet, 2_000, &opus_tone(660.0, 1_600), false).await;
    stream_frames(&alice, spoken, 3_000, &opus_tone(660.0, 1_600), true).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    for p in [&mut alice, &mut bob, &mut carol] {
        assert_no_transcript(p, Duration::from_millis(400), "quiet channel / e2ee").await;
    }
    for p in [&alice, &bob, &carol] {
        drain_udp(p).await;
    }

    // ── TTS as a participant: channel destination ──
    let alice_voice = participant_voice_ssrc(alice.ssrc);
    alice
        .send(&ControlMessage::TtsSpeak {
            channel_id: Some(spoken),
            text: "incoming [dur=600]".into(),
            voice: Some("nova".into()),
            destination: TtsDestination::Channel,
            client_ref: Some("r1".into()),
        })
        .await;
    let (rid, _, _) = expect_tts_status(&mut alice, "r1", TtsState::Queued).await;
    let (rid2, dur, _) = expect_tts_status(&mut alice, "r1", TtsState::Playing).await;
    assert_eq!(rid, rid2);
    assert_eq!(dur, Some(600), "duration of the synthesized clip");
    let ((got_b, ok_b), (got_c, ok_c)) = tokio::join!(
        synth_audio_from(&bob, alice_voice, Duration::from_millis(1_500)),
        synth_audio_from(&carol, alice_voice, Duration::from_millis(1_500)),
    );
    assert!(
        (25..=31).contains(&got_b) && ok_b,
        "bob hears the synthetic voice as 20 ms Opus frames: {got_b} ok={ok_b}"
    );
    assert!(
        (25..=31).contains(&got_c) && ok_c,
        "carol (other node) hears it through the cascade: {got_c} ok={ok_c}"
    );
    let (rid3, dur3, _) = expect_tts_status(&mut alice, "r1", TtsState::Finished).await;
    assert_eq!((rid3, dur3), (rid, Some(600)));
    assert_eq!(
        synth_audio_from(&alice, alice_voice, Duration::from_millis(300))
            .await
            .0,
        0,
        "destination=channel does not echo to the requester"
    );
    // Nobody but the requester sees the lifecycle.
    for p in [&mut bob, &mut carol] {
        assert_none_matching(p, Duration::from_millis(300), "TtsStatus leaked", |m| {
            matches!(m, ControlMessage::TtsStatus { .. })
        })
        .await;
    }

    // ── local destination: only the requester hears it ──
    alice
        .send(&ControlMessage::TtsSpeak {
            channel_id: Some(spoken),
            text: "preview [dur=400]".into(),
            voice: None,
            destination: TtsDestination::Local,
            client_ref: Some("r2".into()),
        })
        .await;
    expect_tts_status(&mut alice, "r2", TtsState::Playing).await;
    let ((got_a, ok_a), (got_b, _)) = tokio::join!(
        synth_audio_from(&alice, alice_voice, Duration::from_millis(1_200)),
        synth_audio_from(&bob, alice_voice, Duration::from_millis(1_200)),
    );
    assert!(
        (16..=21).contains(&got_a) && ok_a,
        "local playback: {got_a}"
    );
    assert_eq!(got_b, 0, "local playback must not reach the channel");
    expect_tts_status(&mut alice, "r2", TtsState::Finished).await;

    // ── validation and authorization, each rejected with the caller's client_ref ──
    struct Rejected(
        Option<ChannelId>,
        String,
        Option<&'static str>,
        &'static str,
        &'static str,
    );
    let cases = [
        Rejected(
            Some(spoken),
            "x".into(),
            Some("hal9000"),
            "VALIDATION_ERROR",
            "unknown voice",
        ),
        Rejected(
            Some(spoken),
            "y".repeat(max_text + 1),
            None,
            "VALIDATION_ERROR",
            "too long",
        ),
        Rejected(
            Some(spoken),
            "tab\u{7}bell".into(),
            None,
            "VALIDATION_ERROR",
            "control chars",
        ),
        Rejected(
            None,
            "which channel?".into(),
            None,
            "VALIDATION_ERROR",
            "ambiguous channel",
        ),
    ];
    for (i, Rejected(channel_id, text, voice, code, why)) in cases.into_iter().enumerate() {
        let r = format!("bad{i}");
        alice
            .send(&ControlMessage::TtsSpeak {
                channel_id,
                text,
                voice: voice.map(str::to_string),
                destination: TtsDestination::Channel,
                client_ref: Some(r.clone()),
            })
            .await;
        let m = expect_within(
            &mut alice,
            why,
            Duration::from_secs(5),
            |m| matches!(m, ControlMessage::Error { client_ref: Some(c), .. } if *c == r),
        )
        .await;
        assert!(
            matches!(&m, ControlMessage::Error { code: c, .. } if c == code),
            "{why}: {m:?}"
        );
    }
    // Carol is not a member of `quiet`.
    carol
        .send(&ControlMessage::TtsSpeak {
            channel_id: Some(quiet),
            text: "sneak".into(),
            voice: None,
            destination: TtsDestination::Channel,
            client_ref: Some("nm".into()),
        })
        .await;
    let m = expect_within(
        &mut carol,
        "non-member rejection",
        Duration::from_secs(5),
        |m| matches!(m, ControlMessage::Error { client_ref: Some(c), .. } if c == "nm"),
    )
    .await;
    assert!(
        matches!(&m, ControlMessage::Error { code, .. } if code == "AUTH_DENIED"),
        "non-member: {m:?}"
    );

    // ── provider failure is reported as a sanitized Failed status ──
    alice
        .send(&ControlMessage::TtsSpeak {
            channel_id: Some(spoken),
            text: "boom [fail]".into(),
            voice: None,
            destination: TtsDestination::Channel,
            client_ref: Some("r3".into()),
        })
        .await;
    expect_tts_status(&mut alice, "r3", TtsState::Queued).await;
    let (_, _, msg) = expect_tts_status(&mut alice, "r3", TtsState::Failed).await;
    let msg = msg.unwrap_or_default();
    assert!(
        !msg.contains("synthetic") && !msg.contains("500") && !msg.is_empty(),
        "provider details must not leak: {msg:?}"
    );

    // ── cancellation: a slow synthesis and everything queued behind it ──
    for r in ["c1", "c2"] {
        alice
            .send(&ControlMessage::TtsSpeak {
                channel_id: Some(spoken),
                text: format!("{r} [slow]"),
                voice: None,
                destination: TtsDestination::Channel,
                client_ref: Some(r.into()),
            })
            .await;
        expect_tts_status(&mut alice, r, TtsState::Queued).await;
    }
    // max_queued_per_session on the node is 2: the third is refused before it costs anything.
    alice
        .send(&ControlMessage::TtsSpeak {
            channel_id: Some(spoken),
            text: "c3 [slow]".into(),
            voice: None,
            destination: TtsDestination::Channel,
            client_ref: Some("c3".into()),
        })
        .await;
    let m = expect_within(
        &mut alice,
        "queue limit",
        Duration::from_secs(5),
        |m| matches!(m, ControlMessage::Error { client_ref: Some(c), .. } if c == "c3"),
    )
    .await;
    assert!(matches!(&m, ControlMessage::Error { code, .. } if code == "RATE_LIMIT_EXCEEDED"));
    alice.send(&ControlMessage::TtsCancel).await;
    // Both land as `Cancelled`, in whichever order the jobs observe the token.
    let mut cancelled = std::collections::BTreeSet::new();
    while cancelled.len() < 2 {
        let m = expect_within(
            &mut alice,
            "TtsStatus Cancelled",
            Duration::from_secs(10),
            |m| matches!(m, ControlMessage::TtsStatus { .. }),
        )
        .await;
        let ControlMessage::TtsStatus {
            client_ref, state, ..
        } = m
        else {
            unreachable!()
        };
        assert_eq!(state, TtsState::Cancelled, "{client_ref:?}");
        cancelled.insert(client_ref.expect("client_ref echoed"));
    }
    assert_eq!(cancelled.into_iter().collect::<Vec<_>>(), ["c1", "c2"]);
    drain_udp(&bob).await;
    assert_eq!(
        synth_audio_from(&bob, alice_voice, Duration::from_secs(4))
            .await
            .0,
        0,
        "cancelled utterances never play"
    );

    // ── per-session request rate (6/min on the node): Bob burns his budget ──
    let mut refused = None;
    for i in 0..8 {
        let r = format!("rate{i}");
        bob.send(&ControlMessage::TtsSpeak {
            channel_id: Some(spoken),
            text: "quick [dur=100]".into(),
            voice: None,
            destination: TtsDestination::Local,
            client_ref: Some(r.clone()),
        })
        .await;
        let m = expect_within(&mut bob, "queued or refused", Duration::from_secs(5), |m| {
            matches!(m, ControlMessage::TtsStatus { client_ref: Some(c), state: TtsState::Queued, .. } if *c == r)
                || matches!(m, ControlMessage::Error { client_ref: Some(c), .. } if *c == r)
        })
        .await;
        if let ControlMessage::Error { code, .. } = m {
            assert_eq!(code, "RATE_LIMIT_EXCEEDED");
            refused = Some(i);
            break;
        }
        // Keep the queue (limit 2) from being the reason for a refusal.
        expect_tts_status(&mut bob, &r, TtsState::Finished).await;
    }
    assert_eq!(
        refused,
        Some(6),
        "seventh request within a minute is refused"
    );

    // ── operator announcement: system SSRC, every node, tenant-scoped ──
    for p in [&alice, &bob, &carol] {
        drain_udp(p).await;
    }
    let r = http
        .post(format!("{}/v1/channels/{}/tts", env.api, spoken))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"text": "server maintenance in five minutes [dur=500]"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap_or_default());
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["state"], "queued");
    uuid::Uuid::parse_str(body["request_id"].as_str().unwrap()).unwrap();
    let system = system_voice_ssrc(&spoken);
    let ((ga, oa), (gb, ob), (gc, oc)) = tokio::join!(
        synth_audio_from(&alice, system, Duration::from_millis(1_500)),
        synth_audio_from(&bob, system, Duration::from_millis(1_500)),
        synth_audio_from(&carol, system, Duration::from_millis(1_500)),
    );
    for (name, got, ok) in [("alice", ga, oa), ("bob", gb, ob), ("carol", gc, oc)] {
        assert!(
            (20..=26).contains(&got) && ok,
            "{name}: announcement frames {got} ok={ok}"
        );
    }
    let r = http
        .post(format!("{}/v1/channels/{}/tts", env.api, spoken))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"text": "nope", "voice": "hal9000"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = http
        .post(format!(
            "{}/v1/channels/{}/tts",
            env.api,
            uuid::Uuid::now_v7()
        ))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"text": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let r = http
            .post(format!("{}/v1/channels/{}/tts", env.api, spoken))
            .header("x-api-key", &api_key2)
            .json(&serde_json::json!({"text": "other tenant"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            404,
            "another tenant cannot announce into our channel"
        );
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation check");
    }

    for p in [&mut alice, &mut bob, &mut carol] {
        let _ = p.ws.close(None).await;
    }
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens the node-local pull WebSocket for `channel_id` with the tenant's API key.
async fn open_pull(env: &Env, api_key: &str, channel_id: ChannelId, query: &str) -> WsClient {
    let url = format!(
        "{}/v1/channels/{channel_id}/audio/streams/pull{query}",
        env.api.replacen("http", "ws", 1)
    );
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("x-api-key", api_key.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("pull websocket upgrade");
    ws
}

/// Collects frames from a stream socket for `wait`, decoding binary ones.
async fn collect_stream(
    ws: &mut WsClient,
    wait: Duration,
) -> (
    Vec<aurix_recording::live::ControlFrame>,
    Vec<(UserId, u8, u8, Vec<u8>)>,
) {
    let deadline = tokio::time::Instant::now() + wait;
    let mut control = Vec::new();
    let mut audio = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                control.push(serde_json::from_str(&t).expect("control frame"));
            }
            Ok(Some(Ok(Message::Binary(b)))) => {
                let f = aurix_recording::live::decode_frame(&b).expect("audio frame");
                audio.push((f.user_id, f.codec, f.flags, f.payload.to_vec()));
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => break,
            Ok(Some(Ok(_))) => {}
            Err(_) => break,
        }
    }
    (control, audio)
}

fn pcm_rms(payload: &[u8]) -> f32 {
    let samples: Vec<f32> = payload
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
        .collect();
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Local WebSocket ingest endpoint for push streams: records the `Authorization` header of
/// each connection and forwards every message.
#[allow(clippy::result_large_err)]
async fn start_push_receiver() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<Option<String>>,
    tokio::sync::mpsc::UnboundedReceiver<Message>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ingest", listener.local_addr().unwrap());
    let (auth_tx, auth_rx) = tokio::sync::mpsc::unbounded_channel();
    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (tcp, _) = listener.accept().await.unwrap();
            let auth_tx = auth_tx.clone();
            let msg_tx = msg_tx.clone();
            tokio::spawn(async move {
                let mut auth = None;
                let ws = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                        auth = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        Ok(resp)
                    },
                )
                .await
                .unwrap();
                let _ = auth_tx.send(auth);
                let (_sink, mut source) = ws.split();
                while let Some(Ok(m)) = source.next().await {
                    let _ = msg_tx.send(m);
                }
            });
        }
    });
    (url, auth_rx, msg_rx)
}

/// Live audio streams: a node-local pull WebSocket and a push connection to an operator
/// endpoint receive framed Opus/PCM per participant, gated by consent (pending/declined
/// participants and E2EE frames never leave the node), scoped to the tenant, announced over
/// SSE, and closed when the consumer disconnects or the operator deletes the stream.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn live_audio_streams_pull_push_consent_and_isolation() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let (env, _app_id) = isolated_env(&base, &http, "live-streams").await;
    let probe = http
        .get(format!("{}/v1/audio/streams", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    if probe.status() == 400 {
        eprintln!("recording.live.enabled is false on this server; skipping");
        return;
    }
    assert_eq!(probe.status(), 200, "{}", probe.text().await.unwrap());

    let channel_id = create_channel(&env, &http).await;
    let (tok_a, uid_a) = issue_token(&env, &http, "live-alice", "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "live-bob", "Bob", channel_id).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    join(&mut alice, channel_id).await;
    join(&mut bob, channel_id).await;
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;
    drain_ws(&mut alice).await;
    drain_ws(&mut bob).await;
    let mut sse = SseClient::open(
        &env,
        &env.api_key,
        Some("audio_stream.started,audio_stream.stopped"),
    )
    .await
    .unwrap();

    // No key -> 401; the upgrade must not open a stream.
    let r = http
        .get(format!(
            "{}/v1/channels/{channel_id}/audio/streams/pull",
            env.api
        ))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // ---- Pull (Opus) ----
    let mut pull = open_pull(&env, &env.api_key, channel_id, "?format=opus&label=e2e").await;
    let (ctl, _) = collect_stream(&mut pull, Duration::from_millis(500)).await;
    let stream_id = match ctl.first() {
        Some(aurix_recording::live::ControlFrame::Hello {
            stream_id,
            consent_required,
            format,
            label,
            ..
        }) => {
            assert!(*consent_required, "dev server requires consent by default");
            assert_eq!(*format, aurix_recording::live::StreamFormat::Opus);
            assert_eq!(label.as_deref(), Some("e2e"));
            *stream_id
        }
        other => panic!("expected hello, got {other:?}"),
    };
    let started = sse
        .expect_stream("audio_stream.started", stream_id, Duration::from_secs(5))
        .await;
    assert_eq!(started["data"]["mode"], "pull");
    for p in [&mut alice, &mut bob] {
        expect_within(p, "live RecordingNotification", Duration::from_secs(3), |m| {
            matches!(m, ControlMessage::RecordingNotification { active: true, live: true, recording_id, .. } if *recording_id == stream_id)
        })
        .await;
    }

    // Pending consent: Alice's audio stays on the node.
    let tone = opus_tone(440.0, 400);
    stream_frames(&alice, channel_id, 1, &tone[..5], false).await;
    let (_, audio) = collect_stream(&mut pull, Duration::from_millis(400)).await;
    assert!(
        audio.is_empty(),
        "frames leaked before consent: {}",
        audio.len()
    );

    alice
        .send(&ControlMessage::RecordingConsentResponse {
            recording_id: stream_id,
            consent: RecordingConsent::Accepted,
        })
        .await;
    bob.send(&ControlMessage::RecordingConsentResponse {
        recording_id: stream_id,
        consent: RecordingConsent::Declined,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (ctl, _) = collect_stream(&mut pull, Duration::from_millis(200)).await;
    assert!(
        ctl.iter().any(|c| matches!(c, aurix_recording::live::ControlFrame::Participant { user_id, consent: Some(RecordingConsent::Accepted), .. } if *user_id == uid_a)),
        "consumer is told about Alice's consent: {ctl:?}"
    );

    // Alice (accepted) is forwarded as Opus; Bob (declined) and Alice's E2EE frames are not.
    let e2ee_payload = Bytes::from_static(b"\xF8\xFF\xFEe2ee-secret");
    tokio::join!(
        async {
            stream_frames(&alice, channel_id, 10, &tone[5..15], false).await;
            stream_frames(
                &alice,
                channel_id,
                20,
                &[e2ee_payload.clone(), e2ee_payload.clone()],
                true,
            )
            .await;
        },
        stream_frames(&bob, channel_id, 1, &tone[..10], false),
    );
    let (_, audio) = collect_stream(&mut pull, Duration::from_millis(500)).await;
    let from_alice: Vec<_> = audio.iter().filter(|a| a.0 == uid_a).collect();
    assert!(
        from_alice.len() >= 8,
        "expected Alice's Opus frames, got {}",
        from_alice.len()
    );
    assert!(from_alice
        .iter()
        .all(|a| a.1 == aurix_recording::live::CODEC_OPUS));
    assert!(
        from_alice[0].2 & aurix_recording::live::FLAG_FIRST != 0,
        "first frame of a participant carries FLAG_FIRST"
    );
    assert!(
        from_alice
            .iter()
            .any(|a| tone[5..15].contains(&Bytes::from(a.3.clone()))),
        "forwarded payloads are the original Opus packets"
    );
    assert!(
        audio.iter().all(|a| a.0 != uid_b),
        "declined participant's audio leaked"
    );
    assert!(
        audio.iter().all(|a| a.3 != e2ee_payload.as_ref()),
        "E2EE frames leaked into the live stream"
    );
    while bob.recv_udp().await.is_some() {}
    while alice.recv_udp().await.is_some() {}

    // Status is tenant-scoped and never shows another tenant's streams.
    let list: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{channel_id}/audio/streams",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let streams = list["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0]["id"], stream_id.to_string());
    assert_eq!(streams[0]["state"], "streaming");
    assert!(streams[0]["frames_sent"].as_u64().unwrap() >= 8);
    assert_eq!(streams[0]["consent"][&uid_a.to_string()], "accepted");
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let r = http
            .get(format!(
                "{}/v1/channels/{channel_id}/audio/streams/{stream_id}",
                env.api
            ))
            .header("x-api-key", &api_key2)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "other tenant must not see the stream");
        let r = http
            .delete(format!(
                "{}/v1/channels/{channel_id}/audio/streams/{stream_id}",
                env.api
            ))
            .header("x-api-key", &api_key2)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "other tenant must not close the stream");
        let other: serde_json::Value = http
            .get(format!("{}/v1/audio/streams", env.api))
            .header("x-api-key", &api_key2)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(other["streams"].as_array().unwrap().len(), 0);
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
    }

    // ---- Push (PCM) to a local ingest endpoint ----
    let (ingest_url, mut auth_rx, mut ingest_rx) = start_push_receiver().await;
    let r = http
        .post(format!(
            "{}/v1/channels/{channel_id}/audio/streams",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "url": ingest_url.replacen("ws://", "ws://user:pw@", 1),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "URL credentials are rejected");
    let r = http
        .post(format!(
            "{}/v1/channels/{channel_id}/audio/streams",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "url": format!("{ingest_url}?token=q"),
            "headers": {"Authorization": "Bearer push-secret-123"},
            "format": "pcm_s16le",
            "users": [uid_a],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "{}", r.text().await.unwrap());
    let body = r.text().await.unwrap();
    assert!(
        !body.contains("push-secret-123") && !body.contains("token=q"),
        "push credentials leaked into the API response: {body}"
    );
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let push_id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(created["mode"], "push");
    assert_eq!(created["format"], "pcm_s16le");
    assert_eq!(created["push_url"], ingest_url);
    let started = sse
        .expect_stream("audio_stream.started", push_id, Duration::from_secs(5))
        .await;
    assert_eq!(started["data"]["mode"], "push");
    assert!(
        started["data"].get("headers").is_none() && !started.to_string().contains("push-secret")
    );

    let auth = tokio::time::timeout(Duration::from_secs(5), auth_rx.recv())
        .await
        .expect("push connected")
        .unwrap();
    assert_eq!(auth.as_deref(), Some("Bearer push-secret-123"));
    let hello = tokio::time::timeout(Duration::from_secs(5), ingest_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let hello: aurix_recording::live::ControlFrame =
        serde_json::from_str(hello.to_text().unwrap()).unwrap();
    assert!(matches!(
        hello,
        aurix_recording::live::ControlFrame::Hello { stream_id, format: aurix_recording::live::StreamFormat::PcmS16le, users: Some(ref u), .. }
            if stream_id == push_id && u == &vec![uid_a]
    ));
    // A second capture means a second consent request for the participants.
    expect_within(
        &mut alice,
        "push RecordingNotification",
        Duration::from_secs(3),
        |m| {
            matches!(m, ControlMessage::RecordingNotification { active: true, live: true, recording_id, .. } if *recording_id == push_id)
        },
    )
    .await;
    alice
        .send(&ControlMessage::RecordingConsentResponse {
            recording_id: push_id,
            consent: RecordingConsent::Accepted,
        })
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::join!(
        stream_frames(&alice, channel_id, 200, &tone[..15], false),
        stream_frames(&bob, channel_id, 200, &tone[..15], false),
    );
    while bob.recv_udp().await.is_some() {}
    while alice.recv_udp().await.is_some() {}
    let mut pcm_frames = Vec::new();
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(300), ingest_rx.recv()).await
    {
        if let Message::Binary(b) = m {
            let f = aurix_recording::live::decode_frame(&b).unwrap();
            assert_eq!(f.codec, aurix_recording::live::CODEC_PCM_S16LE);
            assert_eq!(f.user_id, uid_a, "user filter admits only Alice");
            assert_eq!(f.payload.len(), 960 * 2, "20 ms mono s16le");
            pcm_frames.push(f.payload.to_vec());
        }
    }
    assert!(
        pcm_frames.len() >= 10,
        "expected PCM frames over push, got {}",
        pcm_frames.len()
    );
    let rms = pcm_rms(&pcm_frames[pcm_frames.len() - 1]);
    assert!(
        (0.2..0.3).contains(&rms),
        "decoded PCM RMS {rms} should match the 0.35-amplitude tone (~0.247)"
    );
    // The pull consumer meanwhile also got Alice's Opus (both taps share the packet path).
    let (_, audio) = collect_stream(&mut pull, Duration::from_millis(300)).await;
    assert!(audio.iter().any(|a| a.0 == uid_a));

    // Delete the push stream: the ingest gets `end`, status disappears, SSE reports it.
    let closed: serde_json::Value = http
        .delete(format!(
            "{}/v1/channels/{channel_id}/audio/streams/{push_id}",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(closed["frames_sent"].as_u64().unwrap() >= 10);
    let mut ended = false;
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(2), ingest_rx.recv()).await {
        if let Message::Text(t) = m {
            if let Ok(aurix_recording::live::ControlFrame::End { reason, .. }) =
                serde_json::from_str(&t)
            {
                assert_eq!(reason, "operator");
                ended = true;
                break;
            }
        }
    }
    assert!(ended, "ingest endpoint did not receive the end frame");
    let stopped = sse
        .expect_stream("audio_stream.stopped", push_id, Duration::from_secs(5))
        .await;
    assert_eq!(stopped["data"]["reason"], "operator");
    let r = http
        .get(format!(
            "{}/v1/channels/{channel_id}/audio/streams/{push_id}",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    for p in [&mut alice, &mut bob] {
        expect_within(p, "push stream stopped notification", Duration::from_secs(3), |m| {
            matches!(m, ControlMessage::RecordingNotification { active: false, recording_id, .. } if *recording_id == push_id)
        })
        .await;
    }

    // Closing the pull socket ends that stream too.
    pull.close(None).await.unwrap();
    let stopped = sse
        .expect_stream("audio_stream.stopped", stream_id, Duration::from_secs(5))
        .await;
    assert_eq!(stopped["data"]["reason"], "consumer_disconnected");
    let list: serde_json::Value = http
        .get(format!(
            "{}/v1/channels/{channel_id}/audio/streams",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["streams"].as_array().unwrap().len(), 0);

    for p in [&mut alice, &mut bob] {
        let _ = p.ws.close(None).await;
    }
}

/// Live streams are node-local, but a cascaded channel is not: with Bob on node 1 and Alice
/// on node 2, a pull stream opened on node 1 must receive Alice's relayed frames once her
/// consent — given through node 2's control WebSocket — has been handed over the bus to the
/// node hosting the stream. Opening a stream on a node that hosts none of the participants is
/// refused with 409 instead of silently producing an empty stream.
#[tokio::test]
#[ignore = "requires two running Aurix nodes; see README (Scaling)"]
async fn live_audio_stream_follows_cascaded_participants() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let Ok(ws2) = std::env::var("AURIX_E2E_WS2") else {
        eprintln!("AURIX_E2E_WS2 not set; skipping");
        return;
    };
    let env2 = Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws: ws2,
        api_key: env.api_key.clone(),
    };
    let http = reqwest::Client::new();
    let probe = http
        .get(format!("{}/v1/audio/streams", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    if probe.status() == 400 {
        eprintln!("recording.live.enabled is false on this server; skipping");
        return;
    }

    let channel_id = create_channel(&env, &http).await;
    let (tok_a, uid_a) = issue_token(&env2, &http, "xlive-alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env, &http, "xlive-bob", "Bob", channel_id).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let mut alice = connect(&env2, "alice", tok_a).await;
    join(&mut alice, channel_id).await;
    bind_media(&mut alice).await;
    drain_ws(&mut alice).await;

    // Only Alice (node 2) is in the channel: node 1 refuses to open a stream it cannot feed.
    let r = http
        .post(format!(
            "{}/v1/channels/{channel_id}/audio/streams",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"url": "wss://example.invalid/sink"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "{}", r.text().await.unwrap());

    let mut bob = connect(&env, "bob", tok_b).await;
    assert_ne!(
        alice.media_addr.port(),
        bob.media_addr.port(),
        "players must land on different nodes"
    );
    join(&mut bob, channel_id).await;
    bind_media(&mut bob).await;
    drain_ws(&mut bob).await;
    drain_ws(&mut alice).await;

    let mut pull = open_pull(&env, &env.api_key, channel_id, "").await;
    let (ctl, _) = collect_stream(&mut pull, Duration::from_millis(500)).await;
    let stream_id = match ctl.first() {
        Some(aurix_recording::live::ControlFrame::Hello { stream_id, .. }) => *stream_id,
        other => panic!("expected hello, got {other:?}"),
    };
    // Alice, on the other node, is told about the capture too.
    expect_within(
        &mut alice,
        "live RecordingNotification on the remote node",
        Duration::from_secs(5),
        |m| {
            matches!(m, ControlMessage::RecordingNotification { recording_id, active: true, live: true, .. } if *recording_id == stream_id)
        },
    )
    .await;

    let tone = opus_tone(440.0, 400);
    stream_frames(&alice, channel_id, 1, &tone[..5], false).await;
    let (_, audio) = collect_stream(&mut pull, Duration::from_millis(400)).await;
    assert!(audio.is_empty(), "relayed frames leaked before consent");

    alice
        .send(&ControlMessage::RecordingConsentResponse {
            recording_id: stream_id,
            consent: RecordingConsent::Accepted,
        })
        .await;
    let (ctl, _) = collect_stream(&mut pull, Duration::from_secs(3)).await;
    assert!(
        ctl.iter().any(|c| matches!(c, aurix_recording::live::ControlFrame::Participant { user_id, consent: Some(RecordingConsent::Accepted), .. } if *user_id == uid_a)),
        "consent relayed from node 2 reaches the hosting node: {ctl:?}"
    );

    stream_frames(&alice, channel_id, 10, &tone[5..15], false).await;
    let (_, audio) = collect_stream(&mut pull, Duration::from_millis(500)).await;
    let from_alice = audio.iter().filter(|a| a.0 == uid_a).count();
    assert!(
        from_alice >= 8,
        "expected Alice's relayed Opus frames, got {from_alice}"
    );

    // Alice leaving on node 2 ends her participant state on node 1's stream.
    alice
        .send(&ControlMessage::ChannelLeave { channel_id })
        .await;
    let (ctl, _) = collect_stream(&mut pull, Duration::from_secs(3)).await;
    assert!(
        ctl.iter().any(|c| matches!(c, aurix_recording::live::ControlFrame::Participant { user_id, event: aurix_recording::live::ParticipantEvent::Left, .. } if *user_id == uid_a)),
        "participant-left from the remote node: {ctl:?}"
    );
    pull.close(None).await.ok();
    bob.send(&ControlMessage::ChannelLeave { channel_id }).await;
}

// ── Network quality ──

/// The server merges each client's `QualityReport` (downlink, loss in percent) with the uplink
/// loss/jitter/bitrate the SFU measures from sequence gaps into `NetworkQuality` (1–5 bars,
/// worse direction wins). A lossy report still triggers the `BitrateCommand` adaptation and a
/// `quality.alert`; heavy uplink loss raises an `uplink_packet_loss` alert on its own.
/// `GET /v1/sessions/:id/stats` exposes the same numbers to the session's tenant only.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn network_quality_bars_bitrate_adaptation_and_session_stats() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let (env, _app_id) = isolated_env(&base, &http, "quality").await;
    let channel_id = create_channel(&env, &http).await;
    let (tok_a, uid_a) = issue_token(&env, &http, "quality:alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env, &http, "quality:bob", "Bob", channel_id).await;
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;
    join(&mut alice, channel_id).await;
    join(&mut bob, channel_id).await;
    let mut sse = SseClient::open(&env, &env.api_key, Some("quality.alert"))
        .await
        .expect("open SSE");

    // A clean link: the first evaluation after joining yields 5 bars and no uplink loss.
    let clean = expect_within(&mut alice, "NetworkQuality", Duration::from_secs(12), |m| {
        matches!(m, ControlMessage::NetworkQuality { .. })
    })
    .await;
    let ControlMessage::NetworkQuality { quality } = clean else {
        unreachable!()
    };
    assert_eq!(quality.bars, 5, "{quality:?}");
    assert_eq!(quality.uplink_packets_lost, 0, "{quality:?}");
    assert!(
        quality.r_factor >= 80.0 && quality.mos >= 4.0,
        "{quality:?}"
    );

    // Bob's downlink report: 25 % loss (a percentage, not a fraction) → 16 kbps command and a
    // quality.alert with the reported value; the merged quality drops to 1 bar.
    bob.send(&ControlMessage::QualityReport {
        rtt_ms: 40.0,
        jitter_ms: 5.0,
        packet_loss: 25.0,
    })
    .await;
    let cmd = expect_within(&mut bob, "BitrateCommand", Duration::from_secs(5), |m| {
        matches!(m, ControlMessage::BitrateCommand { .. })
    })
    .await;
    assert!(
        matches!(
            cmd,
            ControlMessage::BitrateCommand {
                target_bitrate_kbps: 16,
                ..
            }
        ),
        "{cmd:?}"
    );
    let alert = sse.expect("quality.alert", Duration::from_secs(10)).await;
    assert_eq!(alert["data"]["metric"], "packet_loss", "{alert}");
    assert_eq!(
        alert["data"]["session_id"].as_str(),
        Some(bob.session_id.to_string().as_str()),
        "{alert}"
    );
    assert_eq!(alert["data"]["value"].as_f64(), Some(25.0), "{alert}");
    let degraded = expect_within(
        &mut bob,
        "NetworkQuality(1 bar)",
        Duration::from_secs(12),
        |m| matches!(m, ControlMessage::NetworkQuality { quality } if quality.bars == 1),
    )
    .await;
    let ControlMessage::NetworkQuality { quality } = degraded else {
        unreachable!()
    };
    assert_eq!(quality.downlink_loss_percent, 25.0, "{quality:?}");
    assert_eq!(quality.rtt_ms, 40.0, "{quality:?}");
    assert!(quality.r_factor < 50.0, "{quality:?}");

    // Alice's uplink with every second batch of frames missing: the SFU sees the sequence gaps.
    let tone = Bytes::from_static(&[0xFC, 0xEC, 0x40, 7, 7, 7, 7]);
    for first in [1u32, 21, 41, 61, 81, 101] {
        send_audio(&alice, channel_id, first, &tone).await;
    }
    let lossy = expect_within(&mut alice, "NetworkQuality(uplink loss)", Duration::from_secs(12), |m| {
        matches!(m, ControlMessage::NetworkQuality { quality } if quality.uplink_loss_percent > 20.0)
    })
    .await;
    let ControlMessage::NetworkQuality { quality } = lossy else {
        unreachable!()
    };
    assert!(quality.uplink_packets_lost >= 10, "{quality:?}");
    assert!(quality.uplink_packets_received >= 10, "{quality:?}");
    assert!(quality.uplink_bitrate_kbps > 0, "{quality:?}");
    assert!(quality.bars <= 2, "{quality:?}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let alert = sse.expect("quality.alert", left).await;
        if alert["data"]["metric"] == "uplink_packet_loss"
            && alert["data"]["session_id"].as_str() == Some(alice.session_id.to_string().as_str())
        {
            assert!(alert["data"]["value"].as_f64().unwrap() > 20.0, "{alert}");
            assert_eq!(
                alert["data"]["user_id"].as_str(),
                Some(uid_a.as_str()),
                "{alert}"
            );
            break;
        }
    }

    // REST view of the same session, scoped to its tenant.
    let path = format!("/v1/sessions/{}/stats", bob.session_id);
    let stats: serde_json::Value = http
        .get(format!("{}{path}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stats["client_report"]["packet_loss_percent"], 25.0,
        "{stats}"
    );
    assert_eq!(stats["quality"]["bars"], 1, "{stats}");
    assert_eq!(
        stats["channels"].as_array().map(|c| c.len()),
        Some(1),
        "{stats}"
    );
    assert!(stats["packets_sent"].as_u64().unwrap() >= 50, "{stats}");
    assert_eq!(
        status_of(
            &env,
            &http,
            &format!("/v1/sessions/{}/stats", uuid::Uuid::new_v4())
        )
        .await,
        404
    );
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let status = http
            .get(format!("{}{path}", env.api))
            .header("x-api-key", &api_key2)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 404, "another tenant must not read session stats");
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation check");
    }

    for p in [&mut alice, &mut bob] {
        p.send(&ControlMessage::ChannelLeave { channel_id }).await;
    }
}

// ── Channel audio policy ──

fn expect_bitrate(m: ControlMessage) -> (u32, u8) {
    match m {
        ControlMessage::BitrateCommand {
            target_bitrate_kbps,
            expected_loss_percent,
            ..
        } => (target_bitrate_kbps, expected_loss_percent),
        other => panic!("expected BitrateCommand, got {other:?}"),
    }
}

async fn expect_audio_policy(p: &mut Player, channel_id: ChannelId) -> AudioPolicy {
    let m = expect_within(p, "ChannelAudioPolicy", Duration::from_secs(5), |m| {
        matches!(m, ControlMessage::ChannelAudioPolicy { channel_id: c, .. } if *c == channel_id)
    })
    .await;
    match m {
        ControlMessage::ChannelAudioPolicy { audio, .. } => audio,
        _ => unreachable!(),
    }
}

/// The channel's Opus settings (bitrate, floor, FEC/DTX, bandwidth, complexity, signal) travel to
/// clients as `ChannelJoinAck.audio`; a `PUT /v1/channels/:id` is validated against the node's
/// media limits, then fans out live as `ChannelAudioPolicy` to every joined session on every
/// node. Adaptive `BitrateCommand`s never go below the policy floor nor above its target, carry
/// the reported loss, and recovery returns to the (possibly updated) target.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn channel_audio_policy_join_ack_live_update_and_bitrate_bounds() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let http = reqwest::Client::new();

    let channel_id = create_channel_with(
        &env,
        &http,
        serde_json::json!({
            "bitrate": 40000, "min_bitrate": 24000,
            "enable_fec": false, "enable_dtx": false,
            "max_bandwidth": "wideband", "complexity": 6,
            "audio_profile": "music"
        }),
    )
    .await;
    let initial = AudioPolicy {
        bitrate_bps: 40_000,
        min_bitrate_bps: 24_000,
        fec: false,
        dtx: false,
        max_bandwidth: OpusBandwidth::Wideband,
        complexity: Some(6),
        signal: OpusSignal::Music,
        stereo: false,
    };

    let (tok_a, _) = issue_token(&env, &http, "opus:alice", "Alice", channel_id).await;
    let bob_env = env2.as_ref().unwrap_or(&env);
    let (tok_b, _) = issue_token(bob_env, &http, "opus:bob", "Bob", channel_id).await;
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(bob_env, "bob", tok_b).await;
    if env2.is_some() {
        assert_ne!(
            alice.media_addr.port(),
            bob.media_addr.port(),
            "players must land on different nodes"
        );
    }
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;

    for p in [&mut alice, &mut bob] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id,
            token: tok,
        })
        .await;
        let ack = p
            .expect("ChannelJoinAck", |m| {
                matches!(m, ControlMessage::ChannelJoinAck { channel_id: c, .. } if *c == channel_id)
            })
            .await;
        let ControlMessage::ChannelJoinAck { audio, .. } = ack else {
            unreachable!()
        };
        assert_eq!(
            audio, initial,
            "{}: join ack carries the channel policy",
            p.name
        );
    }

    // 25 % loss would normally ask for 16 kbps; the channel floor of 24 kbps wins.
    bob.send(&ControlMessage::QualityReport {
        rtt_ms: 30.0,
        jitter_ms: 5.0,
        packet_loss: 25.0,
    })
    .await;
    let cmd = expect_within(&mut bob, "BitrateCommand", Duration::from_secs(5), |m| {
        matches!(m, ControlMessage::BitrateCommand { .. })
    })
    .await;
    assert_eq!(expect_bitrate(cmd), (24, 25));

    // Out-of-range settings are rejected before anything is stored or broadcast.
    for bad in [
        serde_json::json!({"bitrate": 48000, "complexity": 11}),
        serde_json::json!({"bitrate": 48000, "min_bitrate": 64000}),
        serde_json::json!({"bitrate": 48000, "sample_rate": 44100}),
        serde_json::json!({"bitrate": 1000}),
    ] {
        let status = http
            .put(format!("{}/v1/channels/{channel_id}/config", env.api))
            .header("x-api-key", &env.api_key)
            .json(&bad)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 400, "{bad} must be rejected");
    }
    assert_none_matching(
        &mut alice,
        Duration::from_millis(500),
        "rejected updates must not broadcast a policy",
        |m| matches!(m, ControlMessage::ChannelAudioPolicy { .. }),
    )
    .await;

    // A valid update reaches both nodes' sessions as ChannelAudioPolicy.
    let updated: serde_json::Value = http
        .put(format!("{}/v1/channels/{channel_id}/config", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "bitrate": 64000, "min_bitrate": 16000,
            "enable_fec": true, "enable_dtx": true,
            "max_bandwidth": "fullband",
            "audio_profile": "voice"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["config"]["min_bitrate"], 16000, "{updated}");
    assert_eq!(updated["config"]["max_bandwidth"], "fullband", "{updated}");
    let expected = AudioPolicy {
        bitrate_bps: 64_000,
        min_bitrate_bps: 16_000,
        fec: true,
        dtx: true,
        max_bandwidth: OpusBandwidth::Fullband,
        complexity: None,
        signal: OpusSignal::Voice,
        stereo: false,
    };
    assert_eq!(expect_audio_policy(&mut alice, channel_id).await, expected);
    assert_eq!(expect_audio_policy(&mut bob, channel_id).await, expected);

    // The new floor lets the same loss report go down to 16 kbps; a clean report recovers to the
    // new 64 kbps target rather than the original 40.
    bob.send(&ControlMessage::QualityReport {
        rtt_ms: 30.0,
        jitter_ms: 5.0,
        packet_loss: 25.0,
    })
    .await;
    let cmd = expect_within(
        &mut bob,
        "BitrateCommand(16)",
        Duration::from_secs(5),
        |m| matches!(m, ControlMessage::BitrateCommand { .. }),
    )
    .await;
    assert_eq!(expect_bitrate(cmd), (16, 25));
    bob.send(&ControlMessage::QualityReport {
        rtt_ms: 30.0,
        jitter_ms: 5.0,
        packet_loss: 0.0,
    })
    .await;
    let cmd = expect_within(
        &mut bob,
        "BitrateCommand(64)",
        Duration::from_secs(5),
        |m| matches!(m, ControlMessage::BitrateCommand { .. }),
    )
    .await;
    assert_eq!(expect_bitrate(cmd), (64, 0));

    // A late joiner sees the updated policy in its ack.
    let (tok_c, _) = issue_token(&env, &http, "opus:carol", "Carol", channel_id).await;
    let mut carol = connect(&env, "carol", tok_c).await;
    let tok = carol.token.clone();
    carol
        .send(&ControlMessage::ChannelJoin {
            channel_id,
            token: tok,
        })
        .await;
    let ack = carol
        .expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    let ControlMessage::ChannelJoinAck { audio, .. } = ack else {
        unreachable!()
    };
    assert_eq!(audio, expected);

    for p in [&mut alice, &mut bob, &mut carol] {
        p.send(&ControlMessage::ChannelLeave { channel_id }).await;
    }
}

/// Content safety against the mock classifier (`examples/mock_speech.rs`, `/v1/moderations`).
/// Opt in with `AURIX_E2E_SAFETY=1` against a node configured like the CI "speech + content
/// safety" step: `[stt]` + `safety.enabled`, `configs/lexicon.example.toml`,
/// `text.auto_mute = "elevated"`, `voice.auto_kick = "high"`, recording storage on (evidence),
/// default thresholds (incident 0.7, elevated 1.0, high 2.5).
///
/// Chat runs lexicon → classifier → delivery: clean text passes, `shit` is masked, `kys` is
/// blocked with a self-harm incident, `rude` is delivered but recorded, the second incident
/// lifts the user to `elevated` and the pipeline server-mutes them through the moderation
/// primitives (attributed to the system actor); a failing classifier fails open. Voice runs
/// only for channels with `safety_voice` (disclosed in the join ack, no `Transcript`s are
/// delivered for safety-only channels), skips E2EE frames, stores an encrypted Ogg/Opus
/// evidence clip and, at `high`, kicks. Incidents are listed/exported over REST with the
/// clip inline for `recordings:read`, are tenant-scoped, and reach SSE from every node.
#[tokio::test]
#[ignore = "requires a running Aurix server with [safety] pointed at examples/mock_speech.rs"]
async fn content_safety_incidents_evidence_and_auto_actions() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    if std::env::var("AURIX_E2E_SAFETY").is_err() {
        eprintln!("AURIX_E2E_SAFETY not set; skipping (node must run with [safety] enabled)");
        return;
    }
    let http = reqwest::Client::new();
    let (env, _app_id) = isolated_env(&base, &http, "safety").await;
    let env2 = match std::env::var("AURIX_E2E_WS2") {
        Ok(ws) => Env {
            api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
            ws,
            api_key: env.api_key.clone(),
        },
        Err(_) => {
            eprintln!("AURIX_E2E_WS2 not set; running Carol on the same node");
            env.clone()
        }
    };

    let monitored =
        create_channel_with(&env, &http, serde_json::json!({"safety_voice": true})).await;
    let plain = create_channel(&env, &http).await;
    // Risk is per user and persisted; fresh users keep reruns independent.
    let run = uuid::Uuid::now_v7().simple().to_string();
    let (tok_a, uid_a) = issue_token_for(
        &env,
        &http,
        &format!("safety:{run}:alice"),
        "Alice",
        &[monitored, plain],
    )
    .await;
    let (tok_b, _) = issue_token_for(
        &env,
        &http,
        &format!("safety:{run}:bob"),
        "Bob",
        &[monitored, plain],
    )
    .await;
    let (tok_c, uid_c) = issue_token(
        &env2,
        &http,
        &format!("safety:{run}:carol"),
        "Carol",
        monitored,
    )
    .await;
    let alice_id = UserId::from_uuid(uid_a.parse().unwrap());
    let carol_id = UserId::from_uuid(uid_c.parse().unwrap());
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut carol = connect(&env2, "carol", tok_c).await;
    for p in [&mut alice, &mut bob] {
        bind_media(p).await;
    }

    // The join ack discloses safety monitoring; a safety-only channel is not "transcribed".
    async fn join_expecting(p: &mut Player, ch: ChannelId, monitored: bool) {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id: ch,
            token: tok,
        })
        .await;
        let ack = p
            .expect("ChannelJoinAck", |m| {
                matches!(m, ControlMessage::ChannelJoinAck { channel_id, .. } if *channel_id == ch)
            })
            .await;
        let ControlMessage::ChannelJoinAck {
            transcription,
            safety_voice,
            ..
        } = ack
        else {
            unreachable!()
        };
        assert_eq!(safety_voice, monitored, "{}: join ack for {ch}", p.name);
        assert!(
            !transcription,
            "{}: safety-only channels deliver no transcripts",
            p.name
        );
    }
    join_expecting(&mut alice, monitored, true).await;
    join_expecting(&mut alice, plain, false).await;
    join_expecting(&mut bob, monitored, true).await;
    join_expecting(&mut bob, plain, false).await;
    join_expecting(&mut carol, monitored, true).await;
    alice
        .send(&ControlMessage::SetTransmission {
            mode: TransmissionMode::All,
        })
        .await;
    alice
        .expect("TransmissionChanged", |m| {
            matches!(m, ControlMessage::TransmissionChanged { .. })
        })
        .await;
    for p in [&mut alice, &mut bob, &mut carol] {
        drain_ws(p).await;
    }

    let mut sse = SseClient::open(
        &env,
        &env.api_key,
        Some("safety.incident,safety.risk_changed,participant.muted"),
    )
    .await
    .unwrap();
    // Events about other tests' users may interleave; wait for the one about `user`.
    async fn expect_sse_for(
        sse: &mut SseClient,
        event: &str,
        user: &str,
        what: &str,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let (ev, data) = sse
                .next(left)
                .await
                .unwrap_or_else(|| panic!("no `{event}` SSE frame for {what}"));
            if ev == event && data["data"]["user_id"].as_str() == Some(user) {
                return data["data"].clone();
            }
        }
    }
    async fn expect_incident(sse: &mut SseClient, user: &str, what: &str) -> serde_json::Value {
        expect_sse_for(sse, "safety.incident", user, what).await
    }
    async fn expect_risk_change(sse: &mut SseClient, user: &str, to: &str) -> serde_json::Value {
        let data = expect_sse_for(sse, "safety.risk_changed", user, to).await;
        assert_eq!(data["risk_level"], to, "{data}");
        data
    }
    let risk = |user: &str| {
        let http = http.clone();
        let env = env.clone();
        let user = user.to_string();
        async move {
            let r: serde_json::Value = http
                .get(format!("{}/v1/safety/users/{user}/risk", env.api))
                .header("x-api-key", &env.api_key)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            r
        }
    };
    let r0 = risk(&uid_a).await;
    assert_eq!(r0["risk_level"], "none", "{r0}");
    assert_eq!(r0["incidents"], 0);

    // ── chat: clean → delivered untouched; profanity → masked ──
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: monitored,
            text: "gg wp".into(),
            metadata: None,
            client_ref: Some("s-1".into()),
        })
        .await;
    assert_eq!(expect_chat(&mut alice, "clean echo").await.text, "gg wp");
    assert_eq!(expect_chat(&mut bob, "clean").await.text, "gg wp");
    assert_eq!(
        expect_chat(&mut carol, "clean cross-node").await.text,
        "gg wp"
    );
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: monitored,
            text: "oh Sh1t that hurt".into(),
            metadata: None,
            client_ref: Some("s-2".into()),
        })
        .await;
    let masked = expect_chat(&mut alice, "masked echo").await;
    assert_eq!(
        masked.text, "oh **** that hurt",
        "lexicon mask through obfuscation"
    );
    assert_eq!(masked.client_ref.as_deref(), Some("s-2"));
    assert_eq!(
        expect_chat(&mut bob, "masked").await.text,
        "oh **** that hurt"
    );
    assert_eq!(
        expect_chat(&mut carol, "masked").await.text,
        "oh **** that hurt"
    );
    let r1 = risk(&uid_a).await;
    assert_eq!(
        r1["incidents"], 0,
        "a mask below the threshold is no incident: {r1}"
    );

    // ── chat: lexicon block → MESSAGE_BLOCKED, nobody receives it, incident recorded ──
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: monitored,
            text: "just kys already".into(),
            metadata: None,
            client_ref: Some("s-3".into()),
        })
        .await;
    let m = alice
        .expect("blocked ChatSend error", |m| {
            matches!(m, ControlMessage::Error { .. })
        })
        .await;
    assert!(
        matches!(&m, ControlMessage::Error { code, client_ref, .. }
            if code == "MESSAGE_BLOCKED" && client_ref.as_deref() == Some("s-3")),
        "{m:?}"
    );
    assert_no_chat(&mut bob, "the message was blocked").await;
    let inc1 = expect_incident(&mut sse, &uid_a, "kys").await;
    assert_eq!(inc1["source"], "text", "{inc1}");
    assert_eq!(inc1["channel_id"], monitored.to_string());
    assert_eq!(inc1["text"], "just kys already");
    assert_eq!(
        inc1["classifier"], "lexicon",
        "a lexicon block needs no classifier: {inc1}"
    );
    assert!(inc1["score"].as_f64().unwrap() >= 0.89, "{inc1}");
    assert!(
        inc1["categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "self-harm"),
        "{inc1}"
    );
    assert_eq!(
        inc1["risk_level"], "low",
        "0.9 is below `risk_elevated`: {inc1}"
    );
    assert_eq!(inc1["actions"], serde_json::json!([]));
    assert!(inc1["evidence_recording_id"].is_null());
    let rc = expect_risk_change(&mut sse, &uid_a, "low").await;
    assert_eq!(rc["previous_level"], "none", "{rc}");

    // ── chat: classifier score 0.75 → delivered (below block threshold) but recorded; second
    //    incident → elevated → text.auto_mute fires through the moderation primitives ──
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: monitored,
            text: "you are so rude".into(),
            metadata: None,
            client_ref: Some("s-4".into()),
        })
        .await;
    assert_eq!(
        expect_chat(&mut bob, "recorded but delivered").await.text,
        "you are so rude"
    );
    assert_eq!(
        expect_chat(&mut carol, "recorded but delivered").await.text,
        "you are so rude"
    );
    let inc2 = expect_incident(&mut sse, &uid_a, "rude").await;
    assert_eq!(inc2["classifier"], "openai_moderation", "{inc2}");
    assert!(
        (inc2["score"].as_f64().unwrap() - 0.75).abs() < 0.01,
        "{inc2}"
    );
    assert_eq!(inc2["risk_level"], "elevated", "0.9 + 0.75 ≥ 1.0: {inc2}");
    assert_eq!(inc2["actions"], serde_json::json!(["mute"]), "{inc2}");
    expect_risk_change(&mut sse, &uid_a, "elevated").await;
    for p in [&mut alice, &mut bob] {
        expect_within(p, "automatic server mute", Duration::from_secs(5), |m| {
            matches!(m, ControlMessage::MuteStateChanged { channel_id, user_id, server_muted: true, .. }
                if *channel_id == monitored && *user_id == alice_id)
        })
        .await;
    }
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: monitored,
            text: "hello?".into(),
            metadata: None,
            client_ref: Some("s-5".into()),
        })
        .await;
    expect_error(&mut alice, "server-muted sender", "USER_MUTED").await;
    // The mute is a regular moderation action attributed to the system actor.
    let muted = expect_sse_for(&mut sse, "participant.muted", &uid_a, "auto mute").await;
    assert_eq!(muted["server_mute"], true, "{muted}");
    assert_eq!(
        muted["muted_by"],
        aurix_control::SYSTEM_USER.to_string(),
        "{muted}"
    );
    assert_eq!(muted["channel_id"], monitored.to_string());

    // Incident detail + context messages; the incident list filters by source and user.
    let detail: serde_json::Value = http
        .get(format!(
            "{}/v1/safety/incidents/{}",
            env.api,
            inc2["incident_id"].as_str().unwrap()
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["source"], "text");
    assert_eq!(detail["text"], "you are so rude");
    assert_eq!(detail["incident"]["event_type"], "safety.text");
    assert_eq!(detail["incident"]["status"], "pending");
    assert!(detail["audio"].is_null());
    let ctx_texts: Vec<&str> = detail["context"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["text"].as_str().unwrap())
        .collect();
    assert!(
        ctx_texts.contains(&"gg wp") && ctx_texts.contains(&"oh **** that hurt"),
        "delivered messages before the incident are attached as context: {ctx_texts:?}"
    );
    assert!(
        !ctx_texts.contains(&"just kys already"),
        "blocked messages are not context: {ctx_texts:?}"
    );
    let listed: serde_json::Value = http
        .get(format!(
            "{}/v1/safety/incidents?source=text&user_id={uid_a}",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");

    // A failing classifier fails open (text.fail_open = true by default).
    bob.send(&ControlMessage::ChatSend {
        channel_id: monitored,
        text: "[classifier-fail] anyone there?".into(),
        metadata: None,
        client_ref: None,
    })
    .await;
    assert_eq!(
        expect_chat(&mut carol, "fail-open delivery").await.text,
        "[classifier-fail] anyone there?"
    );
    drain_ws(&mut bob).await;

    // Carol's incident on the other node reaches this node's SSE; her risk is shared via the DB.
    carol
        .send(&ControlMessage::ChatSend {
            channel_id: monitored,
            text: "i hate all of you".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    let inc_c = expect_incident(&mut sse, &uid_c, "carol's hate").await;
    assert_eq!(inc_c["user_id"], carol_id.to_string());
    assert_eq!(inc_c["risk_level"], "low");
    let rc = risk(&uid_c).await;
    assert_eq!(rc["incidents"], 1, "{rc}");
    drain_ws(&mut bob).await;
    drain_ws(&mut carol).await;
    drain_ws(&mut alice).await;

    // ── voice: lift the mute, then speak ──
    http.post(format!("{}/v1/moderation/mute", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"user_id": uid_a, "channel_id": monitored, "muted": false}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    expect_within(&mut alice, "unmuted", Duration::from_secs(5), |m| {
        matches!(m, ControlMessage::MuteStateChanged { server_muted: false, user_id, .. } if *user_id == alice_id)
    })
    .await;
    drain_ws(&mut bob).await;

    // A clean tone, a tone in the unmonitored channel and an E2EE tone: no incidents, and a
    // safety-only channel never delivers transcripts.
    stream_frames(&alice, monitored, 1, &opus_tone(440.0, 1_600), false).await;
    stream_frames(&alice, plain, 1_000, &opus_tone(880.0, 1_600), false).await;
    stream_frames(&alice, monitored, 2_000, &opus_tone(880.0, 1_600), true).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    for p in [&mut alice, &mut bob, &mut carol] {
        assert_no_transcript(p, Duration::from_millis(400), "safety-only channel").await;
    }
    let r2 = risk(&uid_a).await;
    assert_eq!(r2["incidents"], 2, "clean / unmonitored / e2ee audio: {r2}");
    for p in [&alice, &bob] {
        drain_udp(p).await;
    }

    // Flagged speech → voice incident with an evidence clip; 0.9 + 0.75 + 0.95 ≥ 2.5 → high →
    // voice.auto_kick removes Alice from the channel.
    stream_frames(&alice, monitored, 3_000, &opus_tone(880.0, 1_600), false).await;
    let inc3 = expect_incident(&mut sse, &uid_a, "880hz").await;
    assert_eq!(inc3["source"], "voice", "{inc3}");
    assert!(
        tone_hz(inc3["text"].as_str().unwrap()).is_some_and(|hz| (hz - 880.0).abs() < 15.0),
        "the transcript is attached: {inc3}"
    );
    assert_eq!(inc3["classifier"], "openai_moderation");
    assert_eq!(inc3["risk_level"], "high", "{inc3}");
    assert_eq!(inc3["actions"], serde_json::json!(["kick"]), "{inc3}");
    let recording_id = inc3["evidence_recording_id"]
        .as_str()
        .expect("evidence clip stored")
        .to_string();
    expect_risk_change(&mut sse, &uid_a, "high").await;
    alice
        .expect("automatic kick", |m| {
            matches!(m, ControlMessage::Kick { channel_id, user_id, .. }
                if *channel_id == monitored && *user_id == alice_id)
        })
        .await;
    bob.expect("Alice removed", |m| {
        matches!(m, ControlMessage::ParticipantLeft { channel_id, user_id, .. }
            if *channel_id == monitored && *user_id == alice_id)
    })
    .await;

    // Evidence: a `kind = evidence` recording, exported inline as decrypted Ogg/Opus for a key
    // with `recordings:read`, downloadable through the recordings API.
    let export: serde_json::Value = http
        .get(format!(
            "{}/v1/safety/incidents/{}/export",
            env.api,
            inc3["incident_id"].as_str().unwrap()
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["source"], "voice");
    assert_eq!(export["incident"]["recording_id"], recording_id);
    assert_eq!(
        export["incident"]["evidence"]["audio"]["recording_id"],
        recording_id
    );
    assert!(
        export["incident"]["evidence"]["audio"]["clip_ms"]
            .as_u64()
            .unwrap()
            >= 1_000,
        "{}",
        export["incident"]["evidence"]["audio"]
    );
    let audio = &export["audio"];
    assert_eq!(audio["recording_id"], recording_id);
    assert_eq!(audio["format"], "ogg_opus");
    assert_eq!(audio["expired"], false);
    assert_eq!(
        audio["download_path"],
        format!("/v1/recordings/{recording_id}/download")
    );
    assert!(audio["duration_secs"].as_f64().unwrap() >= 1.0, "{audio}");
    use base64::Engine as _;
    let clip = base64::engine::general_purpose::STANDARD
        .decode(audio["content_base64"].as_str().expect("inline clip"))
        .unwrap();
    assert_eq!(&clip[..4], b"OggS", "decrypted Ogg container");
    assert_eq!(clip.len() as i64, audio["size_bytes"].as_i64().unwrap());
    let download = http
        .get(format!(
            "{}{}",
            env.api,
            audio["download_path"].as_str().unwrap()
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        &download[..],
        &clip[..],
        "download path serves the same clip"
    );
    let rec: serde_json::Value = http
        .get(format!("{}/v1/recordings/{recording_id}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rec["recording"]["kind"], "evidence", "{rec}");
    assert_eq!(rec["recording"]["channel_id"], monitored.to_string());

    let r3 = risk(&uid_a).await;
    assert_eq!(r3["risk_level"], "high", "{r3}");
    assert_eq!(r3["incidents"], 3);
    let all: serde_json::Value = http
        .get(format!("{}/v1/safety/incidents?user_id={uid_a}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.as_array().unwrap().len(), 3, "{all}");
    let bad = http
        .get(format!("{}/v1/safety/incidents?source=email", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // ── tenant isolation ──
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        for path in [
            format!(
                "/v1/safety/incidents/{}",
                inc3["incident_id"].as_str().unwrap()
            ),
            format!(
                "/v1/safety/incidents/{}/export",
                inc3["incident_id"].as_str().unwrap()
            ),
            format!("/v1/safety/users/{uid_a}/risk"),
            format!("/v1/recordings/{recording_id}"),
        ] {
            let r = http
                .get(format!("{}{path}", env.api))
                .header("x-api-key", &api_key2)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{path} must not exist for another tenant");
        }
        let other: serde_json::Value = http
            .get(format!("{}/v1/safety/incidents?user_id={uid_a}", env.api))
            .header("x-api-key", &api_key2)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(other.as_array().unwrap().len(), 0, "{other}");
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation checks");
    }

    for p in [&mut bob, &mut carol] {
        p.send(&ControlMessage::ChannelLeave {
            channel_id: monitored,
        })
        .await;
    }
}

async fn expect_presence(p: &mut Player, ch: ChannelId, who: UserId, joined: bool) {
    let what = if joined {
        "ParticipantJoined"
    } else {
        "ParticipantLeft"
    };
    expect_within(p, what, Duration::from_secs(3), |m| match m {
        ControlMessage::ParticipantJoined {
            channel_id,
            user_id,
            ..
        } => joined && *channel_id == ch && *user_id == who,
        ControlMessage::ParticipantLeft {
            channel_id,
            user_id,
        } => !joined && *channel_id == ch && *user_id == who,
        _ => false,
    })
    .await;
}

async fn assert_no_presence(p: &mut Player, ch: ChannelId, why: &str) {
    assert_none_matching(p, Duration::from_millis(500), why, |m| {
        matches!(
            m,
            ControlMessage::ParticipantJoined { channel_id, .. }
                | ControlMessage::ParticipantLeft { channel_id, .. }
                if *channel_id == ch
        )
    })
    .await;
}

/// Interleaves 10 labelled frames (`level` in -dBov) from every speaker — all within the
/// ambient hold window — and returns, per speaker SSRC, the last downlink gain `to` saw and
/// the frame count.
async fn ambient_burst(
    speakers: &[(&Player, u32, u8)],
    to: &Player,
    ch: ChannelId,
    payload: &Bytes,
) -> std::collections::HashMap<u32, (f32, usize)> {
    let hash = channel_id_hash(&ch);
    for i in 0..10u32 {
        for (from, first_seq, level) in speakers {
            let seq = first_seq + i;
            let pkt =
                AurixPacket::audio_with_level(seq, seq * 960, from.ssrc, hash, *level, payload);
            from.udp
                .send_to(&pkt.seal(&from.keys), from.media_addr)
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut seen: std::collections::HashMap<u32, (f32, usize)> = Default::default();
    let mut buf = vec![0u8; 2048];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(400), to.udp.recv_from(&mut buf)).await
    {
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: downlink must verify", to.name);
        if p.header.packet_type != PacketType::Audio {
            continue;
        }
        let (volume, _) = p.take_downlink_meta();
        assert_eq!(&p.payload[..], &payload[..]);
        let e = seen.entry(p.header.ssrc).or_insert((volume, 0));
        e.0 = volume;
        e.1 += 1;
    }
    seen
}

/// Positional channels with `roster_radius` / `text_radius` (`PositionalConfig`): the roster
/// starts empty and members appear (`ParticipantJoined`) once both poses are known and within
/// the roster radius, disappear (`ParticipantLeft`) only beyond the 10 % exit hysteresis, and
/// come back when they return; chat / typing follow the independent text radius (sender echo
/// always) while the audio range stays `max_radius`; leaving the channel notifies every
/// observer exactly once; a member on another node (`AURIX_E2E_WS2`) is scoped the same way.
/// Then a team channel with `ambient` keeps the loudest `max_voices` speakers at full gain and
/// dims the rest to `ambient_gain`, ranked by the level the sender reported.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn radius_visibility_text_range_and_ambient_mode() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let carol_env = env2.as_ref().unwrap_or(&env);
    let http = reqwest::Client::new();
    let world = create_channel_with(
        &env,
        &http,
        serde_json::json!({
            "channel_type": "positional",
            "positional_config": {
                "near_distance": 5.0, "far_distance": 25.0, "rolloff": "linear",
                "max_radius": 30.0, "directional": false, "coordinate_system": "left_handed",
                "roster_radius": 10.0, "text_radius": 5.0
            }
        }),
    )
    .await;
    let (tok_a, uid_a) = issue_token(&env, &http, "e2e:radius-alice", "Alice", world).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "e2e:radius-bob", "Bob", world).await;
    let (tok_c, uid_c) = issue_token(carol_env, &http, "e2e:radius-carol", "Carol", world).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());
    let uid_c = UserId::from_uuid(uid_c.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut carol = connect(carol_env, "carol", tok_c).await;
    if env2.is_some() {
        assert_ne!(
            alice.media_addr.port(),
            carol.media_addr.port(),
            "carol must land on the other node"
        );
    }
    for p in [&mut alice, &mut bob, &mut carol] {
        bind_media(p).await;
    }

    // Join ack: the radii are announced and nobody is listed until positions are known.
    for p in [&mut alice, &mut bob, &mut carol] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id: world,
            token: tok,
        })
        .await;
        let ack = p
            .expect("ChannelJoinAck", |m| {
                matches!(m, ControlMessage::ChannelJoinAck { channel_id, .. } if *channel_id == world)
            })
            .await;
        let ControlMessage::ChannelJoinAck {
            participants,
            roster_radius,
            text_radius,
            ..
        } = ack
        else {
            unreachable!()
        };
        assert_eq!(roster_radius, Some(10.0));
        assert_eq!(text_radius, Some(5.0));
        assert!(
            participants.is_empty(),
            "{}: nobody has a position yet: {participants:?}",
            p.name
        );
    }
    for p in [&mut alice, &mut bob, &mut carol] {
        assert_no_presence(p, world, "no positions are known yet").await;
    }

    let pose = |user_id: UserId, x: f32| ControlMessage::PositionUpdate {
        channel_id: world,
        positions: vec![UserPosition {
            user_id,
            position: at(x, 0.0, 0.0),
            orientation: facing(0.0, 0.0, 1.0),
        }],
    };

    // Alice alone at the origin: still nothing to reveal (Bob and Carol have no pose).
    let m = pose(uid_a, 0.0);
    alice.send(&m).await;
    for p in [&mut alice, &mut bob, &mut carol] {
        assert_no_presence(p, world, "only one position is known").await;
    }

    // Bob 3 m away: both see each other; Carol (no pose) sees nothing.
    let m = pose(uid_b, 3.0);
    bob.send(&m).await;
    expect_presence(&mut alice, world, uid_b, true).await;
    expect_presence(&mut bob, world, uid_a, true).await;
    assert_no_presence(&mut carol, world, "carol has no position").await;

    // Text within 5 m reaches Bob (and echoes to Alice); Carol is out of scope.
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: world,
            text: "near".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    assert_eq!(expect_chat(&mut alice, "own echo").await.text, "near");
    assert_eq!(
        expect_chat(&mut bob, "chat within text radius").await.text,
        "near"
    );
    assert_no_chat(&mut carol, "she has no position").await;

    // Bob walks to 7 m: still on the roster (≤ 10 m) but out of text range (> 5 m).
    let m = pose(uid_b, 7.0);
    bob.send(&m).await;
    alice
        .expect("PositionUpdate(bob)", |m| {
            matches!(m, ControlMessage::PositionUpdate { positions, .. }
                if positions.iter().any(|p| p.user_id == uid_b))
        })
        .await;
    assert_no_presence(&mut alice, world, "bob is still within the roster radius").await;
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: world,
            text: "far".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    assert_eq!(expect_chat(&mut alice, "own echo").await.text, "far");
    assert_no_chat(&mut bob, "bob is beyond the text radius").await;
    alice
        .send(&ControlMessage::ChatTyping {
            channel_id: world,
            typing: true,
        })
        .await;
    assert_no_chat(&mut bob, "typing is scoped like chat").await;

    // 10.5 m: inside the 10 % exit hysteresis, nobody leaves. 12 m: both lose each other —
    // yet audio still flows because the audio range is `max_radius` (30 m).
    let m = pose(uid_b, 10.5);
    bob.send(&m).await;
    assert_no_presence(&mut alice, world, "10.5 m is within the exit hysteresis").await;
    assert_no_presence(&mut bob, world, "10.5 m is within the exit hysteresis").await;
    let m = pose(uid_b, 12.0);
    bob.send(&m).await;
    expect_presence(&mut alice, world, uid_b, false).await;
    expect_presence(&mut bob, world, uid_a, false).await;
    let hello = Bytes::from_static(b"hello");
    drain_udp(&bob).await;
    send_audio(&alice, world, 1, &hello).await;
    let (meta, n) = directional_audio_from(&bob, alice.ssrc, &hello).await;
    assert_eq!(n, 10, "audio range is max_radius, not roster_radius");
    let (volume, _) = meta.unwrap();
    assert!(
        (volume - 0.65).abs() < 0.03,
        "linear 5..25 m at 12 m: {volume}"
    );
    assert_none_matching(
        &mut bob,
        Duration::from_millis(300),
        "speaking events are roster-scoped",
        |m| matches!(m, ControlMessage::SpeakingStateChanged { user_id, .. } if *user_id == uid_a),
    )
    .await;

    // Back to 5 m: revealed again, and text reaches Bob again (exactly at the text radius).
    let m = pose(uid_b, 5.0);
    bob.send(&m).await;
    expect_presence(&mut alice, world, uid_b, true).await;
    expect_presence(&mut bob, world, uid_a, true).await;

    // Carol (other node when configured) appears 4 m from Alice, 1 m from Bob: everyone sees
    // everyone, and Alice's message reaches both.
    let m = pose(uid_c, 4.0);
    carol.send(&m).await;
    expect_presence(&mut alice, world, uid_c, true).await;
    expect_presence(&mut bob, world, uid_c, true).await;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2 {
        let m = expect_within(
            &mut carol,
            "ParticipantJoined(alice|bob)",
            Duration::from_secs(3),
            |m| matches!(m, ControlMessage::ParticipantJoined { channel_id, .. } if *channel_id == world),
        )
        .await;
        if let ControlMessage::ParticipantJoined { user_id, .. } = m {
            seen.insert(user_id);
        }
    }
    assert_eq!(seen, [uid_a, uid_b].into_iter().collect());
    alice
        .send(&ControlMessage::ChatSend {
            channel_id: world,
            text: "all".into(),
            metadata: None,
            client_ref: None,
        })
        .await;
    assert_eq!(expect_chat(&mut alice, "own echo").await.text, "all");
    assert_eq!(expect_chat(&mut bob, "bob at 5 m").await.text, "all");
    assert_eq!(expect_chat(&mut carol, "carol at 4 m").await.text, "all");

    // Bob leaves the channel: the observers that saw him are told exactly once.
    bob.send(&ControlMessage::ChannelLeave { channel_id: world })
        .await;
    expect_presence(&mut alice, world, uid_b, false).await;
    expect_presence(&mut carol, world, uid_b, false).await;
    assert_no_presence(&mut alice, world, "a single ParticipantLeft per observer").await;
    assert_no_presence(&mut carol, world, "a single ParticipantLeft per observer").await;
    for p in [&mut alice, &mut carol] {
        p.send(&ControlMessage::ChannelLeave { channel_id: world })
            .await;
    }

    // Ambient (cocktail-party) team channel: one full-gain voice, the rest at 0.2.
    let party = create_channel_with(
        &env,
        &http,
        serde_json::json!({"ambient": {"max_voices": 1, "ambient_gain": 0.2}}),
    )
    .await;
    let (tok_a, _) = issue_token(&env, &http, "e2e:ambient-alice", "Alice", party).await;
    let (tok_b, _) = issue_token(&env, &http, "e2e:ambient-bob", "Bob", party).await;
    let (tok_d, _) = issue_token(carol_env, &http, "e2e:ambient-dave", "Dave", party).await;
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut dave = connect(carol_env, "dave", tok_d).await;
    for p in [&mut alice, &mut bob, &mut dave] {
        bind_media(p).await;
        join(p, party).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain_udp(&dave).await;

    // Alice alone at -6 dBov: full gain, no gain byte.
    let got = ambient_burst(&[(&alice, 1, 6)], &dave, party, &hello).await;
    assert_eq!(got[&alice.ssrc], (1.0, 10), "{got:?}");
    // Alice (-6 dBov) and a quieter Bob (-30 dBov) together: Bob is background at 0.2.
    let got = ambient_burst(&[(&alice, 100, 6), (&bob, 100, 30)], &dave, party, &hello).await;
    assert_eq!(got[&alice.ssrc].1, 10, "{got:?}");
    assert_eq!(
        got[&alice.ssrc].0, 1.0,
        "the loud voice holds the slot: {got:?}"
    );
    assert_eq!(got[&bob.ssrc].1, 10, "{got:?}");
    assert!(
        (got[&bob.ssrc].0 - 0.2).abs() < 0.02,
        "the quiet voice is dimmed: {got:?}"
    );
    // Bob shouts (0 dBov) while Alice whispers (-40 dBov): the slot changes hands.
    let got = ambient_burst(&[(&alice, 200, 40), (&bob, 200, 0)], &dave, party, &hello).await;
    assert_eq!(
        got[&bob.ssrc].0, 1.0,
        "the louder speaker takes the slot: {got:?}"
    );
    assert!(
        (got[&alice.ssrc].0 - 0.2).abs() < 0.02,
        "the whispering holder is dimmed: {got:?}"
    );
    // After the hold time nobody occupies the slot: Alice alone is full gain again.
    let got = ambient_burst(&[(&alice, 300, 6)], &dave, party, &hello).await;
    assert_eq!(
        got[&alice.ssrc],
        (1.0, 10),
        "slots expire after silence: {got:?}"
    );

    for p in [&mut alice, &mut bob, &mut dave] {
        p.send(&ControlMessage::ChannelLeave { channel_id: party })
            .await;
    }
}

/// One frame of any kind from the control socket, or `None` when it stays silent.
async fn next_frame(p: &mut Player, wait: Duration) -> Option<Message> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, p.ws.next()).await {
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(m))) => return Some(m),
            _ => return None,
        }
    }
}

/// Next sealed AURX packet delivered as a binary frame, skipping control messages.
async fn recv_tunnel(p: &mut Player, wait: Duration) -> Option<AurixPacket> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match next_frame(p, left).await? {
            Message::Binary(b) => {
                let mut pkt = AurixPacket::decode(&b).expect("bad AURX frame on the tunnel");
                assert!(
                    pkt.header.has_flag(PacketFlags::Encrypted),
                    "{}: tunnel downlink must be encrypted",
                    p.name
                );
                assert!(
                    pkt.open(&p.keys),
                    "{}: tunnel downlink must be sealed with this session's key",
                    p.name
                );
                return Some(pkt);
            }
            _ => continue,
        }
    }
}

/// True when nothing bind-related (a binary ack or `MediaBound`) shows up within `wait`;
/// unrelated control messages (e.g. `ReceiverPreferences` on session start) are skipped.
async fn no_bind_reply(p: &mut Player, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match next_frame(p, left).await {
            None => return true,
            Some(Message::Binary(_)) => return false,
            Some(Message::Text(t)) => {
                if matches!(
                    serde_json::from_str::<ControlMessage>(&t),
                    Ok(ControlMessage::MediaBound { .. })
                ) {
                    return false;
                }
            }
            Some(_) => {}
        }
    }
}

/// Binds the session's media through its own control WebSocket: signed `SessionBind` as a
/// binary frame → sealed `SessionBindAck` back on the socket and `MediaBound { tunnel }`.
async fn bind_tunnel(p: &mut Player) {
    let pkt = AurixPacket::session_bind(&p.session_id, p.ssrc, now_ms(), rand::random());
    p.ws.send(Message::Binary(pkt.encode_authenticated(&p.keys).to_vec()))
        .await
        .unwrap();
    let (mut acked, mut bound) = (false, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(acked && bound) {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match next_frame(p, left).await {
            Some(Message::Binary(b)) => {
                let mut ack = AurixPacket::decode(&b).unwrap();
                assert!(ack.open(&p.keys));
                assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
                assert_eq!(ack.payload.len(), 8);
                acked = true;
            }
            Some(Message::Text(t)) => {
                let m: ControlMessage = serde_json::from_str(&t).unwrap();
                if let ControlMessage::MediaBound {
                    session_id,
                    transport,
                } = m
                {
                    assert_eq!(session_id, p.session_id);
                    assert_eq!(transport, MediaTransportKind::Tunnel, "{}", p.name);
                    bound = true;
                }
            }
            _ => panic!("{}: tunnel bind got no ack/MediaBound", p.name),
        }
    }
}

async fn session_media_path(env: &Env, http: &reqwest::Client, sid: SessionId) -> String {
    let stats: serde_json::Value = http
        .get(format!("{}/v1/sessions/{sid}/stats", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    stats["media_path"].as_str().unwrap_or("").to_string()
}

/// AURX over the control WebSocket ("UDP is blocked"): the same sealed packets as binary
/// frames, attributed to the connection's own session, routed exactly like UDP, with the
/// newest signed bind deciding which link a session is on.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn native_media_tunnel_over_the_control_websocket() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;
    let hash = channel_id_hash(&channel_id);
    let (tok_a, _) = issue_token(&env, &http, "e2e:tun-alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env, &http, "e2e:tun-bob", "Bob", channel_id).await;

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    assert!(
        alice.media_tunnel && bob.media_tunnel,
        "node must advertise the tunnel in SessionInitAck"
    );

    // A bind for a session this connection does not own is dropped even though it is validly
    // signed by that session's key (Bob's) — no ack, no MediaBound for anyone.
    let foreign = AurixPacket::session_bind(&bob.session_id, bob.ssrc, now_ms(), rand::random());
    alice
        .ws
        .send(Message::Binary(
            foreign.encode_authenticated(&bob.keys).to_vec(),
        ))
        .await
        .unwrap();
    assert!(
        no_bind_reply(&mut alice, Duration::from_millis(700)).await,
        "foreign SessionBind must be ignored"
    );
    assert!(no_bind_reply(&mut bob, Duration::from_millis(300)).await);

    // Media that arrives before the tunnel is bound is dropped like unbound UDP.
    let early = AurixPacket::audio(1, 960, alice.ssrc, hash, Bytes::from_static(&[0xFC, 9]));
    alice
        .ws
        .send(Message::Binary(early.seal(&alice.keys).to_vec()))
        .await
        .unwrap();

    bind_tunnel(&mut alice).await;
    bind_media(&mut bob).await;
    assert_eq!(
        session_media_path(&env, &http, alice.session_id).await,
        "tunnel"
    );
    assert_eq!(session_media_path(&env, &http, bob.session_id).await, "udp");

    // A replayed bind (timestamp not newer than the accepted one) is refused on the tunnel too.
    let stale = AurixPacket::session_bind(&alice.session_id, alice.ssrc, now_ms() - 60_000, 7);
    alice
        .ws
        .send(Message::Binary(
            stale.encode_authenticated(&alice.keys).to_vec(),
        ))
        .await
        .unwrap();
    assert!(
        no_bind_reply(&mut alice, Duration::from_millis(500)).await,
        "stale SessionBind must be ignored"
    );

    for p in [&mut alice, &mut bob] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id,
            token: tok,
        })
        .await;
        p.expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    }
    alice
        .expect("ParticipantJoined", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Bob")
        })
        .await;

    // Alice (tunnel) → Bob (UDP): binary frames come out as ordinary UDP downlink.
    let payload = Bytes::from_static(&[0xFC, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    let mut seq = 10u32;
    for _ in 0..10 {
        seq += 1;
        let pkt = AurixPacket::audio(seq, seq * 960, alice.ssrc, hash, payload.clone());
        alice
            .ws
            .send(Message::Binary(pkt.seal(&alice.keys).to_vec()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut got = 0;
    while let Some(p) = bob.recv_udp().await {
        if p.header.packet_type == PacketType::Audio {
            assert_eq!(p.header.ssrc, alice.ssrc);
            assert_eq!(&p.payload[..], &payload[..]);
            got += 1;
            if got >= 8 {
                break;
            }
        }
    }
    assert!(
        got >= 8,
        "Bob received only {got} tunnelled packets from Alice"
    );

    // Bob (UDP) → Alice (tunnel): downlink sealed with Alice's key, one packet per frame.
    for i in 1..=10u32 {
        let pkt = AurixPacket::audio(i, i * 960, bob.ssrc, hash, payload.clone());
        bob.udp
            .send_to(&pkt.seal(&bob.keys), bob.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut got = 0;
    while let Some(p) = recv_tunnel(&mut alice, Duration::from_secs(3)).await {
        if p.header.packet_type == PacketType::Audio {
            assert_eq!(p.header.ssrc, bob.ssrc);
            assert_eq!(&p.payload[..], &payload[..]);
            got += 1;
            if got >= 8 {
                break;
            }
        }
    }
    assert!(
        got >= 8,
        "Alice received only {got} packets over the tunnel"
    );
    assert!(
        alice.udp.try_recv_from(&mut [0u8; 64]).is_err(),
        "nothing may reach Alice's (unbound) UDP socket"
    );

    // Heartbeats are answered on the tunnel and share the uplink sequence with audio.
    seq += 1;
    let mut hb = AurixPacket::heartbeat(alice.ssrc, seq * 960);
    hb.header.sequence = seq;
    alice
        .ws
        .send(Message::Binary(hb.seal(&alice.keys).to_vec()))
        .await
        .unwrap();
    let mut acked = false;
    while let Some(p) = recv_tunnel(&mut alice, Duration::from_secs(2)).await {
        if p.header.packet_type == PacketType::HeartbeatAck {
            acked = true;
            break;
        }
    }
    assert!(acked, "tunnel heartbeat not acked");
    while recv_tunnel(&mut alice, Duration::from_millis(300))
        .await
        .is_some()
    {}

    // Replay on the tunnel: an already-accepted sequence is dropped, Bob hears nothing new.
    while bob.recv_udp().await.is_some() {}
    let replay = AurixPacket::audio(11, 11 * 960, alice.ssrc, hash, payload.clone());
    alice
        .ws
        .send(Message::Binary(replay.seal(&alice.keys).to_vec()))
        .await
        .unwrap();
    assert!(
        bob.recv_udp().await.is_none(),
        "replayed tunnel packet must not be forwarded"
    );

    // "UDP works again": a newer signed bind from a UDP socket moves Alice off the tunnel;
    // Bob's audio now lands on the socket, the WebSocket carries only control frames …
    bind_media(&mut alice).await;
    assert_eq!(
        session_media_path(&env, &http, alice.session_id).await,
        "udp"
    );
    for i in 11..=15u32 {
        let pkt = AurixPacket::audio(i, i * 960, bob.ssrc, hash, payload.clone());
        bob.udp
            .send_to(&pkt.seal(&bob.keys), bob.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut on_udp = 0;
    while let Some(p) = alice.recv_udp().await {
        if p.header.packet_type == PacketType::Audio && p.header.ssrc == bob.ssrc {
            on_udp += 1;
            if on_udp >= 4 {
                break;
            }
        }
    }
    assert!(
        on_udp >= 4,
        "Alice got {on_udp} packets on UDP after moving back"
    );
    assert!(
        recv_tunnel(&mut alice, Duration::from_millis(400))
            .await
            .is_none(),
        "no media may be pushed down the released tunnel"
    );
    // … and media sent over the (no longer bound) tunnel is refused like a stale UDP source.
    seq += 1;
    let orphan = AurixPacket::audio(seq, seq * 960, alice.ssrc, hash, payload.clone());
    alice
        .ws
        .send(Message::Binary(orphan.seal(&alice.keys).to_vec()))
        .await
        .unwrap();
    assert!(
        bob.recv_udp().await.is_none(),
        "media over an unbound tunnel must be dropped"
    );

    // … and back onto the tunnel with the sequence still counting (a mid-call fallback).
    bind_tunnel(&mut alice).await;
    assert_eq!(
        session_media_path(&env, &http, alice.session_id).await,
        "tunnel"
    );
    for _ in 0..5 {
        seq += 1;
        let pkt = AurixPacket::audio(seq, seq * 960, alice.ssrc, hash, payload.clone());
        alice
            .ws
            .send(Message::Binary(pkt.seal(&alice.keys).to_vec()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut got = 0;
    while let Some(p) = bob.recv_udp().await {
        if p.header.packet_type == PacketType::Audio && p.header.ssrc == alice.ssrc {
            got += 1;
            if got >= 4 {
                break;
            }
        }
    }
    assert!(
        got >= 4,
        "Bob got {got} packets after Alice fell back to the tunnel"
    );

    for p in [&mut alice, &mut bob] {
        p.send(&ControlMessage::ChannelLeave { channel_id }).await;
    }
}

async fn issue_listener_token(
    env: &Env,
    http: &reqwest::Client,
    external_id: &str,
    name: &str,
    ch: ChannelId,
) -> (String, String) {
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": external_id,
            "display_name": name,
            "channels": [{"channel_id": ch, "join": true, "speak": false, "receive": true, "moderate": false}],
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        r["token"].as_str().unwrap().to_string(),
        r["user_id"].as_str().unwrap().to_string(),
    )
}

/// Joins and returns `(role, roster user ids, participant_count, hidden_listeners)`.
async fn join_ack(
    p: &mut Player,
    channel_id: ChannelId,
) -> (aurix_common::types::ChannelRole, Vec<UserId>, u32, bool) {
    let tok = p.token.clone();
    p.send(&ControlMessage::ChannelJoin {
        channel_id,
        token: tok,
    })
    .await;
    let ack = p
        .expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { channel_id: c, .. } if *c == channel_id)
        })
        .await;
    let ControlMessage::ChannelJoinAck {
        participants,
        role,
        participant_count,
        hidden_listeners,
        ..
    } = ack
    else {
        unreachable!()
    };
    (
        role,
        participants.into_iter().map(|p| p.user_id).collect(),
        participant_count,
        hidden_listeners,
    )
}

/// Collects up to 400 ms of downlink audio at `to`: `(frames per SSRC, mixed-flag frames,
/// peak stereo RMS of the mixed frames)`.
async fn downlink_summary(to: &Player) -> (std::collections::HashMap<u32, usize>, usize, f32) {
    let mut per_ssrc: std::collections::HashMap<u32, usize> = Default::default();
    let mut mixed = 0;
    let mut level = 0.0f32;
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0i16; 960 * 2];
    let mut buf = vec![0u8; 2048];
    while let Ok(Ok((n, _))) =
        tokio::time::timeout(Duration::from_millis(400), to.udp.recv_from(&mut buf)).await
    {
        let mut p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
        assert!(p.open(&to.keys), "{}: sealed with another key", to.name);
        if p.header.packet_type != PacketType::Audio {
            continue;
        }
        *per_ssrc.entry(p.header.ssrc).or_default() += 1;
        if p.header.has_flag(PacketFlags::Mixed) {
            mixed += 1;
            assert!(!p.header.has_flag(PacketFlags::E2ee));
            let _ = p.take_downlink_meta();
            let n = dec
                .decode(&p.payload, &mut pcm, false)
                .expect("stereo Opus mix");
            assert_eq!(n, 960, "{}: 20 ms stereo frame", to.name);
            let sum: f32 = pcm[..n * 2]
                .iter()
                .map(|&s| (s as f32 / 32768.0).powi(2))
                .sum();
            level = level.max((sum / (n * 2) as f32).sqrt());
        }
    }
    (per_ssrc, mixed, level)
}

/// Audience channels: a grant with `speak: false` is a listener in every channel type (its
/// frames are refused), `hide_listeners` keeps listeners out of everyone else's roster and
/// presence (also across nodes) while they see the speakers and get the true
/// `participant_count`; native listeners receive one stereo server mix of the channel
/// instead of a stream per speaker, a speaker can opt into the same mix with
/// `SetDownlinkMode`, and `max_speakers` refuses the speaker over the cap with CHANNEL_FULL
/// while listeners keep joining.
#[tokio::test]
#[ignore = "requires a running Aurix server; see the e2e job in .github/workflows/ci.yml"]
async fn audience_channels_hide_listeners_mix_their_downlink_and_cap_speakers() {
    use aurix_common::types::{ChannelRole, DownlinkMode};
    use aurix_media::mix::channel_mix_ssrc;

    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let (env, app_id) = isolated_env(&base, &http, "audience").await;
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let far_env = env2.as_ref().unwrap_or(&env);

    // `POST /v1/channels` clamps `max_participants` to the app quota (256 for apps created
    // with defaults); with an admin token the test raises the quota of its own app first, the
    // way an operator enables large channels for an existing app.
    let admin_token = std::env::var("AURIX_E2E_ADMIN_TOKEN").ok();
    if let (Some(token), Some(app_id)) = (&admin_token, &app_id) {
        let bad = http
            .patch(format!("{}/v1/apps/{app_id}", env.api))
            .bearer_auth(token)
            .json(&serde_json::json!({"max_participants_per_channel": 100_001}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            bad.status(),
            400,
            "quota above the hard ceiling is rejected"
        );
        let app: serde_json::Value = http
            .patch(format!("{}/v1/apps/{app_id}", env.api))
            .bearer_auth(token)
            .json(&serde_json::json!({"max_participants_per_channel": 5000}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(app["max_participants_per_channel"], 5000);
    }

    let stage = create_channel_with(
        &env,
        &http,
        serde_json::json!({
            "channel_type": "team",
            "max_participants": 5000,
            "audience": {"hide_listeners": true, "mix_for_listeners": true, "max_speakers": 2}
        }),
    )
    .await;
    let cfg: serde_json::Value = http
        .get(format!("{}/v1/channels/{stage}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    if admin_token.is_some() {
        assert_eq!(cfg["config"]["max_participants"], 5000);
    } else {
        eprintln!(
            "AURIX_E2E_ADMIN_TOKEN not set: channel clamped to app quota {}",
            cfg["config"]["max_participants"]
        );
    }
    assert_eq!(cfg["config"]["audience"]["max_speakers"], 2);
    assert_eq!(cfg["config"]["audience"]["max_streams"], 0);

    let (tok_a, uid_a) = issue_token(&env, &http, "aud:alice", "Alice", stage).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "aud:bob", "Bob", stage).await;
    let (tok_c, _) = issue_token(&env, &http, "aud:carol", "Carol", stage).await;
    let (tok_l, uid_l) = issue_listener_token(far_env, &http, "aud:lisa", "Lisa", stage).await;
    let (tok_n, uid_n) = issue_listener_token(&env, &http, "aud:ned", "Ned", stage).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());
    let uid_l = UserId::from_uuid(uid_l.parse().unwrap());
    let uid_n = UserId::from_uuid(uid_n.parse().unwrap());

    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    let mut carol = connect(&env, "carol", tok_c).await;
    let mut lisa = connect(far_env, "lisa", tok_l).await;
    let mut ned = connect(&env, "ned", tok_n).await;
    if env2.is_some() {
        assert_ne!(alice.media_addr.port(), lisa.media_addr.port());
    }
    for p in [&mut alice, &mut bob, &mut carol, &mut lisa, &mut ned] {
        bind_media(p).await;
    }

    // ── roster / presence ──
    let (role, roster, count, hidden) = join_ack(&mut alice, stage).await;
    assert_eq!(role, ChannelRole::Speaker);
    assert!(roster.is_empty() && count == 1 && hidden);

    let (role, roster, count, hidden) = join_ack(&mut lisa, stage).await;
    assert_eq!(role, ChannelRole::Listener);
    assert_eq!(roster, vec![uid_a], "the listener sees the speaker");
    assert!(hidden);
    // Presence of a remote member may lag the ack by one replication hop.
    assert!((1..=2).contains(&count), "lisa count {count}");
    assert_no_presence(
        &mut alice,
        stage,
        "hidden listener must not appear to Alice",
    )
    .await;

    let (role, roster, count, _) = join_ack(&mut bob, stage).await;
    assert_eq!(role, ChannelRole::Speaker);
    assert_eq!(
        roster,
        vec![uid_a],
        "Bob's roster has Alice but not the hidden listener"
    );
    assert_eq!(count, 3, "participant_count counts the hidden listener too");
    expect_presence(&mut alice, stage, uid_b, true).await;
    expect_presence(&mut lisa, stage, uid_b, true).await;

    let (role, roster, count, _) = join_ack(&mut ned, stage).await;
    assert_eq!(role, ChannelRole::Listener);
    let mut expect = vec![uid_a, uid_b];
    expect.sort();
    let mut got = roster.clone();
    got.sort();
    assert_eq!(
        got, expect,
        "a listener sees the speakers, not other listeners"
    );
    assert_eq!(count, 4);
    assert_no_presence(&mut alice, stage, "Ned is hidden from Alice").await;
    assert_no_presence(&mut lisa, stage, "Ned is hidden from Lisa too").await;

    // ── speaker cap: a third speaker is refused, listeners are not ──
    let tok = carol.token.clone();
    carol
        .send(&ControlMessage::ChannelJoin {
            channel_id: stage,
            token: tok,
        })
        .await;
    expect_error(&mut carol, "third speaker", "CHANNEL_FULL").await;
    assert_no_presence(&mut alice, stage, "refused speaker never joined").await;

    // ── audio: listeners get the channel mix, speakers get per-speaker streams ──
    let mix_ssrc = channel_mix_ssrc(&stage);
    let tone = opus_tone(440.0, 600);
    let (_, to_alice) = tokio::join!(
        stream_frames(&alice, stage, 1, &tone, false),
        downlink_summary(&bob)
    );
    assert_eq!(to_alice.1, 0, "Bob (streams) gets no mixed frames");
    assert!(
        to_alice.0.get(&alice.ssrc).copied().unwrap_or(0) >= 25,
        "Bob hears Alice's own stream: {:?}",
        to_alice.0
    );
    let (ned_frames, ned_mixed, ned_level) = downlink_summary(&ned).await;
    assert!(
        ned_mixed >= 20,
        "Ned got {ned_mixed} mixed frames: {ned_frames:?}"
    );
    assert_eq!(ned_frames.len(), 1, "one stream only: {ned_frames:?}");
    assert!(ned_frames.contains_key(&mix_ssrc));
    assert!(
        (0.15..0.5).contains(&ned_level),
        "Ned's mix level {ned_level}"
    );
    let (lisa_frames, lisa_mixed, lisa_level) = downlink_summary(&lisa).await;
    assert!(
        lisa_mixed >= 20,
        "Lisa (other node) got {lisa_mixed} mixed frames: {lisa_frames:?}"
    );
    assert_eq!(lisa_frames.len(), 1);
    assert!(lisa_frames.contains_key(&mix_ssrc));
    assert!(
        (0.15..0.5).contains(&lisa_level),
        "Lisa's mix level {lisa_level}"
    );

    // A listener's frames are refused at the SFU.
    let junk = Bytes::from_static(b"listener-mic");
    send_audio(&ned, stage, 1, &junk).await;
    send_audio(&lisa, stage, 1, &junk).await;
    let (frames, mixed, _) = downlink_summary(&alice).await;
    assert_eq!(mixed, 0);
    assert!(!frames.contains_key(&ned.ssrc) && !frames.contains_key(&lisa.ssrc));
    let (frames, _, _) = downlink_summary(&bob).await;
    assert!(!frames.contains_key(&ned.ssrc) && !frames.contains_key(&lisa.ssrc));

    // ── a speaker opts into the mix and back ──
    bob.send(&ControlMessage::SetDownlinkMode {
        mode: DownlinkMode::Mixed,
    })
    .await;
    let m = bob
        .expect("DownlinkModeChanged", |m| {
            matches!(m, ControlMessage::DownlinkModeChanged { .. })
        })
        .await;
    assert!(matches!(
        m,
        ControlMessage::DownlinkModeChanged {
            mode: DownlinkMode::Mixed
        }
    ));
    let (_, to_bob) = tokio::join!(
        stream_frames(&alice, stage, 100, &tone, false),
        downlink_summary(&bob)
    );
    assert!(to_bob.1 >= 20, "Bob (mixed) got {} mixed frames", to_bob.1);
    assert!(
        !to_bob.0.contains_key(&alice.ssrc),
        "no per-speaker stream while mixed"
    );
    // Bob talking must not come back to Bob through his own mix.
    let (_, to_bob) = tokio::join!(
        stream_frames(&bob, stage, 1, &tone, false),
        downlink_summary(&bob)
    );
    assert_eq!(to_bob.1, 0, "no self-audio in Bob's mix: {:?}", to_bob.0);
    let (frames, mixed, _) = downlink_summary(&alice).await;
    assert_eq!(mixed, 0);
    assert!(frames.get(&bob.ssrc).copied().unwrap_or(0) >= 25);

    bob.send(&ControlMessage::SetDownlinkMode {
        mode: DownlinkMode::Streams,
    })
    .await;
    bob.expect("DownlinkModeChanged(streams)", |m| {
        matches!(
            m,
            ControlMessage::DownlinkModeChanged {
                mode: DownlinkMode::Streams
            }
        )
    })
    .await;
    let (_, to_bob) = tokio::join!(
        stream_frames(&alice, stage, 200, &tone, false),
        downlink_summary(&bob)
    );
    assert_eq!(to_bob.1, 0);
    assert!(to_bob.0.get(&alice.ssrc).copied().unwrap_or(0) >= 25);

    // ── leaving: hidden listeners leave silently, speakers do not ──
    ned.send(&ControlMessage::ChannelLeave { channel_id: stage })
        .await;
    lisa.send(&ControlMessage::ChannelLeave { channel_id: stage })
        .await;
    assert_no_presence(&mut alice, stage, "hidden listeners leave silently").await;
    bob.send(&ControlMessage::ChannelLeave { channel_id: stage })
        .await;
    expect_presence(&mut alice, stage, uid_b, false).await;
    assert_ne!(uid_l, uid_n);
}

/// Cross-node failover. Alice (node 1) loses her WebSocket and reconnects to node 2 with her
/// resume credential: node 2 adopts the session from the Redis mirror — same session id and
/// SSRC, fresh media key on node 2's media port, channel membership restored, `migrated`
/// flagged — while Bob (node 2) and Carol (node 1) never see her leave and keep hearing her.
/// Node 1 fences itself off: the used credential can no longer resume there, and node 1
/// closing its stale copy must not close the migrated membership. With
/// `AURIX_E2E_NODE2_STOP` / `AURIX_E2E_NODE2_START` set (shell commands), the test also
/// kills node 2, waits for the lost-node reaper and resumes Bob on node 1.
#[tokio::test]
#[ignore = "requires two running Aurix nodes; see README (Scaling)"]
async fn two_nodes_session_failover_resumes_on_the_other_node() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let Ok(ws2) = std::env::var("AURIX_E2E_WS2") else {
        eprintln!("AURIX_E2E_WS2 not set; skipping");
        return;
    };
    let env2 = Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws: ws2,
        api_key: env.api_key.clone(),
    };
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;

    let (tok_a, uid_a) = issue_token(&env, &http, "failover:alice", "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env2, &http, "failover:bob", "Bob", channel_id).await;
    let (tok_c, _) = issue_token(&env, &http, "failover:carol", "Carol", channel_id).await;
    let uid_a = UserId::from_uuid(uid_a.parse().unwrap());
    let uid_b = UserId::from_uuid(uid_b.parse().unwrap());
    let mut alice = connect(&env, "alice", tok_a.clone()).await;
    let mut bob = connect(&env2, "bob", tok_b.clone()).await;
    let mut carol = connect(&env, "carol", tok_c).await;
    assert_ne!(alice.media_addr.port(), bob.media_addr.port());
    assert!(!alice.migrated && !bob.migrated);
    assert!(
        alice.failover.iter().any(|u| u.contains(":8091")) || alice.failover.is_empty(),
        "node 1 must advertise node 2 as failover: {:?}",
        alice.failover
    );
    assert!(
        !alice.failover.is_empty(),
        "SessionInitAck.failover must list the other healthy node"
    );
    for p in [&mut alice, &mut bob, &mut carol] {
        bind_media(p).await;
        join(p, channel_id).await;
    }
    for p in [&mut alice, &mut bob] {
        p.expect("ParticipantJoined(Carol)", |m| {
            matches!(m, ControlMessage::ParticipantJoined { display_name, .. } if display_name == "Carol")
        })
        .await;
    }
    // Alice prefers the fallback codec and mutes Bob locally: both must survive the move.
    alice
        .send(&ControlMessage::SetParticipantMute {
            user_id: uid_b,
            channel_id: None,
            muted: true,
        })
        .await;
    alice
        .send(&ControlMessage::SetAudioCodec {
            codec: AudioCodec::Pcmu,
        })
        .await;
    alice
        .expect("AudioCodecChanged(pcmu)", |m| {
            matches!(
                m,
                ControlMessage::AudioCodecChanged {
                    codec: AudioCodec::Pcmu,
                    ..
                }
            )
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 3);

    // Alice talks before the move; Bob (node 2) and Carol (node 1) remember the highest
    // downlink sequence they accepted from her SSRC, like a real client's anti-replay window.
    let pcmu = Bytes::from(vec![0xFFu8; 160]);
    let hash = channel_id_hash(&channel_id);
    let mut seq = 0u32;
    let mut highest_before: std::collections::HashMap<&str, u32> = Default::default();
    for _ in 0..10 {
        seq += 1;
        let mut pkt = AurixPacket::audio(seq, seq * 960, alice.ssrc, hash, pcmu.clone());
        pkt.header.set_flag(PacketFlags::Pcmu);
        alice
            .udp
            .send_to(&pkt.seal(&alice.keys), alice.media_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for p in [&bob, &carol] {
        let mut buf = vec![0u8; 2048];
        while let Ok(Ok((len, _))) =
            tokio::time::timeout(Duration::from_millis(400), p.udp.recv_from(&mut buf)).await
        {
            let mut d = AurixPacket::decode(&buf[..len]).expect("bad AURX packet");
            assert!(d.open(&p.keys), "{}: downlink must verify", p.name);
            if d.header.packet_type == PacketType::Audio && d.header.ssrc == alice.ssrc {
                let h = highest_before.entry(p.name).or_insert(0);
                *h = (*h).max(d.header.sequence);
            }
        }
        assert!(
            highest_before.contains_key(p.name),
            "{} must hear Alice before the move",
            p.name
        );
    }

    // ── Alice's socket to node 1 dies; she reconnects to node 2 with the same credential ──
    let Player {
        session_id: sid_a,
        ssrc: ssrc_a,
        media_key: key_a,
        resume_token: token_a1,
        ws: dead_ws,
        ..
    } = alice;
    drop(dead_ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut alice = connect_with(
        &env2,
        "alice@node2",
        tok_a.clone(),
        Some((sid_a, &token_a1)),
    )
    .await;
    assert!(alice.resumed, "node 2 must adopt the mirrored session");
    assert!(alice.migrated, "the ack must flag the cross-node move");
    assert_eq!(alice.session_id, sid_a);
    assert_eq!(alice.ssrc, ssrc_a);
    assert_ne!(
        alice.media_key, key_a,
        "the media key must not travel between nodes"
    );
    assert_eq!(
        alice.media_addr.port(),
        bob.media_addr.port(),
        "media must now be bound to node 2"
    );
    assert_ne!(alice.resume_token, token_a1);
    let prefs = alice
        .expect("ReceiverPreferences", |m| {
            matches!(m, ControlMessage::ReceiverPreferences { .. })
        })
        .await;
    if let ControlMessage::ReceiverPreferences { local_mutes, .. } = prefs {
        assert!(
            local_mutes.iter().any(|m| m.user_id == uid_b),
            "local mute of Bob must survive the move: {local_mutes:?}"
        );
    }

    let ack = alice
        .expect("ChannelJoinAck (restored)", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { .. })
        })
        .await;
    let ControlMessage::ChannelJoinAck {
        channel_id: c,
        participants,
        ..
    } = ack
    else {
        unreachable!()
    };
    assert_eq!(c, channel_id);
    let mut names: Vec<_> = participants
        .iter()
        .map(|p| p.display_name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["Bob", "Carol"],
        "roster on the new node: {participants:?}"
    );
    // Nobody saw Alice leave; the membership row is intact.
    for (p, what) in [(&mut bob, "Bob (node 2)"), (&mut carol, "Carol (node 1)")] {
        if let Some(m) = p.try_recv(Duration::from_millis(700)).await {
            assert!(
                !matches!(m, ControlMessage::ParticipantLeft { .. }),
                "{what} must not see Alice leave during failover, got {m:?}"
            );
        }
    }
    assert_eq!(membership_count(&env, &http, channel_id).await, 3);

    // Media re-binds on node 2 (PCMU, as before the move): Bob hears her locally, Carol via
    // cascade from node 2 to node 1, and her downlink sequence continues above what they had
    // already accepted (a restart at zero would be dropped by their replay windows); Alice
    // does not hear Bob (local mute restored).
    bind_media(&mut alice).await;
    let mut to_bob = 0;
    let mut to_carol = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    while (to_bob < 5 || to_carol < 5) && tokio::time::Instant::now() < deadline {
        for _ in 0..5 {
            seq += 1;
            let mut pkt = AurixPacket::audio(seq, seq * 960, ssrc_a, hash, pcmu.clone());
            pkt.header.set_flag(PacketFlags::Pcmu);
            alice
                .udp
                .send_to(&pkt.seal(&alice.keys), alice.media_addr)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut buf = vec![0u8; 2048];
        for (p, n) in [(&bob, &mut to_bob), (&carol, &mut to_carol)] {
            while let Ok(Ok((len, _))) =
                tokio::time::timeout(Duration::from_millis(250), p.udp.recv_from(&mut buf)).await
            {
                let mut d = AurixPacket::decode(&buf[..len]).expect("bad AURX packet");
                assert!(d.open(&p.keys), "{}: downlink must verify", p.name);
                if d.header.packet_type == PacketType::Audio && d.header.ssrc == ssrc_a {
                    assert!(
                        !d.header.has_flag(PacketFlags::Pcmu),
                        "{}: Opus receivers get transcoded Opus",
                        p.name
                    );
                    assert!(
                        d.header.sequence > highest_before[p.name],
                        "{}: downlink sequence {} after the move must continue above {} \
                         (per-SSRC replay windows on receivers)",
                        p.name,
                        d.header.sequence,
                        highest_before[p.name]
                    );
                    *n += 1;
                }
            }
        }
    }
    assert!(
        to_bob >= 5,
        "Bob heard {to_bob} packets from migrated Alice"
    );
    assert!(
        to_carol >= 5,
        "Carol (node 1) heard {to_carol} packets from Alice via cascade after failover"
    );
    let bob_payload = Bytes::from_static(&[0xFC, 1, 1, 2, 3, 5, 8, 13]);
    while alice.recv_udp().await.is_some() {}
    send_audio(&bob, channel_id, 5000, &bob_payload).await;
    let (_, n) = audio_from(&alice, bob.ssrc, &bob_payload).await;
    assert_eq!(n, 0, "Alice's local mute of Bob must survive the move");

    // ── node 1 is fenced: the spent credential opens nothing there ──
    let mut stale = connect_with(
        &env,
        "alice-stale@node1",
        tok_a.clone(),
        Some((sid_a, &token_a1)),
    )
    .await;
    assert!(!stale.resumed && !stale.migrated);
    assert_ne!(stale.session_id, sid_a);
    stale.ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Closing that unrelated session on node 1 must not touch the migrated membership.
    assert_eq!(membership_count(&env, &http, channel_id).await, 3);
    while let Some(m) = alice.try_recv(Duration::from_millis(300)).await {
        assert!(
            matches!(
                m,
                ControlMessage::NetworkQuality { .. }
                    | ControlMessage::ChannelEnergy { .. }
                    | ControlMessage::SpeakingStateChanged { .. }
            ),
            "the migrated session is unaffected by node 1, got {m:?}"
        );
    }
    // Node 1's detached copy has been evicted, not closed: Alice's own leave is announced
    // exactly once, by node 2.
    alice.ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 2);
    let mut left = 0;
    while let Some(m) = carol.try_recv(Duration::from_millis(500)).await {
        if let ControlMessage::ParticipantLeft { user_id, .. } = m {
            if user_id == uid_a {
                left += 1;
            }
        }
    }
    assert_eq!(left, 1, "exactly one leave for Alice seen on node 1");

    // ── optional: node 2 dies for real; the reaper closes its rows, Bob resumes on node 1 ──
    let (Ok(stop), Ok(start)) = (
        std::env::var("AURIX_E2E_NODE2_STOP"),
        std::env::var("AURIX_E2E_NODE2_START"),
    ) else {
        eprintln!("AURIX_E2E_NODE2_STOP/START not set; skipping the lost-node scenario");
        let _ = bob.ws.close(None).await;
        let _ = carol.ws.close(None).await;
        return;
    };
    let Player {
        session_id: sid_b,
        ssrc: ssrc_b,
        resume_token: token_b,
        ws: bob_ws,
        ..
    } = bob;
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&stop)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "AURIX_E2E_NODE2_STOP failed");
    drop(bob_ws);
    // The reaper announces Bob's membership as lost (`node_lost`) once node 2 is silent for
    // cluster.node_lost_after_secs; membership count drops to Carol alone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        match carol.try_recv(wait).await {
            Some(ControlMessage::ParticipantLeft { user_id, .. }) if user_id == uid_b => break,
            Some(_) => continue,
            None => panic!("Carol never saw Bob reaped after node 2 died"),
        }
    }
    assert_eq!(membership_count(&env, &http, channel_id).await, 1);
    // Bob comes back on node 1 within the mirror TTL: adopted from the mirror, membership
    // re-created and announced.
    let mut bob = connect_with(&env, "bob@node1", tok_b, Some((sid_b, &token_b))).await;
    assert!(
        bob.resumed && bob.migrated,
        "Bob must be adopted after the reaper"
    );
    assert_eq!(bob.session_id, sid_b);
    assert_eq!(bob.ssrc, ssrc_b);
    bob.expect("ChannelJoinAck (restored after reap)", |m| {
        matches!(m, ControlMessage::ChannelJoinAck { .. })
    })
    .await;
    carol
        .expect(
            "ParticipantJoined(Bob) after reap",
            |m| matches!(m, ControlMessage::ParticipantJoined { user_id, .. } if *user_id == uid_b),
        )
        .await;
    assert_eq!(membership_count(&env, &http, channel_id).await, 2);
    bind_media(&mut bob).await;
    send_audio(&bob, channel_id, 7000, &bob_payload).await;
    let (_, n) = audio_from(&carol, ssrc_b, &bob_payload).await;
    assert!(
        n >= 8,
        "Carol heard {n} packets from Bob after his node died"
    );
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&start)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "AURIX_E2E_NODE2_START failed");
    let _ = bob.ws.close(None).await;
    let _ = carol.ws.close(None).await;
}

async fn expect_throttled(resp: reqwest::Response, what: &str) -> u64 {
    assert_eq!(resp.status(), 429, "{what}");
    let retry_after: u64 = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{what}: Retry-After missing"));
    assert!(retry_after >= 1, "{what}: Retry-After {retry_after}");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["code"], "RATE_LIMIT_EXCEEDED",
        "{what}: {body}"
    );
    retry_after
}

/// Rate limits are one budget for the whole fleet: an API key's own `rate_limit` and a player's
/// report allowance are exhausted by requests spread over node 1 and node 2 (`AURIX_E2E_API2`;
/// the same node twice without it), the refusal is `429` + `Retry-After`, `PATCH /v1/api-keys`
/// changes the budget live and `0` lifts it. Needs nodes with `rate_limiting.enabled` and
/// `AURIX_E2E_REPORTS_PER_MINUTE` set to their `reports_per_minute`.
#[tokio::test]
#[ignore = "requires a running Aurix server (AURIX_E2E_API_KEY)"]
async fn fleet_rate_limits_hold_across_nodes() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let Some(reports_per_minute) = std::env::var("AURIX_E2E_REPORTS_PER_MINUTE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    else {
        eprintln!("AURIX_E2E_REPORTS_PER_MINUTE not set; skipping");
        return;
    };
    let api2 = std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| env.api.clone());
    let nodes = [env.api.clone(), api2];
    let http = reqwest::Client::new();
    let channel_id = create_channel(&env, &http).await;

    // A key with a budget of 4 requests per minute.
    let created: serde_json::Value = http
        .post(format!("{}/v1/api-keys", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "name": "e2e-budget",
            "permissions": ["channels:read"],
            "rate_limit": 4,
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let key_id = created["id"].as_str().unwrap().to_string();
    let key = created["key"].as_str().unwrap().to_string();
    let get_channel = |node: &str| {
        http.get(format!("{node}/v1/channels/{channel_id}"))
            .header("x-api-key", &key)
            .send()
    };
    for i in 0..4 {
        let r = get_channel(&nodes[i % 2]).await.unwrap();
        assert_eq!(r.status(), 200, "request {i} within the key budget");
    }
    let wait = expect_throttled(get_channel(&nodes[0]).await.unwrap(), "5th key request").await;
    assert!(wait <= 15, "one token per 15 s, got Retry-After {wait}");
    expect_throttled(
        get_channel(&nodes[1]).await.unwrap(),
        "same bucket on the other node",
    )
    .await;
    // The owning key is untouched by the sub-key's bucket.
    assert_eq!(
        http.get(format!("{}/v1/channels/{channel_id}", nodes[1]))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // Lift the budget on node 2; node 1 honours it at once.
    let patched: serde_json::Value = http
        .patch(format!("{}/v1/api-keys/{key_id}", nodes[1]))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"rate_limit": 0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(patched["rate_limit"], 0);
    assert!(
        patched.get("key").is_none(),
        "PATCH never returns the secret"
    );
    for i in 0..6 {
        let r = get_channel(&nodes[i % 2]).await.unwrap();
        assert_eq!(r.status(), 200, "unlimited key, request {i}");
    }
    assert_eq!(
        http.patch(format!("{}/v1/api-keys/{key_id}", env.api))
            .header("x-api-key", &env.api_key)
            .json(&serde_json::json!({"rate_limit": -1}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        assert_eq!(
            http.patch(format!("{}/v1/api-keys/{key_id}", env.api))
                .header("x-api-key", &api_key2)
                .json(&serde_json::json!({"rate_limit": 1}))
                .send()
                .await
                .unwrap()
                .status(),
            404,
            "another tenant cannot see the key"
        );
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; skipping tenant-isolation check");
    }
    http.delete(format!("{}/v1/api-keys/{key_id}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // Player reports: `reports_per_minute` for the reporter across both nodes.
    let (tok_a, uid_a) = issue_token(&env, &http, "e2e-rl-alice", "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "e2e-rl-bob", "Bob", channel_id).await;
    let alice = connect(&env, "alice", tok_a).await;
    let bob = connect(&env, "bob", tok_b).await;
    let report = |node: &str| {
        http.post(format!("{node}/v1/me/reports"))
            .bearer_auth(&alice.token)
            .json(&serde_json::json!({"target_user_id": uid_b, "reason": "e2e fleet limit"}))
            .send()
    };
    for i in 0..reports_per_minute {
        let r = report(&nodes[i % 2]).await.unwrap();
        assert_eq!(r.status(), 200, "report {i} within the allowance");
    }
    expect_throttled(
        report(&nodes[reports_per_minute % 2]).await.unwrap(),
        "report above the allowance",
    )
    .await;
    expect_throttled(
        report(&nodes[(reports_per_minute + 1) % 2]).await.unwrap(),
        "report above the allowance on the other node",
    )
    .await;
    // Bob's allowance is his own.
    let r = http
        .post(format!("{}/v1/me/reports", nodes[1]))
        .bearer_auth(&bob.token)
        .json(&serde_json::json!({"target_user_id": uid_a, "reason": "e2e"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "another reporter is not throttled");
}

/// Little-endian 16-bit PCM WAV → (sample_rate, channels, samples).
fn parse_wav_pcm16(bytes: &[u8]) -> (u32, u16, Vec<i16>) {
    assert!(
        bytes.len() > 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "not a RIFF/WAVE file ({} bytes)",
        bytes.len()
    );
    let mut pos = 12;
    let mut fmt = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        match id {
            b"fmt " => {
                let format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
                assert_eq!((format, bits), (1, 16), "expected 16-bit PCM");
                fmt = Some((rate, channels));
            }
            b"data" => {
                let (rate, channels) = fmt.expect("fmt chunk precedes data");
                let samples = body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes(*c))
                    .collect();
                return (rate, channels, samples);
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

fn rms_of(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|s| (f32::from(*s) / 32768.0).powi(2))
        .sum::<f32>()
        / samples.len() as f32)
        .sqrt()
}

/// Zero-crossing estimate of a mono tone's fundamental.
fn zero_crossing_hz(samples: &[i16], sample_rate: u32) -> f32 {
    let crossings = samples
        .windows(2)
        .filter(|w| (w[0] < 0) != (w[1] < 0))
        .count();
    crossings as f32 / 2.0 * sample_rate as f32 / samples.len().max(1) as f32
}

/// `GET` as JSON; `GET /v1/recordings/{id}` wraps the row (`{recording, consent, download_url}`),
/// which is unwrapped here so callers see the same shape as the list and the processing
/// endpoints.
async fn recording_json(env: &Env, http: &reqwest::Client, path: &str) -> serde_json::Value {
    let mut v: serde_json::Value = http
        .get(format!("{}{path}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    if let Some(row) = v.get_mut("recording") {
        return row.take();
    }
    v
}

/// Polls `path` until `["status"]` leaves `processing`/`queued`/`running`.
async fn await_processed(env: &Env, http: &reqwest::Client, path: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let v = recording_json(env, http, path).await;
        let status = v["status"].as_str().unwrap_or("");
        if !matches!(status, "processing" | "queued" | "running") {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{path} still `{status}` after 30 s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Records Alice and Bob separately, then renders channel mixdowns (WAV and Ogg/Opus) whose
/// timeline follows each track's first packet, and transcribes the mixdown track by track with
/// the mock STT so every segment names its speaker; lifecycle events reach SSE, downloads carry
/// the right content type, invalid requests are refused, and everything is tenant-scoped.
#[tokio::test]
#[ignore = "requires a running Aurix server with recording enabled and STT pointed at examples/mock_speech.rs"]
async fn recording_mixdown_and_post_hoc_transcript() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let (env, _app_id) = isolated_env(&base, &http, "mixdown").await;
    let channel_id = create_channel(&env, &http).await;
    let (tok_a, uid_a) = issue_token(&env, &http, "mixdown:alice", "Alice", channel_id).await;
    let (tok_b, uid_b) = issue_token(&env, &http, "mixdown:bob", "Bob", channel_id).await;
    let mut alice = connect(&env, "alice", tok_a).await;
    let mut bob = connect(&env, "bob", tok_b).await;
    for p in [&mut alice, &mut bob] {
        bind_media(p).await;
        join(p, channel_id).await;
    }
    drain_ws(&mut alice).await;
    drain_ws(&mut bob).await;

    // One track per participant; both consent.
    async fn record(
        env: &Env,
        http: &reqwest::Client,
        p: &mut Player,
        channel_id: ChannelId,
        uid: &str,
    ) -> uuid::Uuid {
        let rec: serde_json::Value = http
            .post(format!("{}/v1/recordings/start", env.api))
            .header("x-api-key", &env.api_key)
            .json(&serde_json::json!({"channel_id": channel_id, "user_id": uid}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(rec["kind"], "recording");
        assert_eq!(rec["status"], "recording");
        let recording_id: uuid::Uuid = rec["id"].as_str().unwrap().parse().unwrap();
        p.expect("RecordingNotification", |m| {
            matches!(m, ControlMessage::RecordingNotification { active: true, recording_id: r, .. } if *r == recording_id)
        })
        .await;
        p.send(&ControlMessage::RecordingConsentResponse {
            recording_id,
            consent: RecordingConsent::Accepted,
        })
        .await;
        recording_id
    }
    let rec_a = record(&env, &http, &mut alice, channel_id, &uid_a).await;
    let rec_b = record(&env, &http, &mut bob, channel_id, &uid_b).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A mixdown of a channel whose tracks are still running is refused.
    let r = http
        .post(format!("{}/v1/recordings/mixdown", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "sources": [rec_a]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "running track must not be mixed down");

    // Alice speaks first (440 Hz, 1 s); Bob starts ~1 s later (880 Hz, 1 s) — their tracks
    // therefore start at different wall-clock instants and must be aligned in the mix.
    let tone_a = opus_tone(440.0, 1000);
    let tone_b = opus_tone(880.0, 1000);
    stream_frames(&alice, channel_id, 1, &tone_a, false).await;
    stream_frames(&bob, channel_id, 1, &tone_b, false).await;
    drain_udp(&alice).await;
    drain_udp(&bob).await;
    for id in [rec_a, rec_b] {
        http.post(format!("{}/v1/recordings/{id}/stop", env.api))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let row = recording_json(&env, &http, &format!("/v1/recordings/{id}")).await;
        assert_eq!(row["status"], "ready", "{row}");
        assert!(
            row["audio_started_at"].is_string(),
            "first-packet timestamp recorded: {row}"
        );
    }

    let mut sse = SseClient::open(&env, &env.api_key, Some("recording.processed"))
        .await
        .expect("SSE stream");

    // WAV mixdown of the whole channel (sources omitted → every finished track).
    let mix: serde_json::Value = http
        .post(format!("{}/v1/recordings/mixdown", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "format": "wav"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mix["kind"], "mixdown", "{mix}");
    assert_eq!(mix["status"], "processing", "{mix}");
    assert_eq!(mix["format"], "wav", "{mix}");
    let mut sources = id_list(&mix["sources"]);
    sources.sort();
    let mut want = vec![rec_a.to_string(), rec_b.to_string()];
    want.sort();
    assert_eq!(sources, want, "{mix}");
    let mix_id: uuid::Uuid = mix["id"].as_str().unwrap().parse().unwrap();
    // Not downloadable while rendering.
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{mix_id}/download")).await,
        409
    );
    let done = await_processed(&env, &http, &format!("/v1/recordings/{mix_id}")).await;
    assert_eq!(done["status"], "ready", "{done}");
    let duration = done["duration_secs"].as_f64().unwrap();
    assert!(
        (1.8..=3.5).contains(&duration),
        "two 1 s tracks offset by ~1 s → ~2 s mix, got {duration}: {done}"
    );
    assert!(done["file_size_bytes"].as_u64().unwrap() > 44);
    let ev = sse
        .expect("recording.processed", Duration::from_secs(5))
        .await;
    assert_eq!(ev["data"]["recording_id"], mix_id.to_string(), "{ev}");
    assert_eq!(ev["data"]["job"], "mixdown", "{ev}");
    assert_eq!(ev["data"]["status"], "ready", "{ev}");

    let resp = http
        .get(format!("{}/v1/recordings/{mix_id}/download", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "audio/wav"
    );
    let disposition = resp.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.ends_with(".wav\""), "{disposition}");
    let wav = resp.bytes().await.unwrap();
    let (rate, channels, pcm) = parse_wav_pcm16(&wav);
    assert_eq!((rate, channels), (48_000, 1));
    let secs = pcm.len() as f64 / 48_000.0;
    assert!(
        (secs - duration).abs() < 0.1,
        "wav {secs}s vs row {duration}s"
    );
    // First 0.5 s: only Alice at 440 Hz; last 0.5 s: only Bob at 880 Hz. A 0.35 amplitude sine
    // has RMS ≈ 0.247.
    let head = &pcm[4_800..28_800];
    let tail = &pcm[pcm.len() - 24_000..];
    let (rms_head, rms_tail) = (rms_of(head), rms_of(tail));
    assert!(
        (0.18..=0.32).contains(&rms_head) && (0.18..=0.32).contains(&rms_tail),
        "both tracks present at their volume: head {rms_head} tail {rms_tail}"
    );
    let (hz_head, hz_tail) = (
        zero_crossing_hz(head, 48_000),
        zero_crossing_hz(tail, 48_000),
    );
    assert!(
        (hz_head - 440.0).abs() < 40.0,
        "Alice's tone leads the mix: {hz_head} Hz"
    );
    assert!(
        (hz_tail - 880.0).abs() < 60.0,
        "Bob's tone ends the mix: {hz_tail} Hz"
    );
    // Between Alice's end and Bob's start there is neither silence padding lost nor overlap
    // that doubles the level.
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
    assert!(peak < 20_000, "no doubled/overlapping tracks: peak {peak}");

    // Ogg/Opus mixdown of the same tracks, explicitly listed, downloads as audio/ogg.
    let mix2: serde_json::Value = http
        .post(format!("{}/v1/recordings/mixdown", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"channel_id": channel_id, "sources": [rec_a, rec_b]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mix2["format"], "ogg_opus", "{mix2}");
    let mix2_id: uuid::Uuid = mix2["id"].as_str().unwrap().parse().unwrap();
    let done2 = await_processed(&env, &http, &format!("/v1/recordings/{mix2_id}")).await;
    assert_eq!(done2["status"], "ready", "{done2}");
    let resp = http
        .get(format!("{}/v1/recordings/{mix2_id}/download", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "audio/ogg"
    );
    let ogg = resp.bytes().await.unwrap();
    assert!(ogg.starts_with(b"OggS") && ogg.windows(8).any(|w| w == b"OpusHead"));
    let ev = sse
        .expect("recording.processed", Duration::from_secs(5))
        .await;
    assert_eq!(ev["data"]["recording_id"], mix2_id.to_string(), "{ev}");

    // Listing shows tracks and mixdowns side by side.
    let list = recording_json(
        &env,
        &http,
        &format!("/v1/recordings?channel_id={channel_id}"),
    )
    .await;
    let kinds: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "mixdown").count(),
        2,
        "{list}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "recording").count(),
        2,
        "{list}"
    );

    // Invalid requests: a source from another channel, an unknown source, a mixdown of a
    // mixdown, an empty channel.
    let other = create_channel(&env, &http).await;
    for (body, want) in [
        (
            serde_json::json!({"channel_id": other, "sources": [rec_a]}),
            400,
        ),
        (
            serde_json::json!({"channel_id": channel_id, "sources": [uuid::Uuid::now_v7()]}),
            404,
        ),
        (
            serde_json::json!({"channel_id": channel_id, "sources": [mix_id]}),
            400,
        ),
        (serde_json::json!({"channel_id": other}), 404),
    ] {
        let r = http
            .post(format!("{}/v1/recordings/mixdown", env.api))
            .header("x-api-key", &env.api_key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), want, "{body}: {}", r.text().await.unwrap());
    }

    // Post-hoc transcript of the mixdown: one segment per speaker, on the mix timeline.
    let r = http
        .post(format!("{}/v1/recordings/{mix_id}/transcribe", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    if r.status() == 400 {
        let body = r.text().await.unwrap();
        assert!(body.contains("INVALID_CONFIG"), "{body}");
        eprintln!("no [stt] provider on the node; transcript part skipped (start examples/mock_speech.rs)");
    } else {
        let queued: serde_json::Value = r.error_for_status().unwrap().json().await.unwrap();
        assert!(
            matches!(queued["status"].as_str(), Some("queued" | "running")),
            "{queued}"
        );
        assert_eq!(queued["recording_id"], mix_id.to_string());
        let t = await_processed(&env, &http, &format!("/v1/recordings/{mix_id}/transcript")).await;
        assert_eq!(t["status"], "ready", "{t}");
        assert_eq!(t["language"], "en", "{t}");
        assert!(t["provider"].is_string(), "{t}");
        let segments = t["segments"].as_array().unwrap();
        let seg_a = segments
            .iter()
            .find(|s| s["speaker"] == uid_a)
            .unwrap_or_else(|| panic!("no segment for Alice: {t}"));
        let seg_b = segments
            .iter()
            .find(|s| s["speaker"] == uid_b)
            .unwrap_or_else(|| panic!("no segment for Bob: {t}"));
        let hz_a = tone_hz(seg_a["text"].as_str().unwrap()).unwrap();
        let hz_b = tone_hz(seg_b["text"].as_str().unwrap()).unwrap();
        assert!((hz_a - 440.0).abs() < 30.0, "Alice's segment: {seg_a}");
        assert!((hz_b - 880.0).abs() < 60.0, "Bob's segment: {seg_b}");
        // Leading silence (Opus pre-skip) is trimmed, so the first segment starts a few ms in.
        assert!(seg_a["start_ms"].as_u64().unwrap() < 50, "{seg_a}");
        let b_start = seg_b["start_ms"].as_u64().unwrap();
        assert!(
            (800..=2_000).contains(&b_start),
            "Bob's offset on the mix timeline: {seg_b}"
        );
        assert!(
            seg_a["words"].as_array().unwrap().len() == 2
                && seg_a["words"][1]["start_ms"].as_u64().unwrap() > 0,
            "word timings preserved: {seg_a}"
        );
        let text = t["text"].as_str().unwrap();
        assert!(
            text.contains("440") && text.contains("880"),
            "joined text in timeline order: {text}"
        );
        assert!(
            text.find("440").unwrap() < text.find("880").unwrap(),
            "{text}"
        );
        assert!(
            t["duration_ms"].as_u64().unwrap() >= 1_800,
            "duration of the mix: {t}"
        );
        let ev = sse
            .expect("recording.processed", Duration::from_secs(5))
            .await;
        assert_eq!(ev["data"]["recording_id"], mix_id.to_string(), "{ev}");
        assert_eq!(ev["data"]["job"], "transcript", "{ev}");
        assert_eq!(ev["data"]["status"], "ready", "{ev}");

        // Subtitle renderings.
        let srt = http
            .get(format!(
                "{}/v1/recordings/{mix_id}/transcript?format=srt",
                env.api
            ))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert!(srt.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/x-subrip"));
        let srt = srt.text().await.unwrap();
        assert!(
            srt.starts_with("1\n")
                && srt.contains(" --> ")
                && srt.contains(&uid_a)
                && srt.contains(&uid_b),
            "{srt}"
        );
        let vtt = http
            .get(format!(
                "{}/v1/recordings/{mix_id}/transcript?format=vtt",
                env.api
            ))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            vtt.starts_with("WEBVTT") && vtt.contains(&format!("<v {uid_b}>")),
            "{vtt}"
        );
        assert_eq!(
            status_of(
                &env,
                &http,
                &format!("/v1/recordings/{mix_id}/transcript?format=doc")
            )
            .await,
            400
        );

        // A single track is transcribed as one speaker.
        http.post(format!("{}/v1/recordings/{rec_b}/transcribe", env.api))
            .header("x-api-key", &env.api_key)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let tb = await_processed(&env, &http, &format!("/v1/recordings/{rec_b}/transcript")).await;
        assert_eq!(tb["status"], "ready", "{tb}");
        let segs = tb["segments"].as_array().unwrap();
        assert!(
            segs.iter().all(|s| s["speaker"] == uid_b) && !segs.is_empty(),
            "{tb}"
        );
        assert!(
            segs[0]["start_ms"].as_u64().unwrap() < 50,
            "own timeline starts at zero: {tb}"
        );
    }
    // No transcript yet → 404; unknown recording → 404.
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{rec_a}/transcript")).await,
        404
    );
    assert_eq!(
        status_of(
            &env,
            &http,
            &format!("/v1/recordings/{}/transcript", uuid::Uuid::now_v7())
        )
        .await,
        404
    );

    // Tenant isolation: another application sees none of it.
    if let Ok(api_key2) = std::env::var("AURIX_E2E_API_KEY2") {
        let env2 = Env {
            api_key: api_key2,
            ..env.clone()
        };
        for path in [
            format!("/v1/recordings/{mix_id}"),
            format!("/v1/recordings/{mix_id}/download"),
            format!("/v1/recordings/{mix_id}/transcript"),
        ] {
            assert_eq!(
                status_of(&env2, &http, &path).await,
                404,
                "foreign tenant must not read {path}"
            );
        }
        for (method, path, body) in [
            (
                "POST",
                "/v1/recordings/mixdown".to_string(),
                Some(serde_json::json!({"channel_id": channel_id, "sources": [rec_a, rec_b]})),
            ),
            ("POST", format!("/v1/recordings/{mix_id}/transcribe"), None),
        ] {
            let mut req = http
                .request(method.parse().unwrap(), format!("{}{path}", env2.api))
                .header("x-api-key", &env2.api_key);
            if let Some(b) = body {
                req = req.json(&b);
            }
            let r = req.send().await.unwrap();
            assert_eq!(
                r.status(),
                404,
                "foreign tenant must not {method} {path}: {}",
                r.text().await.unwrap()
            );
        }
    } else {
        eprintln!("AURIX_E2E_API_KEY2 not set; tenant isolation of mixdowns not exercised");
    }

    // Deleting a source track leaves the mixdown; deleting the mixdown removes its transcript.
    http.delete(format!("{}/v1/recordings/{rec_a}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{mix_id}")).await,
        200,
        "mixdown survives its source"
    );
    http.delete(format!("{}/v1/recordings/{mix_id}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{mix_id}")).await,
        404
    );
    assert_eq!(
        status_of(&env, &http, &format!("/v1/recordings/{mix_id}/transcript")).await,
        404,
        "transcript goes with the recording"
    );

    let _ = alice.ws.close(None).await;
    let _ = bob.ws.close(None).await;
}

// ── Admin SSO (OIDC) and administrator lifecycle ──

struct SsoHarness {
    api: String,
    mock: String,
    http: reqwest::Client,
}

struct SsoLoginResult {
    status: u16,
    body: serde_json::Value,
}

impl SsoHarness {
    async fn set_user(&self, user: serde_json::Value) {
        self.http
            .post(format!("{}/_mock/user", self.mock))
            .json(&user)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    async fn mock_stats(&self) -> serde_json::Value {
        self.http
            .get(format!("{}/_mock/stats", self.mock))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// `GET /admin/oidc/login` → (authorization URL at the provider, login cookie value).
    async fn begin(&self, return_to: Option<&str>) -> (reqwest::Url, String) {
        let mut url = format!("{}/admin/oidc/login", self.api);
        if let Some(path) = return_to {
            url.push_str("?return_to=");
            url.push_str(path);
        }
        let resp = self.http.get(url).send().await.unwrap();
        assert_eq!(resp.status(), 302, "login must redirect to the provider");
        let location = resp.headers()[reqwest::header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        let cookie = resp.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            cookie.starts_with("aurix_oidc_login="),
            "login cookie: {cookie}"
        );
        for attr in ["Path=/admin/oidc", "HttpOnly", "SameSite=Lax"] {
            assert!(cookie.contains(attr), "login cookie lacks {attr}: {cookie}");
        }
        assert!(
            !cookie.contains("Secure"),
            "plain-http redirect_url must not set a Secure cookie (browsers would drop it)"
        );
        let value = cookie
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches("aurix_oidc_login=")
            .to_string();
        (reqwest::Url::parse(&location).unwrap(), value)
    }

    /// Plays the browser: visits the provider's authorization endpoint and returns the URL the
    /// provider redirects back to (Aurix's callback with `code`/`state` or `error`).
    async fn authorize(&self, auth_url: &reqwest::Url) -> reqwest::Url {
        let resp = self.http.get(auth_url.clone()).send().await.unwrap();
        assert_eq!(resp.status(), 303, "mock provider redirects back");
        reqwest::Url::parse(resp.headers()[reqwest::header::LOCATION].to_str().unwrap()).unwrap()
    }

    async fn callback(&self, url: &reqwest::Url, cookie: Option<&str>) -> SsoLoginResult {
        let mut req = self.http.get(url.clone());
        if let Some(cookie) = cookie {
            req = req.header(
                reqwest::header::COOKIE,
                format!("aurix_oidc_login={cookie}"),
            );
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let clear = resp
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_string());
        if status != 500 {
            let clear = clear.expect("callback clears the login cookie");
            assert!(
                clear.starts_with("aurix_oidc_login=;") && clear.contains("Max-Age=0"),
                "{clear}"
            );
        }
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        SsoLoginResult { status, body }
    }

    /// Full happy-path browser dance for the currently configured mock user.
    async fn login(&self) -> SsoLoginResult {
        let (auth_url, cookie) = self.begin(None).await;
        let back = self.authorize(&auth_url).await;
        self.callback(&back, Some(&cookie)).await
    }

    async fn me(&self, token: &str) -> (u16, serde_json::Value) {
        let resp = self
            .http
            .get(format!("{}/admin/me", self.api))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(serde_json::Value::Null))
    }

    async fn status_of(&self, method: reqwest::Method, path: &str, token: &str) -> u16 {
        self.http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn admin_call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.api))
            .bearer_auth(token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(serde_json::Value::Null))
    }

    async fn password_login(&self, email: &str, password: &str) -> (u16, serde_json::Value) {
        let resp = self
            .http
            .post(format!("{}/admin/login", self.api))
            .json(&serde_json::json!({"email": email, "password": password}))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(serde_json::Value::Null))
    }
}

fn sso_user(sub: &str, email: &str, groups: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "sub": sub,
        "email": email,
        "email_verified": true,
        "name": format!("{sub} (sso)"),
        "groups": groups,
    })
}

/// Admin single sign-on against the `mock_oidc` example provider plus the administrator
/// lifecycle API. The node must run with `auth.oidc` pointing at the mock (see the
/// "admin SSO" CI step): client `aurix-admin`, `role_mapping = { aurix_admins = "admin",
/// aurix_ops = "moderator" }`, no `default_role`, `superadmin_emails = [root.sso@example.com]`,
/// `allowed_domains = [example.com]`, verified email required, auto-provision and role sync
/// on, no `frontend_redirect` (JSON callback). Opt in with `AURIX_E2E_OIDC=1`; the mock is at
/// `AURIX_E2E_OIDC_MOCK` (default `http://127.0.0.1:18791`); `AURIX_E2E_ADMIN_TOKEN` is a
/// superadmin token for the lifecycle part.
#[tokio::test]
#[ignore = "requires an Aurix node with auth.oidc pointed at the mock_oidc example (AURIX_E2E_OIDC=1)"]
async fn admin_sso_login_roles_and_lifecycle() {
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    if std::env::var("AURIX_E2E_OIDC").ok().as_deref() != Some("1") {
        eprintln!("AURIX_E2E_OIDC not set; skipping");
        return;
    }
    let superadmin = std::env::var("AURIX_E2E_ADMIN_TOKEN").expect("AURIX_E2E_ADMIN_TOKEN");
    let h = SsoHarness {
        api: env.api.clone(),
        mock: std::env::var("AURIX_E2E_OIDC_MOCK")
            .unwrap_or_else(|_| "http://127.0.0.1:18791".into()),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
    };
    let run = uuid::Uuid::new_v4().simple().to_string();
    let run = &run[..8];
    let alice_email = format!("Alice.{run}@Example.com");
    let alice_lower = alice_email.to_ascii_lowercase();

    // ── discovery for login pages ──
    let methods: serde_json::Value = h
        .http
        .get(format!("{}/admin/auth/methods", h.api))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(methods["password_login"], true);
    assert_eq!(methods["oidc"]["issuer"], h.mock.trim_end_matches('/'));
    assert_eq!(methods["oidc"]["login_url"], "/admin/oidc/login");

    // ── authorization request shape ──
    let (auth_url, cookie) = h.begin(Some("/apps")).await;
    assert!(
        auth_url
            .as_str()
            .starts_with(&format!("{}/authorize?", h.mock)),
        "{auth_url}"
    );
    let q: std::collections::HashMap<_, _> = auth_url.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "aurix-admin");
    assert!(q["redirect_uri"].ends_with("/admin/oidc/callback"), "{q:?}");
    assert!(q["scope"].split(' ').any(|s| s == "openid"), "{q:?}");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["code_challenge"].len() >= 43 && q["state"].len() >= 32 && q["nonce"].len() >= 16);

    // Provider error (user cancelled) is reported, not treated as a login.
    h.set_user(serde_json::json!({"sub": format!("cancel-{run}"), "deny": true}))
        .await;
    let back = h.authorize(&auth_url).await;
    assert!(
        back.query().unwrap().contains("error=access_denied"),
        "{back}"
    );
    let res = h.callback(&back, Some(&cookie)).await;
    assert_eq!(res.status, 401, "{}", res.body);
    assert_eq!(res.body["error"]["code"], "AUTH_FAILED");

    // ── happy path: provisioning + group → role mapping ──
    h.set_user(sso_user(
        &format!("sso-alice-{run}"),
        &alice_email,
        &["aurix_ops", "unrelated-group"],
    ))
    .await;
    let (auth_url, cookie) = h.begin(Some("/apps")).await;
    let back = h.authorize(&auth_url).await;
    let bq: std::collections::HashMap<_, _> = back.query_pairs().into_owned().collect();
    let aq: std::collections::HashMap<_, _> = auth_url.query_pairs().into_owned().collect();
    assert_eq!(bq["state"], aq["state"], "provider echoes the sealed state");
    assert!(bq.contains_key("code"));

    // Without the login cookie the callback is refused (CSRF / login fixation) and the state
    // is not consumed, so the same browser can finish with the cookie.
    let res = h.callback(&back, None).await;
    assert_eq!(res.status, 401, "{}", res.body);
    let res = h.callback(&back, Some("not-the-cookie")).await;
    assert_eq!(res.status, 401, "{}", res.body);
    let first_login_at = std::time::Instant::now();
    let res = h.callback(&back, Some(&cookie)).await;
    assert_eq!(res.status, 200, "{}", res.body);
    let alice_token = res.body["token"].as_str().unwrap().to_string();
    assert_eq!(res.body["return_to"], "/apps");
    assert!(res.body["expires_in_secs"].as_i64().unwrap() > 0);
    let alice = &res.body["admin"];
    assert_eq!(alice["email"], alice_lower, "emails are normalised");
    assert_eq!(alice["role"], "moderator", "highest mapped group wins");
    assert_eq!(alice["auth_source"], "oidc");
    assert_eq!(alice["sso_bound"], true);
    assert_eq!(alice["has_password"], false);
    assert_eq!(alice["display_name"], format!("sso-alice-{run} (sso)"));
    let alice_id = alice["id"].as_str().unwrap().to_string();
    assert!(alice["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p == "audit:read"));

    // Replaying the same code/state/cookie triple is refused.
    let res = h.callback(&back, Some(&cookie)).await;
    assert_eq!(res.status, 401, "replay: {}", res.body);

    // ── the token carries the *current* role and permission checks bite ──
    let (status, me) = h.me(&alice_token).await;
    assert_eq!(status, 200, "{me}");
    assert_eq!(me["role"], "moderator");
    assert_eq!(me["auth_source"], "oidc");
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/admin/audit-log", &alice_token)
            .await,
        200
    );
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/v1/apps", &alice_token)
            .await,
        200
    );
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/v1/nodes", &alice_token)
            .await,
        200
    );
    let (status, _) = h
        .admin_call(
            reqwest::Method::POST,
            "/v1/apps",
            &alice_token,
            Some(serde_json::json!({"name": format!("sso-e2e-{run}")})),
        )
        .await;
    assert_eq!(status, 403, "moderators cannot create apps");
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/admin/admins", &alice_token)
            .await,
        403
    );
    assert_eq!(
        h.status_of(
            reqwest::Method::POST,
            "/admin/retention/sweep",
            &alice_token
        )
        .await,
        403
    );

    // ── role sync: the provider now says "admin"; the moderator token dies ──
    h.set_user(sso_user(
        &format!("sso-alice-{run}"),
        &alice_email,
        &["aurix_admins", "aurix_ops"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(
        res.body["admin"]["id"], alice_id,
        "same account, not a duplicate"
    );
    assert_eq!(res.body["admin"]["role"], "admin");
    let alice_admin_token = res.body["token"].as_str().unwrap().to_string();
    assert_eq!(
        h.me(&alice_token).await.0,
        401,
        "a role change revokes tokens issued before it"
    );
    assert_eq!(h.me(&alice_admin_token).await.1["role"], "admin");
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/admin/admins", &alice_admin_token)
            .await,
        403,
        "admins still cannot manage administrators"
    );

    // ── superadmin allow-list beats groups; unmapped / foreign / unverified users are refused ──
    // The allow-listed email is fixed by the node config, so the subject is fixed as well:
    // the account persists between runs and a different subject would be a hijack.
    h.set_user(sso_user("sso-root", "Root.SSO@example.com", &[]))
        .await;
    let res = h.login().await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(res.body["admin"]["role"], "superadmin");
    let root_sso_token = res.body["token"].as_str().unwrap().to_string();
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/admin/admins", &root_sso_token)
            .await,
        200
    );

    h.set_user(sso_user(
        &format!("sso-nobody-{run}"),
        &format!("nobody.{run}@example.com"),
        &["random"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 403, "no default role → denied: {}", res.body);
    assert_eq!(res.body["error"]["code"], "AUTH_DENIED");

    h.set_user(sso_user(
        &format!("sso-evil-{run}"),
        &format!("mallory.{run}@evil.example.net"),
        &["aurix_admins"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 403, "domain allow-list: {}", res.body);

    let mut unverified = sso_user(
        &format!("sso-unverified-{run}"),
        &format!("unverified.{run}@example.com"),
        &["aurix_ops"],
    );
    unverified["email_verified"] = serde_json::Value::Bool(false);
    h.set_user(unverified).await;
    let res = h.login().await;
    assert_eq!(res.status, 401, "unverified email: {}", res.body);

    let mut tampered = sso_user(&format!("sso-alice-{run}"), &alice_email, &["aurix_admins"]);
    tampered["wrong_nonce"] = serde_json::Value::Bool(true);
    h.set_user(tampered).await;
    let res = h.login().await;
    assert_eq!(res.status, 401, "nonce mismatch: {}", res.body);

    let mut wrong_aud = sso_user(&format!("sso-alice-{run}"), &alice_email, &["aurix_admins"]);
    wrong_aud["wrong_audience"] = serde_json::Value::Bool(true);
    h.set_user(wrong_aud).await;
    let res = h.login().await;
    assert_eq!(res.status, 401, "audience mismatch: {}", res.body);

    // Providers that keep e-mail/groups out of the ID token are served from userinfo.
    let users_before = h.mock_stats().await["userinfo"].as_u64().unwrap();
    let mut via_userinfo = sso_user(
        &format!("sso-carol-{run}"),
        &format!("carol.{run}@example.com"),
        &["aurix_ops"],
    );
    via_userinfo["email_in_userinfo_only"] = serde_json::Value::Bool(true);
    h.set_user(via_userinfo).await;
    let res = h.login().await;
    assert_eq!(res.status, 200, "userinfo fallback: {}", res.body);
    assert_eq!(res.body["admin"]["role"], "moderator");
    assert!(
        h.mock_stats().await["userinfo"].as_u64().unwrap() > users_before,
        "email/groups missing from the ID token are fetched from userinfo"
    );

    // ── binding an SSO identity to an existing password account by e-mail ──
    let bob_email = format!("bob.{run}@example.com");
    let (status, bob) = h
        .admin_call(
            reqwest::Method::POST,
            "/admin/admins",
            &superadmin,
            Some(serde_json::json!({
                "email": bob_email,
                "password": "bob-password-0123456789",
                "display_name": "Bob",
                "role": "viewer",
            })),
        )
        .await;
    assert_eq!(status, 200, "{bob}");
    let bob_id = bob["id"].as_str().unwrap().to_string();
    assert_eq!(bob["auth_source"], "password");
    assert_eq!(bob["sso_bound"], false);
    h.set_user(sso_user(
        &format!("sso-bob-{run}"),
        &bob_email,
        &["aurix_ops"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 200, "{}", res.body);
    assert_eq!(
        res.body["admin"]["id"], bob_id,
        "bound to the existing account"
    );
    assert_eq!(
        res.body["admin"]["role"], "moderator",
        "role synced from the provider"
    );
    assert_eq!(res.body["admin"]["sso_bound"], true);
    assert_eq!(res.body["admin"]["has_password"], true);
    let (status, _) = h
        .password_login(&bob_email, "bob-password-0123456789")
        .await;
    assert_eq!(
        status, 200,
        "the local password keeps working after binding"
    );
    // Someone else at the provider claiming Bob's e-mail cannot take over his account.
    h.set_user(sso_user(
        &format!("sso-bob-impostor-{run}"),
        &bob_email,
        &["aurix_admins"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 401, "identity hijack: {}", res.body);
    assert_eq!(
        h.admin_call(
            reqwest::Method::GET,
            &format!("/admin/admins/{bob_id}"),
            &superadmin,
            None
        )
        .await
        .1["role"],
        "moderator",
        "the refused login changed nothing"
    );

    // ── lifecycle via the REST API (superadmin) ──
    let (status, list) = h
        .admin_call(reqwest::Method::GET, "/admin/admins", &superadmin, None)
        .await;
    assert_eq!(status, 200, "{list}");
    let listed: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["email"].as_str())
        .collect();
    assert!(listed.contains(&alice_lower.as_str()) && listed.contains(&bob_email.as_str()));

    // Demote Alice → her admin token dies; a display-name edit alone keeps tokens.
    let (status, updated) = h
        .admin_call(
            reqwest::Method::PATCH,
            &format!("/admin/admins/{alice_id}"),
            &superadmin,
            Some(serde_json::json!({"role": "viewer"})),
        )
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["role"], "viewer");
    assert_eq!(h.me(&alice_admin_token).await.0, 401);
    let res = h.password_login(&alice_lower, "irrelevant").await;
    assert_eq!(res.0, 401, "SSO-only accounts have no usable password");
    // Give her a local password (break-glass when the IdP is down); that revokes tokens too.
    h.set_user(sso_user(
        &format!("sso-alice-{run}"),
        &alice_email,
        &["aurix_ops"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 200);
    assert_eq!(
        res.body["admin"]["role"], "moderator",
        "sync_roles re-applies the provider role"
    );
    let alice_token = res.body["token"].as_str().unwrap().to_string();
    let (status, _) = h
        .admin_call(
            reqwest::Method::PATCH,
            &format!("/admin/admins/{alice_id}"),
            &superadmin,
            Some(serde_json::json!({"display_name": "Alice Renamed"})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        h.me(&alice_token).await.0,
        200,
        "display-name edits keep tokens"
    );
    let (status, _) = h
        .admin_call(
            reqwest::Method::POST,
            &format!("/admin/admins/{alice_id}/password"),
            &superadmin,
            Some(serde_json::json!({"password": "alice-break-glass-0123"})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        h.me(&alice_token).await.0,
        401,
        "password reset revokes tokens"
    );
    let (status, login) = h
        .password_login(&alice_lower, "alice-break-glass-0123")
        .await;
    assert_eq!(status, 200, "{login}");
    let alice_pw_token = login["token"].as_str().unwrap().to_string();
    let (_, me) = h.me(&alice_pw_token).await;
    assert_eq!(me["auth_source"], "password");
    assert_eq!(me["has_password"], true);
    assert_eq!(me["sso_bound"], true);
    assert_eq!(me["display_name"], "Alice Renamed");

    // Own password change: wrong current password refused, correct one revokes everything.
    let (status, _) = h
        .admin_call(
            reqwest::Method::POST,
            "/admin/me/password",
            &alice_pw_token,
            Some(serde_json::json!({
                "current_password": "wrong-password-0123456",
                "new_password": "alice-new-password-0123",
            })),
        )
        .await;
    assert_eq!(status, 401);
    let (status, _) = h
        .admin_call(
            reqwest::Method::POST,
            "/admin/me/password",
            &alice_pw_token,
            Some(serde_json::json!({
                "current_password": "alice-break-glass-0123",
                "new_password": "alice-new-password-0123",
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.me(&alice_pw_token).await.0, 401);
    let (status, login) = h
        .password_login(&alice_lower, "alice-new-password-0123")
        .await;
    assert_eq!(status, 200, "{login}");
    let alice_pw_token = login["token"].as_str().unwrap().to_string();

    // Logout-all (self), then logout-all by a superadmin for another admin.
    let res = h.login().await;
    let alice_sso_token = res.body["token"].as_str().unwrap().to_string();
    let (status, _) = h
        .admin_call(
            reqwest::Method::POST,
            "/admin/logout-all",
            &alice_pw_token,
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.me(&alice_pw_token).await.0, 401);
    assert_eq!(
        h.me(&alice_sso_token).await.0,
        401,
        "logout-all covers SSO tokens too"
    );
    let (_, login) = h
        .password_login(&alice_lower, "alice-new-password-0123")
        .await;
    let alice_pw_token = login["token"].as_str().unwrap().to_string();
    let (status, _) = h
        .admin_call(
            reqwest::Method::POST,
            &format!("/admin/admins/{alice_id}/logout-all"),
            &superadmin,
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.me(&alice_pw_token).await.0, 401);

    // Deactivate: tokens die, SSO and password logins are refused, reactivation restores.
    let (status, deactivated) = h
        .admin_call(
            reqwest::Method::PATCH,
            &format!("/admin/admins/{alice_id}"),
            &superadmin,
            Some(serde_json::json!({"active": false})),
        )
        .await;
    assert_eq!(status, 200, "{deactivated}");
    assert_eq!(deactivated["active"], false);
    assert_eq!(
        h.login().await.status,
        401,
        "deactivated accounts cannot sign in via SSO"
    );
    assert_eq!(
        h.password_login(&alice_lower, "alice-new-password-0123")
            .await
            .0,
        401
    );
    let (status, _) = h
        .admin_call(
            reqwest::Method::PATCH,
            &format!("/admin/admins/{alice_id}"),
            &superadmin,
            Some(serde_json::json!({"active": true})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(h.login().await.status, 200, "reactivated");

    // Guards: nobody can demote or deactivate themself; deactivating another superadmin is
    // fine while one remains, and a superadmin signed in through SSO manages admins too.
    let (_, me) = h.me(&superadmin).await;
    let super_id = me["id"].as_str().unwrap();
    for body in [
        serde_json::json!({"role": "admin"}),
        serde_json::json!({"active": false}),
    ] {
        let (status, err) = h
            .admin_call(
                reqwest::Method::PATCH,
                &format!("/admin/admins/{super_id}"),
                &superadmin,
                Some(body),
            )
            .await;
        assert_eq!(status, 400, "{err}");
    }
    let (_, root_me) = h.me(&root_sso_token).await;
    let root_sso_id = root_me["id"].as_str().unwrap();
    let (status, err) = h
        .admin_call(
            reqwest::Method::PATCH,
            &format!("/admin/admins/{root_sso_id}"),
            &root_sso_token,
            Some(serde_json::json!({"role": "admin"})),
        )
        .await;
    assert_eq!(
        status, 400,
        "SSO superadmin cannot demote themself either: {err}"
    );
    let second_email = format!("second.{run}@example.com");
    let (status, second) = h
        .admin_call(
            reqwest::Method::POST,
            "/admin/admins",
            &root_sso_token,
            Some(serde_json::json!({
                "email": second_email,
                "password": "second-password-0123456789",
                "display_name": "Second",
                "role": "superadmin",
            })),
        )
        .await;
    assert_eq!(status, 200, "{second}");
    let second_id = second["id"].as_str().unwrap().to_string();
    let (_, login) = h
        .password_login(&second_email, "second-password-0123456789")
        .await;
    let second_token = login["token"].as_str().unwrap().to_string();
    assert_eq!(
        h.status_of(reqwest::Method::GET, "/admin/admins", &second_token)
            .await,
        200
    );
    let (status, _) = h
        .admin_call(
            reqwest::Method::PATCH,
            &format!("/admin/admins/{second_id}"),
            &root_sso_token,
            Some(serde_json::json!({"active": false})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        h.me(&second_token).await.0,
        401,
        "deactivation revokes tokens"
    );
    assert_eq!(
        h.password_login(&second_email, "second-password-0123456789")
            .await
            .0,
        401
    );
    assert_eq!(
        h.me(&superadmin).await.0,
        200,
        "the bootstrap superadmin is untouched"
    );

    // ── signing-key rotation at the provider: unknown kid → JWKS refetch ──
    // The provider refetches JWKS for an unknown `kid` at most once per 30 s.
    let since = first_login_at.elapsed();
    if since < Duration::from_secs(31) {
        tokio::time::sleep(Duration::from_secs(31) - since).await;
    }
    let jwks_before = h.mock_stats().await["jwks"].as_u64().unwrap();
    let rotated: serde_json::Value = h
        .http
        .post(format!("{}/_mock/rotate", h.mock))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rotated["alg"], "ES256");
    h.set_user(sso_user(
        &format!("sso-alice-{run}"),
        &alice_email,
        &["aurix_ops"],
    ))
    .await;
    let res = h.login().await;
    assert_eq!(res.status, 200, "login after key rotation: {}", res.body);
    assert!(h.mock_stats().await["jwks"].as_u64().unwrap() > jwks_before);

    let stats = h.mock_stats().await;
    assert_eq!(
        stats["token_rejected"], 0,
        "every code exchange carried the right PKCE verifier, redirect and client secret"
    );
}

// ── Usage analytics, export and per-application quotas ──

/// Opens a WebSocket and returns the error code the server refuses the session with, or
/// `None` when a `SessionInitAck` arrives (the socket is dropped either way).
async fn connect_refused(env: &Env, token: &str) -> Option<String> {
    let mut req = format!("{}/ws", env.ws).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws connect");
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("first control message")
            .expect("ws closed before the first control message")
            .expect("ws error");
        match m {
            Message::Text(t) => {
                return match serde_json::from_str::<ControlMessage>(&t).unwrap() {
                    ControlMessage::SessionInitAck { .. } => None,
                    ControlMessage::Error { code, .. } => Some(code),
                    other => panic!("unexpected first message {other:?}"),
                }
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn admin_json(
    http: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut req = http.request(method, url).bearer_auth(token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn tenant_get(env: &Env, http: &reqwest::Client, path: &str) -> (u16, serde_json::Value) {
    let resp = http
        .get(format!("{}{path}", env.api))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn quota_of(env: &Env, http: &reqwest::Client) -> serde_json::Value {
    let (status, q) = tenant_get(env, http, "/v1/analytics/quota").await;
    assert_eq!(status, 200, "quota: {q}");
    q
}

/// Polls `f` every two seconds until it returns `Some`, or panics after `wait`.
async fn eventually<T, F, Fut>(wait: Duration, what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Usage accounting end to end. An administrator creates a throwaway application capped at two
/// concurrent sessions; two players (on two nodes when `AURIX_E2E_WS2` is set) talk and chat in a
/// channel, a third is refused with `QUOTA_EXCEEDED` while a reconnect of an admitted player is
/// not, raising the cap admits the third. After the aggregator ran, `GET /v1/analytics*` shows
/// the CCU, minutes, media bytes and chat of the application and the channel (derived across
/// both nodes), the JSON/CSV exports carry the raw buckets, another tenant and a key without
/// `analytics:read` see nothing, and the fleet views require an administrator. Finally a
/// one-minute monthly quota refuses a new channel join once the live members have used it,
/// and deleting the application takes its usage with it.
#[tokio::test]
#[ignore = "requires a running Aurix server and AURIX_E2E_ADMIN_TOKEN; see the e2e job in .github/workflows/ci.yml"]
async fn usage_analytics_export_and_per_app_quotas() {
    let Some(base) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let Ok(admin) = std::env::var("AURIX_E2E_ADMIN_TOKEN") else {
        eprintln!("AURIX_E2E_ADMIN_TOKEN not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let started = chrono::Utc::now();

    // ── A dedicated application with a CCU cap of 2 ──
    let (status, bad) = admin_json(
        &http,
        reqwest::Method::POST,
        format!("{}/v1/apps", base.api),
        &admin,
        Some(serde_json::json!({"name": "usage-e2e", "max_concurrent_sessions": -1})),
    )
    .await;
    assert_eq!(status, 400, "negative quota is rejected: {bad}");
    let (status, app) = admin_json(
        &http,
        reqwest::Method::POST,
        format!("{}/v1/apps", base.api),
        &admin,
        Some(serde_json::json!({
            "name": format!("usage-e2e-{}", uuid::Uuid::now_v7()),
            "max_concurrent_sessions": 2,
        })),
    )
    .await;
    assert_eq!(status, 200, "create app: {app}");
    let app_id = app["id"].as_str().unwrap().to_string();
    let (status, shown) = admin_json(
        &http,
        reqwest::Method::GET,
        format!("{}/v1/apps/{app_id}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200, "get app: {shown}");
    assert_eq!(shown["max_concurrent_sessions"], 2);
    assert_eq!(shown["monthly_participant_minutes"], 0);
    let env = Env {
        api_key: app["api_key"].as_str().unwrap().to_string(),
        ..base.clone()
    };
    let env2 = std::env::var("AURIX_E2E_WS2").ok().map(|ws| Env {
        api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
        ws,
        api_key: env.api_key.clone(),
    });
    let far = env2.as_ref().unwrap_or(&env);
    eprintln!(
        "app {app_id}; Bob on {}",
        if env2.is_some() { "node 2" } else { "node 1" }
    );

    let channel = create_channel(&env, &http).await;
    let (tok_a, _) = issue_token(&env, &http, "usage:alice", "Alice", channel).await;
    let (tok_b, _) = issue_token(&env, &http, "usage:bob", "Bob", channel).await;
    let (tok_c, _) = issue_token(&env, &http, "usage:carol", "Carol", channel).await;
    let (tok_d, _) = issue_token(&env, &http, "usage:dave", "Dave", channel).await;

    let mut alice = connect(&env, "Alice", tok_a.clone()).await;
    let mut bob = connect(far, "Bob", tok_b).await;
    join(&mut alice, channel).await;
    join(&mut bob, channel).await;
    bind_media(&mut alice).await;
    bind_media(&mut bob).await;
    // Bob's join ack carried Alice in the roster; give the fleet a moment to fan the join out.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Media (metered bytes) and chat (metered count) in the channel.
    let payload = Bytes::from_static(&[0x77u8; 120]);
    let mut heard = 0;
    for round in 0..3u32 {
        send_audio(&alice, channel, 1 + round * 10, &payload).await;
        heard += count_audio_from(&bob, alice.ssrc, &payload).await;
        if heard >= 5 {
            break;
        }
    }
    assert!(heard >= 5, "Bob hears Alice ({heard} packets)");
    for i in 0..3 {
        alice
            .send(&ControlMessage::ChatSend {
                channel_id: channel,
                text: format!("usage {i}"),
                metadata: None,
                client_ref: None,
            })
            .await;
        expect_chat(&mut bob, "chat").await;
    }

    // ── Concurrent-session quota ──
    let q = quota_of(&env, &http).await;
    assert_eq!(q["max_concurrent_sessions"], 2, "{q}");
    assert_eq!(q["active_sessions"], 2, "{q}");
    assert_eq!(
        connect_refused(&env, &tok_c).await.as_deref(),
        Some("QUOTA_EXCEEDED"),
        "a third session is refused at the cap"
    );
    if let Some(far) = &env2 {
        assert_eq!(
            connect_refused(far, &tok_c).await.as_deref(),
            Some("QUOTA_EXCEEDED"),
            "the cap is fleet-wide, not per node"
        );
    }
    // Alice reconnecting (same user, same node) replaces her session and is not counted twice.
    let mut alice2 = connect(&env, "Alice", tok_a).await;
    assert_ne!(alice2.session_id, alice.session_id);
    join(&mut alice2, channel).await;
    let _ = alice.try_recv(Duration::from_secs(3)).await; // old socket is closed by the server
    eventually(Duration::from_secs(10), "old session closed", || async {
        (quota_of(&env, &http).await["active_sessions"] == 2).then_some(())
    })
    .await;
    assert_eq!(
        connect_refused(&env, &tok_c).await.as_deref(),
        Some("QUOTA_EXCEEDED"),
        "still full after the replacement"
    );
    let (status, patched) = admin_json(
        &http,
        reqwest::Method::PATCH,
        format!("{}/v1/apps/{app_id}", base.api),
        &admin,
        Some(serde_json::json!({"max_concurrent_sessions": 3})),
    )
    .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["max_concurrent_sessions"], 3);
    let mut carol = connect(&env, "Carol", tok_c.clone()).await;
    let q = quota_of(&env, &http).await;
    assert_eq!(q["active_sessions"], 3, "{q}");
    assert_eq!(q["max_concurrent_sessions"], 3, "{q}");

    // ── Isolation: other tenant, key without analytics:read, no admin token ──
    let (status, _) = tenant_get(&base, &http, &format!("/v1/analytics/channels/{channel}")).await;
    assert_eq!(
        status, 404,
        "another tenant cannot read the channel's usage"
    );
    let restricted: serde_json::Value = http
        .post(format!("{}/v1/api-keys", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({"name": "no-analytics", "permissions": ["channels:read"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let restricted = Env {
        api_key: restricted["key"].as_str().unwrap().to_string(),
        ..env.clone()
    };
    for path in [
        "/v1/analytics",
        "/v1/analytics/quota",
        "/v1/analytics/channels",
        "/v1/analytics/export",
    ] {
        let (status, _) = tenant_get(&restricted, &http, path).await;
        assert_eq!(status, 403, "{path} needs analytics:read");
    }
    for path in ["/admin/analytics/usage", "/admin/analytics/export"] {
        let (status, _) = tenant_get(&env, &http, path).await;
        assert_eq!(status, 401, "{path} is an administrator view");
        let status = http
            .get(format!("{}{path}", env.api))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert_eq!(status, 401, "{path} without credentials");
    }
    for bad in [
        "/v1/analytics?from=yesterday",
        "/v1/analytics?from=2024-01-02T00:00:00Z&to=2024-01-01T00:00:00Z",
        "/v1/analytics?step=299",
        "/v1/analytics?from=2020-01-01T00:00:00Z&to=2024-01-01T00:00:00Z",
        "/v1/analytics?from=2024-01-01T00:00:00Z&to=2024-12-31T00:00:00Z&step=300",
        "/v1/analytics/export?scope=fleet",
        "/v1/analytics/export?format=xml",
    ] {
        let (status, body) = tenant_get(&env, &http, bad).await;
        assert_eq!(status, 400, "{bad}: {body}");
    }

    // ── The aggregator derives CCU/minutes from both nodes' intervals ──
    let from = (started - chrono::Duration::minutes(10))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let range = format!("from={from}");
    let usage = eventually(Duration::from_secs(150), "usage aggregation", || async {
        let (status, u) = tenant_get(&env, &http, &format!("/v1/analytics?{range}")).await;
        assert_eq!(status, 200, "{u}");
        let t = &u["totals"];
        let ready = t["peak_sessions"].as_i64().unwrap_or(0) >= 2
            && t["participant_minutes"].as_f64().unwrap_or(0.0) > 0.0
            && t["media_bytes_in"].as_i64().unwrap_or(0) > 0
            && t["chat_messages"].as_i64().unwrap_or(0) >= 3;
        ready.then_some(u)
    })
    .await;
    eprintln!("usage totals: {}", usage["totals"]);
    assert_eq!(usage["current"]["active_sessions"], 3, "{usage}");
    assert_eq!(usage["current"]["active_channels"], 1, "{usage}");
    assert_eq!(usage["range"]["step_secs"], 300, "{usage}");
    assert!(usage["range"]["finalized_through"].is_string(), "{usage}");
    let totals = &usage["totals"];
    assert!(totals["peak_sessions"].as_i64().unwrap() <= 3, "{totals}");
    assert!(
        totals["sessions_started"].as_i64().unwrap() >= 3,
        "{totals}"
    );
    assert!(
        totals["peak_participants"].as_i64().unwrap() >= 2,
        "{totals}"
    );
    assert!(
        totals["session_minutes"].as_f64().unwrap() > 0.0,
        "{totals}"
    );
    assert!(totals["media_bytes_out"].as_i64().unwrap() > 0, "{totals}");
    let series = usage["series"].as_array().unwrap();
    assert!(!series.is_empty(), "{usage}");
    assert!(
        series
            .iter()
            .any(|b| b["peak_sessions"].as_i64().unwrap() >= 2
                && b["active_channels"].as_i64().unwrap() >= 1
                && b["unique_users"].as_i64().unwrap() >= 2),
        "a 5-minute bucket holds both players: {series:?}"
    );
    let (status, hourly) =
        tenant_get(&env, &http, &format!("/v1/analytics?{range}&step=3600")).await;
    assert_eq!(status, 200, "{hourly}");
    assert_eq!(hourly["range"]["step_secs"], 3600);
    assert_eq!(hourly["totals"]["peak_sessions"], totals["peak_sessions"]);
    assert!(
        hourly["series"].as_array().unwrap().len() <= 2,
        "hourly rollup: {}",
        hourly["series"]
    );

    let (status, channels) =
        tenant_get(&env, &http, &format!("/v1/analytics/channels?{range}")).await;
    assert_eq!(status, 200, "{channels}");
    let mine = channels["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channel_id"] == channel.to_string())
        .unwrap_or_else(|| panic!("channel listed: {channels}"))
        .clone();
    assert!(mine["peak_participants"].as_i64().unwrap() >= 2, "{mine}");
    assert!(
        mine["participant_minutes"].as_f64().unwrap() > 0.0,
        "{mine}"
    );
    assert!(mine["joins"].as_i64().unwrap() >= 3, "{mine}");
    assert_eq!(mine["unique_users"], 2, "{mine}");
    assert!(mine["chat_messages"].as_i64().unwrap() >= 3, "{mine}");
    let (status, one) = tenant_get(
        &env,
        &http,
        &format!("/v1/analytics/channels/{channel}?{range}"),
    )
    .await;
    assert_eq!(status, 200, "{one}");
    assert_eq!(one["range"]["step_secs"], 3600);
    assert_eq!(
        one["totals"]["peak_participants"],
        mine["peak_participants"]
    );
    assert!(!one["series"].as_array().unwrap().is_empty(), "{one}");

    // ── Exports ──
    let (status, export) = tenant_get(&env, &http, &format!("/v1/analytics/export?{range}")).await;
    assert_eq!(status, 200, "{export}");
    assert_eq!(export["scope"], "app");
    assert_eq!(export["truncated"], false);
    let rows = export["rows"].as_array().unwrap();
    assert_eq!(export["count"], rows.len());
    assert!(
        !rows.is_empty() && rows.iter().all(|r| r["app_id"] == app_id),
        "{export}"
    );
    let csv = http
        .get(format!(
            "{}/v1/analytics/export?{range}&scope=channels&format=csv",
            env.api
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(csv.status(), 200);
    assert_eq!(
        csv.headers()["content-type"].to_str().unwrap(),
        "text/csv; charset=utf-8"
    );
    assert!(csv.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment; filename=\"usage-channels-"));
    assert!(csv.headers().get("x-aurix-truncated").is_none());
    let body = csv.text().await.unwrap();
    let mut lines = body.lines();
    assert_eq!(
        lines.next().unwrap(),
        "app_id,channel_id,bucket,peak_participants,participant_minutes,joins,unique_users,chat_messages,tts_requests,tts_characters,stt_audio_ms"
    );
    let data: Vec<&str> = lines.collect();
    assert!(
        data.iter()
            .all(|l| l.starts_with(&format!("{app_id},{channel},"))),
        "{body}"
    );
    assert!(!data.is_empty(), "{body}");
    let (status, other) = tenant_get(
        &base,
        &http,
        &format!("/v1/analytics/export?{range}&scope=channels"),
    )
    .await;
    assert_eq!(status, 200, "{other}");
    assert!(
        other["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["app_id"] != app_id),
        "another tenant's export never carries this application"
    );

    // ── Fleet views ──
    let (status, fleet) = admin_json(
        &http,
        reqwest::Method::GET,
        format!("{}/admin/analytics/usage?{range}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200, "{fleet}");
    let fleet_app = fleet["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["app_id"] == app_id)
        .unwrap_or_else(|| panic!("application in the fleet view: {fleet}"));
    assert_eq!(fleet_app["peak_sessions"], totals["peak_sessions"]);
    assert_eq!(fleet_app["chat_messages"], totals["chat_messages"]);
    let (status, admin_app) = admin_json(
        &http,
        reqwest::Method::GET,
        format!("{}/admin/analytics/apps/{app_id}?{range}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200, "{admin_app}");
    assert_eq!(admin_app["app_id"], app_id);
    assert_eq!(
        admin_app["totals"]["peak_sessions"],
        totals["peak_sessions"]
    );
    assert_eq!(admin_app["quota"]["max_concurrent_sessions"], 3);
    assert_eq!(admin_app["quota"]["active_sessions"], 3);
    let fleet_csv = http
        .get(format!(
            "{}/admin/analytics/export?{range}&format=csv",
            base.api
        ))
        .bearer_auth(&admin)
        .send()
        .await
        .unwrap();
    assert_eq!(fleet_csv.status(), 200);
    let fleet_csv = fleet_csv.text().await.unwrap();
    assert!(
        fleet_csv.starts_with("app_id,bucket,peak_sessions,"),
        "{fleet_csv}"
    );
    assert!(
        fleet_csv
            .lines()
            .any(|l| l.starts_with(&format!("{app_id},"))),
        "fleet export carries the application"
    );

    // ── Monthly participant-minutes quota ──
    let (status, patched) = admin_json(
        &http,
        reqwest::Method::PATCH,
        format!("{}/v1/apps/{app_id}", base.api),
        &admin,
        Some(serde_json::json!({"max_concurrent_sessions": 0, "monthly_participant_minutes": 1})),
    )
    .await;
    assert_eq!(status, 200, "{patched}");
    assert_eq!(patched["monthly_participant_minutes"], 1);
    // Alice and Bob have been in the channel for a while: their open memberships count live.
    let q = eventually(
        Duration::from_secs(90),
        "a participant-minute used",
        || async {
            let q = quota_of(&env, &http).await;
            (q["participant_minutes_this_month"].as_f64().unwrap() >= 1.0).then_some(q)
        },
    )
    .await;
    assert_eq!(q["monthly_participant_minutes"], 1, "{q}");
    assert_eq!(q["max_concurrent_sessions"], 0, "{q}");
    let mut dave = connect(&env, "Dave", tok_d).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        dave.send(&ControlMessage::ChannelJoin {
            channel_id: channel,
            token: dave.token.clone(),
        })
        .await;
        let m = dave
            .expect("join outcome", |m| {
                matches!(
                    m,
                    ControlMessage::ChannelJoinAck { .. } | ControlMessage::Error { .. }
                )
            })
            .await;
        match m {
            ControlMessage::Error { code, .. } => {
                assert_eq!(code, "QUOTA_EXCEEDED");
                break;
            }
            // Another node's quota cache may still hold the old limit for a moment.
            _ => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "join is refused once the monthly minutes are used"
                );
                dave.send(&ControlMessage::ChannelLeave {
                    channel_id: channel,
                })
                .await;
                tokio::time::sleep(Duration::from_secs(3)).await;
                drain_ws(&mut dave).await;
            }
        }
    }
    // Members already present are not evicted, and sessions still connect.
    assert_eq!(membership_count(&env, &http, channel).await, 2);
    carol
        .send(&ControlMessage::ChannelJoin {
            channel_id: channel,
            token: carol.token.clone(),
        })
        .await;
    expect_error(&mut carol, "Carol's join", "QUOTA_EXCEEDED").await;

    // ── Teardown: deleting (deactivating) the application kills its API keys and every
    // live session on every node, but keeps its usage readable for administrators ──
    for p in [&mut alice2, &mut carol] {
        p.ws.close(None).await.unwrap();
    }
    eventually(Duration::from_secs(15), "two sessions left", || async {
        (quota_of(&env, &http).await["active_sessions"] == 2).then_some(())
    })
    .await;
    let (status, deleted) = admin_json(
        &http,
        reqwest::Method::DELETE,
        format!("{}/v1/apps/{app_id}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200, "{deleted}");
    // Bob (second node when configured) and Dave are evicted fleet-wide.
    for p in [&mut bob, &mut dave] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match tokio::time::timeout_at(deadline, p.ws.next()).await {
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) => continue,
                Err(_) => panic!("{}: session survived the deactivation", p.name),
            }
        }
    }
    let (status, gone) = tenant_get(&env, &http, "/v1/analytics").await;
    assert_eq!(status, 401, "deactivated app's key is dead: {gone}");
    let (status, deleted_again) = admin_json(
        &http,
        reqwest::Method::DELETE,
        format!("{}/v1/apps/{app_id}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 404, "{deleted_again}");
    let (status, kept) = admin_json(
        &http,
        reqwest::Method::GET,
        format!("{}/admin/analytics/apps/{app_id}?{range}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200, "{kept}");
    assert_eq!(kept["active"], false);
    eventually(
        Duration::from_secs(15),
        "evicted sessions closed",
        || async {
            let (_, k) = admin_json(
                &http,
                reqwest::Method::GET,
                format!("{}/admin/analytics/apps/{app_id}", base.api),
                &admin,
                None,
            )
            .await;
            (k["quota"]["active_sessions"] == 0).then_some(())
        },
    )
    .await;
    assert_eq!(
        kept["totals"]["chat_messages"], 3,
        "history survives deactivation"
    );
    let (status, fleet) = admin_json(
        &http,
        reqwest::Method::GET,
        format!("{}/admin/analytics/usage?{range}", base.api),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        fleet["apps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["app_id"] == app_id),
        "the fleet listing still bills the deactivated application"
    );
    let (status, unknown) = admin_json(
        &http,
        reqwest::Method::GET,
        format!("{}/admin/analytics/apps/{}", base.api, uuid::Uuid::now_v7()),
        &admin,
        None,
    )
    .await;
    assert_eq!(status, 404, "{unknown}");
}

async fn set_translation(
    p: &mut Player,
    language: Option<&str>,
    spoken_language: Option<&str>,
    speech: bool,
) -> (Option<String>, Option<String>, bool) {
    p.send(&ControlMessage::SetTranslation {
        language: language.map(str::to_string),
        spoken_language: spoken_language.map(str::to_string),
        speech,
    })
    .await;
    let m = p
        .expect("TranslationChanged", |m| {
            matches!(m, ControlMessage::TranslationChanged { .. })
        })
        .await;
    match m {
        ControlMessage::TranslationChanged {
            language,
            spoken_language,
            speech,
        } => (language, spoken_language, speech),
        _ => unreachable!(),
    }
}

/// Live translation against the mock provider (`examples/mock_speech.rs`, `POST /translate`):
/// listeners who asked for another language get the segment translated (with the original
/// attached) while everyone else — including the speaker, whatever they asked for — gets the
/// untranslated text at once; a failing or too slow provider degrades to the original text;
/// spoken translations reach the requesting listener alone on the channel's translation SSRC;
/// eligibility (local mute) and the node's language offer are enforced; clearing the
/// preference restores originals. Needs `translation.*` enabled on the node(s).
#[tokio::test]
#[ignore = "requires a running Aurix server with STT/TTS/translation pointed at examples/mock_speech.rs"]
async fn live_translation_per_listener_language() {
    use aurix_media::tts::translation_voice_ssrc;

    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
    let same_node = std::env::var("AURIX_E2E_WS2").is_err();
    let env2 = match std::env::var("AURIX_E2E_WS2") {
        Ok(ws) => Env {
            api: std::env::var("AURIX_E2E_API2").unwrap_or_else(|_| "http://127.0.0.1:8090".into()),
            ws,
            api_key: env.api_key.clone(),
        },
        Err(_) => {
            eprintln!(
                "AURIX_E2E_WS2 not set; running the spoken-translation listener on the same node"
            );
            env.clone()
        }
    };

    let spoken = create_channel_with(&env, &http, serde_json::json!({"transcription": true})).await;
    let mut players = Vec::new();
    for (ext, name, e) in [
        ("mt:alice", "alice", &env),
        ("mt:bob", "bob", &env),
        ("mt:carol", "carol", &env2),
        ("mt:dave", "dave", &env),
        ("mt:erin", "erin", &env),
        ("mt:frank", "frank", &env),
    ] {
        let (tok, uid) = issue_token(e, &http, ext, name, spoken).await;
        let mut p = connect(e, name, tok).await;
        bind_media(&mut p).await;
        players.push((p, UserId::from_uuid(uid.parse().unwrap())));
    }
    let [(mut alice, uid_a), (mut bob, _), (mut carol, _), (mut dave, _), (mut erin, _), (mut frank, _)] =
        <[_; 6]>::try_from(players).ok().unwrap();

    let Some(info) = alice.translation.clone() else {
        eprintln!("translation not enabled on the node; skipping (set AURIX__TRANSLATION__*)");
        return;
    };
    assert!(
        info.speech,
        "the E2E node runs with TTS, so spoken translations are offered: {info:?}"
    );
    for lang in ["de", "fr", "it", "nl"] {
        assert!(
            info.languages.iter().any(|l| l == lang),
            "the E2E node offers {lang}: {info:?}"
        );
    }
    assert_eq!(carol.translation.as_ref().map(|i| i.speech), Some(true));

    for p in [
        &mut alice, &mut bob, &mut carol, &mut dave, &mut erin, &mut frank,
    ] {
        let tok = p.token.clone();
        p.send(&ControlMessage::ChannelJoin {
            channel_id: spoken,
            token: tok,
        })
        .await;
        p.expect("ChannelJoinAck", |m| {
            matches!(m, ControlMessage::ChannelJoinAck { channel_id, .. } if *channel_id == spoken)
        })
        .await;
    }

    // ── preference validation ──
    bob.send(&ControlMessage::SetTranslation {
        language: Some("not a tag!".into()),
        spoken_language: None,
        speech: false,
    })
    .await;
    expect_error(&mut bob, "garbage language tag", "VALIDATION_ERROR").await;
    bob.send(&ControlMessage::SetTranslation {
        language: Some("ja".into()),
        spoken_language: None,
        speech: false,
    })
    .await;
    expect_error(
        &mut bob,
        "language outside the node's offer",
        "VALIDATION_ERROR",
    )
    .await;
    assert_eq!(
        set_translation(&mut bob, None, Some("EN_us"), true).await,
        (None, Some("en-us".into()), false),
        "speech without a target is meaningless and normalisation is the server's"
    );
    assert_eq!(
        set_translation(&mut bob, Some(" DE "), Some("en"), false).await,
        (Some("de".into()), Some("en".into()), false)
    );
    assert_eq!(
        set_translation(&mut carol, Some("fr"), None, true).await,
        (Some("fr".into()), None, true)
    );
    assert_eq!(
        set_translation(&mut erin, Some("it"), None, false).await.0,
        Some("it".into())
    );
    assert!(set_translation(&mut frank, Some("nl"), None, true).await.2);
    for p in [
        &mut alice, &mut bob, &mut carol, &mut dave, &mut erin, &mut frank,
    ] {
        drain_ws(p).await;
    }
    for p in [&alice, &bob, &carol, &dave, &erin, &frank] {
        drain_udp(p).await;
    }

    let before = translation_counters(&http).await;
    if before.is_none() {
        eprintln!("AURIX_E2E_METRICS not set; skipping translation counters");
    }

    // ── one segment, five listeners, four languages ──
    let carol_voice = translation_voice_ssrc(&spoken);
    let started = tokio::time::Instant::now();
    stream_frames(&alice, spoken, 1, &opus_tone(440.0, 1_600), false).await;
    let original = expect_transcript(&mut alice, spoken, uid_a).await;
    assert!(
        tone_hz(&original.text).is_some_and(|hz| (hz - 440.0).abs() < 10.0),
        "speaker captions stay in the spoken language: {original:?}"
    );
    assert_eq!(original.language.as_deref(), Some("en"));
    assert!(original.original.is_none());

    let td = expect_transcript(&mut dave, spoken, uid_a).await;
    assert_eq!(
        (td.id, &td.text, td.original.is_none()),
        (original.id, &original.text, true)
    );

    let tb = expect_transcript(&mut bob, spoken, uid_a).await;
    assert_eq!(tb.id, original.id, "the translation is the same segment");
    assert_eq!(tb.text, format!("[de] {}", original.text));
    assert_eq!(tb.language.as_deref(), Some("de"));
    let orig = tb
        .original
        .clone()
        .expect("translated transcripts carry the original");
    assert_eq!(
        (orig.text.as_str(), orig.language.as_deref()),
        (original.text.as_str(), Some("en"))
    );
    assert!(
        tb.words.is_empty(),
        "word timings do not survive translation: {tb:?}"
    );

    let tc = expect_transcript(&mut carol, spoken, uid_a).await;
    assert_eq!(tc.text, format!("[fr] {}", original.text));
    assert_eq!(tc.language.as_deref(), Some("fr"));

    // Provider failure (it) and timeout (nl, 3 s against a 1.5 s budget) fall back to the original.
    let te = expect_transcript(&mut erin, spoken, uid_a).await;
    assert_eq!(
        (te.id, &te.text, te.language.as_deref()),
        (original.id, &original.text, Some("en"))
    );
    assert!(
        te.original.is_none(),
        "a fallback is not marked as translated: {te:?}"
    );
    let tf = expect_transcript(&mut frank, spoken, uid_a).await;
    assert_eq!((tf.id, &tf.text), (original.id, &original.text));
    assert!(tf.original.is_none());
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "the timeout fallback must not wait for the slow provider"
    );

    // Spoken translation: Carol alone hears it (mock TTS: 1 s of 24 kHz sine → 50 Opus frames).
    let ((got_c, ok_c), got_b, got_d, got_f) = tokio::join!(
        synth_audio_from(&carol, carol_voice, Duration::from_millis(2_500)),
        synth_audio_from(&bob, carol_voice, Duration::from_millis(2_500)),
        synth_audio_from(&dave, carol_voice, Duration::from_millis(2_500)),
        synth_audio_from(&frank, carol_voice, Duration::from_millis(2_500)),
    );
    assert!(
        (45..=55).contains(&got_c) && ok_c,
        "carol hears her translation spoken: {got_c} ok={ok_c}"
    );
    assert_eq!(
        (got_b.0, got_d.0, got_f.0),
        (0, 0, 0),
        "spoken translations never leak to other listeners (bob/dave/frank)"
    );
    assert_eq!(
        synth_audio_from(&alice, carol_voice, Duration::from_millis(300))
            .await
            .0,
        0,
        "nor to the speaker"
    );
    for p in [
        &mut alice, &mut bob, &mut carol, &mut dave, &mut erin, &mut frank,
    ] {
        drain_ws(p).await;
    }

    // ── speaker preference does not translate their own captions; local mute suppresses
    // the translated copy exactly like the original ──
    assert_eq!(
        set_translation(&mut alice, Some("de"), None, false).await.0,
        Some("de".into())
    );
    bob.send(&ControlMessage::SetParticipantMute {
        user_id: uid_a,
        channel_id: Some(spoken),
        muted: true,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    drain_ws(&mut bob).await;
    stream_frames(&alice, spoken, 1_000, &opus_tone(880.0, 1_600), false).await;
    let ta = expect_transcript(&mut alice, spoken, uid_a).await;
    assert!(
        tone_hz(&ta.text).is_some_and(|hz| (hz - 880.0).abs() < 15.0) && ta.original.is_none(),
        "speaker keeps original captions: {ta:?}"
    );
    assert_eq!(ta.language.as_deref(), Some("en"));
    let tc = expect_transcript(&mut carol, spoken, uid_a).await;
    assert_eq!(tc.text, format!("[fr] {}", ta.text));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_no_transcript(
        &mut bob,
        Duration::from_millis(500),
        "locally muted the speaker",
    )
    .await;
    bob.send(&ControlMessage::SetParticipantMute {
        user_id: uid_a,
        channel_id: Some(spoken),
        muted: false,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    for p in [
        &mut alice, &mut bob, &mut carol, &mut dave, &mut erin, &mut frank,
    ] {
        drain_ws(p).await;
    }
    for p in [&alice, &bob, &carol, &dave, &erin, &frank] {
        drain_udp(p).await;
    }

    // ── clearing the preference restores the original; speech off stops the audio ──
    assert_eq!(
        set_translation(&mut bob, None, None, false).await,
        (None, None, false)
    );
    assert_eq!(
        set_translation(&mut carol, Some("fr"), None, false).await,
        (Some("fr".into()), None, false)
    );
    drain_ws(&mut bob).await;
    drain_ws(&mut carol).await;
    stream_frames(&alice, spoken, 2_000, &opus_tone(660.0, 1_600), false).await;
    let ta = expect_transcript(&mut alice, spoken, uid_a).await;
    let tb = expect_transcript(&mut bob, spoken, uid_a).await;
    assert_eq!(
        (tb.id, &tb.text, tb.original.is_none()),
        (ta.id, &ta.text, true)
    );
    let tc = expect_transcript(&mut carol, spoken, uid_a).await;
    assert_eq!(tc.text, format!("[fr] {}", ta.text));
    assert_eq!(
        synth_audio_from(&carol, carol_voice, Duration::from_millis(1_500))
            .await
            .0,
        0,
        "no spoken translation once speech is off"
    );

    if let Some(before) = before {
        let after = translation_counters(&http).await.unwrap();
        let delta = |outcome: &str| {
            after.get(outcome).copied().unwrap_or(0.0) - before.get(outcome).copied().unwrap_or(0.0)
        };
        // Only Bob's German is translated by the speaker's node (Carol's French happens where she
        // is connected) and only for the first segment: he is locally deaf to Alice for the second
        // and has no target for the third. Italian fails and Dutch times out on all three.
        let expected_ok = if same_node { 4.0 } else { 1.0 };
        assert_eq!(
            delta("ok") + delta("cached"),
            expected_ok,
            "German once (plus Carol's French per segment when she shares the node): {after:?} vs {before:?}"
        );
        assert_eq!(
            delta("error"),
            6.0,
            "failed and timed-out translations are counted and never cached: {after:?} vs {before:?}"
        );
        assert_eq!(delta("busy") + delta("skipped"), 0.0);
    }
}

/// `aurix_translations_total{outcome}` of the node behind `AURIX_E2E_METRICS`, if set.
async fn translation_counters(http: &reqwest::Client) -> Option<HashMap<String, f64>> {
    let url = std::env::var("AURIX_E2E_METRICS").ok()?;
    let body = http.get(&url).send().await.ok()?.text().await.ok()?;
    Some(
        body.lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("aurix_translations_total{outcome=\"")?;
                let (outcome, value) = rest.split_once("\"}")?;
                Some((outcome.to_string(), value.trim().parse().ok()?))
            })
            .collect(),
    )
}
