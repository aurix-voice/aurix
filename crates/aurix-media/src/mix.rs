//! Server-side channel mixing for native receivers (`DownlinkMode::Mixed`,
//! `ChannelConfig::audience.mix_for_listeners`).
//!
//! A receiver in mixed mode gets one stereo Opus stream per channel — the channel's system
//! stream ([`channel_mix_ssrc`]) flagged `PacketFlags::Mixed` — instead of a stream per
//! speaker. The router hands every frame it would otherwise seal for such a receiver to the
//! [`MixHub`], already reduced to the receiver's `Mix` (mutes, blocks, volume, focus,
//! distance attenuation and direction, ambient slot); the hub decodes it once per mixer,
//! sums, encodes one frame every 20 ms and seals it per receiver with the receiver's own
//! keys, so authentication and queue isolation are exactly those of per-speaker delivery.
//!
//! Two kinds of mixer exist per channel:
//! * **shared** — one for all receivers that hear identical audio: receive-only members of a
//!   Team / Command channel without ambient mixing whose receiver preferences are uniform
//!   (no local mutes, volumes or blocks). One decode per speaker and one encode serve them
//!   all; channel focus, the only per-receiver difference left, travels as the packet's
//!   volume byte.
//! * **private** — one per (receiver, channel) otherwise (speakers must not hear
//!   themselves; positional / ambient gains and local rules differ per receiver).
//!
//! End-to-end encrypted frames never reach a mixer (the router forwards them as separate
//! streams); recording, transcription, safety and the cascade tap the sender's frame before
//! delivery, so mixing is a pure downlink optimisation.

use aurix_common::protocol::{channel_id_hash, AurixPacket, PacketFlags, PacketHeader, PacketType};
use aurix_common::types::{AudioCodec, ChannelId, SessionId};
use bytes::Bytes;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

use crate::channel::{MediaChannel, Mix};
use crate::mixer::{OpusMixer, FRAME_SAMPLES};
use crate::session::{MediaEndpoint, MediaSession};
use crate::transcode::PcmuDownlink;
use crate::tts::SYNTH_SSRC_FLAG;

/// One mixed frame per 20 ms, like every Opus stream in a channel.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(20);
/// A mixer without input for this long is torn down (its receivers re-create one with the
/// next frame; the stream's sequence continues).
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Hard bound on live mixers per node; receivers beyond it fall back to per-speaker streams.
pub const MAX_MIXERS: usize = 8192;
/// Salt that keeps a channel's mixed stream apart from its announcement stream
/// (`system_voice_ssrc`), which is derived from the same hash.
const MIX_SSRC_SALT: u32 = 0x5A5A_5A5A;

/// SSRC of the server mix of `channel_id` (stable per channel, top bit set like every
/// synthesized stream, never equal to the channel's `system_voice_ssrc`).
pub fn channel_mix_ssrc(channel_id: &ChannelId) -> u32 {
    ((channel_id_hash(channel_id) ^ MIX_SSRC_SALT) & !SYNTH_SSRC_FLAG) | SYNTH_SSRC_FLAG
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Shared(ChannelId),
    Private(SessionId, ChannelId),
}

impl Slot {
    fn channel(&self) -> &ChannelId {
        match self {
            Slot::Shared(c) | Slot::Private(_, c) => c,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Slot::Shared(_) => "shared",
            Slot::Private(..) => "private",
        }
    }
}

struct Subscriber {
    session: Arc<MediaSession>,
    /// Sequence of this receiver's mixed stream of the channel; shared with the hub so the
    /// stream stays continuous when the receiver moves between mixers.
    sequence: Arc<AtomicU32>,
}

pub struct MixNode {
    slot: Slot,
    ssrc: u32,
    channel_hash: u32,
    mixer: Mutex<OpusMixer>,
    subscribers: Mutex<HashMap<SessionId, Subscriber>>,
    last_fed: Mutex<Instant>,
    pcmu: Mutex<Option<PcmuDownlink>>,
    started: AtomicBool,
    closed: AtomicBool,
}

impl MixNode {
    fn subscribe(&self, session: &Arc<MediaSession>, sequence: Arc<AtomicU32>) {
        let mut subs = self.subscribers.lock();
        subs.entry(session.session_id)
            .or_insert_with(|| Subscriber {
                session: session.clone(),
                sequence,
            });
    }

    fn unsubscribe(&self, session_id: &SessionId) {
        self.subscribers.lock().remove(session_id);
    }

