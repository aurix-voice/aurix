//! Lightweight in-game text chat: validation, anti-flood, filter hook, optional history and
//! cross-node fan-out of channel / directed messages and typing indicators.
//!
//! Delivery itself happens in the WebSocket layer of every node from the `ServerEvent`s this
//! service publishes, so a message accepted here reaches recipients on any node.

use crate::event_bus::{EventBus, ServerEvent};
use aurix_common::config::ChatConfig;
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::{ChatMessage, ChatReadMarker};
use aurix_common::rate_limit::RateLimiter;
use aurix_common::types::{AppId, ChannelId, SessionId, UserId};
use aurix_common::usage::{UsageMeter, UsageMetric};
use aurix_db::models::{ChatConversation, ChatMessageRow, ChatReadMarkerRow, MessageCursor};
use aurix_db::DbPool;
use chrono::Utc;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Sender of REST/system messages (`POST /v1/channels/:id/messages`).
pub const SYSTEM_USER: UserId = UserId(uuid::Uuid::nil());

/// Message as submitted by a client or the REST API, before filtering.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub app_id: AppId,
    /// Exactly one of `channel_id` / `to_user_id` is set.
    pub channel_id: Option<ChannelId>,
    pub to_user_id: Option<UserId>,
    pub from_user_id: UserId,
    pub display_name: String,
    pub text: String,
    pub metadata: Option<serde_json::Value>,
    pub client_ref: Option<String>,
    pub from_session_id: Option<SessionId>,
    /// Directed message whose recipient has no active session anywhere (stored for replay).
    pub offline: bool,
}

/// A stored conversation as seen by one user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversation {
    Channel(ChannelId),
    /// Directed messages between `user` and `peer` (both directions).
    Direct {
        user: UserId,
        peer: UserId,
    },
    /// Everything `user` sent or received (moderation view; has no read marker).
    User(UserId),
}

impl Conversation {
    fn contains(&self, m: &ChatMessageRow, reader: UserId) -> bool {
        match self {
            Self::Channel(c) => m.channel_id == Some(c.0),
            Self::Direct { user, peer } => {
                m.channel_id.is_none()
                    && ((m.from_user_id == user.0 && m.to_user_id == Some(peer.0))
                        || (m.from_user_id == peer.0 && m.to_user_id == Some(user.0)))
            }
            Self::User(u) => *u == reader && (m.from_user_id == u.0 || m.to_user_id == Some(u.0)),
        }
    }

    fn stored(&self, reader: UserId) -> Option<ChatConversation> {
        match self {
            Self::Channel(c) => Some(ChatConversation::Channel(c.0)),
            Self::Direct { user, peer } if *user == reader => {
                Some(ChatConversation::Direct(peer.0))
            }
            _ => None,
        }
    }
}

/// One page of history, newest first. `next_before` is set when older messages exist,
/// `next_after` when newer ones do.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryPage {
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

/// Request body sent to `chat.filter_webhook`.
#[derive(Debug, Serialize)]
pub struct FilterRequest<'a> {
    pub app_id: AppId,
    pub channel_id: Option<ChannelId>,
    pub from_user_id: UserId,
    pub to_user_id: Option<UserId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub display_name: &'a str,
    pub text: &'a str,
    pub metadata: Option<&'a serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterAction {
    Allow,
    /// Deliver `text` from the response instead of the original.
    Replace,
    Block,
}

/// Response body expected from the filter webhook.
#[derive(Debug, Clone, Deserialize)]
pub struct FilterResponse {
    pub action: FilterAction,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Outcome of running the content filter over a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Replace(String),
    Block(String),
}

