//! Machine translation of transcripts: the provider abstraction, language-code handling and
//! two HTTP reference providers — a LibreTranslate-compatible server and an OpenAI-compatible
//! chat-completions endpoint (vLLM, Ollama, llama.cpp server, hosted LLM APIs).

use crate::error::{AurixError, Result};
use crate::tts_stt::{http_client, read_body_limited, trim_endpoint, HttpProviderOptions};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MAX_MT_RESPONSE_BYTES: usize = 256 * 1024;

/// One translated text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Translation {
    pub text: String,
    /// Source language as detected/confirmed by the provider (normalised code).
    pub source_language: Option<String>,
}

/// Pluggable machine-translation backend.
#[async_trait]
pub trait MtProvider: Send + Sync {
    /// Translate `text` into `target` (normalised language code). `source` is the known
    /// source language when there is one; providers detect it otherwise.
    async fn translate(
        &self,
        text: &str,
        source: Option<&str>,
        target: &str,
    ) -> Result<Translation>;

    fn provider_name(&self) -> &str;
}

/// Normalise a language tag to the form used on the wire: lower-case primary subtag
/// (2–3 letters) with optional `-` separated subtags (`en`, `pt-br`, `zh-hant`). English
/// language names as returned by Whisper (`"english"`) map to their code. `None` when the
/// value is not a language tag.
pub fn normalize_language(tag: &str) -> Option<String> {
    let tag = tag.trim();
    if tag.is_empty() || tag.len() > 35 {
        return None;
    }
    let lower = tag.to_ascii_lowercase().replace('_', "-");
    if let Some(code) = language_name_to_code(&lower) {
        return Some(code.to_string());
    }
    let mut parts = lower.split('-');
    let primary = parts.next()?;
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    for sub in parts {
        if !(1..=8).contains(&sub.len()) || !sub.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
    }
    Some(lower)
}

/// Primary subtag of a normalised tag (`pt-br` → `pt`).
pub fn primary_language(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// Two normalised tags denote the same language when their primary subtags match; a
/// listener asking for `en` does not need `en-us` speech translated.
pub fn same_language(a: &str, b: &str) -> bool {
    primary_language(a) == primary_language(b)
}

fn language_name_to_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "english" => "en",
        "russian" => "ru",
        "german" => "de",
        "french" => "fr",
        "spanish" | "castilian" => "es",
        "portuguese" => "pt",
        "italian" => "it",
        "dutch" | "flemish" => "nl",
        "polish" => "pl",
        "ukrainian" => "uk",
        "czech" => "cs",
        "slovak" => "sk",
        "turkish" => "tr",
        "swedish" => "sv",
        "norwegian" => "no",
        "danish" => "da",
        "finnish" => "fi",
        "greek" => "el",
        "hungarian" => "hu",
        "romanian" | "moldavian" | "moldovan" => "ro",
        "bulgarian" => "bg",
        "serbian" => "sr",
        "croatian" => "hr",
        "hebrew" => "he",
        "arabic" => "ar",
        "persian" | "farsi" => "fa",
        "hindi" => "hi",
        "bengali" => "bn",
        "urdu" => "ur",
        "chinese" | "mandarin" => "zh",
        "cantonese" => "yue",
        "japanese" => "ja",
        "korean" => "ko",
        "vietnamese" => "vi",
        "thai" => "th",
        "indonesian" => "id",
        "malay" => "ms",
        "filipino" | "tagalog" => "tl",
        "swahili" => "sw",
        "catalan" | "valencian" => "ca",
        "basque" => "eu",
        "galician" => "gl",
        "latvian" => "lv",
        "lithuanian" => "lt",
        "estonian" => "et",
        "slovenian" => "sl",
        "kazakh" => "kk",
        "azerbaijani" => "az",
        "georgian" => "ka",
        "armenian" => "hy",
        "tamil" => "ta",
        "telugu" => "te",
        "marathi" => "mr",
        "gujarati" => "gu",
        "kannada" => "kn",
        "malayalam" => "ml",
        "punjabi" | "panjabi" => "pa",
        "afrikaans" => "af",
        "icelandic" => "is",
        "irish" => "ga",
        "welsh" => "cy",
        "belarusian" => "be",
        "macedonian" => "mk",
        "albanian" => "sq",
        "bosnian" => "bs",
        "maltese" => "mt",
        "mongolian" => "mn",
        "nepali" => "ne",
        "sinhala" | "sinhalese" => "si",
        "khmer" => "km",
        "lao" => "lo",
        "burmese" | "myanmar" => "my",
        "amharic" => "am",
        "somali" => "so",
        "yoruba" => "yo",
        "hausa" => "ha",
        "zulu" => "zu",
        "uzbek" => "uz",
        "kyrgyz" => "ky",
        "tajik" => "tg",
        "turkmen" => "tk",
        "latin" => "la",
        _ => return None,
    })
}

