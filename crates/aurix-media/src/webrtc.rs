//! WebRTC session management using the str0m sans-IO library.
//!
//! Each browser client gets a dedicated `WebRtcSession` running in its own tokio task
//! that owns the `Rtc` state machine. Uplink audio (`Event::MediaData`) is forwarded to
//! the SFU router; downlink audio from every other participant is mixed server-side
//! (`OpusMixer`) into the single negotiated audio track. The `WebRtcManager` demuxes
//! incoming UDP packets to the correct session based on source address or ICE ufrag.

use aurix_common::error::{AurixError, Result};
use aurix_common::types::*;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use str0m::change::SdpOffer;
use str0m::format::{Codec, FormatParams};
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::mixer::{OpusMixer, FRAME_SAMPLES};

/// Dynamic payload type offered for Opus (the same one browsers use by default).
const OPUS_PT: Pt = Pt::new_with_value(111);

/// Handle to communicate with a running WebRTC session task.
struct SessionHandle {
    net_tx: mpsc::Sender<(Vec<u8>, SocketAddr, SocketAddr)>,
    media_tx: mpsc::Sender<ForwardMedia>,
    ice_ufrag: String,
}

/// Opus payload from another participant to be mixed into this client's downlink.
pub struct ForwardMedia {
    pub sender_ssrc: u32,
    pub volume: f32,
    /// Where the sender is relative to this listener (directional positional channels).
    pub direction: Option<Direction>,
    pub payload: Vec<u8>,
}

/// Event emitted from a WebRTC session to the SFU.
#[derive(Debug)]
pub enum WebRtcMediaEvent {
    /// Depayloaded Opus audio received from the browser.
    AudioReceived {
        session_id: SessionId,
        user_id: UserId,
        rtp_time: u32,
        payload: Vec<u8>,
        /// RFC 6464 audio level (`-dBov`, 0..=127) from the RTP header extension.
        level: Option<u8>,
    },
    /// ICE/DTLS connected; `remote` is the authenticated peer address.
    Connected {
        session_id: SessionId,
        remote: SocketAddr,
    },
    Disconnected {
        session_id: SessionId,
    },
}

/// Manages all WebRTC sessions. Shared across the SFU.
pub struct WebRtcManager {
    sessions: Arc<DashMap<SessionId, SessionHandle>>,
    addr_map: Arc<DashMap<SocketAddr, SessionId>>,
    ufrag_map: Arc<DashMap<String, SessionId>>,
    socket: Arc<UdpSocket>,
    /// Address advertised to browsers as the ICE host candidate (external IP + media port).
    advertised_addr: SocketAddr,
    event_tx: mpsc::Sender<WebRtcMediaEvent>,
    downlink_bitrate: i32,
}

#[derive(Deserialize)]
pub struct WebRtcOfferRequest {
    pub sdp: String,
    /// Client JWT (same token used for the WebSocket control channel).
    pub token: String,
}

#[derive(Serialize)]
pub struct WebRtcOfferResponse {
    pub session_id: String,
    pub sdp: String,
}

