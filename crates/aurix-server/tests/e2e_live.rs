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
    ChannelId, Direction, Orientation3D, Position3D, RecordingConsent, SessionId, UserId,
};
use aurix_turn::stun::{StunAttributeType, StunMessage, StunMessageType};
use base64::Engine;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
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
    udp: UdpSocket,
    media_key: Vec<u8>,
    resume_token: String,
    resume_grace: Duration,
    resumed: bool,
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
        media_key,
        resume_token,
        resume_grace_ms,
        resumed,
    } = msg
    else {
        panic!("{name}: expected SessionInitAck, got {t}");
    };
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
        udp,
        media_key,
        resume_token,
        resume_grace: Duration::from_millis(resume_grace_ms),
        resumed,
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
    assert!(matches!(m, ControlMessage::MediaBound { session_id } if session_id == p.session_id));
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
        if p.header.packet_type == PacketType::Audio && p.header.sequence >= 20 {
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

    // Bob sees nothing, the membership is still persisted.
    assert!(
        bob.try_recv(Duration::from_millis(700)).await.is_none(),
        "peers must not be notified while the session is detached"
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
    expect_error(&mut bob, "direct to unknown user", "USER_OFFLINE").await;
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
    let r = http
        .get(format!(
            "{}/v1/channels/{}/messages?limit=100",
            env.api, channel_id
        ))
        .header("x-api-key", &env.api_key)
        .send()
        .await
        .unwrap();
    if r.status() == 404 {
        eprintln!("chat.persist is off on this server; skipping history checks");
    } else {
        let body: serde_json::Value = r.error_for_status().unwrap().json().await.unwrap();
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
