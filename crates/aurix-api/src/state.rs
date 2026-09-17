use aurix_control::ControlPlane;
use aurix_media::SfuNode;
use aurix_moderation::ModerationService;
use aurix_recording::RecordingService;
use std::sync::Arc;
use parking_lot::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub control: Arc<ControlPlane>,
    pub sfu: Arc<RwLock<SfuNode>>,
    pub moderation: Arc<ModerationService>,
    pub recording: Option<Arc<RecordingService>>,
}