/// Pluggable content filter; the webhook is the built-in implementation.
#[async_trait::async_trait]
pub trait TextFilter: Send + Sync {
    async fn check(&self, req: FilterRequest<'_>) -> std::result::Result<Verdict, String>;
}

pub struct WebhookFilter {
    client: reqwest::Client,
    url: String,
}

impl WebhookFilter {
    pub fn new(url: String, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self { client, url }
    }
}

#[async_trait::async_trait]
impl TextFilter for WebhookFilter {
    async fn check(&self, req: FilterRequest<'_>) -> std::result::Result<Verdict, String> {
        let resp = self
            .client
            .post(&self.url)
            .json(&req)
            .send()
            .await
            .map_err(|e| format!("filter webhook request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("filter webhook returned HTTP {}", resp.status()));
        }
        let body: FilterResponse = resp
            .json()
            .await
            .map_err(|e| format!("filter webhook returned invalid JSON: {e}"))?;
        Ok(match body.action {
            FilterAction::Allow => Verdict::Allow,
            FilterAction::Replace => Verdict::Replace(body.text.unwrap_or_default()),
            FilterAction::Block => Verdict::Block(
                body.reason
                    .unwrap_or_else(|| "Message rejected by content filter".into()),
            ),
        })
    }
}

pub struct ChatService {
    cfg: ChatConfig,
    pool: DbPool,
    events: Arc<EventBus>,
    /// Run in order; a `Replace` feeds the next stage, the first `Block` wins.
    filters: Vec<Arc<dyn TextFilter>>,
    flood: RateLimiter,
    typing: DashMap<(SessionId, ChannelId), Instant>,
    usage: Option<Arc<UsageMeter>>,
}

impl ChatService {
    /// `pre_filter` (the safety lexicon/classifier stage) runs before `chat.filter_webhook`.
    pub fn new(
        cfg: ChatConfig,
        pool: DbPool,
        events: Arc<EventBus>,
        pre_filter: Option<Arc<dyn TextFilter>>,
    ) -> Self {
        let webhook: Option<Arc<dyn TextFilter>> = cfg.filter_webhook.clone().map(|url| {
            Arc::new(WebhookFilter::new(
                url,
                Duration::from_millis(cfg.filter_timeout_ms.max(100)),
            )) as Arc<dyn TextFilter>
        });
        Self::with_filters(
            cfg,
            pool,
            events,
            pre_filter.into_iter().chain(webhook).collect(),
        )
    }

    pub fn with_filter(
        cfg: ChatConfig,
        pool: DbPool,
        events: Arc<EventBus>,
        filter: Option<Arc<dyn TextFilter>>,
    ) -> Self {
        Self::with_filters(cfg, pool, events, filter.into_iter().collect())
    }

    pub fn with_filters(
        cfg: ChatConfig,
        pool: DbPool,
        events: Arc<EventBus>,
        filters: Vec<Arc<dyn TextFilter>>,
    ) -> Self {
        let flood = RateLimiter::new(cfg.messages_per_second, cfg.message_burst);
        Self {
            cfg,
            pool,
            events,
            filters,
            flood,
            typing: DashMap::new(),
            usage: None,
        }
    }

    /// Counts accepted messages into the usage meter.
    pub fn with_usage(mut self, meter: Arc<UsageMeter>) -> Self {
        self.usage = Some(meter);
        self
    }

    pub fn config(&self) -> &ChatConfig {
        &self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn persists(&self) -> bool {
        self.cfg.enabled && self.cfg.persist
    }

    fn ensure_enabled(&self) -> Result<()> {
        if self.cfg.enabled {
            Ok(())
        } else {
            Err(AurixError::ChatDisabled)
        }
    }

    /// Per-session anti-flood bucket (messages of both kinds share it).
    pub fn check_flood(&self, session_id: SessionId) -> Result<()> {
        if self.flood.check(&session_id.to_string()) {
            Ok(())
        } else {
            Err(AurixError::RateLimitExceeded(
                "Too many chat messages".into(),
            ))
        }
    }

    /// Typing indicators are throttled per session and channel; excess ones are dropped
    /// silently (they are best-effort UX, not commands).
    pub fn typing_allowed(&self, session_id: SessionId, channel_id: ChannelId) -> bool {
        let min = Duration::from_millis(self.cfg.typing_interval_ms);
        let now = Instant::now();
        let mut entry = self.typing.entry((session_id, channel_id)).or_insert(
            now.checked_sub(min.max(Duration::from_millis(1)))
                .unwrap_or(now),
        );
        if now.duration_since(*entry) >= min {
            *entry = now;
            true
        } else {
            false
        }
    }

    pub fn forget_session(&self, session_id: SessionId) {
        self.typing.retain(|(s, _), _| *s != session_id);
    }

    pub fn validate(&self, text: &str, metadata: Option<&serde_json::Value>) -> Result<()> {
        self.ensure_enabled()?;
        if text.trim().is_empty() && metadata.is_none() {
            return Err(AurixError::Validation("Message text is empty".into()));
        }
        if text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(AurixError::Validation(
                "Message text contains control characters".into(),
            ));
        }
        let meta_len = metadata
            .map(|m| serde_json::to_vec(m).map(|v| v.len()).unwrap_or(usize::MAX))
            .unwrap_or(0);
        if text.len().saturating_add(meta_len) > self.cfg.max_message_bytes {
            return Err(AurixError::Validation(format!(
                "Message exceeds {} bytes",
                self.cfg.max_message_bytes
            )));
        }
        Ok(())
    }

