//! Stand-in for an OpenAI-compatible speech and moderation server, used by the live E2E test
//! and handy for local development when no Whisper/Piper/classifier is around:
//!
//! ```text
//! cargo run -p aurix-server --example mock_speech -- 127.0.0.1:18790
//! AURIX__STT__ENABLED=true AURIX__STT__ENDPOINT=http://127.0.0.1:18790 \
//! AURIX__TTS__ENABLED=true AURIX__TTS__ENDPOINT=http://127.0.0.1:18790 \
//! AURIX__SAFETY__ENABLED=true AURIX__SAFETY__CLASSIFIER__ENDPOINT=http://127.0.0.1:18790/v1/moderations \
//! cargo run --bin aurix-server
//! ```
//!
//! * `POST /v1/audio/transcriptions` (multipart WAV) answers `verbose_json` whose `text`
//!   describes the audio it received: `tone 440hz rms=0.35 dur=2000ms`, plus one word timing.
//! * `POST /v1/audio/speech` returns a 24 kHz mono WAV sine. The `input` text may carry
//!   directives: `[fail]` → HTTP 500, `[slow]` → 3 s delay before answering, `[dur=NNNN]` →
//!   duration in ms (default 1000), `[hz=NNN]` → frequency (default 440).
//! * `POST /v1/moderations` (OpenAI moderation shape) scores the `input` by content: text with
//!   `hate` or a `880hz` tone → `harassment` 0.95 (flagged); `rude` or a `660hz` tone →
//!   `harassment` 0.75 (flagged); `[classifier-fail]` → HTTP 500; anything else 0.01. The
//!   E2E test drives voice incidents with tones and chat incidents with those words.
//!
//! An `Authorization` header, when the server is started with a bearer token as the second
//! argument, must match or the request is refused with 401.

use aurix_common::tts_stt::{parse_wav, pcm_to_wav};
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
struct Shared {
    bearer: Option<Arc<str>>,
}

#[tokio::main]
async fn main() {
    let bind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:18790".to_string());
    let bearer = std::env::args().nth(2).map(Arc::from);
    let app = Router::new()
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/v1/audio/speech", post(speak))
        .route("/v1/moderations", post(moderate))
        .with_state(Shared { bearer });
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    eprintln!("mock speech server on http://{bind}");
    axum::serve(listener, app).await.expect("serve");
}

fn authorized(shared: &Shared, headers: &HeaderMap) -> bool {
    match &shared.bearer {
        None => true,
        Some(expected) => headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|got| got == expected.as_ref()),
    }
}

async fn transcribe(
    State(shared): State<Shared>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    if !authorized(&shared, &headers) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let mut wav = None;
    let mut fields = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            wav = field.bytes().await.ok();
        } else if let Ok(v) = field.text().await {
            fields.push(format!("{name}={v}"));
        }
    }
    let Some(wav) = wav else {
        return (StatusCode::BAD_REQUEST, "missing file").into_response();
    };
    let pcm = match parse_wav(&wav) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad wav: {e}")).into_response(),
    };
    let frames = pcm.frames().max(1);
    let rms = (pcm
        .samples
        .iter()
        .map(|s| (f64::from(*s) / 32768.0).powi(2))
        .sum::<f64>()
        / pcm.samples.len().max(1) as f64)
        .sqrt();
    let duration_ms = frames as u64 * 1000 / u64::from(pcm.sample_rate.max(1));
    let hz = dominant_hz(&pcm.samples, pcm.channels, pcm.sample_rate);
    let text = format!("tone {hz}hz rms={rms:.2} dur={duration_ms}ms");
    eprintln!(
        "stt: {} ({} Hz, {})",
        text,
        pcm.sample_rate,
        fields.join(" ")
    );
    let seconds = duration_ms as f64 / 1000.0;
    Json(serde_json::json!({
        "task": "transcribe",
        "language": "en",
        "duration": seconds,
        "text": text,
        "words": [
            {"word": "tone", "start": 0.0, "end": (seconds / 2.0), "probability": 0.99},
            {"word": format!("{hz}hz"), "start": (seconds / 2.0), "end": seconds, "probability": 0.98}
        ]
    }))
    .into_response()
}