impl WebRtcManager {
    pub fn new(
        socket: Arc<UdpSocket>,
        advertised_addr: SocketAddr,
        event_tx: mpsc::Sender<WebRtcMediaEvent>,
        downlink_bitrate: u32,
    ) -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            addr_map: Arc::new(DashMap::new()),
            ufrag_map: Arc::new(DashMap::new()),
            socket,
            advertised_addr,
            event_tx,
            downlink_bitrate: downlink_bitrate as i32,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Create a new WebRTC session from a browser SDP offer. Returns the SDP answer.
    pub fn create_session(
        &self,
        offer_sdp: &str,
        session_id: SessionId,
        user_id: UserId,
    ) -> Result<String> {
        let mut config = Rtc::builder()
            .set_ice_lite(true)
            .clear_codecs()
            .set_stats_interval(Some(Duration::from_secs(5)));
        // `sprop-stereo=1`: the downlink mix is stereo (speakers panned by direction). No
        // `stereo=1`: we prefer to *receive* mono, so browsers keep encoding their mic in mono
        // (RFC 7587 §6.1); decoding both channels is the receiver's `stereo=1` in its offer.
        config.codec_config().add_config(
            OPUS_PT,
            None,
            Codec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams {
                min_p_time: Some(10),
                use_inband_fec: Some(true),
                sprop_stereo: Some(true),
                ..FormatParams::default()
            },
        );
        let mut rtc = config.build(Instant::now());

        let candidate = Candidate::host(self.advertised_addr, "udp")
            .map_err(|e| AurixError::Transport(format!("ICE candidate: {e}")))?;
        rtc.add_local_candidate(candidate);

        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|e| AurixError::Transport(format!("SDP parse error: {e}")))?;
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| AurixError::Transport(format!("SDP accept error: {e}")))?;
        let answer_sdp = answer.to_sdp_string();

        let local_ufrag = rtc.direct_api().local_ice_credentials().ufrag.clone();
        if local_ufrag.is_empty() {
            return Err(AurixError::Transport("ICE credentials unavailable".into()));
        }

        let mixer = OpusMixer::new(self.downlink_bitrate)?;

        let (net_tx, net_rx) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr)>(512);
        let (media_tx, media_rx) = mpsc::channel::<ForwardMedia>(512);

        self.sessions.insert(
            session_id,
            SessionHandle {
                net_tx,
                media_tx,
                ice_ufrag: local_ufrag.clone(),
            },
        );
        self.ufrag_map.insert(local_ufrag, session_id);

        let ctx = SessionTaskCtx {
            session_id,
            user_id,
            socket: self.socket.clone(),
            event_tx: self.event_tx.clone(),
            addr_map: self.addr_map.clone(),
            sessions: self.sessions.clone(),
            ufrag_map: self.ufrag_map.clone(),
        };
        tokio::spawn(async move { session_task(rtc, ctx, net_rx, media_rx, mixer).await });

        info!("WebRTC session {} created for user {}", session_id, user_id);
        Ok(answer_sdp)
    }

    /// RFC 7983 demux: STUN (0-3), DTLS (20-63), RTP/RTCP (128-191).
    pub fn is_webrtc_packet(data: &[u8]) -> bool {
        match data.first() {
            None => false,
            Some(&b) => b <= 3 || (20..=63).contains(&b) || (128..=191).contains(&b),
        }
    }

    /// Route an incoming packet to the correct WebRTC session.
    pub async fn handle_packet(&self, data: &[u8], src: SocketAddr) {
        // str0m matches the destination against the advertised host candidate; the socket
        // itself is usually bound to a wildcard address, so report the candidate address.
        let dst = self.advertised_addr;
        let sid = self.addr_map.get(&src).map(|s| *s);
        if let Some(sid) = sid {
            let tx = self.sessions.get(&sid).map(|h| h.net_tx.clone());
            if let Some(tx) = tx {
                let _ = tx.try_send((data.to_vec(), src, dst));
                return;
            }
        }

        // Before the address is known, only STUN binding requests carrying our ufrag are accepted.
        if data.len() >= 20 && data[0] <= 3 {
            if let Some(username) = Self::extract_stun_username(data) {
                let local_ufrag = username.split(':').next().unwrap_or("");
                let sid = self.ufrag_map.get(local_ufrag).map(|s| *s);
                if let Some(sid) = sid {
                    let tx = self.sessions.get(&sid).map(|h| h.net_tx.clone());
                    if let Some(tx) = tx {
                        self.addr_map.insert(src, sid);
                        let _ = tx.try_send((data.to_vec(), src, dst));
                    }
                    return;
                }
            }
        }
        debug!("WebRTC packet from unknown source {}, dropping", src);
    }

    /// Queue Opus audio from another participant for mixing into this client's downlink.
    pub fn send_to_session(&self, session_id: &SessionId, media: ForwardMedia) -> bool {
        match self.sessions.get(session_id) {
            Some(handle) => handle.media_tx.try_send(media).is_ok(),
            None => false,
        }
    }

    pub fn has_session(&self, session_id: &SessionId) -> bool {
        self.sessions.contains_key(session_id)
    }

    pub fn remove_session(&self, session_id: &SessionId) {
        if let Some((_, handle)) = self.sessions.remove(session_id) {
            self.ufrag_map.remove(&handle.ice_ufrag);
        }
        self.addr_map.retain(|_, sid| sid != session_id);
    }

    /// Extract the USERNAME attribute from a STUN message.
    fn extract_stun_username(data: &[u8]) -> Option<String> {
        if data.len() < 20 {
            return None;
        }
        let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if magic != 0x2112A442 {
            return None;
        }
        let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
        let end = (20 + msg_len).min(data.len());
        let mut offset = 20;
        while offset + 4 <= end {
            let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4;
            if offset + attr_len > end {
                return None;
            }
            if attr_type == 0x0006 {
                return String::from_utf8(data[offset..offset + attr_len].to_vec()).ok();
            }
            offset += attr_len + (4 - (attr_len % 4)) % 4;
        }
        None
    }
}