    /// Runs the filter, stores the message when history is enabled and publishes it for
    /// delivery on every node. Returns the message as recipients will see it (without
    /// `client_ref`, which only the sender's echo carries).
    pub async fn accept(&self, mut msg: OutgoingMessage) -> Result<ChatMessage> {
        self.ensure_enabled()?;
        debug_assert!(msg.channel_id.is_some() != msg.to_user_id.is_some());
        self.validate(&msg.text, msg.metadata.as_ref())?;

        // Operator messages come from the REST API and bypass the player-content filter.
        if msg.from_user_id != SYSTEM_USER {
            let req = FilterRequest {
                app_id: msg.app_id,
                channel_id: msg.channel_id,
                from_user_id: msg.from_user_id,
                to_user_id: msg.to_user_id,
                session_id: msg.from_session_id,
                display_name: &msg.display_name,
                text: &msg.text,
                metadata: msg.metadata.as_ref(),
            };
            if let Some(text) = self.filter_text(req, self.cfg.max_message_bytes).await? {
                msg.text = text;
            }
        }

        let message = ChatMessage {
            id: uuid::Uuid::new_v4(),
            channel_id: msg.channel_id,
            from_user_id: msg.from_user_id,
            display_name: msg.display_name,
            to_user_id: msg.to_user_id,
            text: msg.text,
            metadata: msg.metadata,
            sent_at: Utc::now(),
            client_ref: None,
            offline: msg.offline,
        };

        if self.cfg.persist {
            let row = ChatMessageRow {
                id: message.id,
                app_id: msg.app_id.0,
                channel_id: message.channel_id.map(|c| c.0),
                from_user_id: message.from_user_id.0,
                display_name: message.display_name.clone(),
                to_user_id: message.to_user_id.map(|u| u.0),
                text: message.text.clone(),
                metadata: message.metadata.clone(),
                sent_at: message.sent_at,
                offline: message.offline,
            };
            aurix_db::queries::insert_chat_message(&self.pool, &row)
                .await
                .map_err(|e| AurixError::Database(format!("chat_messages insert: {e}")))?;
        }

        if let Some(meter) = &self.usage {
            meter.record(msg.app_id, message.channel_id, UsageMetric::ChatMessages, 1);
        }
        let mut published = message.clone();
        published.client_ref = msg.client_ref;
        self.events.publish(ServerEvent::ChatMessage {
            app_id: msg.app_id,
            message: published,
            from_session_id: msg.from_session_id,
        });
        Ok(message)
    }

    /// Runs the configured content filters (safety stage, then webhook) over player-authored
    /// text. Returns the replacement text when a filter rewrote it, `Ok(None)` when it passes
    /// unchanged, and `MessageBlocked` when it must not be delivered (including a failed
    /// filter call unless `filter_fail_open`). Voice-related text (TTS) shares this hook.
    pub async fn filter_text(
        &self,
        req: FilterRequest<'_>,
        max_replacement_bytes: usize,
    ) -> Result<Option<String>> {
        if self.filters.is_empty() {
            return Ok(None);
        }
        let mut replacement: Option<String> = None;
        for filter in &self.filters {
            let stage = FilterRequest {
                text: replacement.as_deref().unwrap_or(req.text),
                ..req
            };
            match filter.check(stage).await {
                Ok(Verdict::Allow) => {}
                Ok(Verdict::Replace(text)) => {
                    if text.len() > max_replacement_bytes {
                        return Err(AurixError::MessageBlocked(
                            "Filter replacement exceeds the size limit".into(),
                        ));
                    }
                    replacement = Some(text);
                }
                Ok(Verdict::Block(reason)) => return Err(AurixError::MessageBlocked(reason)),
                Err(e) if self.cfg.filter_fail_open => {
                    warn!("content filter unavailable, delivering unfiltered: {e}");
                }
                Err(e) => {
                    warn!("content filter unavailable, blocking message: {e}");
                    return Err(AurixError::MessageBlocked(
                        "Content filter is unavailable".into(),
                    ));
                }
            }
        }
        Ok(replacement)
    }

