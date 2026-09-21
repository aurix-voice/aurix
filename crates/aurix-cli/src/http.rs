//! Thin REST layer: auth headers, JSON/raw responses, error → exit-code mapping.

use std::time::Duration;

use anyhow::{anyhow, Context};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::config::Resolved;

/// Which credential a command needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// No credentials (health, ready, openapi).
    None,
    /// Application API key (`X-API-Key`).
    ApiKey,
    /// Operator JWT (`Authorization: Bearer`).
    Admin,
    /// API key if present, else admin token — for endpoints accepting both.
    Any,
}

pub struct Api {
    client: reqwest::Client,
    pub server: String,
    api_key: Option<String>,
    admin_token: Option<String>,
    bootstrap_token: Option<String>,
}

pub struct Response {
    pub status: StatusCode,
    pub content_type: String,
    pub body: Vec<u8>,
    pub request_id: Option<String>,
    pub retry_after: Option<String>,
    pub date: Option<String>,
}

impl Response {
    pub fn json(&self) -> Option<Value> {
        if self.body.is_empty() {
            return None;
        }
        serde_json::from_slice(&self.body).ok()
    }
}

/// A non-2xx response, already decoded from the `{"error":{code,message}}` envelope.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
    pub retry_after: Option<String>,
    pub body: Value,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HTTP {} {}: {}",
            self.status.as_u16(),
            self.code,
            self.message
        )?;
        if let Some(id) = &self.request_id {
            write!(f, " (request id {id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn exit_code(&self) -> i32 {
        match self.status.as_u16() {
            401 | 403 => 3,
            404 => 4,
            429 => 5,
            _ => 1,
        }
    }

    /// Machine-readable form for `--output json` (the original envelope is kept under `body`).
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "error": {
                "status": self.status.as_u16(),
                "code": self.code,
                "message": self.message,
                "request_id": self.request_id,
                "retry_after": self.retry_after,
            },
            "body": self.body,
        })
    }
}

/// Transport failure without an HTTP response.
#[derive(Debug)]
pub struct NetworkError(pub String);

impl std::fmt::Display for NetworkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "network error: {}", self.0)
    }
}

impl std::error::Error for NetworkError {}

impl Api {
    pub fn new(r: &Resolved, bootstrap_token: Option<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(r.timeout_secs))
            .user_agent(format!("aurix-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            server: r.server.clone(),
            api_key: r.api_key.clone(),
            admin_token: r.admin_token.clone(),
            bootstrap_token,
        })
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn has_admin_token(&self) -> bool {
        self.admin_token.is_some()
    }

