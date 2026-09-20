//! Live translation of transcripts: the policy and plumbing around an [`MtProvider`].
//!
//! Every node translates for its own listeners: a `Transcript` event (produced on the speaker's
//! node, replicated through the event bus) is delivered untranslated to listeners that did not
//! ask for another language, and for the rest the WebSocket layer groups them by requested
//! language, asks this service for one translation per language and delivers each result only
//! to the listeners that requested that language. Translation never delays the untranslated
//! delivery. Requests are bounded (concurrency, text length, languages per segment, timeout)
//! and identical `(source, target, text)` requests are answered from a small per-node cache,
//! so the same segment requested on two nodes costs one provider call per node at most.

use aurix_common::config::{MtProviderKind, TranslationConfig};
use aurix_common::error::{AurixError, Result};
use aurix_common::protocol::TranslationInfo;
use aurix_common::translate::{
    normalize_language, same_language, LibreTranslateProvider, MtProvider, OpenAiChatMtProvider,
    Translation,
};
use aurix_common::tts_stt::HttpProviderOptions;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// What a listener asked for (`SetTranslation`), normalised.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListenerTranslation {
    /// Language transcripts are translated into; `None` = untranslated.
    pub language: Option<String>,
    /// Language this participant speaks, for segments whose language the transcriber did not
    /// report.
    pub spoken_language: Option<String>,
    /// Also speak the translation to this listener (needs TTS).
    pub speech: bool,
}

/// One `(source, target, text)` cache key.
type CacheKey = (Option<String>, String, String);

#[derive(Default)]
struct Cache {
    entries: HashMap<CacheKey, Translation>,
    order: VecDeque<CacheKey>,
}

impl Cache {
    fn get(&self, key: &CacheKey) -> Option<Translation> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: CacheKey, value: Translation, cap: usize) {
        if cap == 0 {
            return;
        }
        if self.entries.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.entries.len() > cap {
            match self.order.pop_front() {
                Some(old) => {
                    self.entries.remove(&old);
                }
                None => break,
            }
        }
    }
}

pub struct TranslationService {
    cfg: TranslationConfig,
    provider: Option<Arc<dyn MtProvider>>,
    permits: Arc<tokio::sync::Semaphore>,
    cache: Mutex<Cache>,
    tts_enabled: bool,
}

impl TranslationService {
    pub fn new(cfg: TranslationConfig, tts_enabled: bool) -> Self {
        let provider: Option<Arc<dyn MtProvider>> = match (&cfg.enabled, &cfg.endpoint) {
            (true, Some(endpoint)) => {
                let options = HttpProviderOptions {
                    api_key: cfg.api_key.clone(),
                    model: cfg.model.clone(),
                    timeout: Some(Duration::from_millis(cfg.timeout_ms)),
                };
                Some(match cfg.provider {
                    MtProviderKind::Libretranslate => {
                        Arc::new(LibreTranslateProvider::new(endpoint, options))
                    }
                    MtProviderKind::OpenaiChat => {
                        Arc::new(OpenAiChatMtProvider::new(endpoint, options))
                    }
                })
            }
            _ => None,
        };
        Self::with_provider(cfg, tts_enabled, provider)
    }