    pub fn has_filter(&self) -> bool {
        !self.filters.is_empty()
    }

    pub fn publish_typing(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
        user_id: UserId,
        session_id: SessionId,
        typing: bool,
    ) {
        self.events.publish(ServerEvent::ParticipantTyping {
            app_id,
            channel_id,
            user_id,
            session_id,
            typing,
        });
    }

    fn ensure_history(&self) -> Result<()> {
        self.ensure_enabled()?;
        if self.cfg.persist {
            Ok(())
        } else {
            Err(AurixError::NotFound(
                "Chat history is not enabled (chat.persist = false)".into(),
            ))
        }
    }

    /// Directed messages to users without an active session are stored and replayed on
    /// connect instead of being refused.
    pub fn offline_delivery(&self) -> bool {
        self.persists() && self.cfg.offline_delivery
    }

    /// Requested page size clamped to `1..=chat.history_page_max` (default 50).
    pub fn page_limit(&self, requested: Option<u32>) -> i64 {
        i64::from(requested.unwrap_or(50).clamp(1, self.cfg.history_page_max))
    }

    fn cursor(raw: Option<&str>, what: &str) -> Result<Option<MessageCursor>> {
        match raw {
            None => Ok(None),
            Some(c) => aurix_common::protocol::decode_chat_cursor(c)
                .map(|(sent_at, id)| Some(MessageCursor { sent_at, id }))
                .ok_or_else(|| AurixError::Validation(format!("Invalid `{what}` cursor"))),
        }
    }

    /// One page of a conversation's stored messages, newest first. `before` / `after` are
    /// cursors from an earlier page (or built with `ChatMessage::cursor`). Without `before`
    /// the page is anchored at the present (or right after `after` when paging forward).
    pub async fn history(
        &self,
        app_id: AppId,
        conversation: Conversation,
        before: Option<&str>,
        after: Option<&str>,
        limit: Option<u32>,
    ) -> Result<HistoryPage> {
        self.ensure_history()?;
        let before = Self::cursor(before, "before")?;
        let after = Self::cursor(after, "after")?;
        let limit = self.page_limit(limit);
        let forward = before.is_none() && after.is_some();
        let rows = match conversation {
            Conversation::Channel(channel_id) => {
                aurix_db::queries::list_channel_messages(
                    &self.pool,
                    app_id.0,
                    channel_id.0,
                    before,
                    after,
                    forward,
                    limit + 1,
                )
                .await
            }
            Conversation::Direct { user, peer } => {
                aurix_db::queries::list_direct_messages(
                    &self.pool,
                    app_id.0,
                    user.0,
                    peer.0,
                    before,
                    after,
                    forward,
                    limit + 1,
                )
                .await
            }
            Conversation::User(user) => {
                aurix_db::queries::list_user_messages(
                    &self.pool,
                    app_id.0,
                    user.0,
                    before,
                    after,
                    forward,
                    limit + 1,
                )
                .await
            }
        }
        .map_err(|e| AurixError::Database(format!("chat_messages query: {e}")))?;
        Ok(Self::page(rows, before, after, forward, limit as usize))
    }

    fn page(
        mut rows: Vec<ChatMessageRow>,
        before: Option<MessageCursor>,
        after: Option<MessageCursor>,
        forward: bool,
        limit: usize,
    ) -> HistoryPage {
        let more = rows.len() > limit;
        rows.truncate(limit);
        if forward {
            rows.reverse();
        }
        let messages: Vec<ChatMessage> = rows.into_iter().map(row_to_message).collect();
        let newest = messages.first().map(ChatMessage::cursor);
        let oldest = messages.last().map(ChatMessage::cursor);
        let (older_exist, newer_exist) = if forward {
            (after.is_some(), more)
        } else {
            (more, before.is_some())
        };
        HistoryPage {
            next_before: oldest.filter(|_| older_exist),
            next_after: newest.filter(|_| newer_exist),
            messages,
        }
    }

