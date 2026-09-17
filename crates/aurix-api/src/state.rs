use aurix_control::ControlPlane;
use aurix_media::SfuNode;
use aurix_moderation::ModerationService;
use aurix_recording::RecordingService;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub control: Arc<ControlPlane>,
    pub sfu: Arc<RwLock<SfuNode>>,
    pub moderation: Arc<ModerationService>,
    pub recording: Option<Arc<RecordingService>>,
    pub trusted_proxies: Arc<Vec<ipnetwork::IpNetwork>>,
}

impl AppState {
    pub fn new(control: Arc<ControlPlane>, sfu: Arc<RwLock<SfuNode>>, moderation: Arc<ModerationService>, recording: Option<Arc<RecordingService>>) -> Self {
        let trusted_proxies = Arc::new(aurix_common::net::parse_trusted_proxies(&control.config.server.trusted_proxies));
        Self { control, sfu, moderation, recording, trusted_proxies }
    }
}
