//! WebRTC session management using the str0m sans-IO library.
//!
//! Each browser client gets a dedicated `WebRtcSession` running in its own tokio task
//! that owns the `Rtc` state machine. Uplink audio (`Event::MediaData`) is forwarded to
//! the SFU router. Downlink: the browser's first (sendrecv) audio m-line carries a
//! server-side mix (`OpusMixer`) of everyone who has no track of their own; every further
//! (recvonly) audio m-line the browser offers is a per-participant track that forwards one
//! speaker's Opus frames unmodified (`SlotTable` decides who gets one), so the browser can
//! spatialize them itself. The `WebRtcManager` demuxes incoming UDP packets to the correct
//! session based on source address or ICE ufrag.

use crate::transport::MediaSocket;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::ParticipantStream;
use aurix_common::types::*;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use str0m::change::SdpOffer;
use str0m::format::{Codec, FormatParams};
use str0m::media::{Direction as RtcDirection, Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::mixer::{MixerConfig, OpusMixer, FRAME_SAMPLES};

/// Dynamic payload type offered for Opus (the same one browsers use by default).
const OPUS_PT: Pt = Pt::new_with_value(111);

/// A per-participant track whose speaker has been silent this long may be handed to another
/// speaker that needs a track.
const SLOT_EVICT_IDLE: Duration = Duration::from_secs(1);
/// A per-participant track whose speaker has been silent this long is released outright.
const SLOT_RELEASE_IDLE: Duration = Duration::from_secs(30);
/// Largest plausible RTP clock jump between two consecutive frames of one speaker (48 kHz);
/// anything larger is treated as a new talkspurt.
const MAX_TS_STEP: u32 = 48_000 * 5;

/// Handle to communicate with a running WebRTC session task.
struct SessionHandle {
    net_tx: mpsc::Sender<(Vec<u8>, SocketAddr, SocketAddr)>,
    media_tx: mpsc::Sender<SessionInput>,
    ice_ufrag: String,
}

/// Opus payload from another participant for this client's downlink: forwarded as-is on the
/// sender's per-participant track when they hold one, mixed into the shared track otherwise.
pub struct ForwardMedia {
    /// Whose microphone the frame is; `None` for synthesized audio (always mixed).
    pub speaker: Option<UserId>,
    pub sender_ssrc: u32,
    /// Sender's RTP clock (48 kHz) for the frame; keeps the track's timeline continuous.
    pub sender_ts: u32,
    pub volume: f32,
    /// Where the sender is relative to this listener (directional positional channels).
    pub direction: Option<Direction>,
    /// End-to-end encrypted: only ever written to a per-participant track (the mixer cannot
    /// decode it); dropped when the speaker has no track.
    pub e2ee: bool,
    pub payload: Vec<u8>,
}

enum SessionInput {
    Media(ForwardMedia),
    /// Replace the set of participants that must keep their own track (`SetParticipantStreams`).
    Pin(Vec<UserId>),
}

/// Event emitted from a WebRTC session to the SFU.
#[derive(Debug)]
pub enum WebRtcMediaEvent {
    /// Depayloaded Opus audio received from the browser.
    AudioReceived {
        session_id: SessionId,
        user_id: UserId,
        /// RTP sequence number (extended, monotonic per SSRC).
        seq: u64,
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
    /// Full snapshot of the session's per-participant tracks after a binding changed (also
    /// once when the media path comes up).
    ParticipantStreams {
        session_id: SessionId,
        streams: Vec<ParticipantStream>,
    },
}

/// Manages all WebRTC sessions. Shared across the SFU.
pub struct WebRtcManager {
    sessions: Arc<DashMap<SessionId, SessionHandle>>,
    addr_map: Arc<DashMap<SocketAddr, SessionId>>,
    ufrag_map: Arc<DashMap<String, SessionId>>,
    socket: Arc<MediaSocket>,
    /// Addresses advertised to browsers as ICE host candidates (public IPv4 and/or IPv6 with
    /// the media port); all of them lead to `socket`.
    advertised_addrs: Vec<SocketAddr>,
    event_tx: mpsc::Sender<WebRtcMediaEvent>,
    mixer: MixerConfig,
    max_participant_streams: usize,
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
        socket: Arc<MediaSocket>,
        advertised_addrs: Vec<SocketAddr>,
        event_tx: mpsc::Sender<WebRtcMediaEvent>,
        mixer: MixerConfig,
        max_participant_streams: u32,
    ) -> Self {
        let advertised_addrs: Vec<SocketAddr> = advertised_addrs
            .into_iter()
            .filter(|a| socket.can_reach(*a))
            .collect();
        Self {
            sessions: Arc::new(DashMap::new()),
            addr_map: Arc::new(DashMap::new()),
            ufrag_map: Arc::new(DashMap::new()),
            socket,
            advertised_addrs,
            event_tx,
            mixer,
            max_participant_streams: max_participant_streams as usize,
        }
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Per-participant downlink tracks one browser may negotiate (`media.webrtc_participant_streams`).
    pub fn max_participant_streams(&self) -> u32 {
        self.max_participant_streams as u32
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

        for addr in &self.advertised_addrs {
            let candidate = Candidate::host(*addr, "udp")
                .map_err(|e| AurixError::Transport(format!("ICE candidate: {e}")))?;
            rtc.add_local_candidate(candidate);
        }

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

        let mixer = OpusMixer::new(self.mixer)?;

        let (net_tx, net_rx) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr)>(512);
        let (media_tx, media_rx) = mpsc::channel::<SessionInput>(512);

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
            slots: SlotTable::new(self.max_participant_streams),
        };
        tokio::spawn(async move { session_task(rtc, ctx, net_rx, media_rx, mixer).await });

        info!("WebRTC session {} created for user {}", session_id, user_id);
        Ok(answer_sdp)
    }

    /// The advertised host candidate a packet from `src` was sent to (same address family).
    fn candidate_for(&self, src: SocketAddr) -> SocketAddr {
        self.advertised_addrs
            .iter()
            .find(|a| a.is_ipv4() == src.is_ipv4())
            .or_else(|| self.advertised_addrs.first())
            .copied()
            .unwrap_or_else(|| self.socket.local_addr())
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
        // itself is usually bound to a wildcard address, so report the candidate address of
        // the family the packet arrived on.
        let dst = self.candidate_for(src);
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

    /// Queue Opus audio from another participant for this client's downlink.
    pub fn send_to_session(&self, session_id: &SessionId, media: ForwardMedia) -> bool {
        match self.sessions.get(session_id) {
            Some(handle) => handle.media_tx.try_send(SessionInput::Media(media)).is_ok(),
            None => false,
        }
    }

    pub fn set_pinned(&self, session_id: &SessionId, pinned: Vec<UserId>) -> Result<()> {
        match self.sessions.get(session_id) {
            Some(handle) => handle
                .media_tx
                .try_send(SessionInput::Pin(pinned))
                .map_err(|_| AurixError::Transport("WebRTC session busy".into())),
            None => Err(AurixError::Validation(
                "Session has no WebRTC media path".into(),
            )),
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
    socket: Arc<MediaSocket>,
    event_tx: mpsc::Sender<WebRtcMediaEvent>,
    addr_map: Arc<DashMap<SocketAddr, SessionId>>,
    sessions: Arc<DashMap<SessionId, SessionHandle>>,
    ufrag_map: Arc<DashMap<String, SessionId>>,
    slots: SlotTable,
}

struct Downlink {
    mid: Option<Mid>,
    pt: Option<Pt>,
    rtp_time: u64,
}

/// One per-participant downlink track and the speaker it currently carries.
#[derive(Debug, Clone)]
pub(crate) struct Slot {
    user: Option<UserId>,
    /// Last frame forwarded on the track (or the moment it was bound).
    last_packet: Instant,
    /// Sender's RTP clock of that frame, to carry its cadence onto the track's own clock.
    last_sender_ts: Option<u32>,
    rtp_time: u64,
    /// Next frame starts a talkspurt (marker bit): fresh binding or a gap in the sender's clock.
    talkspurt: bool,
}

/// Which speaker each per-participant track of a browser session carries.
///
/// Bindings are sticky: a speaker keeps their track through pauses, so the browser's audio
/// graph for that track stays put. Only when another speaker needs a track and none is free
/// does the longest-silent unpinned track (silent ≥ `SLOT_EVICT_IDLE`) change hands; tracks
/// silent ≥ `SLOT_RELEASE_IDLE` are freed. Pinned participants (`SetParticipantStreams`) are
/// never displaced and may displace an active unpinned speaker. Speakers without a track are
/// heard in the mixed track instead.
#[derive(Debug)]
pub(crate) struct SlotTable {
    slots: Vec<Slot>,
    pinned: Vec<UserId>,
    max: usize,
    dirty: bool,
}

impl SlotTable {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            slots: Vec::new(),
            pinned: Vec::new(),
            max,
            dirty: false,
        }
    }

    /// Register one more negotiated track; `None` once the node's cap is reached (the extra
    /// m-line stays silent).
    pub(crate) fn add(&mut self, now: Instant) -> Option<usize> {
        if self.slots.len() >= self.max {
            return None;
        }
        self.slots.push(Slot {
            user: None,
            last_packet: now,
            last_sender_ts: None,
            rtp_time: 0,
            talkspurt: true,
        });
        self.dirty = true;
        Some(self.slots.len() - 1)
    }

    fn is_pinned(&self, user: &UserId) -> bool {
        self.pinned.contains(user)
    }

    fn bound_index(&self, user: &UserId) -> Option<usize> {
        self.slots.iter().position(|s| s.user == Some(*user))
    }

    /// Track for `user`'s next frame, binding one if needed and possible.
    pub(crate) fn assign(&mut self, user: UserId, now: Instant) -> Option<usize> {
        if let Some(i) = self.bound_index(&user) {
            return Some(i);
        }
        let min_idle = if self.is_pinned(&user) {
            Duration::ZERO
        } else {
            SLOT_EVICT_IDLE
        };
        let idx = self.victim(min_idle, now)?;
        self.bind(idx, user, now);
        Some(idx)
    }

    /// A free track, else the longest-silent unpinned one silent for at least `min_idle`.
    fn victim(&self, min_idle: Duration, now: Instant) -> Option<usize> {
        if let Some(i) = self.slots.iter().position(|s| s.user.is_none()) {
            return Some(i);
        }
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.user.is_some_and(|u| !self.is_pinned(&u)))
            .filter(|(_, s)| now.saturating_duration_since(s.last_packet) >= min_idle)
            .min_by_key(|(_, s)| s.last_packet)
            .map(|(i, _)| i)
    }

    fn bind(&mut self, idx: usize, user: UserId, now: Instant) {
        let slot = &mut self.slots[idx];
        let step = Self::wall_step(slot, now);
        slot.user = Some(user);
        slot.rtp_time = slot.rtp_time.wrapping_add(step);
        slot.last_packet = now;
        slot.last_sender_ts = None;
        slot.talkspurt = true;
        self.dirty = true;
    }

    /// Clock advance for a track whose speaker's cadence cannot be followed (new speaker, gap):
    /// the wall time since the previous frame, at least one frame.
    fn wall_step(slot: &Slot, now: Instant) -> u64 {
        let elapsed = now
            .saturating_duration_since(slot.last_packet)
            .as_secs_f64();
        ((elapsed * 48_000.0) as u64).max(FRAME_SAMPLES as u64)
    }

    /// RTP clock for a frame of the bound speaker stamped `sender_ts` by them, and whether it
    /// starts a talkspurt. Consecutive frames keep the sender's cadence (so DTX gaps survive);
    /// a fresh binding or an implausible jump falls back to wall time. `None` for a repeat of
    /// the previous frame — the same packet reaching this receiver through a second shared
    /// channel — which must not be written twice.
    pub(crate) fn stamp(
        &mut self,
        idx: usize,
        sender_ts: u32,
        now: Instant,
    ) -> Option<(u64, bool)> {
        let slot = &mut self.slots[idx];
        let step = match slot.last_sender_ts {
            Some(prev) => {
                let d = sender_ts.wrapping_sub(prev);
                if d == 0 {
                    return None;
                }
                if d > MAX_TS_STEP {
                    slot.talkspurt = true;
                    Self::wall_step(slot, now)
                } else {
                    d as u64
                }
            }
            None => 0,
        };
        slot.rtp_time = slot.rtp_time.wrapping_add(step);
        slot.last_sender_ts = Some(sender_ts);
        slot.last_packet = now;
        let talkspurt = std::mem::replace(&mut slot.talkspurt, false);
        Some((slot.rtp_time, talkspurt))
    }

    /// Replace the pinned set; a pinned participant currently heard in the mix takes a track
    /// with their next frame.
    pub(crate) fn set_pinned(&mut self, pinned: Vec<UserId>) {
        self.pinned = pinned;
        self.pinned.truncate(self.max);
    }

    /// Free tracks whose speaker has been silent for `SLOT_RELEASE_IDLE`.
    pub(crate) fn expire(&mut self, now: Instant) {
        for slot in &mut self.slots {
            if slot.user.is_some()
                && now.saturating_duration_since(slot.last_packet) >= SLOT_RELEASE_IDLE
            {
                slot.user = None;
                slot.last_sender_ts = None;
                slot.talkspurt = true;
                self.dirty = true;
            }
        }
    }

    /// Whether the bindings changed since the last `take_dirty`.
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn bindings(&self) -> Vec<Option<UserId>> {
        self.slots.iter().map(|s| s.user).collect()
    }
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
    mut ctx: SessionTaskCtx,
    mut net_rx: mpsc::Receiver<(Vec<u8>, SocketAddr, SocketAddr)>,
    mut media_rx: mpsc::Receiver<SessionInput>,
    mut mixer: OpusMixer,
) {
    let mut connected = false;
    let mut downlink = Downlink {
        mid: None,
        pt: None,
        rtp_time: 0,
    };
    // Per-participant tracks in `ctx.slots` order.
    let mut slot_mids: Vec<(Mid, Pt)> = Vec::new();
    let mut ice = IceState {
        disconnected_since: None,
    };
    let mut mix_tick = tokio::time::interval(Duration::from_millis(20));
    mix_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // str0m tells us when it next needs a timeout; start with a short one.
    let mut next_timeout = Instant::now() + Duration::from_millis(100);
    let mut last_expire = Instant::now();
    let mut layout_sent = false;

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
            Some(input) = media_rx.recv() => {
                match input {
                    SessionInput::Media(media) => {
                        let now = Instant::now();
                        let slot = media
                            .speaker
                            .filter(|_| connected)
                            .and_then(|user| ctx.slots.assign(user, now))
                            .filter(|i| *i < slot_mids.len());
                        match slot {
                            Some(i) => {
                                let (mid, pt) = slot_mids[i];
                                let Some((rtp_time, talkspurt)) = ctx.slots.stamp(i, media.sender_ts, now) else {
                                    continue;
                                };
                                let ts = MediaTime::new(rtp_time, Frequency::FORTY_EIGHT_KHZ);
                                if let Some(writer) = rtc.writer(mid) {
                                    let res = writer
                                        .start_of_talkspurt(talkspurt)
                                        .write(pt, now, ts, media.payload);
                                    if let Err(e) = res {
                                        debug!("participant track write error for {}: {}", ctx.session_id, e);
                                    } else {
                                        aurix_metrics::PACKETS_SENT.inc();
                                    }
                                }
                            }
                            None => {
                                if media.e2ee {
                                    aurix_metrics::PACKETS_DROPPED.inc();
                                    continue;
                                }
                                if let Err(e) = mixer.push_opus(
                                    media.sender_ssrc,
                                    media.sender_ts,
                                    media.volume,
                                    media.direction,
                                    &media.payload,
                                ) {
                                    debug!("mixer decode error for {}: {}", ctx.session_id, e);
                                }
                                continue;
                            }
                        }
                    }
                    SessionInput::Pin(pinned) => {
                        ctx.slots.set_pinned(pinned);
                        continue;
                    }
                }
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
                    let now = Instant::now();
                    if now.duration_since(last_expire) >= Duration::from_secs(1) {
                        last_expire = now;
                        ctx.slots.expire(now);
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

        if let Some(t) = poll_outputs(
            &mut rtc,
            &mut ctx,
            &mut connected,
            &mut downlink,
            &mut slot_mids,
            &mut ice,
        )
        .await
        {
            next_timeout = t;
        }
        // The browser learns its track layout once media can flow, then on every change.
        let dirty = ctx.slots.take_dirty();
        if connected && (dirty || !layout_sent) {
            layout_sent = true;
            let streams = slot_mids
                .iter()
                .zip(ctx.slots.bindings())
                .map(|((mid, _), user_id)| ParticipantStream {
                    mid: mid.to_string(),
                    user_id,
                })
                .collect();
            let _ = ctx
                .event_tx
                .send(WebRtcMediaEvent::ParticipantStreams {
                    session_id: ctx.session_id,
                    streams,
                })
                .await;
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
    ctx: &mut SessionTaskCtx,
    connected: &mut bool,
    downlink: &mut Downlink,
    slot_mids: &mut Vec<(Mid, Pt)>,
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
                    if added.kind != MediaKind::Audio {
                        continue;
                    }
                    let pt = rtc.writer(added.mid).and_then(|w| {
                        w.payload_params()
                            .find(|p| p.spec().codec == Codec::Opus)
                            .map(|p| p.pt())
                    });
                    // The browser's sendrecv m-line carries the mix; its recvonly m-lines
                    // (sendonly from here) are per-participant tracks.
                    if downlink.mid.is_none() && added.direction == RtcDirection::SendRecv {
                        downlink.mid = Some(added.mid);
                        downlink.pt = pt;
                        if pt.is_none() {
                            warn!(
                                "WebRTC session {}: no Opus payload negotiated for downlink",
                                ctx.session_id
                            );
                        }
                    } else if added.direction == RtcDirection::SendOnly {
                        match pt {
                            Some(pt) => {
                                if ctx.slots.add(Instant::now()).is_some() {
                                    slot_mids.push((added.mid, pt));
                                } else {
                                    debug!(
                                        "WebRTC session {}: participant track {} beyond the node cap, left idle",
                                        ctx.session_id, added.mid
                                    );
                                }
                            }
                            None => warn!(
                                "WebRTC session {}: no Opus payload negotiated for participant track {}",
                                ctx.session_id, added.mid
                            ),
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
                            seq: **data.seq_range.start(),
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

    fn table(tracks: usize, cap: usize) -> (SlotTable, Instant) {
        let now = Instant::now();
        let mut t = SlotTable::new(cap);
        for _ in 0..tracks {
            t.add(now);
        }
        (t, now)
    }

    #[test]
    fn slots_are_capped_and_sticky() {
        let (mut t, now) = table(3, 2);
        assert_eq!(
            t.bindings().len(),
            2,
            "third m-line beyond the node cap is not served"
        );
        let (a, b, c) = (UserId::new(), UserId::new(), UserId::new());
        assert_eq!(t.assign(a, now), Some(0));
        assert_eq!(t.assign(b, now), Some(1));
        // Both tracks busy and fresh: the third speaker stays in the mix.
        assert_eq!(t.assign(c, now), None);
        // Same speaker keeps the same track.
        assert_eq!(t.assign(a, now + Duration::from_millis(20)), Some(0));
        assert!(t.take_dirty());
        assert!(!t.take_dirty());
        assert_eq!(t.bindings(), vec![Some(a), Some(b)]);
    }

    #[test]
    fn longest_silent_unpinned_track_changes_hands() {
        let (mut t, now) = table(2, 2);
        let (a, b, c) = (UserId::new(), UserId::new(), UserId::new());
        t.assign(a, now);
        t.assign(b, now);
        t.stamp(0, 960, now + Duration::from_millis(500));
        t.take_dirty();
        // `b` has been silent longest but not for SLOT_EVICT_IDLE yet.
        assert_eq!(t.assign(c, now + Duration::from_millis(900)), None);
        assert_eq!(t.assign(c, now + SLOT_EVICT_IDLE), Some(1));
        assert!(t.take_dirty());
        assert_eq!(t.bindings(), vec![Some(a), Some(c)]);
    }

    #[test]
    fn pinned_speakers_displace_and_are_never_displaced() {
        let (mut t, now) = table(1, 1);
        let (a, p, c) = (UserId::new(), UserId::new(), UserId::new());
        t.set_pinned(vec![p]);
        t.assign(a, now);
        // Pinned speaker takes the only track from an active unpinned one right away.
        assert_eq!(t.assign(p, now + Duration::from_millis(20)), Some(0));
        // Nobody takes it back, however long the pinned speaker pauses (short of release).
        assert_eq!(t.assign(c, now + Duration::from_secs(10)), None);
        assert_eq!(t.assign(a, now + Duration::from_secs(10)), None);
        // Until they are unpinned.
        t.set_pinned(Vec::new());
        assert_eq!(t.assign(c, now + Duration::from_secs(10)), Some(0));
    }

    #[test]
    fn release_frees_silent_tracks() {
        let (mut t, now) = table(1, 1);
        let a = UserId::new();
        t.assign(a, now);
        t.take_dirty();
        t.expire(now + SLOT_RELEASE_IDLE - Duration::from_secs(1));
        assert_eq!(t.bindings(), vec![Some(a)]);
        t.expire(now + SLOT_RELEASE_IDLE);
        assert_eq!(t.bindings(), vec![None]);
        assert!(t.take_dirty());
    }

    #[test]
    fn track_clock_follows_sender_and_marks_talkspurts() {
        let (mut t, now) = table(1, 1);
        let (a, b) = (UserId::new(), UserId::new());
        t.assign(a, now);
        let (t0, m0) = t.stamp(0, 10_000, now).unwrap();
        assert!(m0, "first frame of a binding starts a talkspurt");
        let (t1, m1) = t.stamp(0, 10_960, now + Duration::from_millis(20)).unwrap();
        assert_eq!(t1 - t0, 960);
        assert!(!m1);
        // The same frame again (delivered through a second shared channel) is dropped.
        assert_eq!(t.stamp(0, 10_960, now + Duration::from_millis(21)), None);
        // DTX pause of 5 frames on the sender's clock is carried over as-is.
        let (t2, m2) = t
            .stamp(0, 10_960 + 5 * 960, now + Duration::from_millis(120))
            .unwrap();
        assert_eq!(t2 - t1, 5 * 960);
        assert!(!m2);
        // A sender clock reset (implausible jump) starts a talkspurt on wall time.
        let (t3, m3) = t.stamp(0, 42, now + Duration::from_millis(220)).unwrap();
        assert!(m3);
        assert!(t3 - t2 >= 960 && t3 - t2 <= 48_000 / 5, "{}", t3 - t2);
        // A new speaker after a 2 s pause: clock jumps by the pause, new talkspurt.
        t.expire(now + Duration::from_secs(1));
        assert_eq!(t.bindings(), vec![Some(a)]);
        t.set_pinned(vec![b]);
        assert_eq!(t.assign(b, now + Duration::from_millis(2220)), Some(0));
        let (t4, m4) = t.stamp(0, 7, now + Duration::from_millis(2220)).unwrap();
        assert!(m4);
        assert!(t4 - t3 >= 2 * 48_000 - 960, "{}", t4 - t3);
    }
}
