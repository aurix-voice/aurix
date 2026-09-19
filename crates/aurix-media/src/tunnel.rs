//! AURX over the control WebSocket: the fallback media path for native clients whose UDP is
//! blocked (corporate NAT, hotel Wi-Fi, some mobile carriers).
//!
//! The wire format is unchanged — every binary WebSocket frame carries exactly one sealed AURX
//! packet, in both directions — so authentication (per-session HMAC + AEAD), replay windows,
//! sequence continuity and the whole fan-out stay identical to the UDP path. Only the
//! "source address" used to attribute an uplink packet differs: a tunnel is owned by exactly one
//! WebSocket connection and therefore by exactly one session, which the connection has already
//! authenticated with its bearer/resume token.
//!
//! The trade-off is the usual TCP one: head-of-line blocking under loss. The downlink queue is
//! bounded and dropping, so a stalled client connection costs its own audio, never the SFU's.

use aurix_common::types::SessionId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

static NEXT_TUNNEL_ID: AtomicU64 = AtomicU64::new(1);

/// Downlink side of one AURX-over-WebSocket tunnel. Held by the [`MediaSession`] while the
/// session is bound through it; the WebSocket send loop drains the paired receiver.
///
/// [`MediaSession`]: crate::session::MediaSession
#[derive(Debug)]
pub struct MediaTunnel {
    id: u64,
    session_id: SessionId,
    tx: mpsc::Sender<Vec<u8>>,
    sent: AtomicU64,
    dropped: AtomicU64,
}

impl MediaTunnel {
    /// Creates a tunnel whose downlink queue holds at most `capacity` packets (≥ 1).
    pub fn new(session_id: SessionId, capacity: usize) -> (Arc<Self>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let tunnel = Arc::new(Self {
            id: NEXT_TUNNEL_ID.fetch_add(1, Ordering::Relaxed),
            session_id,
            tx,
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        (tunnel, rx)
    }

    /// Session this tunnel's WebSocket connection authenticated as; uplink packets arriving
    /// through it can only ever be attributed to this session.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Node-unique identity (a resumed session gets a new tunnel per connection).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// False once the WebSocket send loop is gone.
    pub fn is_open(&self) -> bool {
        !self.tx.is_closed()
    }

    /// Queues one sealed packet for the client; returns `false` (and counts a drop) when the
    /// connection is gone or its queue is full.
    pub fn send(&self, packet: Vec<u8>) -> bool {
        match self.tx.try_send(packet) {
            Ok(()) => {
                self.sent.fetch_add(1, Ordering::Relaxed);
                aurix_metrics::TUNNEL_PACKETS
                    .with_label_values(&["downlink", "sent"])
                    .inc();
                true
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                aurix_metrics::TUNNEL_PACKETS
                    .with_label_values(&["downlink", "dropped"])
                    .inc();
                false
            }
        }
    }

    pub fn packets_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    pub fn packets_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl PartialEq for MediaTunnel {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for MediaTunnel {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_bounded_and_dropping() {
        let (tunnel, mut rx) = MediaTunnel::new(SessionId::new(), 2);
        assert!(tunnel.send(vec![1]));
        assert!(tunnel.send(vec![2]));
        assert!(!tunnel.send(vec![3]));
        assert_eq!(tunnel.packets_sent(), 2);
        assert_eq!(tunnel.packets_dropped(), 1);
        assert_eq!(rx.try_recv().unwrap(), vec![1]);
        assert!(tunnel.send(vec![4]));
        assert!(tunnel.is_open());
        drop(rx);
        assert!(!tunnel.is_open());
        assert!(!tunnel.send(vec![5]));
    }

    #[test]
    fn tunnels_are_distinct_per_connection() {
        let sid = SessionId::new();
        let (a, _ra) = MediaTunnel::new(sid, 1);
        let (b, _rb) = MediaTunnel::new(sid, 1);
        assert_ne!(a.id(), b.id());
        assert!(*a != *b);
        assert_eq!(a.session_id(), b.session_id());
    }
}
