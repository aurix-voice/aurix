//! End-to-end smoke test against a *running* Aurix server (PostgreSQL + Redis required).
//!
//! Skipped unless `AURIX_E2E_API_KEY` is set; see `scripts/e2e.sh` which boots the stack,
//! bootstraps an admin/app and then runs this test:
//!
//! ```text
//! AURIX_E2E_API=http://127.0.0.1:8080 AURIX_E2E_WS=ws://127.0.0.1:8081 \
//! AURIX_E2E_API_KEY=aurx_... cargo test -p aurix-server --test e2e_live -- --ignored
//! ```
//!
//! Flow covered: API key -> player tokens -> WebSocket SessionInit/ChannelJoin ->
//! authenticated AURX SessionBind -> audio routed Alice -> Bob over UDP ->
//! TURN credentials from the API accepted by the TURN server -> leave/close persisted.

use aurix_common::protocol::{channel_id_hash, AurixPacket, ControlMessage, PacketType};
use aurix_common::types::{ChannelId, RecordingConsent, SessionId};
use aurix_turn::stun::{StunAttributeType, StunMessage, StunMessageType};
use base64::Engine;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

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
    media_key: Vec<u8>,
    media_addr: SocketAddr,
    udp: UdpSocket,
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
            if let ControlMessage::Error { code, message } = &m {
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
            Ok(Ok((n, _))) => Some(AurixPacket::decode(&buf[..n]).expect("bad AURX packet")),
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
    let r: serde_json::Value = http
        .post(format!("{}/v1/tokens", env.api))
        .header("x-api-key", &env.api_key)
        .json(&serde_json::json!({
            "external_id": external_id,
            "display_name": name,
            "channels": [{"channel_id": ch, "join": true, "speak": true, "receive": true, "moderate": false}]
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
    let mut req = format!("{}/ws", env.ws).into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
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
    } = msg
    else {
        panic!("{name}: expected SessionInitAck, got {t}");
    };
    let media_key = base64::engine::general_purpose::STANDARD
        .decode(media_key)
        .unwrap();
    assert_eq!(media_key.len(), 32);
    let udp = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    Player {
        name,
        token,
        ws,
        session_id,
        ssrc,
        media_key,
        media_addr: media_addr.parse().unwrap(),
        udp,
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn bind_media(p: &mut Player) {
    let pkt = AurixPacket::session_bind(&p.session_id, p.ssrc, now_ms(), rand::random());
    p.udp
        .send_to(&pkt.encode_authenticated(&p.media_key), p.media_addr)
        .await
        .unwrap();
    let ack = p.recv_udp().await.expect("no SessionBindAck");
    assert_eq!(ack.header.packet_type, PacketType::SessionBindAck);
    assert!(
        ack.verify_auth(&p.media_key),
        "bind ack must be authenticated"
    );
    let m = p
        .expect("MediaBound", |m| {
            matches!(m, ControlMessage::MediaBound { .. })
        })
        .await;
    assert!(matches!(m, ControlMessage::MediaBound { session_id } if session_id == p.session_id));
}

#[tokio::test]
#[ignore = "requires a running Aurix server; see scripts/e2e.sh"]
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
            .send_to(
                &pkt.encode_authenticated(&alice.media_key),
                alice.media_addr,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Some(p) = bob.recv_udp().await {
        if p.header.packet_type == PacketType::Audio {
            assert_eq!(p.header.ssrc, alice.ssrc, "downlink must carry sender SSRC");
            assert_eq!(&p.payload[..], &payload[..]);
            assert!(
                p.verify_auth(&bob.media_key),
                "downlink must be authenticated with Bob's key"
            );
            got += 1;
        }
        if got >= 5 {
            break;
        }
    }
    assert!(got >= 5, "Bob received only {got} audio packets from Alice");

    // Forged audio (wrong key) or a spoofed SSRC must not be forwarded.
    let forged = AurixPacket::audio(
        100,
        100 * 960,
        alice.ssrc,
        hash,
        Bytes::from_static(b"forged"),
    );
    bob.udp
        .send_to(&forged.encode_authenticated(&bob.media_key), bob.media_addr)
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
            .send_to(
                &pkt.encode_authenticated(&alice.media_key),
                alice.media_addr,
            )
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

    let (tok_a, _) = issue_token(&env, &http, "cascade:alice", "Alice", channel_id).await;
    let (tok_b, _) = issue_token(&env2, &http, "cascade:bob", "Bob", channel_id).await;
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
                .send_to(
                    &pkt.encode_authenticated(&alice.media_key),
                    alice.media_addr,
                )
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut buf = vec![0u8; 2048];
        while let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(300), bob.udp.recv_from(&mut buf)).await
        {
            let p = AurixPacket::decode(&buf[..n]).expect("bad AURX packet");
            if p.header.packet_type == PacketType::Audio {
                assert_eq!(p.header.ssrc, alice.ssrc);
                assert_eq!(&p.payload[..], &payload[..]);
                assert!(
                    p.verify_auth(&bob.media_key),
                    "relayed downlink must be re-signed with Bob's key"
                );
                got += 1;
            }
        }
    }
    assert!(
        got >= 5,
        "Bob (node 2) received only {got} audio packets from Alice (node 1) via cascade"
    );

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
            .send_to(
                &pkt.encode_authenticated(&alice.media_key),
                alice.media_addr,
            )
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
