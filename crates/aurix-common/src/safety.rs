//! Content-safety primitives shared by the voice (transcript) and text (chat) pipelines:
//! the toxicity classifier contract with HTTP adapters, an obfuscation-resistant lexicon
//! filter, and the decaying per-user risk score.

use crate::config::{
    LexiconAction, LexiconEntryConfig, SafetyClassifierConfig, SafetyClassifierFormat,
    SafetyConfig, SafetyTrigger,
};
use crate::tts_stt::{http_client, read_body_limited, trim_endpoint};
use crate::{AurixError, Result};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::Range;
use std::time::Duration;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// Where a piece of content came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetySource {
    Voice,
    Text,
}

impl SafetySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Text => "text",
        }
    }
}

impl std::fmt::Display for SafetySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Text handed to a classifier. `context` holds preceding messages (oldest first) when the
/// adapter supports conversational context.
#[derive(Debug, Clone)]
pub struct ClassifyRequest<'a> {
    pub text: &'a str,
    pub language: Option<&'a str>,
    pub context: &'a [String],
    pub source: SafetySource,
}

/// Classifier verdict. Scores are `0..=1`; `score` is the provider's overall toxicity (the
/// highest category when the provider has no overall score).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Classification {
    pub score: f32,
    pub categories: BTreeMap<String, f32>,
    /// Categories the provider itself flagged (OpenAI `categories: {x: true}`) or free-form
    /// labels from an Aurix-format classifier.
    pub labels: Vec<String>,
    pub flagged: bool,
    pub provider: String,
}

impl Classification {
    /// Score restricted to `categories` (empty = the provider's overall score, or the highest
    /// category if there is none).
    pub fn effective_score(&self, categories: &[String]) -> f32 {
        let clamp = |v: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        if categories.is_empty() {
            let max_category = self.categories.values().copied().fold(0.0f32, f32::max);
            return clamp(self.score.max(max_category));
        }
        categories
            .iter()
            .filter_map(|c| self.categories.get(c).copied())
            .fold(0.0f32, |acc, v| acc.max(clamp(v)))
    }

    /// Categories at or above `threshold`, highest first.
    pub fn categories_at_or_above(&self, threshold: f32) -> Vec<String> {
        let mut hits: Vec<(&String, f32)> = self
            .categories
            .iter()
            .filter(|(_, v)| **v >= threshold)
            .map(|(k, v)| (k, *v))
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        hits.into_iter().map(|(k, _)| k.clone()).collect()
    }
}

/// Toxicity classifier for transcripts and chat text.
#[async_trait]
pub trait TextClassifier: Send + Sync {
    async fn classify(&self, req: ClassifyRequest<'_>) -> Result<Classification>;
    fn classifier_name(&self) -> &str;
}

const MAX_CLASSIFIER_RESPONSE_BYTES: usize = 1024 * 1024;
/// Longest text sent in one classifier call; longer inputs are truncated on a char boundary.
pub const MAX_CLASSIFIER_INPUT_CHARS: usize = 4_000;

/// HTTP adapter speaking either the OpenAI moderations contract or Aurix's minimal one.
pub struct HttpTextClassifier {
    endpoint: String,
    format: SafetyClassifierFormat,
    api_key: Option<String>,
    model: Option<String>,
    client: reqwest::Client,
    name: String,
}

impl HttpTextClassifier {
    /// `None` when no endpoint is configured.
    pub fn from_config(cfg: &SafetyClassifierConfig) -> Option<Self> {
        let endpoint = cfg.endpoint.as_deref()?;
        Some(Self::new(
            endpoint,
            cfg.format,
            cfg.api_key.clone(),
            cfg.model.clone(),
            Duration::from_millis(cfg.timeout_ms),
        ))
    }