    fn push(&self, sender_ssrc: u32, mix: &Mix, payload: &[u8]) {
        *self.last_fed.lock() = Instant::now();
        if let Err(e) = self
            .mixer
            .lock()
            .push_opus(sender_ssrc, mix.volume, mix.direction, payload)
        {
            aurix_metrics::DOWNLINK_MIX_FRAMES
                .with_label_values(&["failed"])
                .inc();
            debug!("downlink mix {:?}: {e}", self.slot);
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().len()
    }

    /// Seals and sends one mixed frame to every subscriber.
    async fn emit(&self, frame: &[u8], socket: &UdpSocket) {
        let subs: Vec<(Arc<MediaSession>, Arc<AtomicU32>)> = self
            .subscribers
            .lock()
            .values()
            .map(|s| (s.session.clone(), s.sequence.clone()))
            .collect();
        let channel = *self.slot.channel();
        let shared = matches!(self.slot, Slot::Shared(_));
        let mut pcmu_frame: Option<Option<Bytes>> = None;
        let mut gone = Vec::new();
        for (session, sequence) in subs {
            if !session.is_active() || !session.channels.read().contains(&channel) {
                gone.push(session.session_id);
                continue;
            }
            let Some(endpoint) = session.endpoint() else {
                continue;
            };
            let body: Bytes = if session.codec() == AudioCodec::Pcmu {
                let f = pcmu_frame
                    .get_or_insert_with(|| self.pcmu_frame(frame))
                    .clone();
                let Some(f) = f else {
                    aurix_metrics::DOWNLINK_MIX_FRAMES
                        .with_label_values(&["dropped"])
                        .inc();
                    continue;
                };
                f
            } else {
                Bytes::copy_from_slice(frame)
            };
            let seq = sequence.fetch_add(1, Ordering::Relaxed);
            let mut header = PacketHeader::new(
                PacketType::Audio,
                seq,
                seq.wrapping_mul(FRAME_SAMPLES as u32),
                self.ssrc,
            );
            header.channel_id_hash = self.channel_hash;
            header.flags |= PacketFlags::Mixed as u16;
            if session.codec() == AudioCodec::Pcmu {
                header.flags |= PacketFlags::Pcmu as u16;
            }
            // A private mix already carries the receiver's focus gain (it is part of every
            // sender's delivery gain); a shared mix is at unity and applies it here.
            let volume = if shared {
                session.focus_gain(&channel)
            } else {
                1.0
            };
            let (header, body) = AurixPacket::new(header, body).downlink_parts(volume, None);
            let out = AurixPacket::seal_parts(&header, &body, &session.keys);
            let n = out.len() as u64;
            let sent = match endpoint {
                MediaEndpoint::Udp(addr) => match socket.try_send_to(&out, addr) {
                    Ok(_) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        socket.send_to(&out, addr).await.is_ok()
                    }
                    Err(e) => {
                        debug!("downlink mix to {}: {e}", session.user_id);
                        false
                    }
                },
                MediaEndpoint::Tunnel(tunnel) => tunnel.send(out.to_vec()),
            };
            if sent {
                session.record_packet_sent(n);
                aurix_metrics::PACKETS_SENT.inc();
                aurix_metrics::BYTES_SENT.inc_by(n);
                aurix_metrics::DOWNLINK_MIX_FRAMES
                    .with_label_values(&["sent"])
                    .inc();
            } else {
                aurix_metrics::PACKETS_DROPPED.inc();
                aurix_metrics::DOWNLINK_MIX_FRAMES
                    .with_label_values(&["dropped"])
                    .inc();
            }
        }
        if !gone.is_empty() {
            let mut subs = self.subscribers.lock();
            for sid in gone {
                subs.remove(&sid);
            }
        }
    }

    fn pcmu_frame(&self, opus: &[u8]) -> Option<Bytes> {
        let mut guard = self.pcmu.lock();
        if guard.is_none() {
            match PcmuDownlink::new() {
                Ok(d) => *guard = Some(d),
                Err(e) => {
                    warn!("PCMU downlink decoder for mix: {e}");
                    return None;
                }
            }
        }
        let result = guard.as_mut()?.transcode(opus);
        match result {
            Ok(ulaw) => {
                aurix_metrics::PCMU_FRAMES
                    .with_label_values(&["downlink", "ok"])
                    .inc();
                Some(ulaw)
            }
            Err(e) => {
                aurix_metrics::PCMU_FRAMES
                    .with_label_values(&["downlink", "error"])
                    .inc();
                debug!("PCMU transcode of mix: {e}");
                None
            }
        }
    }
}

