//! WebRTC session management using the str0m sans-IO library.
//!
//! Each browser client gets a dedicated `WebRtcSession` running in its own
//! tokio task. The `WebRtcManager` demuxes incoming UDP packets to the
//! correct session based on source address or ICE ufrag.

use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use str0m::change::SdpOffer;
use str0m::net::{Protocol, Receive};
use str0m::{Event, Input, Output, Rtc};

/// A handle to communicate with a running WebRTC session task.
#[allow(dead_code)]
struct SessionHandle {
    /// Send incoming network bytes to the session
    net_tx: mpsc::Sender<(Vec<u8>, SocketAddr, SocketAddr)>,
    /// Send media (Opus RTP payload) to be forwarded TO this client
    media_tx: mpsc::Sender<ForwardMedia>,
    session_id: SessionId,
    user_id: UserId,
    app_id: AppId,
    /// ICE ufrag for this session (used for demuxing before address is known)
    ice_ufrag: String,
}

/// Media to forward to a WebRTC client.
pub struct ForwardMedia {
    pub ssrc: u32,
    pub sequence: u16,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// Event emitted from a WebRTC session to the SFU.
#[derive(Debug)]
pub enum WebRtcMediaEvent {
    /// Decoded RTP audio received from the browser
    AudioReceived {
        session_id: SessionId,
        user_id: UserId,
        ssrc: u32,
        sequence: u16,
        timestamp: u32,
        payload: Vec<u8>,
    },
    /// ICE connection established
    Connected { session_id: SessionId },
    /// Session disconnected
    Disconnected { session_id: SessionId },
}

/// Manages all WebRTC sessions. Shared across the SFU.
pub struct WebRtcManager {
    sessions: Arc<DashMap<SessionId, SessionHandle>>,
    /// source address → session_id (learned after first STUN)
    addr_map: Arc<DashMap<SocketAddr, SessionId>>,
    /// ICE ufrag → session_id (for initial demux before address known)
    ufrag_map: Arc<DashMap<String, SessionId>>,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    /// Channel to deliver media events to the SFU router
    event_tx: mpsc::Sender<WebRtcMediaEvent>,
}

#[derive(Deserialize)]
pub struct WebRtcOfferRequest {
    pub sdp: String,
    pub user_id: String,
    pub app_id: String,
    pub display_name: String,
}

#[derive(Serialize)]
pub struct WebRtcOfferResponse {
    pub session_id: String,
    pub sdp: String,
}

impl WebRtcManager {
    pub fn new(
        socket: Arc<UdpSocket>,
        local_addr: SocketAddr,
        event_tx: mpsc::Sender<WebRtcMediaEvent>,
    ) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            addr_map: Arc::new(DashMap::new()),
            ufrag_map: Arc::new(DashMap::new()),
            socket,
            local_addr,
            event_tx,
        }
    }

    /// Create a new WebRTC session from a browser SDP offer.
    /// Returns the session ID and the SDP answer to send back.
    pub fn create_session(
        &self,
        offer_sdp: &str,
        session_id: SessionId,
        user_id: UserId,
        app_id: AppId,
    ) -> Result<String> {
        // Build the str0m Rtc instance (ICE-lite for server)
        let mut rtc = Rtc::builder().set_ice_lite(true).build();

        // Parse the browser's SDP offer
        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| AurixError::Transport(format!("SDP parse error: {e}")))?;

        // Accept the offer and generate an answer
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| AurixError::Transport(format!("SDP accept error: {e}")))?;

        let answer_sdp = answer.to_sdp_string();

        // Extract ICE ufrag from the Rtc for demuxing
        let local_ufrag = rtc
            .direct_api()
            .local_ice_credentials()
            .ufrag
            .clone();

        // Create channels for the session task
        let (net_tx, net_rx) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr)>(512);
        let (media_tx, media_rx) = mpsc::channel::<ForwardMedia>(256);

        let handle = SessionHandle {
            net_tx,
            media_tx,
            session_id,
            user_id,
            app_id,
            ice_ufrag: local_ufrag.clone(),
        };

        self.sessions.insert(session_id, handle);
        if !local_ufrag.is_empty() {
            self.ufrag_map.insert(local_ufrag, session_id);
        }

        // Spawn the session actor task
        let socket = self.socket.clone();
        let event_tx = self.event_tx.clone();
        let addr_map = self.addr_map.clone();
        let sessions = self.sessions.clone();
        let ufrag_map = self.ufrag_map.clone();

        tokio::spawn(async move {
            Self::session_task(
                rtc,
                session_id,
                user_id,
                net_rx,
                media_rx,
                socket,
                event_tx,
                addr_map,
                sessions,
                ufrag_map,
            )
            .await;
        });

        info!(
            "WebRTC session {} created for user {}",
            session_id, user_id
        );
        Ok(answer_sdp)
    }

    /// Determine if an incoming UDP packet belongs to a WebRTC session.
    /// Uses RFC 7983 demuxing: STUN (0-3), DTLS (20-63), RTP/RTCP (128-191).
    pub fn is_webrtc_packet(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        let b = data[0];
        // STUN: first byte 0..=3
        // DTLS: first byte 20..=63
        // RTP/RTCP: first byte 128..=191
        b <= 3 || (20..=63).contains(&b) || (128..=191).contains(&b)
    }

    /// Route an incoming packet to the correct WebRTC session.
    pub async fn handle_packet(&self, data: &[u8], src: SocketAddr) {
        // Fast path: known address
        if let Some(sid) = self.addr_map.get(&src) {
            if let Some(handle) = self.sessions.get(&*sid) {
                let _ = handle
                    .net_tx
                    .send((data.to_vec(), src, self.local_addr))
                    .await;
                return;
            }
        }

        // Slow path: parse STUN to extract ufrag for session lookup
        if data.len() >= 20 && data[0] <= 3 {
            if let Some(ufrag) = Self::extract_stun_ufrag(data) {
                // str0m uses "local:remote" format in STUN USERNAME
                let local_ufrag = ufrag.split(':').next().unwrap_or("");
                if let Some(sid) = self.ufrag_map.get(local_ufrag) {
                    self.addr_map.insert(src, *sid);
                    if let Some(handle) = self.sessions.get(&*sid) {
                        let _ = handle
                            .net_tx
                            .send((data.to_vec(), src, self.local_addr))
                            .await;
                    }
                    return;
                }
            }
        }

        // Unknown packet, drop
        debug!("WebRTC packet from unknown source {}, dropping", src);
    }

    /// Send media to a specific WebRTC session (for forwarding audio to browser).
    pub async fn send_to_session(&self, session_id: &SessionId, media: ForwardMedia) {
        if let Some(handle) = self.sessions.get(session_id) {
            let _ = handle.media_tx.send(media).await;
        }
    }

    pub fn remove_session(&self, session_id: &SessionId) {
        if let Some((_, handle)) = self.sessions.remove(session_id) {
            self.ufrag_map.remove(&handle.ice_ufrag);
            // addr_map entries will be cleaned up on next access
        }
        self.addr_map.retain(|_, sid| sid != session_id);
    }

    /// The per-session actor loop. Owns the `Rtc` instance (which is Send but not Sync).
    async fn session_task(
        mut rtc: Rtc,
        session_id: SessionId,
        user_id: UserId,
        mut net_rx: mpsc::Receiver<(Vec<u8>, SocketAddr, SocketAddr)>,
        mut media_rx: mpsc::Receiver<ForwardMedia>,
        socket: Arc<UdpSocket>,
        event_tx: mpsc::Sender<WebRtcMediaEvent>,
        addr_map: Arc<DashMap<SocketAddr, SessionId>>,
        sessions: Arc<DashMap<SessionId, SessionHandle>>,
        _ufrag_map: Arc<DashMap<String, SessionId>>,
    ) {
        let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(5));
        let mut connected = false;

        loop {
            tokio::select! {
                Some((data, src, dst)) = net_rx.recv() => {
                    // Try to interpret the data as a DatagramRecv
                    if let Ok(contents) = (&data[..]).try_into() {
                        let receive = Receive {
                            proto: Protocol::Udp,
                            source: src,
                            destination: dst,
                            contents,
                        };
                        if let Err(e) = rtc.handle_input(Input::Receive(Instant::now(), receive)) {
                            warn!("WebRTC input error for {}: {}", session_id, e);
                        }
                    }
                    Self::poll_outputs(&mut rtc, session_id, user_id, &socket, &event_tx, &mut connected).await;
                }
                Some(media) = media_rx.recv() => {
                    // Forward media to browser: write RTP via str0m's direct API
                    // str0m handles SRTP encryption internally
                    let ssrc_val = media.ssrc.into();
                    if let Some(writer) = rtc.direct_api().stream_tx(&ssrc_val) {
                        let _ = writer.write_rtp(
                            111.into(), // Opus payload type (standard)
                            (media.sequence as u64).into(),
                            media.timestamp,
                            Instant::now(),
                            false,      // marker
                            Default::default(), // extensions
                            false,      // nackable
                            media.payload.into(),
                        );
                    }
                    Self::poll_outputs(&mut rtc, session_id, user_id, &socket, &event_tx, &mut connected).await;
                }
                _ = tick_interval.tick() => {
                    if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                        debug!("WebRTC timeout error for {}: {}", session_id, e);
                    }
                    Self::poll_outputs(&mut rtc, session_id, user_id, &socket, &event_tx, &mut connected).await;

                    // Check if session is dead
                    if !rtc.is_alive() {
                        info!("WebRTC session {} ended", session_id);
                        let _ = event_tx.send(WebRtcMediaEvent::Disconnected { session_id }).await;
                        break;
                    }
                }
                else => break,
            }
        }

        // Cleanup
        sessions.remove(&session_id);
        addr_map.retain(|_, sid| *sid != session_id);
        info!("WebRTC session task {} exited", session_id);
    }

    async fn poll_outputs(
        rtc: &mut Rtc,
        session_id: SessionId,
        user_id: UserId,
        socket: &UdpSocket,
        event_tx: &mpsc::Sender<WebRtcMediaEvent>,
        connected: &mut bool,
    ) {
        loop {
            match rtc.poll_output() {
                Ok(output) => match output {
                    Output::Transmit(t) => {
                        if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                            warn!("WebRTC transmit error: {}", e);
                        }
                    }
                    Output::Event(event) => {
                        match event {
                            Event::IceConnectionStateChange(state) => {
                                info!("WebRTC ICE state for {}: {:?}", session_id, state);
                                if !*connected
                                    && matches!(
                                        state,
                                        str0m::IceConnectionState::Connected
                                            | str0m::IceConnectionState::Completed
                                    )
                                {
                                    *connected = true;
                                    let _ = event_tx
                                        .send(WebRtcMediaEvent::Connected { session_id })
                                        .await;
                                }
                            }
                            Event::RtpPacket(rtp) => {
                                let _ = event_tx
                                    .send(WebRtcMediaEvent::AudioReceived {
                                        session_id,
                                        user_id,
                                        ssrc: *rtp.header.ssrc,
                                        sequence: rtp.header.sequence_number,
                                        timestamp: rtp.header.timestamp,
                                        payload: rtp.payload.to_vec(),
                                    })
                                    .await;
                            }
                            _ => {}
                        }
                    }
                    Output::Timeout(_) => break,
                },
                Err(_) => break,
            }
        }
    }

    /// Extract the USERNAME attribute from a STUN binding request.
    fn extract_stun_ufrag(data: &[u8]) -> Option<String> {
        if data.len() < 20 {
            return None;
        }
        let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if magic != 0x2112A442 {
            return None;
        }
        let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
        let mut offset = 20;
        while offset + 4 <= 20 + msg_len && offset + 4 <= data.len() {
            let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4;
            if attr_type == 0x0006 && offset + attr_len <= data.len() {
                // USERNAME attribute
                return String::from_utf8(data[offset..offset + attr_len].to_vec()).ok();
            }
            offset += attr_len;
            offset += (4 - (attr_len % 4)) % 4; // padding
        }
        None
    }
}