    pub fn new(
        endpoint: &str,
        format: SafetyClassifierFormat,
        api_key: Option<String>,
        model: Option<String>,
        timeout: Duration,
    ) -> Self {
        let name = match format {
            SafetyClassifierFormat::OpenaiModeration => "openai_moderation",
            SafetyClassifierFormat::Aurix => "aurix",
        };
        Self {
            endpoint: trim_endpoint(endpoint),
            format,
            api_key: api_key.filter(|k| !k.trim().is_empty()),
            model: model.filter(|m| !m.trim().is_empty()),
            client: http_client(timeout),
            name: name.to_string(),
        }
    }

    fn request_body(&self, req: &ClassifyRequest<'_>) -> serde_json::Value {
        let text = truncate_chars(req.text, MAX_CLASSIFIER_INPUT_CHARS);
        match self.format {
            SafetyClassifierFormat::OpenaiModeration => {
                let mut body = serde_json::json!({ "input": text });
                if let Some(model) = &self.model {
                    body["model"] = serde_json::Value::String(model.clone());
                }
                body
            }
            SafetyClassifierFormat::Aurix => {
                let mut body = serde_json::json!({
                    "text": text,
                    "source": req.source.as_str(),
                });
                if let Some(language) = req.language {
                    body["language"] = serde_json::Value::String(language.to_string());
                }
                if !req.context.is_empty() {
                    body["context"] = serde_json::json!(req.context);
                }
                if let Some(model) = &self.model {
                    body["model"] = serde_json::Value::String(model.clone());
                }
                body
            }
        }
    }

