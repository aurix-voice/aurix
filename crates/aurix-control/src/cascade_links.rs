//! Fleet-wide view of measured cascade links, as the topology planner consumes it.
//!
//! Every node probes its cascade peers (UDP first, TCP fallback) and publishes the confirmed
//! ones to `media_node_links`; [`LinkMatrix`] is the fresh subset of that table. Links are
//! looked up symmetrically: the worse of the two directions wins, a TCP link is charged a
//! latency penalty (head-of-line blocking), and a pair that *both* sides measure without
//! confirming each other is [`LinkCost::Unconfirmed`]. Pairs involving a node that has never
//! published anything (older release, just started) stay [`LinkCost::Unknown`] and are planned
//! exactly as before measurements existed.

use aurix_common::types::MediaNodeId;
use std::collections::{HashMap, HashSet};

/// Planning cost of one node pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCost {
    /// Neither side publishes link tables: assume a working link of average latency.
    Unknown,
    /// Both sides measure, neither reached the other on any transport.
    Unconfirmed,
    /// Reached; `rtt_ms` is the worse of the two directions.
    Confirmed { rtt_ms: u32, tcp: bool },
}

/// Assumed latency of a link nobody has measured.
pub const UNKNOWN_LINK_MS: u32 = 150;
/// Added to the measured RTT of a TCP fallback link when ranking routes.
pub const TCP_PENALTY_MS: u32 = 100;
/// Cost of a link both ends failed to confirm: usable only when nothing else is.
pub const UNCONFIRMED_LINK_MS: u32 = 10_000;
/// RTT bucket for hub ranking, so a few milliseconds of jitter do not re-elect hubs.
pub const RANK_BUCKET_MS: u32 = 25;
/// A two-hop path through a core hub replaces a direct hub link only when it saves this much.
pub const RELAY_GAIN_MS: u32 = 30;

impl LinkCost {
    /// Cost in milliseconds for ranking and path comparison.
    pub fn ms(self) -> u32 {
        match self {
            Self::Unknown => UNKNOWN_LINK_MS,
            Self::Unconfirmed => UNCONFIRMED_LINK_MS,
            Self::Confirmed { rtt_ms, tcp } => {
                rtt_ms.saturating_add(if tcp { TCP_PENALTY_MS } else { 0 })
            }
        }
    }

    pub fn is_unconfirmed(self) -> bool {
        matches!(self, Self::Unconfirmed)
    }
}

/// Directed measurements plus the set of nodes that publish them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkMatrix {
    links: HashMap<(MediaNodeId, MediaNodeId), (u32, bool)>,
    reporters: HashSet<MediaNodeId>,
}

impl LinkMatrix {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `node` as publishing link tables (possibly empty ones).
    pub fn reporter(&mut self, node: MediaNodeId) {
        self.reporters.insert(node);
    }

    /// Records that `from` reaches `to` with `rtt_ms` (over TCP when `tcp`).
    pub fn report(&mut self, from: MediaNodeId, to: MediaNodeId, rtt_ms: u32, tcp: bool) {
        self.reporters.insert(from);
        self.links.insert((from, to), (rtt_ms, tcp));
    }

    pub fn is_reporter(&self, node: MediaNodeId) -> bool {
        self.reporters.contains(&node)
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty() && self.reporters.is_empty()
    }

    /// Symmetric cost of the pair `(a, b)`.
    pub fn cost(&self, a: MediaNodeId, b: MediaNodeId) -> LinkCost {
        if a == b {
            return LinkCost::Confirmed {
                rtt_ms: 0,
                tcp: false,
            };
        }
        let ab = self.links.get(&(a, b)).copied();
        let ba = self.links.get(&(b, a)).copied();
        match (ab, ba) {
            (None, None) => {
                if self.reporters.contains(&a) && self.reporters.contains(&b) {
                    LinkCost::Unconfirmed
                } else {
                    LinkCost::Unknown
                }
            }
            (Some(l), None) | (None, Some(l)) => LinkCost::Confirmed {
                rtt_ms: l.0,
                tcp: l.1,
            },
            (Some(x), Some(y)) => {
                let cx = LinkCost::Confirmed {
                    rtt_ms: x.0,
                    tcp: x.1,
                };
                let cy = LinkCost::Confirmed {
                    rtt_ms: y.0,
                    tcp: y.1,
                };
                if cx.ms() >= cy.ms() {
                    cx
                } else {
                    cy
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_is_symmetric_worst_of_both_directions_and_penalizes_tcp() {
        let (a, b, c) = (MediaNodeId::new(), MediaNodeId::new(), MediaNodeId::new());
        let mut m = LinkMatrix::new();
        assert_eq!(m.cost(a, b), LinkCost::Unknown);
        m.report(a, b, 20, false);
        m.report(b, a, 35, false);
        assert_eq!(
            m.cost(a, b),
            LinkCost::Confirmed {
                rtt_ms: 35,
                tcp: false
            }
        );
        assert_eq!(
            m.cost(b, a),
            LinkCost::Confirmed {
                rtt_ms: 35,
                tcp: false
            }
        );
        // One direction over TCP is worse than a slightly slower UDP direction.
        m.report(a, b, 10, true);
        assert_eq!(
            m.cost(a, b),
            LinkCost::Confirmed {
                rtt_ms: 10,
                tcp: true
            }
        );
        assert_eq!(m.cost(a, b).ms(), 10 + TCP_PENALTY_MS);
        // Both a and b publish tables, neither lists c: only c's silence decides.
        assert_eq!(m.cost(a, c), LinkCost::Unknown, "c never published");
        m.reporter(c);
        assert_eq!(m.cost(a, c), LinkCost::Unconfirmed);
        assert!(m.cost(a, c).ms() > m.cost(a, b).ms());
        assert_eq!(
            m.cost(a, a),
            LinkCost::Confirmed {
                rtt_ms: 0,
                tcp: false
            }
        );
    }
}