/// LibreTranslate-compatible server: `POST {endpoint}/translate` with
/// `{q, source, target, format: "text"}`; `source: "auto"` asks for detection.
pub struct LibreTranslateProvider {
    endpoint: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl LibreTranslateProvider {
    pub fn new(endpoint: &str, options: HttpProviderOptions) -> Self {
        Self {
            endpoint: trim_endpoint(endpoint),
            api_key: options.api_key,
            client: http_client(options.timeout.unwrap_or(Duration::from_secs(10))),
        }
    }
}

#[async_trait]
impl MtProvider for LibreTranslateProvider {
    async fn translate(
        &self,
        text: &str,
        source: Option<&str>,
        target: &str,
    ) -> Result<Translation> {
        let mut body = serde_json::json!({
            "q": text,
            // LibreTranslate models are keyed by primary language.
            "source": source.map(primary_language).unwrap_or("auto"),
            "target": primary_language(target),
            "format": "text",
        });
        if let Some(key) = &self.api_key {
            body["api_key"] = serde_json::Value::String(key.clone());
        }
        let response = self
            .client
            .post(format!("{}/translate", self.endpoint))
            .json(&body)
            .send()
            .await
            .map_err(|e| AurixError::Translation(format!("request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = read_body_limited(response, 4096).await.unwrap_or_default();
            return Err(AurixError::Translation(format!(
                "provider returned {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let body = read_body_limited(response, MAX_MT_RESPONSE_BYTES).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AurixError::Translation(format!("parse error: {e}")))?;
        let translated = body["translatedText"]
            .as_str()
            .ok_or_else(|| AurixError::Translation("response has no translatedText".into()))?;
        let detected = body["detectedLanguage"]["language"]
            .as_str()
            .and_then(normalize_language)
            .or_else(|| source.map(str::to_string));
        Ok(Translation {
            text: translated.trim().to_string(),
            source_language: detected,
        })
    }

    fn provider_name(&self) -> &str {
        "libretranslate"
    }
}

/// OpenAI-compatible `POST {endpoint}/v1/chat/completions` driven by a fixed translation
/// prompt; the model's reply is the translation.
pub struct OpenAiChatMtProvider {
    endpoint: String,
    api_key: Option<String>,
    model: String,
    client: reqwest::Client,
}

impl OpenAiChatMtProvider {
    pub fn new(endpoint: &str, options: HttpProviderOptions) -> Self {
        Self {
            endpoint: trim_endpoint(endpoint),
            api_key: options.api_key,
            model: options.model.unwrap_or_else(|| "gpt-4o-mini".to_string()),
            client: http_client(options.timeout.unwrap_or(Duration::from_secs(10))),
        }
    }

    fn prompt(source: Option<&str>, target: &str) -> String {
        let from = source
            .map(|s| format!(" from language `{s}`"))
            .unwrap_or_default();
        format!(
            "You translate live speech transcripts of players talking in a game{from} into \
             language `{target}`. Reply with the translation only: no quotes, no notes, no \
             explanations. Keep names, numbers and game terms; preserve the tone."
        )
    }
}

#[async_trait]
impl MtProvider for OpenAiChatMtProvider {
    async fn translate(
        &self,
        text: &str,
        source: Option<&str>,
        target: &str,
    ) -> Result<Translation> {
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": Self::prompt(source, target)},
                {"role": "user", "content": text},
            ],
        });
        let mut request = self
            .client
            .post(format!("{}/v1/chat/completions", self.endpoint))
            .json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AurixError::Translation(format!("request failed: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = read_body_limited(response, 4096).await.unwrap_or_default();
            return Err(AurixError::Translation(format!(
                "provider returned {status}: {}",
                String::from_utf8_lossy(&body)
            )));
        }
        let body = read_body_limited(response, MAX_MT_RESPONSE_BYTES).await?;
        let body: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AurixError::Translation(format!("parse error: {e}")))?;
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| AurixError::Translation("response has no choices".into()))?;
        Ok(Translation {
            text: content.trim().trim_matches('"').trim().to_string(),
            source_language: source.map(str::to_string),
        })
    }

    fn provider_name(&self) -> &str {
        "openai_chat"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_tags_and_english_names() {
        assert_eq!(normalize_language("EN").as_deref(), Some("en"));
        assert_eq!(normalize_language(" pt_BR ").as_deref(), Some("pt-br"));
        assert_eq!(normalize_language("zh-Hant").as_deref(), Some("zh-hant"));
        assert_eq!(normalize_language("english").as_deref(), Some("en"));
        assert_eq!(normalize_language("Russian").as_deref(), Some("ru"));
        assert_eq!(normalize_language("yue").as_deref(), Some("yue"));
        assert_eq!(normalize_language(""), None);
        assert_eq!(normalize_language("e"), None);
        assert_eq!(normalize_language("klingon"), None);
        assert_eq!(normalize_language("en-"), None);
        assert_eq!(normalize_language("en-us!"), None);
        assert_eq!(normalize_language("1234"), None);
    }

    #[test]
    fn same_language_compares_primary_subtags() {
        assert!(same_language("en", "en-us"));
        assert!(same_language("pt-br", "pt-pt"));
        assert!(!same_language("en", "de"));
        assert_eq!(primary_language("zh-hant"), "zh");
    }

    #[test]
    fn chat_prompt_names_languages() {
        let p = OpenAiChatMtProvider::prompt(Some("ru"), "en");
        assert!(p.contains("from language `ru`"));
        assert!(p.contains("into language `en`"));
        let p = OpenAiChatMtProvider::prompt(None, "de");
        assert!(!p.contains("from language"));
    }
}