struct SessionTaskCtx {
    session_id: SessionId,
    user_id: UserId,
    socket: Arc<UdpSocket>,
    event_tx: mpsc::Sender<WebRtcMediaEvent>,
    addr_map: Arc<DashMap<SocketAddr, SessionId>>,
    sessions: Arc<DashMap<SessionId, SessionHandle>>,
    ufrag_map: Arc<DashMap<String, SessionId>>,
}

struct Downlink {
    mid: Option<Mid>,
    pt: Option<Pt>,
    rtp_time: u64,
}

/// ICE-lite agents report `Disconnected` transiently until the first binding request arrives,
/// so only tear the session down after the state persists this long.
const ICE_DISCONNECT_GRACE: Duration = Duration::from_secs(15);

struct IceState {
    disconnected_since: Option<Instant>,
}

/// Per-session actor loop. Owns the `Rtc` instance (Send but not Sync).
async fn session_task(
    mut rtc: Rtc,
    ctx: SessionTaskCtx,
    mut net_rx: mpsc::Receiver<(Vec<u8>, SocketAddr, SocketAddr)>,
    mut media_rx: mpsc::Receiver<ForwardMedia>,
    mut mixer: OpusMixer,
) {
    let mut connected = false;
    let mut downlink = Downlink {
        mid: None,
        pt: None,
        rtp_time: 0,
    };
    let mut ice = IceState {
        disconnected_since: None,
    };
    let mut mix_tick = tokio::time::interval(Duration::from_millis(20));
    mix_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // str0m tells us when it next needs a timeout; start with a short one.
    let mut next_timeout = Instant::now() + Duration::from_millis(100);

    loop {
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(next_timeout));
        tokio::select! {
            Some((data, src, dst)) = net_rx.recv() => {
                if let Ok(contents) = (&data[..]).try_into() {
                    let receive = Receive { proto: Protocol::Udp, source: src, destination: dst, contents };
                    if let Err(e) = rtc.handle_input(Input::Receive(Instant::now(), receive)) {
                        warn!("WebRTC input error for {}: {}", ctx.session_id, e);
                    }
                }
            }
            Some(media) = media_rx.recv() => {
                if let Err(e) = mixer.push_opus(
                    media.sender_ssrc,
                    media.volume,
                    media.direction,
                    &media.payload,
                ) {
                    debug!("mixer decode error for {}: {}", ctx.session_id, e);
                }
                continue;
            }
            _ = mix_tick.tick() => {
                if connected {
                    if let (Some(mid), Some(pt)) = (downlink.mid, downlink.pt) {
                        match mixer.mix_frame() {
                            Ok(Some(frame)) => {
                                let data = frame.to_vec();
                                let ts = MediaTime::new(downlink.rtp_time, Frequency::FORTY_EIGHT_KHZ);
                                if let Some(writer) = rtc.writer(mid) {
                                    if let Err(e) = writer.write(pt, Instant::now(), ts, data) {
                                        debug!("downlink write error for {}: {}", ctx.session_id, e);
                                    } else {
                                        aurix_metrics::PACKETS_SENT.inc();
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(e) => debug!("mix error for {}: {}", ctx.session_id, e),
                        }
                        // RTP clock advances even for silent frames so the receiver sees DTX gaps correctly.
                        downlink.rtp_time = downlink.rtp_time.wrapping_add(FRAME_SAMPLES as u64);
                    }
                }
            }
            _ = sleep => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    debug!("WebRTC timeout error for {}: {}", ctx.session_id, e);
                }
            }
            else => break,
        }

        if let Some(t) = poll_outputs(&mut rtc, &ctx, &mut connected, &mut downlink, &mut ice).await
        {
            next_timeout = t;
        }
        if let Some(since) = ice.disconnected_since {
            if since.elapsed() > ICE_DISCONNECT_GRACE {
                info!(
                    "WebRTC session {} ICE disconnected for too long, closing",
                    ctx.session_id
                );
                rtc.disconnect();
            }
        }

        if !rtc.is_alive() {
            info!("WebRTC session {} ended", ctx.session_id);
            let _ = ctx
                .event_tx
                .send(WebRtcMediaEvent::Disconnected {
                    session_id: ctx.session_id,
                })
                .await;
            break;
        }
    }

    if let Some((_, h)) = ctx.sessions.remove(&ctx.session_id) {
        ctx.ufrag_map.remove(&h.ice_ufrag);
    }
    ctx.addr_map.retain(|_, sid| *sid != ctx.session_id);
    info!("WebRTC session task {} exited", ctx.session_id);
}

