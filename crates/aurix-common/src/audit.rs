use crate::types::{AuditAction, AuditLogEntry, UserId};
use crate::crypto::compute_audit_hash;
use chrono::Utc;
use uuid::Uuid;
use parking_lot::Mutex;
use std::sync::Arc;

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

    pub fn log(
        &self,
        actor_id: UserId,
        action: AuditAction,
        target_type: &str,
        target_id: &str,
        details: serde_json::Value,
        ip_address: Option<String>,
    ) {
        let mut last = self.last_hash.lock();
        let data = serde_json::to_vec(&details).unwrap_or_default();
        let hash = compute_audit_hash(&last, &data);
        let entry = AuditLogEntry {
            id: Uuid::now_v7(),
            timestamp: Utc::now(),
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
        let _ = self.sender.try_send(entry);
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