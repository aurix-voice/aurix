use crate::crypto::compute_audit_hash;
use crate::types::{AppId, AuditAction, AuditLogEntry, UserId};
use chrono::Utc;
use parking_lot::Mutex;
use std::sync::Arc;
use uuid::Uuid;

pub struct AuditLogger {
    last_hash: Arc<Mutex<String>>,
    sender: flume::Sender<AuditLogEntry>,
}

impl AuditLogger {
    pub fn new(sender: flume::Sender<AuditLogEntry>) -> Self {
        Self {
            last_hash: Arc::new(Mutex::new("genesis".to_string())),
            sender,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        app_id: Option<AppId>,
        actor_id: UserId,
        action: AuditAction,
        target_type: &str,
        target_id: &str,
        details: serde_json::Value,
        ip_address: Option<String>,
    ) {
        let mut last = self.last_hash.lock();
        let mut data = serde_json::to_vec(&details).unwrap_or_default();
        data.extend_from_slice(actor_id.0.as_bytes());
        data.extend_from_slice(target_type.as_bytes());
        data.extend_from_slice(target_id.as_bytes());
        let hash = compute_audit_hash(&last, &data);
        let entry = AuditLogEntry {
            id: Uuid::now_v7(),
            timestamp: Utc::now(),
            app_id,
            actor_id,
            action,
            target_type: target_type.to_string(),
            target_id: target_id.to_string(),
            details,
            ip_address,
            previous_hash: last.clone(),
            hash: hash.clone(),
        };
        *last = hash;
        if self.sender.try_send(entry).is_err() {
            tracing::error!("audit log queue full or closed; entry dropped");
        }
    }
}

impl Clone for AuditLogger {
    fn clone(&self) -> Self {
        Self {
            last_hash: self.last_hash.clone(),
            sender: self.sender.clone(),
        }
    }
}