    fn parse_response(&self, body: &serde_json::Value) -> Result<Classification> {
        match self.format {
            SafetyClassifierFormat::OpenaiModeration => {
                let result = body["results"]
                    .as_array()
                    .and_then(|r| r.first())
                    .ok_or_else(|| AurixError::Safety("response has no results[0]".into()))?;
                let categories = score_map(&result["category_scores"]);
                let mut labels: Vec<String> = result["categories"]
                    .as_object()
                    .map(|obj| {
                        obj.iter()
                            .filter(|(_, v)| v.as_bool() == Some(true))
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                labels.sort();
                let score = categories.values().copied().fold(0.0f32, f32::max);
                Ok(Classification {
                    score,
                    categories,
                    labels,
                    flagged: result["flagged"].as_bool().unwrap_or(false),
                    provider: self.name.clone(),
                })
            }
            SafetyClassifierFormat::Aurix => {
                let categories = score_map(&body["categories"]);
                let max_category = categories.values().copied().fold(0.0f32, f32::max);
                let score = match body["score"].as_f64() {
                    Some(v) => v as f32,
                    None if body["categories"].is_object() => max_category,
                    None => {
                        return Err(AurixError::Safety(
                            "response has neither score nor categories".into(),
                        ))
                    }
                };
                if !score.is_finite() || !(0.0..=1.0).contains(&score) {
                    return Err(AurixError::Safety(format!(
                        "score {score} is outside 0..=1"
                    )));
                }
                let labels = body["labels"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Classification {
                    score,
                    categories,
                    labels,
                    flagged: body["flagged"].as_bool().unwrap_or(false),
                    provider: self.name.clone(),
                })
            }
        }
    }
}

fn score_map(value: &serde_json::Value) -> BTreeMap<String, f32> {
    value
        .as_object()
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    v.as_f64()
                        .filter(|f| f.is_finite())
                        .map(|f| (k.clone(), (f as f32).clamp(0.0, 1.0)))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

#[async_trait]
impl TextClassifier for HttpTextClassifier {
    async fn classify(&self, req: ClassifyRequest<'_>) -> Result<Classification> {
        let mut request = self
            .client
            .post(&self.endpoint)
            .json(&self.request_body(&req));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AurixError::Safety(format!("request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = read_body_limited(response, 4096).await.unwrap_or_default();
            return Err(AurixError::Safety(format!(
                "classifier returned {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let body = read_body_limited(response, MAX_CLASSIFIER_RESPONSE_BYTES)
            .await
            .map_err(|e| AurixError::Safety(e.to_string()))?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AurixError::Safety(format!("parse error: {e}")))?;
        self.parse_response(&body)
    }

    fn classifier_name(&self) -> &str {
        &self.name
    }
}

// ── Normalization ──

/// Text folded for matching: lower-cased, accents/zero-width characters stripped, common
/// leet-speak and Latin-lookalike Cyrillic letters mapped, repeated letters collapsed,
/// spaced-out letters (`s.h.i.t`, `f u c k`) re-joined, punctuation turned into word breaks.
/// `spans[i]` is the byte range in the original text that produced `text`'s `i`-th char.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub text: String,
    pub spans: Vec<Range<usize>>,
}

impl Normalized {
    /// Byte range in the original text covered by normalized chars `range`.
    pub fn original_range(&self, range: Range<usize>) -> Range<usize> {
        if range.is_empty() || range.end > self.spans.len() {
            return 0..0;
        }
        self.spans[range.start].start..self.spans[range.end - 1].end
    }
}

fn is_leet_symbol(c: char) -> bool {
    matches!(c, '@' | '$' | '!' | '|' | '¡' | '€' | '^' | '§')
}

/// Symbols that stand in for letters when attached to a word (`sh!t`, `a$$`, `$hit`, `@ss`).
/// `!`-like symbols only count strictly inside a word so `wow!` keeps its punctuation;
/// `left_alnum`/`right_alnum` say whether the nearest non-symbol neighbour on each side is a
/// letter or digit.
fn leet_symbol(c: char, left_alnum: bool, right_alnum: bool) -> Option<char> {
    let inside = left_alnum && right_alnum;
    let attached = left_alnum || right_alnum;
    match c {
        '@' if attached => Some('a'),
        '$' | '§' if attached => Some('s'),
        '€' if attached => Some('e'),
        '^' if attached => Some('a'),
        '!' | '|' | '¡' if inside => Some('i'),
        _ => None,
    }
}

fn fold_char(c: char) -> Option<char> {
    // Zero-width and formatting characters used to split words invisibly.
    if matches!(
        c,
        '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}' | '\u{034F}' | '\u{180E}'
    ) {
        return None;
    }
    Some(match c {
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '4' => 'a',
        '5' => 's',
        '7' => 't',
        '8' => 'b',
        '9' => 'g',
        // Cyrillic letters that render like Latin ones.
        'а' => 'a',
        'е' | 'ё' => 'e',
        'о' => 'o',
        'р' => 'p',
        'с' => 'c',
        'у' => 'y',
        'х' => 'x',
        'к' => 'k',
        'м' => 'm',
        'т' => 't',
        'в' => 'b',
        'н' => 'h',
        'і' => 'i',
        'ј' => 'j',
        'ѕ' => 's',
        'ԁ' => 'd',
        'ɡ' => 'g',
        'ß' => 's',
        'ł' => 'l',
        'ø' => 'o',
        'æ' => 'a',
        'œ' => 'o',
        'đ' => 'd',
        'ı' => 'i',
        other => other,
    })
}

fn is_joiner(c: char) -> bool {
    matches!(
        c,
        ' ' | '.' | '-' | '_' | '*' | '·' | '•' | '~' | '\'' | '`' | ',' | '/' | '\\'
    )
}

/// See [`Normalized`].
pub fn normalize_text(input: &str) -> Normalized {
    // Pass 1: per original char → folded chars (lowercase, decomposed, leet/homoglyph mapped),
    // dropping combining marks and zero-width characters. Non-alphanumerics become ' '.
    #[derive(Clone, Copy)]
    struct Unit {
        ch: char,
        span_start: usize,
        span_end: usize,
    }
    let mut units: Vec<Unit> = Vec::with_capacity(input.len());
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    for (i, &(offset, ch)) in chars.iter().enumerate() {
        let end = offset + ch.len_utf8();
        if is_leet_symbol(ch) {
            let left_alnum = chars[..i]
                .iter()
                .rev()
                .find(|(_, c)| !is_leet_symbol(*c))
                .is_some_and(|(_, c)| c.is_alphanumeric());
            let right_alnum = chars[i + 1..]
                .iter()
                .find(|(_, c)| !is_leet_symbol(*c))
                .is_some_and(|(_, c)| c.is_alphanumeric());
            if let Some(letter) = leet_symbol(ch, left_alnum, right_alnum) {
                units.push(Unit {
                    ch: letter,
                    span_start: offset,
                    span_end: end,
                });
                continue;
            }
        }
        // Keep the raw punctuation category to decide word-joining before mapping symbols.
        let raw_is_joiner = is_joiner(ch);
        for lowered in ch.to_lowercase() {
            for decomposed in lowered.nfkd() {
                if is_combining_mark(decomposed) {
                    continue;
                }
                let Some(folded) = fold_char(decomposed) else {
                    continue;
                };
                let ch = if folded.is_alphanumeric() {
                    folded
                } else if raw_is_joiner {
                    // Marker for "separator that may join spaced-out letters".
                    '\u{1}'
                } else {
                    ' '
                };
                units.push(Unit {
                    ch,
                    span_start: offset,
                    span_end: end,
                });
            }
        }
    }

    // Pass 2: tokens of alphanumerics with the separator that preceded them.
    struct Token {
        chars: Vec<Unit>,
        joiner_before: bool,
    }
    let mut tokens: Vec<Token> = Vec::new();
    let mut pending_joiner = false;
    let mut pending_break = false;
    for u in units {
        if u.ch.is_alphanumeric() {
            let starts_new = tokens.is_empty() || pending_joiner || pending_break;
            if starts_new {
                tokens.push(Token {
                    chars: Vec::new(),
                    joiner_before: pending_joiner && !pending_break,
                });
            }
            tokens.last_mut().expect("token pushed").chars.push(u);
            pending_joiner = false;
            pending_break = false;
        } else if u.ch == '\u{1}' {
            // A single joiner may glue spaced-out letters; two in a row is a hard break.
            if pending_joiner {
                pending_break = true;
            }
            pending_joiner = true;
        } else {
            pending_break = true;
            pending_joiner = false;
        }
    }

    // Pass 3: re-join runs of ≥3 single-char tokens linked by joiners ("s h i t", "f.u.c.k").
    let mut merged: Vec<Vec<Unit>> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut j = i;
        while j + 1 < tokens.len()
            && tokens[j].chars.len() == 1
            && tokens[j + 1].chars.len() == 1
            && tokens[j + 1].joiner_before
        {
            j += 1;
        }
        if j - i + 1 >= 3 {
            let mut word = Vec::new();
            for t in &tokens[i..=j] {
                word.extend_from_slice(&t.chars);
            }
            merged.push(word);
            i = j + 1;
        } else {
            merged.push(tokens[i].chars.clone());
            i += 1;
        }
    }

    // Pass 4: collapse repeated letters within a word and emit space-separated words.
    let mut text = String::with_capacity(input.len());
    let mut spans = Vec::with_capacity(input.len());
    for (idx, word) in merged.iter().enumerate() {
        if idx > 0 {
            let prev_end = spans.last().map(|s: &Range<usize>| s.end).unwrap_or(0);
            text.push(' ');
            spans.push(prev_end..prev_end);
        }
        let mut last: Option<char> = None;
        for u in word {
            if last == Some(u.ch) {
                if let Some(span) = spans.last_mut() {
                    span.end = span.end.max(u.span_end);
                }
                continue;
            }
            text.push(u.ch);
            spans.push(u.span_start..u.span_end);
            last = Some(u.ch);
        }
    }
    Normalized { text, spans }
}

// ── Lexicon ──

#[derive(Debug, Clone)]
struct LexiconRule {
    entry: LexiconEntryConfig,
    normalized: String,
}

/// One lexicon hit in the original text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LexiconMatch {
    pub pattern: String,
    pub category: String,
    pub severity: f32,
    pub action: LexiconAction,
    /// Byte range in the original text.
    pub start: usize,
    pub end: usize,
}

/// Outcome of running the lexicon over a text.
#[derive(Debug, Clone, PartialEq)]
pub struct LexiconVerdict {
    /// Strongest action among the matches (`Block` > `Mask` > `Flag`); `None` when clean.
    pub action: Option<LexiconAction>,
    /// The text with masked matches replaced (only when `action` is `Mask`).
    pub masked: Option<String>,
    pub matches: Vec<LexiconMatch>,
    /// Highest severity among the matches.
    pub severity: f32,
}

impl LexiconVerdict {
    pub fn is_clean(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn categories(&self) -> Vec<String> {
        let mut cats: Vec<String> = self.matches.iter().map(|m| m.category.clone()).collect();
        cats.sort();
        cats.dedup();
        cats
    }
}

/// Dictionary of patterns matched against normalized text.
pub struct Lexicon {
    automaton: AhoCorasick,
    rules: Vec<LexiconRule>,
}

impl std::fmt::Debug for Lexicon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lexicon")
            .field("rules", &self.rules.len())
            .finish()
    }
}

#[derive(Deserialize)]
struct LexiconFile {
    #[serde(default)]
    entries: Vec<LexiconEntryConfig>,
}

impl Lexicon {
    /// Loads `path` (TOML with `[[entries]]`) and merges `inline`. Errors on an unreadable file
    /// or an entry whose pattern normalizes to nothing.
    pub fn load(path: Option<&str>, inline: &[LexiconEntryConfig]) -> Result<Self> {
        let mut entries: Vec<LexiconEntryConfig> = Vec::new();
        if let Some(path) = path {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                AurixError::InvalidConfiguration(format!("cannot read lexicon {path}: {e}"))
            })?;
            let file: LexiconFile = toml::from_str(&raw).map_err(|e| {
                AurixError::InvalidConfiguration(format!("lexicon {path} is not valid: {e}"))
            })?;
            entries.extend(file.entries);
        }
        entries.extend_from_slice(inline);
        Self::from_entries(entries)
    }

    pub fn from_entries(entries: Vec<LexiconEntryConfig>) -> Result<Self> {
        let mut rules = Vec::with_capacity(entries.len());
        for entry in entries {
            let normalized = normalize_text(&entry.pattern).text;
            if normalized.trim().is_empty() {
                return Err(AurixError::InvalidConfiguration(format!(
                    "lexicon pattern {:?} normalizes to nothing",
                    entry.pattern
                )));
            }
            if !(0.0..=10.0).contains(&entry.severity) || !entry.severity.is_finite() {
                return Err(AurixError::InvalidConfiguration(format!(
                    "lexicon pattern {:?}: severity must be within 0..=10",
                    entry.pattern
                )));
            }
            rules.push(LexiconRule { entry, normalized });
        }
        let automaton = AhoCorasickBuilder::new()
            .match_kind(MatchKind::Standard)
            .build(rules.iter().map(|r| r.normalized.as_bytes()))
            .map_err(|e| AurixError::InvalidConfiguration(format!("lexicon build failed: {e}")))?;
        Ok(Self { automaton, rules })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Scans `text`; `mask_char` fills masked spans (one per original char).
    pub fn apply(&self, text: &str, mask_char: char) -> LexiconVerdict {
        let clean = LexiconVerdict {
            action: None,
            masked: None,
            matches: Vec::new(),
            severity: 0.0,
        };
        if self.rules.is_empty() || text.is_empty() {
            return clean;
        }
        let normalized = normalize_text(text);
        let haystack = normalized.text.as_bytes();
        let mut matches: Vec<LexiconMatch> = Vec::new();
        for m in self.automaton.find_overlapping_iter(haystack) {
            let rule = &self.rules[m.pattern().as_usize()];
            if !rule.entry.substring {
                let before_ok = m.start() == 0 || haystack[m.start() - 1] == b' ';
                let after_ok = m.end() == haystack.len() || haystack[m.end()] == b' ';
                if !before_ok || !after_ok {
                    continue;
                }
            }
            // Byte offsets in the normalized string → char indices → original byte range.
            let start_char = normalized.text[..m.start()].chars().count();
            let end_char = start_char + normalized.text[m.start()..m.end()].chars().count();
            let range = normalized.original_range(start_char..end_char);
            if range.is_empty() {
                continue;
            }
            matches.push(LexiconMatch {
                pattern: rule.entry.pattern.clone(),
                category: rule.entry.category.clone(),
                severity: rule.entry.severity,
                action: rule.entry.action,
                start: range.start,
                end: range.end,
            });
        }
        if matches.is_empty() {
            return clean;
        }
        matches.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        matches.dedup();
        let action = matches
            .iter()
            .map(|m| m.action)
            .max_by_key(|a| match a {
                LexiconAction::Flag => 0,
                LexiconAction::Mask => 1,
                LexiconAction::Block => 2,
            })
            .expect("non-empty");
        let severity = matches.iter().map(|m| m.severity).fold(0.0f32, f32::max);
        let masked = (action == LexiconAction::Mask).then(|| {
            let mut out = String::with_capacity(text.len());
            let mut cursor = 0;
            for m in matches.iter().filter(|m| m.action == LexiconAction::Mask) {
                if m.start < cursor {
                    // Overlaps a span already masked; extend if it reaches further.
                    if m.end > cursor {
                        out.extend(text[cursor..m.end].chars().map(|_| mask_char));
                        cursor = m.end;
                    }
                    continue;
                }
                out.push_str(&text[cursor..m.start]);
                out.extend(text[m.start..m.end].chars().map(|_| mask_char));
                cursor = m.end;
            }
            out.push_str(&text[cursor..]);
            out
        });
        LexiconVerdict {
            action: Some(action),
            masked,
            matches,
            severity,
        }
    }
}

// ── Risk ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    None,
    Low,
    Elevated,
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Elevated => "elevated",
            Self::High => "high",
        }
    }
}

