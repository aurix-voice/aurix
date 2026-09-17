use std::collections::VecDeque;

pub struct QualityEstimator {
    rtt_samples: VecDeque<f32>,
    jitter_samples: VecDeque<f32>,
    loss_window: VecDeque<(u64, bool)>,
    max_samples: usize,
    last_arrival_time: Option<u64>,
    last_transit: Option<i64>,
}

impl QualityEstimator {
    pub fn new(max_samples: usize) -> Self {
        Self {
            rtt_samples: VecDeque::with_capacity(max_samples),
            jitter_samples: VecDeque::with_capacity(max_samples),
            loss_window: VecDeque::with_capacity(max_samples * 10),
            max_samples,
            last_arrival_time: None,
            last_transit: None,
        }
    }

    pub fn record_rtt(&mut self, rtt_ms: f32) {
        if self.rtt_samples.len() >= self.max_samples {
            self.rtt_samples.pop_front();
        }
        self.rtt_samples.push_back(rtt_ms);
    }

    pub fn record_packet_arrival(&mut self, rtp_timestamp: u32, arrival_time_us: u64) {
        let transit = arrival_time_us as i64 - rtp_timestamp as i64;

        if let Some(last_transit) = self.last_transit {
            let jitter = (transit - last_transit).unsigned_abs() as f32 / 1000.0;
            if self.jitter_samples.len() >= self.max_samples {
                self.jitter_samples.pop_front();
            }
            self.jitter_samples.push_back(jitter);
        }
        self.last_transit = Some(transit);
        self.last_arrival_time = Some(arrival_time_us);
    }

    pub fn record_packet_received(&mut self, seq: u64) {
        if self.loss_window.len() >= self.max_samples * 10 {
            self.loss_window.pop_front();
        }
        self.loss_window.push_back((seq, true));
    }

    pub fn record_packet_lost(&mut self, seq: u64) {
        if self.loss_window.len() >= self.max_samples * 10 {
            self.loss_window.pop_front();
        }
        self.loss_window.push_back((seq, false));
    }

    pub fn average_rtt(&self) -> f32 {
        if self.rtt_samples.is_empty() {
            return 0.0;
        }
        self.rtt_samples.iter().sum::<f32>() / self.rtt_samples.len() as f32
    }

    pub fn average_jitter(&self) -> f32 {
        if self.jitter_samples.is_empty() {
            return 0.0;
        }
        self.jitter_samples.iter().sum::<f32>() / self.jitter_samples.len() as f32
    }

    pub fn packet_loss_percent(&self) -> f32 {
        if self.loss_window.is_empty() {
            return 0.0;
        }
        let lost = self.loss_window.iter().filter(|(_, received)| !received).count();
        (lost as f32 / self.loss_window.len() as f32) * 100.0
    }

    pub fn p95_rtt(&self) -> f32 {
        if self.rtt_samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self.rtt_samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f32) * 0.95) as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    pub fn p99_rtt(&self) -> f32 {
        if self.rtt_samples.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self.rtt_samples.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((sorted.len() as f32) * 0.99) as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    pub fn mos_estimate(&self) -> f32 {
        let rtt = self.average_rtt();
        let jitter = self.average_jitter();
        let loss = self.packet_loss_percent();

        let effective_latency = rtt + jitter * 2.0 + 10.0;
        let r = if effective_latency < 160.0 {
            93.2 - (effective_latency / 40.0)
        } else {
            93.2 - ((effective_latency - 120.0) / 10.0)
        };
        let r = r - (loss * 2.5);
        let r = r.max(0.0).min(100.0);
        1.0 + 0.035 * r + r * (r - 60.0) * (100.0 - r) * 7.0e-6
    }
}