    /// Advances `user_id`'s marker in `conversation` to `message_id`. The message must belong
    /// to that conversation (`NOT_FOUND` otherwise). Returns the marker when it moved and
    /// publishes it for fan-out; `None` when it already was at or past that message.
    pub async fn mark_read(
        &self,
        app_id: AppId,
        user_id: UserId,
        conversation: Conversation,
        message_id: uuid::Uuid,
    ) -> Result<Option<ChatReadMarker>> {
        self.ensure_history()?;
        let row = aurix_db::queries::get_chat_message(&self.pool, app_id.0, message_id)
            .await
            .map_err(|e| AurixError::Database(format!("chat_messages query: {e}")))?
            .filter(|m| conversation.contains(m, user_id))
            .ok_or_else(|| AurixError::NotFound("Message not found in this conversation".into()))?;
        let stored = conversation
            .stored(user_id)
            .ok_or_else(|| AurixError::Validation("Not a readable conversation".into()))?;
        let marker = aurix_db::queries::advance_read_marker(
            &self.pool,
            app_id.0,
            user_id.0,
            stored,
            MessageCursor::of(&row),
        )
        .await
        .map_err(|e| AurixError::Database(format!("chat_read_markers upsert: {e}")))?
        .map(row_to_marker);
        if let Some(marker) = &marker {
            self.events.publish(ServerEvent::ChatReadMarker {
                app_id,
                marker: marker.clone(),
            });
        }
        Ok(marker)
    }

    /// `user_id`'s own marker in `conversation` plus, with `chat.read_receipts`, the markers of
    /// `others` (channel participants or the direct peer), and the user's unread count there.
    pub async fn read_markers(
        &self,
        app_id: AppId,
        user_id: UserId,
        conversation: Conversation,
        others: &[UserId],
    ) -> Result<(Vec<ChatReadMarker>, u32)> {
        self.ensure_history()?;
        let stored = conversation
            .stored(user_id)
            .ok_or_else(|| AurixError::Validation("Not a readable conversation".into()))?;
        let mut users: Vec<uuid::Uuid> = vec![user_id.0];
        if self.cfg.read_receipts {
            users.extend(others.iter().map(|u| u.0).filter(|u| *u != user_id.0));
        }
        let rows = match stored {
            ChatConversation::Channel(channel_id) => {
                aurix_db::queries::list_channel_read_markers(
                    &self.pool,
                    app_id.0,
                    channel_id,
                    Some(&users),
                    users.len() as i64,
                )
                .await
            }
            ChatConversation::Direct(peer) => {
                let mut rows = Vec::new();
                for u in users {
                    // The peer's marker of *this* user is their position in the same
                    // conversation, so the direct conversation is mirrored.
                    let conv = if u == user_id.0 {
                        ChatConversation::Direct(peer)
                    } else {
                        ChatConversation::Direct(user_id.0)
                    };
                    if let Some(row) =
                        aurix_db::queries::get_read_marker(&self.pool, app_id.0, u, conv).await?
                    {
                        rows.push(row);
                    }
                }
                Ok(rows)
            }
        }
        .map_err(|e| AurixError::Database(format!("chat_read_markers query: {e}")))?;
        let own = rows
            .iter()
            .find(|r| r.user_id == user_id.0)
            .map(|r| MessageCursor {
                sent_at: r.message_sent_at,
                id: r.message_id,
            });
        let unread = self.unread_count(app_id, user_id, stored, own).await?;
        Ok((rows.into_iter().map(row_to_marker).collect(), unread))
    }