pub struct MixHub {
    socket: Arc<UdpSocket>,
    bitrate_bps: i32,
    nodes: DashMap<Slot, Arc<MixNode>>,
    /// Which mixer currently serves each (receiver, channel).
    assignments: DashMap<(SessionId, ChannelId), Slot>,
    /// Sequence counters of the mixed streams, per (receiver, channel).
    sequences: DashMap<(SessionId, ChannelId), Arc<AtomicU32>>,
    me: Weak<MixHub>,
}

impl MixHub {
    pub fn new(socket: Arc<UdpSocket>, bitrate_bps: i32) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            socket,
            bitrate_bps,
            nodes: DashMap::new(),
            assignments: DashMap::new(),
            sequences: DashMap::new(),
            me: me.clone(),
        })
    }

    /// Feeds `packet` (plain Opus, level already stripped) into the mixers of the given
    /// receivers, each at its own `Mix`. Returns the receivers no mixer could take (node at
    /// [`MAX_MIXERS`] or codec failure); the caller serves those per-speaker. `shared_ok =
    /// false` keeps every receiver on a private mixer (the frame is theirs alone).
    pub fn push(
        &self,
        channel: &Arc<MediaChannel>,
        receivers: Vec<(Arc<MediaSession>, Mix)>,
        packet: &AurixPacket,
        shared_ok: bool,
    ) -> Vec<(Arc<MediaSession>, Mix)> {
        let mut leftovers = Vec::new();
        let mut shared_fed = false;
        let shareable = shared_ok && channel.supports_shared_mix();
        for (receiver, mix) in receivers {
            let key = (receiver.session_id, channel.channel_id);
            let private = Slot::Private(receiver.session_id, channel.channel_id);
            // A receiver stays on its private mixer once it has one (a frame meant for it
            // alone, a local mute, a direction): flipping between mixers would tear the
            // stream, and the shared one is only ever an optimisation.
            let slot = if shareable
                && mix.direction.is_none()
                && !channel.can_transmit(&receiver.user_id)
                && receiver.has_uniform_prefs()
                && self.assignments.get(&key).is_none_or(|s| *s != private)
            {
                Slot::Shared(channel.channel_id)
            } else {
                private
            };
            let Some(node) = self.node(slot) else {
                leftovers.push((receiver, mix));
                continue;
            };
            let sequence = self
                .sequences
                .entry(key)
                .or_insert_with(|| Arc::new(AtomicU32::new(0)))
                .clone();
            let previous = self.assignments.insert(key, slot);
            if let Some(prev) = previous.filter(|p| *p != slot) {
                if let Some(old) = self.nodes.get(&prev) {
                    old.unsubscribe(&receiver.session_id);
                }
            }
            node.subscribe(&receiver, sequence);
            match slot {
                Slot::Shared(_) => {
                    if !shared_fed {
                        // Uniform receivers of a Team/Command channel all hear the sender at
                        // unity; their focus gain is applied per packet on emit.
                        node.push(packet.header.ssrc, &Mix::UNITY, &packet.payload);
                        shared_fed = true;
                    }
                }
                Slot::Private(..) => node.push(packet.header.ssrc, &mix, &packet.payload),
            }
        }
        leftovers
    }

    fn node(&self, slot: Slot) -> Option<Arc<MixNode>> {
        if let Some(n) = self.nodes.get(&slot) {
            return Some(n.clone());
        }
        if self.nodes.len() >= MAX_MIXERS {
            aurix_metrics::DOWNLINK_MIX_FRAMES
                .with_label_values(&["failed"])
                .inc();
            return None;
        }
        let mixer = match OpusMixer::new(self.bitrate_bps) {
            Ok(m) => m,
            Err(e) => {
                warn!("downlink mixer: {e}");
                aurix_metrics::DOWNLINK_MIX_FRAMES
                    .with_label_values(&["failed"])
                    .inc();
                return None;
            }
        };
        let channel = *slot.channel();
        let node = Arc::new(MixNode {
            slot,
            ssrc: channel_mix_ssrc(&channel),
            channel_hash: channel_id_hash(&channel),
            mixer: Mutex::new(mixer),
            subscribers: Mutex::new(HashMap::new()),
            last_fed: Mutex::new(Instant::now()),
            pcmu: Mutex::new(None),
            started: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        let node = self.nodes.entry(slot).or_insert_with(|| node).clone();
        if node.closed.load(Ordering::Relaxed) {
            return None;
        }
        // Exactly one ticker per node.
        if !node.started.swap(true, Ordering::AcqRel) {
            aurix_metrics::DOWNLINK_MIXERS
                .with_label_values(&[slot.kind()])
                .inc();
            let hub = self.me.clone();
            let socket = self.socket.clone();
            let ticker = node.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(FRAME_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    if ticker.closed.load(Ordering::Relaxed) {
                        break;
                    }
                    let since_fed = ticker.last_fed.lock().elapsed();
                    // Subscribers attach right after creation; give them a few frames.
                    let abandoned =
                        ticker.subscriber_count() == 0 && since_fed > FRAME_INTERVAL * 5;
                    if since_fed > IDLE_TIMEOUT || abandoned {
                        if let Some(hub) = hub.upgrade() {
                            hub.retire(&ticker);
                        }
                        break;
                    }
                    let frame: Option<Vec<u8>> = {
                        let mut mixer = ticker.mixer.lock();
                        match mixer.mix_frame() {
                            Ok(f) => f.map(<[u8]>::to_vec),
                            Err(e) => {
                                aurix_metrics::DOWNLINK_MIX_FRAMES
                                    .with_label_values(&["failed"])
                                    .inc();
                                debug!("downlink mix {:?}: {e}", ticker.slot);
                                None
                            }
                        }
                    };
                    if let Some(frame) = frame {
                        ticker.emit(&frame, &socket).await;
                    }
                }
                aurix_metrics::DOWNLINK_MIXERS
                    .with_label_values(&[ticker.slot.kind()])
                    .dec();
            });
        }
        Some(node)
    }

    fn retire(&self, node: &Arc<MixNode>) {
        node.closed.store(true, Ordering::Relaxed);
        self.nodes
            .remove_if(&node.slot, |_, n| Arc::ptr_eq(n, node));
        let subs: Vec<SessionId> = node.subscribers.lock().keys().copied().collect();
        for sid in subs {
            self.assignments
                .remove_if(&(sid, *node.slot.channel()), |_, s| *s == node.slot);
        }
    }

    /// The receiver left `channel` (or switched back to per-speaker streams for it). The
    /// stream's sequence counter is kept while the session lives: the receiver's replay
    /// window for the channel's mix SSRC must never see it restart.
    pub fn forget_receiver(&self, session_id: &SessionId, channel_id: &ChannelId) {
        let key = (*session_id, *channel_id);
        if let Some((_, slot)) = self.assignments.remove(&key) {
            if let Some(node) = self.nodes.get(&slot) {
                node.unsubscribe(session_id);
            }
        }
    }

    /// The session is gone or wants per-speaker streams again: drop every mixed stream it
    /// had. Channels that force mixing on it re-subscribe with the next frame; the stream's
    /// sequence continues while the session lives (`keep_sequences`).
    pub fn forget_session(&self, session_id: &SessionId, keep_sequences: bool) {
        let keys: Vec<(SessionId, ChannelId)> = self
            .assignments
            .iter()
            .filter(|e| e.key().0 == *session_id)
            .map(|e| *e.key())
            .collect();
        for key in keys {
            if let Some((_, slot)) = self.assignments.remove(&key) {
                if let Some(node) = self.nodes.get(&slot) {
                    node.unsubscribe(session_id);
                }
            }
        }
        if !keep_sequences {
            self.sequences.retain(|k, _| k.0 != *session_id);
        }
    }

    /// `(shared, private)` mixers alive right now.
    pub fn mixer_counts(&self) -> (usize, usize) {
        let mut shared = 0;
        let mut private = 0;
        for n in self.nodes.iter() {
            match n.key() {
                Slot::Shared(_) => shared += 1,
                Slot::Private(..) => private += 1,
            }
        }
        (shared, private)
    }

    /// Stops every ticker (node shutdown).
    pub fn shutdown(&self) {
        for n in self.nodes.iter() {
            n.closed.store(true, Ordering::Relaxed);
        }
        self.nodes.clear();
        self.assignments.clear();
        self.sequences.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tts::system_voice_ssrc;

    #[test]
    fn mix_ssrc_is_synthesized_and_distinct_from_the_announcement_stream() {
        for _ in 0..1000 {
            let ch = ChannelId::new();
            let mix = channel_mix_ssrc(&ch);
            assert_ne!(mix & SYNTH_SSRC_FLAG, 0);
            assert_ne!(mix, system_voice_ssrc(&ch));
        }
    }
}