    pub fn with_provider(
        cfg: TranslationConfig,
        tts_enabled: bool,
        provider: Option<Arc<dyn MtProvider>>,
    ) -> Self {
        if let Some(p) = &provider {
            info!(
                "Live translation enabled (provider {}, spoken translations {})",
                p.provider_name(),
                if cfg.speech && tts_enabled {
                    "on"
                } else {
                    "off"
                }
            );
        }
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(
                cfg.max_concurrent_requests.max(1) as usize,
            )),
            cfg,
            provider,
            cache: Mutex::new(Cache::default()),
            tts_enabled,
        }
    }

    pub fn enabled(&self) -> bool {
        self.provider.is_some()
    }

    pub fn config(&self) -> &TranslationConfig {
        &self.cfg
    }

    /// Translations may also be spoken to listeners on this node.
    pub fn speech_enabled(&self) -> bool {
        self.enabled() && self.cfg.speech && self.tts_enabled
    }

    /// Capability advertised in `SessionInitAck` (`None` when translation is off).
    pub fn info(&self) -> Option<TranslationInfo> {
        self.enabled().then(|| TranslationInfo {
            speech: self.speech_enabled(),
            languages: self.cfg.languages.clone(),
        })
    }

    /// Validate and normalise a listener's `SetTranslation`. A target outside the configured
    /// `languages` list and a speech request the node cannot honour are rejected.
    pub fn listener_preferences(
        &self,
        language: Option<&str>,
        spoken_language: Option<&str>,
        speech: bool,
    ) -> Result<ListenerTranslation> {
        let language = match language.map(str::trim).filter(|l| !l.is_empty()) {
            None => None,
            Some(tag) => {
                if !self.enabled() {
                    return Err(AurixError::TranslationDisabled);
                }
                let tag = normalize_language(tag).ok_or_else(|| {
                    AurixError::Validation(format!("Unknown language tag `{tag}`"))
                })?;
                if !self.cfg.languages.is_empty()
                    && !self.cfg.languages.iter().any(|l| same_language(l, &tag))
                {
                    return Err(AurixError::Validation(format!(
                        "Translation into `{tag}` is not offered"
                    )));
                }
                Some(tag)
            }
        };
        let spoken_language =
            match spoken_language.map(str::trim).filter(|l| !l.is_empty()) {
                None => None,
                Some(tag) => Some(normalize_language(tag).ok_or_else(|| {
                    AurixError::Validation(format!("Unknown language tag `{tag}`"))
                })?),
            };
        if speech && language.is_some() && !self.speech_enabled() {
            return Err(AurixError::Validation(
                "Spoken translations are not available on this node".into(),
            ));
        }
        let speech = speech && language.is_some();
        Ok(ListenerTranslation {
            language,
            spoken_language,
            speech,
        })
    }

    /// TTS voice for translations into `language` (`voices[lang]`, else the TTS default).
    pub fn voice_for(&self, language: &str, default_voice: &str) -> String {
        self.cfg
            .voices
            .iter()
            .find(|(lang, _)| same_language(lang, language))
            .map(|(_, voice)| voice.clone())
            .unwrap_or_else(|| default_voice.to_string())
    }

    /// Whether a segment in `source` (as detected, or `None`) needs translating for a listener
    /// wanting `target`.
    pub fn needs_translation(source: Option<&str>, target: &str) -> bool {
        !source.is_some_and(|s| same_language(s, target))
    }

    /// Target languages to translate one segment into, most-requested first, capped by
    /// `max_languages_per_channel`. Ties are broken alphabetically so every node picks the same
    /// set for the same demand.
    pub fn select_targets<'a>(&self, requested: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut demand: HashMap<&str, usize> = HashMap::new();
        for lang in requested {
            *demand.entry(lang).or_insert(0) += 1;
        }
        let mut ranked: Vec<(&str, usize)> = demand.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        ranked
            .into_iter()
            .take(self.cfg.max_languages_per_channel.max(1) as usize)
            .map(|(lang, _)| lang.to_string())
            .collect()
    }

    /// Translate `text` (a transcript segment) into `target`. Fails fast when translation is
    /// off, the segment is too long or every provider slot is busy; provider errors and the
    /// configured timeout surface as [`AurixError::Translation`].
    pub async fn translate(
        &self,
        text: &str,
        source: Option<&str>,
        target: &str,
    ) -> Result<Translation> {
        let provider = self
            .provider
            .as_ref()
            .ok_or(AurixError::TranslationDisabled)?;
        let text = text.trim();
        if text.is_empty() {
            return Err(AurixError::Validation("Nothing to translate".into()));
        }
        if text.chars().count() > self.cfg.max_text_chars {
            aurix_metrics::TRANSLATIONS
                .with_label_values(&["skipped"])
                .inc();
            return Err(AurixError::Translation(format!(
                "segment longer than {} characters",
                self.cfg.max_text_chars
            )));
        }
        let source = source.and_then(normalize_language);
        let key: CacheKey = (source.clone(), target.to_string(), text.to_string());
        if let Some(hit) = self.cache.lock().get(&key) {
            aurix_metrics::TRANSLATIONS
                .with_label_values(&["cached"])
                .inc();
            return Ok(hit);
        }
        let Ok(_permit) = self.permits.clone().try_acquire_owned() else {
            aurix_metrics::TRANSLATIONS
                .with_label_values(&["busy"])
                .inc();
            return Err(AurixError::Translation(
                "translation backlog full on this node".into(),
            ));
        };
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_millis(self.cfg.timeout_ms),
            provider.translate(text, source.as_deref(), target),
        )
        .await;
        aurix_metrics::TRANSLATION_LATENCY.observe(started.elapsed().as_secs_f64());
        let translation = match outcome {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                aurix_metrics::TRANSLATIONS
                    .with_label_values(&["error"])
                    .inc();
                debug!("translation into {target} failed: {e}");
                return Err(e);
            }
            Err(_) => {
                aurix_metrics::TRANSLATIONS
                    .with_label_values(&["error"])
                    .inc();
                return Err(AurixError::Translation(format!(
                    "provider did not answer within {} ms",
                    self.cfg.timeout_ms
                )));
            }
        };
        aurix_metrics::TRANSLATIONS.with_label_values(&["ok"]).inc();
        self.cache
            .lock()
            .insert(key, translation.clone(), self.cfg.cache_entries);
        Ok(translation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        calls: AtomicUsize,
        delay: Duration,
        fail: bool,
    }

    #[async_trait]
    impl MtProvider for Upper {
        async fn translate(
            &self,
            text: &str,
            source: Option<&str>,
            target: &str,
        ) -> Result<Translation> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            if self.fail {
                return Err(AurixError::Translation("boom".into()));
            }
            Ok(Translation {
                text: format!("{}:{}", target, text.to_uppercase()),
                source_language: source.map(str::to_string).or(Some("xx".into())),
            })
        }

        fn provider_name(&self) -> &str {
            "upper"
        }
    }

    fn service(cfg: TranslationConfig, delay: Duration, fail: bool) -> Arc<TranslationService> {
        let provider = Arc::new(Upper {
            calls: AtomicUsize::new(0),
            delay,
            fail,
        });
        Arc::new(TranslationService::with_provider(cfg, true, Some(provider)))
    }

    fn cfg() -> TranslationConfig {
        TranslationConfig {
            enabled: true,
            endpoint: Some("http://mt".into()),
            languages: vec!["en".into(), "ru".into(), "de".into()],
            max_languages_per_channel: 2,
            max_concurrent_requests: 1,
            timeout_ms: 500,
            max_text_chars: 20,
            ..TranslationConfig::default()
        }
    }

    #[test]
    fn listener_preferences_are_normalised_and_checked_against_the_offer() {
        let svc = service(cfg(), Duration::ZERO, false);
        let p = svc
            .listener_preferences(Some(" RU_ru "), Some("English"), true)
            .unwrap();
        assert_eq!(p.language.as_deref(), Some("ru-ru"));
        assert_eq!(p.spoken_language.as_deref(), Some("en"));
        assert!(p.speech);
        assert!(svc.listener_preferences(Some("fr"), None, false).is_err());
        assert!(svc
            .listener_preferences(Some("zz-!!"), None, false)
            .is_err());
        // No target: speech is meaningless and dropped, spoken language still recorded.
        let p = svc.listener_preferences(None, Some("de"), true).unwrap();
        assert_eq!(
            p,
            ListenerTranslation {
                language: None,
                spoken_language: Some("de".into()),
                speech: false
            }
        );

        let off = TranslationService::with_provider(TranslationConfig::default(), true, None);
        assert!(matches!(
            off.listener_preferences(Some("en"), None, false),
            Err(AurixError::TranslationDisabled)
        ));
        assert!(off.listener_preferences(None, None, false).is_ok());
        assert!(off.info().is_none());

        let no_tts = TranslationService::with_provider(
            cfg(),
            false,
            Some(Arc::new(Upper {
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
                fail: false,
            })),
        );
        assert!(no_tts.listener_preferences(Some("en"), None, true).is_err());
        assert!(!no_tts.info().unwrap().speech);
    }

    #[test]
    fn targets_are_ranked_by_demand_and_capped() {
        let svc = service(cfg(), Duration::ZERO, false);
        let t = svc.select_targets(["de", "ru", "ru", "en", "de", "fr"]);
        assert_eq!(t, vec!["de".to_string(), "ru".to_string()]);
        assert!(TranslationService::needs_translation(Some("en-US"), "ru"));
        assert!(!TranslationService::needs_translation(Some("en-US"), "en"));
        assert!(TranslationService::needs_translation(None, "en"));
    }

    #[test]
    fn voices_fall_back_to_the_default() {
        let mut c = cfg();
        c.voices.insert("ru".into(), "irina".into());
        let svc = service(c, Duration::ZERO, false);
        assert_eq!(svc.voice_for("ru-RU", "alloy"), "irina");
        assert_eq!(svc.voice_for("de", "alloy"), "alloy");
    }

    #[tokio::test]
    async fn translations_are_cached_bounded_and_time_limited() {
        let svc = service(cfg(), Duration::ZERO, false);
        let a = svc.translate("hello", Some("en"), "ru").await.unwrap();
        assert_eq!(a.text, "ru:HELLO");
        let b = svc.translate("hello", Some("EN"), "ru").await.unwrap();
        assert_eq!(a, b);
        let c = svc.translate("hello", None, "ru").await.unwrap();
        assert_eq!(c.source_language.as_deref(), Some("xx"));
        assert!(svc
            .translate("this segment is far too long", Some("en"), "ru")
            .await
            .is_err());

        let slow = service(cfg(), Duration::from_millis(200), false);
        let s1 = slow.clone();
        let first = tokio::spawn(async move { s1.translate("one", Some("en"), "ru").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let busy = slow.translate("two", Some("en"), "ru").await;
        assert!(matches!(busy, Err(AurixError::Translation(m)) if m.contains("backlog")));
        assert!(first.await.unwrap().is_ok());

        let mut c = cfg();
        c.timeout_ms = 50;
        let stalled = service(c, Duration::from_secs(5), false);
        let err = stalled
            .translate("one", Some("en"), "ru")
            .await
            .unwrap_err();
        assert!(matches!(err, AurixError::Translation(m) if m.contains("within")));

        let failing = service(cfg(), Duration::ZERO, true);
        assert!(failing.translate("one", Some("en"), "ru").await.is_err());

        let off = TranslationService::with_provider(TranslationConfig::default(), true, None);
        assert!(matches!(
            off.translate("x", None, "en").await,
            Err(AurixError::TranslationDisabled)
        ));
    }

    #[test]
    fn cache_evicts_oldest() {
        let mut cache = Cache::default();
        let t = |s: &str| Translation {
            text: s.into(),
            source_language: None,
        };
        for i in 0..5 {
            cache.insert((None, "en".into(), i.to_string()), t("x"), 3);
        }
        assert_eq!(cache.entries.len(), 3);
        assert!(cache.get(&(None, "en".into(), "0".into())).is_none());
        assert!(cache.get(&(None, "en".into(), "4".into())).is_some());
        cache.insert((None, "en".into(), "z".into()), t("x"), 0);
        assert!(cache.get(&(None, "en".into(), "z".into())).is_none());
    }
}