    /// Messages in `conversation` newer than `after` that `user_id` did not send, saturating
    /// at `chat.unread_count_cap`.
    pub async fn unread_count(
        &self,
        app_id: AppId,
        user_id: UserId,
        conversation: ChatConversation,
        after: Option<MessageCursor>,
    ) -> Result<u32> {
        let n = aurix_db::queries::count_unread_messages(
            &self.pool,
            app_id.0,
            user_id.0,
            conversation,
            after,
            i64::from(self.cfg.unread_count_cap),
        )
        .await
        .map_err(|e| AurixError::Database(format!("chat_messages count: {e}")))?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Reading positions of everyone who read in a channel (dashboards), newest first.
    pub async fn channel_read_markers(
        &self,
        app_id: AppId,
        channel_id: ChannelId,
    ) -> Result<Vec<ChatReadMarker>> {
        self.ensure_history()?;
        let rows = aurix_db::queries::list_channel_read_markers(
            &self.pool,
            app_id.0,
            channel_id.0,
            None,
            1000,
        )
        .await
        .map_err(|e| AurixError::Database(format!("chat_read_markers query: {e}")))?;
        Ok(rows.into_iter().map(row_to_marker).collect())
    }

    /// Every stored read marker of a user (support tooling / export), newest first.
    pub async fn user_read_markers(
        &self,
        app_id: AppId,
        user_id: UserId,
        limit: i64,
    ) -> Result<Vec<ChatReadMarker>> {
        self.ensure_history()?;
        let rows =
            aurix_db::queries::list_user_read_markers(&self.pool, app_id.0, user_id.0, limit)
                .await
                .map_err(|e| AurixError::Database(format!("chat_read_markers query: {e}")))?;
        Ok(rows.into_iter().map(row_to_marker).collect())
    }

    /// Directed messages queued while `user_id` was offline that the user has not read yet,
    /// oldest first, at most `chat.offline_max_messages` (the newest ones; `truncated` tells
    /// whether older unread ones were left out) and not older than `offline_max_age_hours`.
    /// Empty when offline delivery is off.
    pub async fn offline_backlog(
        &self,
        app_id: AppId,
        user_id: UserId,
    ) -> Result<(Vec<ChatMessage>, bool)> {
        if !self.offline_delivery() {
            return Ok((Vec::new(), false));
        }
        let min_sent_at = (self.cfg.offline_max_age_hours > 0)
            .then(|| Utc::now() - chrono::Duration::hours(self.cfg.offline_max_age_hours as i64));
        let limit = i64::from(self.cfg.offline_max_messages);
        let mut rows = aurix_db::queries::list_unread_offline_messages(
            &self.pool,
            app_id.0,
            user_id.0,
            min_sent_at,
            limit + 1,
        )
        .await
        .map_err(|e| AurixError::Database(format!("chat_messages inbox query: {e}")))?;
        let truncated = rows.len() > limit as usize;
        if truncated {
            rows.remove(0);
        }
        Ok((rows.into_iter().map(row_to_message).collect(), truncated))
    }

    /// Hourly sweep of stored messages older than `chat.retention_days`.
    pub fn start_retention_sweep(self: &Arc<Self>) {
        if !self.persists() || self.cfg.retention_days == 0 {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let cutoff = Utc::now() - chrono::Duration::days(svc.cfg.retention_days as i64);
                match aurix_db::queries::delete_chat_messages_before(&svc.pool, cutoff).await {
                    Ok(n) if n > 0 => info!("Deleted {n} expired chat messages"),
                    Ok(_) => {}
                    Err(e) => warn!("Chat retention sweep failed: {e}"),
                }
            }
        });
    }
}

fn row_to_message(r: ChatMessageRow) -> ChatMessage {
    ChatMessage {
        id: r.id,
        channel_id: r.channel_id.map(ChannelId::from_uuid),
        from_user_id: UserId::from_uuid(r.from_user_id),
        display_name: r.display_name,
        to_user_id: r.to_user_id.map(UserId::from_uuid),
        text: r.text,
        metadata: r.metadata,
        sent_at: r.sent_at,
        client_ref: None,
        offline: r.offline,
    }
}

fn row_to_marker(r: ChatReadMarkerRow) -> ChatReadMarker {
    let direct = r.kind == "direct";
    ChatReadMarker {
        user_id: UserId::from_uuid(r.user_id),
        channel_id: (!direct).then(|| ChannelId::from_uuid(r.conversation_id)),
        peer_user_id: direct.then(|| UserId::from_uuid(r.conversation_id)),
        message_id: r.message_id,
        message_sent_at: r.message_sent_at,
        read_at: r.read_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(cfg: ChatConfig, filter: Option<Arc<dyn TextFilter>>) -> ChatService {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .expect("lazy pool");
        ChatService::with_filter(cfg, pool, Arc::new(EventBus::new(16)), filter)
    }

    struct Fixed(std::result::Result<Verdict, String>);

    #[async_trait::async_trait]
    impl TextFilter for Fixed {
        async fn check(&self, _req: FilterRequest<'_>) -> std::result::Result<Verdict, String> {
            self.0.clone()
        }
    }

    fn msg(text: &str) -> OutgoingMessage {
        OutgoingMessage {
            app_id: AppId::new(),
            channel_id: Some(ChannelId::new()),
            to_user_id: None,
            from_user_id: UserId::new(),
            display_name: "p".into(),
            text: text.into(),
            metadata: None,
            client_ref: Some("ref-1".into()),
            from_session_id: Some(SessionId::new()),
            offline: false,
        }
    }

    #[tokio::test]
    async fn validation_limits_size_and_control_chars() {
        let s = svc(
            ChatConfig {
                max_message_bytes: 16,
                ..ChatConfig::default()
            },
            None,
        );
        assert!(s.validate("hello", None).is_ok());
        assert!(s.validate("   ", None).is_err());
        assert!(s.validate("", Some(&serde_json::json!({"k": 1}))).is_ok());
        assert!(s.validate("a\u{7}b", None).is_err());
        assert!(s.validate("line1\nline2", None).is_ok());
        assert!(s.validate("0123456789abcdefg", None).is_err());
        assert!(s
            .validate("0123", Some(&serde_json::json!({"long": "xxxxxxxxxx"})))
            .is_err());

        let off = svc(
            ChatConfig {
                enabled: false,
                ..ChatConfig::default()
            },
            None,
        );
        assert!(matches!(
            off.validate("hi", None),
            Err(AurixError::ChatDisabled)
        ));
    }

    #[tokio::test]
    async fn flood_bucket_is_per_session() {
        let s = svc(
            ChatConfig {
                messages_per_second: 1,
                message_burst: 3,
                ..ChatConfig::default()
            },
            None,
        );
        let a = SessionId::new();
        let b = SessionId::new();
        for _ in 0..3 {
            assert!(s.check_flood(a).is_ok());
        }
        assert!(matches!(
            s.check_flood(a),
            Err(AurixError::RateLimitExceeded(_))
        ));
        assert!(s.check_flood(b).is_ok());
    }

    #[tokio::test]
    async fn typing_is_throttled_per_session_and_channel() {
        let s = svc(
            ChatConfig {
                typing_interval_ms: 60_000,
                ..ChatConfig::default()
            },
            None,
        );
        let sid = SessionId::new();
        let c1 = ChannelId::new();
        let c2 = ChannelId::new();
        assert!(s.typing_allowed(sid, c1));
        assert!(!s.typing_allowed(sid, c1));
        assert!(s.typing_allowed(sid, c2));
        s.forget_session(sid);
        assert!(s.typing_allowed(sid, c1));
    }

    #[tokio::test]
    async fn filter_verdicts_are_applied() {
        let cfg = ChatConfig::default();
        let allow = svc(cfg.clone(), Some(Arc::new(Fixed(Ok(Verdict::Allow)))));
        let mut rx = allow.events.subscribe();
        let m = allow.accept(msg("gg")).await.expect("allowed");
        assert_eq!(m.text, "gg");
        assert_eq!(m.client_ref, None, "recipients' copy carries no client_ref");
        match rx.try_recv().expect("published") {
            ServerEvent::ChatMessage { message, .. } => {
                assert_eq!(message.id, m.id);
                assert_eq!(message.client_ref.as_deref(), Some("ref-1"));
            }
            other => panic!("unexpected event {other:?}"),
        }

        let replace = svc(
            cfg.clone(),
            Some(Arc::new(Fixed(Ok(Verdict::Replace("****".into()))))),
        );
        assert_eq!(replace.accept(msg("darn")).await.unwrap().text, "****");

        let block = svc(
            cfg.clone(),
            Some(Arc::new(Fixed(Ok(Verdict::Block("toxic".into()))))),
        );
        assert!(matches!(
            block.accept(msg("x")).await,
            Err(AurixError::MessageBlocked(r)) if r == "toxic"
        ));

        let closed = svc(cfg.clone(), Some(Arc::new(Fixed(Err("down".into())))));
        assert!(matches!(
            closed.accept(msg("x")).await,
            Err(AurixError::MessageBlocked(_))
        ));

        let open = svc(
            ChatConfig {
                filter_fail_open: true,
                ..cfg
            },
            Some(Arc::new(Fixed(Err("down".into())))),
        );
        assert_eq!(open.accept(msg("x")).await.unwrap().text, "x");
    }

    /// Minimal HTTP/1.1 responder: answers every request with `status` and `body`, records the
    /// request bodies it saw, and optionally stalls to exercise the client timeout.
    async fn webhook_stub(
        status: &'static str,
        body: &'static str,
        stall: Option<Duration>,
    ) -> (String, Arc<parking_lot::Mutex<Vec<serde_json::Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/filter", listener.local_addr().unwrap());
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen_task = seen.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_task.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let Some(split) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let len = text[..split]
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        let body_start = split + 4;
                        if buf.len() < body_start + len {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_slice(&buf[body_start..body_start + len]) {
                            seen.lock().push(v);
                        }
                        break;
                    }
                    if let Some(d) = stall {
                        tokio::time::sleep(d).await;
                    }
                    let resp = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (url, seen)
    }

    fn webhook_svc(url: String, timeout_ms: u64, fail_open: bool) -> ChatService {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .expect("lazy pool");
        ChatService::new(
            ChatConfig {
                filter_webhook: Some(url),
                filter_timeout_ms: timeout_ms,
                filter_fail_open: fail_open,
                ..ChatConfig::default()
            },
            pool,
            Arc::new(EventBus::new(16)),
            None,
        )
    }

    #[tokio::test]
    async fn webhook_filter_contract_over_http() {
        let (url, seen) = webhook_stub(
            "200 OK",
            r#"{"action":"replace","text":"[redacted]"}"#,
            None,
        )
        .await;
        let svc = webhook_svc(url, 1500, false);
        let mut m = msg("buy gold at example.com");
        m.metadata = Some(serde_json::json!({"k": 1}));
        let out = svc.accept(m.clone()).await.expect("replaced, not rejected");
        assert_eq!(out.text, "[redacted]");
        let reqs = seen.lock().clone();
        assert_eq!(reqs.len(), 1, "one webhook call per message");
        let r = &reqs[0];
        assert_eq!(r["text"], "buy gold at example.com");
        assert_eq!(r["app_id"], serde_json::json!(m.app_id));
        assert_eq!(r["channel_id"], serde_json::json!(m.channel_id.unwrap()));
        assert_eq!(r["from_user_id"], serde_json::json!(m.from_user_id));
        assert_eq!(r["display_name"], "p");
        assert_eq!(r["metadata"], serde_json::json!({"k": 1}));
        assert!(r["to_user_id"].is_null());

        let (url, _) = webhook_stub("200 OK", r#"{"action":"block","reason":"spam"}"#, None).await;
        assert!(matches!(
            webhook_svc(url, 1500, false).accept(msg("x")).await,
            Err(AurixError::MessageBlocked(r)) if r == "spam"
        ));

        // System messages never hit the filter.
        let (url, seen) = webhook_stub("200 OK", r#"{"action":"block"}"#, None).await;
        let mut sys = msg("Match starts");
        sys.from_user_id = SYSTEM_USER;
        sys.from_session_id = None;
        webhook_svc(url, 1500, false)
            .accept(sys)
            .await
            .expect("system message bypasses filter");
        assert!(seen.lock().is_empty());

        // Webhook failure (HTTP 5xx) and timeout: fail-closed by default, fail-open when configured.
        let (url, _) = webhook_stub("500 Internal Server Error", "oops", None).await;
        assert!(matches!(
            webhook_svc(url.clone(), 1500, false).accept(msg("x")).await,
            Err(AurixError::MessageBlocked(_))
        ));
        assert_eq!(
            webhook_svc(url, 1500, true)
                .accept(msg("x"))
                .await
                .unwrap()
                .text,
            "x"
        );
        let (url, _) = webhook_stub(
            "200 OK",
            r#"{"action":"allow"}"#,
            Some(Duration::from_millis(600)),
        )
        .await;
        let started = Instant::now();
        assert!(matches!(
            webhook_svc(url, 150, false).accept(msg("x")).await,
            Err(AurixError::MessageBlocked(_))
        ));
        assert!(
            started.elapsed() < Duration::from_millis(550),
            "timeout is enforced client-side"
        );
    }
}