/// Zero-crossing estimate of the fundamental — enough to tell test tones apart.
fn dominant_hz(samples: &[i16], channels: u16, sample_rate: u32) -> u32 {
    let step = channels.max(1) as usize;
    let mono: Vec<i16> = samples.iter().step_by(step).copied().collect();
    if mono.len() < 2 {
        return 0;
    }
    let crossings = mono.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count() as f64;
    let seconds = mono.len() as f64 / f64::from(sample_rate.max(1));
    (crossings / 2.0 / seconds).round() as u32
}

/// Frequency of a `tone NNNhz` transcript produced by [`transcribe`].
fn tone_hz(text: &str) -> Option<u32> {
    let end = text.find("hz")?;
    let digits = text[..end]
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .len();
    text[digits..end].parse().ok()
}

#[derive(serde::Deserialize)]
struct SpeechRequest {
    input: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    response_format: Option<String>,
}

fn directive(text: &str, key: &str) -> Option<u64> {
    let start = text.find(&format!("[{key}="))? + key.len() + 2;
    let end = start + text[start..].find(']')?;
    text[start..end].parse().ok()
}

async fn speak(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<SpeechRequest>,
) -> axum::response::Response {
    if !authorized(&shared, &headers) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    eprintln!(
        "tts: {:?} voice={:?} format={:?}",
        req.input, req.voice, req.response_format
    );
    if req.input.contains("[fail]") {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": {"message": "synthetic provider failure"}})),
        )
            .into_response();
    }
    if req.input.contains("[slow]") {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    let duration_ms = directive(&req.input, "dur")
        .unwrap_or(1000)
        .clamp(20, 120_000);
    let hz = directive(&req.input, "hz").unwrap_or(440).clamp(20, 8000) as f64;
    let sample_rate = 24_000u32;
    let n = (u64::from(sample_rate) * duration_ms / 1000) as usize;
    let samples: Vec<i16> = (0..n)
        .map(|i| {
            let t = i as f64 / f64::from(sample_rate);
            (0.5 * (2.0 * std::f64::consts::PI * hz * t).sin() * 32767.0) as i16
        })
        .collect();
    let wav = pcm_to_wav(&samples, sample_rate, 1);
    ([(axum::http::header::CONTENT_TYPE, "audio/wav")], wav).into_response()
}

#[derive(serde::Deserialize)]
struct ModerationRequest {
    input: String,
    #[serde(default)]
    model: Option<String>,
}

async fn moderate(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<ModerationRequest>,
) -> axum::response::Response {
    if !authorized(&shared, &headers) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let text = req.input.to_lowercase();
    if text.contains("[classifier-fail]") {
        eprintln!("moderation: {:?} -> 500", req.input);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": {"message": "synthetic classifier failure"}})),
        )
            .into_response();
    }
    let tone = tone_hz(&text);
    let near = |hz: u32| tone.is_some_and(|t| t.abs_diff(hz) <= 15);
    let harassment = if text.contains("hate") || near(880) {
        0.95
    } else if text.contains("rude") || near(660) {
        0.75
    } else {
        0.01
    };
    let flagged = harassment >= 0.5;
    eprintln!(
        "moderation: {:?} model={:?} -> harassment={harassment} flagged={flagged}",
        req.input, req.model
    );
    Json(serde_json::json!({
        "id": "modr-mock",
        "model": req.model.unwrap_or_else(|| "mock-moderation".into()),
        "results": [{
            "flagged": flagged,
            "categories": {"harassment": flagged, "hate": false, "self-harm": false},
            "category_scores": {"harassment": harassment, "hate": 0.01, "self-harm": 0.0}
        }]
    }))
    .into_response()
}