    fn headers(&self, auth: Auth) -> anyhow::Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, */*;q=0.5"),
        );
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("aurix-cli/{}", env!("CARGO_PKG_VERSION")))?,
        );
        let bearer = |t: &str| -> anyhow::Result<HeaderValue> {
            let mut v = HeaderValue::from_str(&format!("Bearer {t}"))
                .context("admin token contains invalid header characters")?;
            v.set_sensitive(true);
            Ok(v)
        };
        match auth {
            Auth::None => {}
            Auth::ApiKey => {
                let key = self.api_key.as_deref().ok_or_else(|| anyhow!(
                    "this command needs an application API key: set AURIX_API_KEY, pass --api-key-file, or configure a profile (aurix config init)"
                ))?;
                let mut v = HeaderValue::from_str(key)
                    .context("API key contains invalid header characters")?;
                v.set_sensitive(true);
                h.insert("X-API-Key", v);
            }
            Auth::Admin => {
                let t = self.admin_token.as_deref().ok_or_else(|| anyhow!(
                    "this command needs an operator token: run `aurix admin login --save`, set AURIX_ADMIN_TOKEN, or pass --admin-token-file"
                ))?;
                h.insert(AUTHORIZATION, bearer(t)?);
            }
            Auth::Any => {
                if let Some(key) = &self.api_key {
                    let mut v = HeaderValue::from_str(key)
                        .context("API key contains invalid header characters")?;
                    v.set_sensitive(true);
                    h.insert("X-API-Key", v);
                } else if let Some(t) = &self.admin_token {
                    h.insert(AUTHORIZATION, bearer(t)?);
                } else {
                    return Err(anyhow!(
                        "no credentials: set AURIX_API_KEY / --api-key-file or log in with `aurix admin login --save`"
                    ));
                }
            }
        }
        if let Some(b) = &self.bootstrap_token {
            let mut v = HeaderValue::from_str(b)
                .context("bootstrap token contains invalid header characters")?;
            v.set_sensitive(true);
            h.insert("X-Bootstrap-Token", v);
        }
        Ok(h)
    }

    /// Performs one request. Returns `Ok` for 2xx, `ApiError` for other statuses.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        auth: Auth,
    ) -> anyhow::Result<Response> {
        let url = format!("{}{}", self.server, path);
        let mut req = self
            .client
            .request(method.clone(), &url)
            .headers(self.headers(auth)?);
        if !query.is_empty() {
            req = req.query(query);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| NetworkError(describe_reqwest(&e)))
            .with_context(|| format!("{method} {path}"))?;
        let status = resp.status();
        let hdr = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let content_type = hdr("content-type").unwrap_or_default();
        let request_id = hdr("x-request-id");
        let retry_after = hdr("retry-after");
        let date = hdr("date");
        let body = resp
            .bytes()
            .await
            .map_err(|e| NetworkError(describe_reqwest(&e)))
            .with_context(|| format!("{method} {path}: reading body"))?
            .to_vec();
        let out = Response {
            status,
            content_type,
            body,
            request_id,
            retry_after,
            date,
        };
        if status.is_success() {
            return Ok(out);
        }
        let json = out.json().unwrap_or(Value::Null);
        let (code, message) = match json.get("error") {
            Some(Value::Object(e)) => (
                e.get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            _ => (
                format!("http_{}", status.as_u16()),
                String::from_utf8_lossy(&out.body[..out.body.len().min(300)]).to_string(),
            ),
        };
        Err(ApiError {
            status,
            code,
            message,
            request_id: out.request_id,
            retry_after: out.retry_after,
            body: json,
        }
        .into())
    }

    /// JSON request; the body must be JSON (or empty → `null`).
    pub async fn json(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        auth: Auth,
    ) -> anyhow::Result<Value> {
        let resp = self.send(method.clone(), path, query, body, auth).await?;
        if resp.body.is_empty() {
            return Ok(serde_json::json!({ "ok": true, "status": resp.status.as_u16() }));
        }
        serde_json::from_slice(&resp.body).with_context(|| {
            format!(
                "{method} {path}: expected JSON, got {} ({} bytes)",
                resp.content_type,
                resp.body.len()
            )
        })
    }

    pub async fn get(
        &self,
        path: &str,
        query: &[(String, String)],
        auth: Auth,
    ) -> anyhow::Result<Value> {
        self.json(Method::GET, path, query, None, auth).await
    }

    pub async fn post(&self, path: &str, body: Value, auth: Auth) -> anyhow::Result<Value> {
        self.json(Method::POST, path, &[], Some(&body), auth).await
    }

    pub async fn delete(
        &self,
        path: &str,
        query: &[(String, String)],
        auth: Auth,
    ) -> anyhow::Result<Value> {
        self.json(Method::DELETE, path, query, None, auth).await
    }

    /// Streams `GET /v1/events` (SSE) and calls `on_event` for each message until it returns `false`.
    pub async fn sse(
        &self,
        path: &str,
        query: &[(String, String)],
        last_event_id: Option<&str>,
        auth: Auth,
        on_event: &mut dyn FnMut(SseEvent) -> bool,
    ) -> anyhow::Result<Option<String>> {
        let mut headers = self.headers(auth)?;
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if let Some(id) = last_event_id {
            headers.insert("Last-Event-ID", HeaderValue::from_str(id)?);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        let mut req = client
            .get(format!("{}{}", self.server, path))
            .headers(headers);
        if !query.is_empty() {
            req = req.query(query);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| NetworkError(describe_reqwest(&e)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();
            let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (code, message) = match json.get("error") {
                Some(Value::Object(e)) => (
                    e.get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("error")
                        .to_string(),
                    e.get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                _ => (format!("http_{}", status.as_u16()), String::new()),
            };
            return Err(ApiError {
                status,
                code,
                message,
                request_id: None,
                retry_after: None,
                body: json,
            }
            .into());
        }
        let mut resp = resp;
        let mut parser = SseParser::default();
        let mut last_id = last_event_id.map(str::to_string);
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| NetworkError(describe_reqwest(&e)))?
        {
            for ev in parser.push(&chunk) {
                if let Some(id) = &ev.id {
                    last_id = Some(id.clone());
                }
                if !on_event(ev) {
                    return Ok(last_id);
                }
            }
        }
        Ok(last_id)
    }
}

fn describe_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out".to_string();
    }
    if e.is_connect() {
        return format!("connection failed: {}", root_cause(e));
    }
    root_cause(e)
}

fn root_cause(e: &dyn std::error::Error) -> String {
    let mut cur: &dyn std::error::Error = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub id: Option<String>,
    pub data: String,
}

/// Incremental `text/event-stream` parser (comments, multi-line data, CRLF).
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line = self.buf.drain(..=nl).collect::<Vec<u8>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            if line.is_empty() {
                if !self.data.is_empty() || self.event.is_some() || self.id.is_some() {
                    out.push(SseEvent {
                        event: self.event.take().unwrap_or_else(|| "message".to_string()),
                        id: self.id.take(),
                        data: std::mem::take(&mut self.data).join("\n"),
                    });
                }
                self.event = None;
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.find(':') {
                Some(i) => (
                    &line[..i],
                    line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..]),
                ),
                None => (line.as_str(), ""),
            };
            match field {
                "event" => self.event = Some(value.to_string()),
                "data" => self.data.push(value.to_string()),
                "id" if !value.contains('\0') => self.id = Some(value.to_string()),
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_chunks_comments_and_multiline() {
        let mut p = SseParser::default();
        let mut got = p.push(b": hi\n\nevent: stream.open\ndata: {\"ok\":");
        assert!(got.is_empty());
        got.extend(p.push(b"true}\n\nid: 7\r\ndata: a\r\ndata: b\r\n\r\n"));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].event, "stream.open");
        assert_eq!(got[0].data, "{\"ok\":true}");
        assert_eq!(got[1].event, "message");
        assert_eq!(got[1].id.as_deref(), Some("7"));
        assert_eq!(got[1].data, "a\nb");
    }

    #[test]
    fn api_error_exit_codes() {
        let mk = |s: u16| ApiError {
            status: StatusCode::from_u16(s).unwrap(),
            code: String::new(),
            message: String::new(),
            request_id: None,
            retry_after: None,
            body: Value::Null,
        };
        assert_eq!(mk(401).exit_code(), 3);
        assert_eq!(mk(403).exit_code(), 3);
        assert_eq!(mk(404).exit_code(), 4);
        assert_eq!(mk(429).exit_code(), 5);
        assert_eq!(mk(500).exit_code(), 1);
    }
}