/// Drain str0m outputs. Returns the next timeout instant if str0m reported one.
async fn poll_outputs(
    rtc: &mut Rtc,
    ctx: &SessionTaskCtx,
    connected: &mut bool,
    downlink: &mut Downlink,
    ice: &mut IceState,
) -> Option<Instant> {
    loop {
        match rtc.poll_output() {
            Ok(Output::Transmit(t)) => {
                if let Err(e) = ctx.socket.send_to(&t.contents, t.destination).await {
                    warn!("WebRTC transmit error: {}", e);
                }
            }
            Ok(Output::Event(event)) => match event {
                Event::Connected => {
                    ice.disconnected_since = None;
                    if !*connected {
                        *connected = true;
                        let remote = ctx
                            .addr_map
                            .iter()
                            .find(|e| *e.value() == ctx.session_id)
                            .map(|e| *e.key());
                        if let Some(remote) = remote {
                            let _ = ctx
                                .event_tx
                                .send(WebRtcMediaEvent::Connected {
                                    session_id: ctx.session_id,
                                    remote,
                                })
                                .await;
                        }
                    }
                }
                Event::IceConnectionStateChange(state) => {
                    debug!("WebRTC ICE state for {}: {:?}", ctx.session_id, state);
                    match state {
                        IceConnectionState::Disconnected => {
                            if ice.disconnected_since.is_none() {
                                ice.disconnected_since = Some(Instant::now());
                            }
                        }
                        _ => ice.disconnected_since = None,
                    }
                }
                Event::MediaAdded(added) => {
                    if added.kind == MediaKind::Audio && downlink.mid.is_none() {
                        downlink.mid = Some(added.mid);
                        downlink.pt = rtc.writer(added.mid).and_then(|w| {
                            w.payload_params()
                                .find(|p| p.spec().codec == Codec::Opus)
                                .map(|p| p.pt())
                        });
                        if downlink.pt.is_none() {
                            warn!(
                                "WebRTC session {}: no Opus payload negotiated for downlink",
                                ctx.session_id
                            );
                        }
                    }
                }
                Event::MediaData(data) => {
                    if data.params.spec().codec != Codec::Opus {
                        continue;
                    }
                    let _ = ctx
                        .event_tx
                        .send(WebRtcMediaEvent::AudioReceived {
                            session_id: ctx.session_id,
                            user_id: ctx.user_id,
                            rtp_time: data.time.numer() as u32,
                            payload: data.data.to_vec(),
                            level: data
                                .ext_vals
                                .audio_level
                                .map(|dbov| dbov.unsigned_abs().min(127)),
                        })
                        .await;
                }
                _ => {}
            },
            Ok(Output::Timeout(t)) => return Some(t),
            Err(e) => {
                debug!("WebRTC poll error for {}: {}", ctx.session_id, e);
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demux_classifies_first_byte() {
        assert!(WebRtcManager::is_webrtc_packet(&[0x00, 0x01]));
        assert!(WebRtcManager::is_webrtc_packet(&[22, 0]));
        assert!(WebRtcManager::is_webrtc_packet(&[0x80, 0]));
        assert!(!WebRtcManager::is_webrtc_packet(b"AURX"));
        assert!(!WebRtcManager::is_webrtc_packet(&[]));
    }

    #[test]
    fn stun_username_extraction_is_bounds_checked() {
        // header: type=0x0001, len=12, magic, txid(12)
        let mut msg = vec![0x00, 0x01, 0x00, 0x0c, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&[0u8; 12]);
        // USERNAME attr: type 0x0006 len 7 "abc:xyz" + 1 pad
        msg.extend_from_slice(&[0x00, 0x06, 0x00, 0x07]);
        msg.extend_from_slice(b"abc:xyz\0");
        assert_eq!(
            WebRtcManager::extract_stun_username(&msg).as_deref(),
            Some("abc:xyz")
        );
        // Truncated attribute must not panic or return partial data
        msg.truncate(26);
        msg[3] = 0x0c;
        assert!(WebRtcManager::extract_stun_username(&msg).is_none());
    }
}