/// Sum of incident severities, each halved every `half_life_secs` since it happened.
/// Incidents in the future count at full weight.
pub fn decayed_risk<I>(incidents: I, now: DateTime<Utc>, half_life_secs: u64) -> f32
where
    I: IntoIterator<Item = (f32, DateTime<Utc>)>,
{
    let half_life = half_life_secs.max(1) as f64;
    incidents
        .into_iter()
        .map(|(severity, at)| {
            let age = (now - at).num_milliseconds().max(0) as f64 / 1000.0;
            f64::from(severity.max(0.0)) * 0.5f64.powf(age / half_life)
        })
        .sum::<f64>() as f32
}

pub fn risk_level(score: f32, cfg: &SafetyConfig) -> RiskLevel {
    if score >= cfg.risk_high {
        RiskLevel::High
    } else if score >= cfg.risk_elevated {
        RiskLevel::Elevated
    } else if score > 0.0 {
        RiskLevel::Low
    } else {
        RiskLevel::None
    }
}

/// Whether an automatic action configured with `trigger` fires for an incident that left the
/// user at `level`.
pub fn trigger_fires(trigger: SafetyTrigger, level: RiskLevel) -> bool {
    match trigger {
        SafetyTrigger::Never => false,
        SafetyTrigger::Incident => true,
        SafetyTrigger::Elevated => level >= RiskLevel::Elevated,
        SafetyTrigger::High => level >= RiskLevel::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn entry(pattern: &str, action: LexiconAction) -> LexiconEntryConfig {
        LexiconEntryConfig {
            pattern: pattern.into(),
            action,
            severity: 0.5,
            category: "profanity".into(),
            substring: false,
        }
    }

    #[test]
    fn normalization_folds_obfuscation() {
        assert_eq!(normalize_text("Hello, World!").text, "helo world");
        assert_eq!(normalize_text("Sh1t").text, "shit");
        assert_eq!(normalize_text("s.h.i.t happens").text, "shit hapens");
        assert_eq!(normalize_text("f u c k").text, "fuck");
        assert_eq!(normalize_text("shiiiit").text, "shit");
        assert_eq!(normalize_text("idi\u{200B}ot").text, "idiot");
        assert_eq!(normalize_text("idiоt").text, "idiot"); // Cyrillic о
        assert_eq!(normalize_text("crème brûlée").text, "creme brule");
        assert_eq!(normalize_text("@ss").text, "as");
        assert_eq!(normalize_text("sh!t a$$ $hit").text, "shit as shit");
        assert_eq!(normalize_text("wow! really?").text, "wow realy");
        assert_eq!(normalize_text("a b").text, "a b");
        assert_eq!(normalize_text("end.Start").text, "end start");
        assert_eq!(normalize_text("").text, "");
        assert_eq!(normalize_text("...").text, "");
    }

    #[test]
    fn normalization_spans_map_back_to_original() {
        let n = normalize_text("You Sh1t!");
        assert_eq!(n.text, "you shit");
        let start = n.text.find("shit").unwrap();
        let range = n.original_range(start..start + 4);
        assert_eq!(&"You Sh1t!"[range], "Sh1t");

        let n = normalize_text("f.u.c.k off");
        let range = n.original_range(0..4);
        assert_eq!(&"f.u.c.k off"[range], "f.u.c.k");
    }

    #[test]
    fn lexicon_masks_blocks_and_flags() {
        let lex = Lexicon::from_entries(vec![
            entry("shit", LexiconAction::Mask),
            entry("kill yourself", LexiconAction::Block),
            LexiconEntryConfig {
                pattern: "noob".into(),
                action: LexiconAction::Flag,
                severity: 0.1,
                category: "mild".into(),
                substring: false,
            },
        ])
        .unwrap();

        let v = lex.apply("this is Sh1t, really", '*');
        assert_eq!(v.action, Some(LexiconAction::Mask));
        assert_eq!(v.masked.as_deref(), Some("this is ****, really"));
        assert_eq!(v.matches.len(), 1);
        assert_eq!(v.matches[0].pattern, "shit");

        let v = lex.apply("go k.i.l.l  y-o-u-r-s-e-l-f now", '*');
        assert_eq!(v.action, Some(LexiconAction::Block));
        assert!(v.masked.is_none());

        let v = lex.apply("hi noob", '*');
        assert_eq!(v.action, Some(LexiconAction::Flag));
        assert!(v.masked.is_none());
        assert_eq!(v.categories(), vec!["mild".to_string()]);

        let v = lex.apply("shitake mushrooms", '*');
        assert!(v.is_clean(), "word-boundary match must not hit shitake");
        assert!(lex.apply("", '*').is_clean());
    }

    #[test]
    fn lexicon_substring_and_overlaps() {
        let lex = Lexicon::from_entries(vec![
            LexiconEntryConfig {
                substring: true,
                ..entry("fuck", LexiconAction::Mask)
            },
            entry("motherfucker", LexiconAction::Mask),
        ])
        .unwrap();
        let v = lex.apply("you MotherFucker, fucking hell", '#');
        assert_eq!(v.action, Some(LexiconAction::Mask));
        assert_eq!(v.masked.as_deref(), Some("you ############, ####ing hell"));
    }

    #[test]
    fn lexicon_rejects_empty_patterns() {
        assert!(Lexicon::from_entries(vec![entry("...", LexiconAction::Mask)]).is_err());
        let lex = Lexicon::from_entries(Vec::new()).unwrap();
        assert!(lex.is_empty());
        assert!(lex.apply("anything", '*').is_clean());
    }

    #[test]
    fn classification_effective_score() {
        let mut c = Classification {
            score: 0.2,
            ..Default::default()
        };
        c.categories.insert("harassment".into(), 0.9);
        c.categories.insert("sexual".into(), 0.1);
        assert_eq!(c.effective_score(&[]), 0.9);
        assert_eq!(c.effective_score(&["sexual".into()]), 0.1);
        assert_eq!(c.effective_score(&["missing".into()]), 0.0);
        assert_eq!(
            c.categories_at_or_above(0.5),
            vec!["harassment".to_string()]
        );
    }

    #[test]
    fn parses_openai_and_aurix_responses() {
        let openai = HttpTextClassifier::new(
            "http://localhost/v1/moderations",
            SafetyClassifierFormat::OpenaiModeration,
            None,
            None,
            Duration::from_secs(1),
        );
        let body = serde_json::json!({
            "results": [{
                "flagged": true,
                "categories": {"harassment": true, "hate": false},
                "category_scores": {"harassment": 0.93, "hate": 0.02}
            }]
        });
        let c = openai.parse_response(&body).unwrap();
        assert!(c.flagged);
        assert_eq!(c.labels, vec!["harassment".to_string()]);
        assert!((c.score - 0.93).abs() < 1e-6);
        assert!(openai.parse_response(&serde_json::json!({})).is_err());

        let aurix = HttpTextClassifier::new(
            "http://localhost/classify/",
            SafetyClassifierFormat::Aurix,
            Some("k".into()),
            Some("m".into()),
            Duration::from_secs(1),
        );
        let c = aurix
            .parse_response(&serde_json::json!({"score": 0.4, "categories": {"insult": 0.4}, "labels": ["insult"]}))
            .unwrap();
        assert_eq!(c.score, 0.4);
        assert_eq!(c.labels, vec!["insult".to_string()]);
        let c = aurix
            .parse_response(&serde_json::json!({"categories": {"a": 0.2, "b": 0.7}}))
            .unwrap();
        assert!((c.score - 0.7).abs() < 1e-6);
        assert!(aurix
            .parse_response(&serde_json::json!({"score": 1.5}))
            .is_err());
        assert!(aurix
            .parse_response(&serde_json::json!({"other": 1}))
            .is_err());

        let req = ClassifyRequest {
            text: "hi",
            language: Some("en"),
            context: &["prev".to_string()],
            source: SafetySource::Text,
        };
        let body = aurix.request_body(&req);
        assert_eq!(body["text"], "hi");
        assert_eq!(body["language"], "en");
        assert_eq!(body["context"][0], "prev");
        assert_eq!(body["model"], "m");
        let body = openai.request_body(&req);
        assert_eq!(body["input"], "hi");
        assert!(body.get("model").is_none());
    }

    #[test]
    fn risk_decays_by_half_life() {
        let now = Utc::now();
        let cfg = SafetyConfig::default();
        let score = decayed_risk(
            [
                (1.0, now),
                (1.0, now - ChronoDuration::seconds(900)),
                (1.0, now - ChronoDuration::seconds(1800)),
            ],
            now,
            900,
        );
        assert!((score - 1.75).abs() < 1e-3);
        assert_eq!(risk_level(0.0, &cfg), RiskLevel::None);
        assert_eq!(risk_level(0.5, &cfg), RiskLevel::Low);
        assert_eq!(risk_level(1.0, &cfg), RiskLevel::Elevated);
        assert_eq!(risk_level(2.5, &cfg), RiskLevel::High);
        assert!(trigger_fires(SafetyTrigger::Incident, RiskLevel::Low));
        assert!(!trigger_fires(SafetyTrigger::Elevated, RiskLevel::Low));
        assert!(trigger_fires(SafetyTrigger::Elevated, RiskLevel::High));
        assert!(!trigger_fires(SafetyTrigger::Never, RiskLevel::High));
        assert_eq!(decayed_risk(std::iter::empty(), now, 900), 0.0);
    }
}
