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
    AudioCodec, AudioPolicy, ChannelId, Direction, OpusBandwidth, OpusSignal, Orientation3D,
    Position3D, RecordingConsent, SessionId, UserId,
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
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
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
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
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
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
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
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    let http = reqwest::Client::new();
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
    let Some(env) = env() else {
        eprintln!("AURIX_E2E_API_KEY not set; skipping");
        return;
    };
    if std::env::var("AURIX_E2E_SAFETY").is_err() {
        eprintln!("AURIX_E2E_SAFETY not set; skipping (node must run with [safety] enabled)");
        return;
    }
    let http = reqwest::Client::new();
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
