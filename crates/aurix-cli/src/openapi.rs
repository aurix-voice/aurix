//! The REST contract embedded at build time (`api/openapi.json`) — drives `aurix api`
//! and lets `diagnose` compare the CLI's contract version with the node's.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{anyhow, bail};
use reqwest::Method;
use serde_json::Value;

const SPEC: &str = include_str!("../../../api/openapi.json");

#[derive(Debug, Clone)]
pub struct Operation {
    pub id: String,
    pub method: Method,
    /// Template such as `/v1/channels/{channel_id}`.
    pub path: String,
    pub summary: String,
    pub path_params: Vec<String>,
    pub query_params: Vec<Param>,
    /// Component name of the JSON request body, if any.
    pub request_schema: Option<String>,
    pub request_required: bool,
    /// Security scheme names accepted (empty = public).
    pub security: Vec<String>,
    pub deprecated: bool,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub required: bool,
    pub description: String,
}

pub struct Spec {
    pub version: String,
    pub operations: BTreeMap<String, Operation>,
    pub schemas: Value,
}

/// A clap value parser restricted to a component schema's enum values.
pub fn enum_parser(schema: &str) -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(spec().enum_values(schema))
}

pub fn spec() -> &'static Spec {
    static CELL: OnceLock<Spec> = OnceLock::new();
    CELL.get_or_init(|| parse(SPEC).expect("embedded api/openapi.json is valid"))
}

fn parse(raw: &str) -> anyhow::Result<Spec> {
    let doc: Value = serde_json::from_str(raw)?;
    let version = doc["info"]["version"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let global_security = security_names(doc.get("security"));
    let mut operations = BTreeMap::new();
    let paths = doc["paths"]
        .as_object()
        .ok_or_else(|| anyhow!("spec has no paths"))?;
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        let shared_params: Vec<Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (m, op) in item {
            let method = match m.as_str() {
                "get" => Method::GET,
                "post" => Method::POST,
                "put" => Method::PUT,
                "patch" => Method::PATCH,
                "delete" => Method::DELETE,
                _ => continue,
            };
            let id = op["operationId"]
                .as_str()
                .ok_or_else(|| anyhow!("{m} {path}: missing operationId"))?
                .to_string();
            let mut path_params = Vec::new();
            let mut query_params = Vec::new();
            let params = shared_params.iter().chain(
                op.get("parameters")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            );
            for p in params {
                let name = p["name"].as_str().unwrap_or_default().to_string();
                match p["in"].as_str() {
                    Some("path") => path_params.push(name),
                    Some("query") => query_params.push(Param {
                        name,
                        required: p["required"].as_bool().unwrap_or(false),
                        description: p["description"].as_str().unwrap_or_default().to_string(),
                    }),
                    _ => {}
                }
            }
            let body = op.get("requestBody");
            let request_schema = body
                .and_then(|b| b.pointer("/content/application~1json/schema/$ref"))
                .and_then(Value::as_str)
                .and_then(|r| r.rsplit('/').next())
                .map(str::to_string);
            let request_required = body.and_then(|b| b["required"].as_bool()).unwrap_or(false);
            let security = match op.get("security") {
                Some(s) => security_names(Some(s)),
                None => global_security.clone(),
            };
            if operations.contains_key(&id) {
                bail!("duplicate operationId {id}");
            }
            operations.insert(
                id.clone(),
                Operation {
                    id,
                    method,
                    path: path.clone(),
                    summary: op["summary"].as_str().unwrap_or_default().to_string(),
                    path_params,
                    query_params,
                    request_schema,
                    request_required,
                    security,
                    deprecated: op["deprecated"].as_bool().unwrap_or(false),
                },
            );
        }
    }
    Ok(Spec {
        version,
        operations,
        schemas: doc["components"]["schemas"].clone(),
    })
}

fn security_names(v: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(arr) = v.and_then(Value::as_array) {
        for req in arr {
            if let Some(obj) = req.as_object() {
                for k in obj.keys() {
                    if !out.contains(k) {
                        out.push(k.clone());
                    }
                }
            }
        }
    }
    out
}

impl Spec {
    /// String enum values of a component schema (empty when the schema is not a string enum).
    pub fn enum_values(&self, schema: &str) -> Vec<&str> {
        self.schemas
            .get(schema)
            .and_then(|s| s.get("enum"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    }

    /// Finds an operation by operationId (case-insensitive) or by `METHOD /path/template`.
    pub fn find(&self, selector: &str, method_hint: Option<&Method>) -> anyhow::Result<&Operation> {
        if let Some(op) = self.operations.get(selector) {
            return Ok(op);
        }
        if let Some(op) = self
            .operations
            .values()
            .find(|o| o.id.eq_ignore_ascii_case(selector))
        {
            return Ok(op);
        }
        if selector.starts_with('/') {
            let candidates: Vec<&Operation> = self
                .operations
                .values()
                .filter(|o| o.path == selector && method_hint.is_none_or(|m| *m == o.method))
                .collect();
            match candidates.len() {
                1 => return Ok(candidates[0]),
                0 => {}
                _ => bail!(
                    "{selector} has several methods ({}); pass --method",
                    candidates
                        .iter()
                        .map(|o| o.method.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
        let mut similar: Vec<&str> = self
            .operations
            .keys()
            .filter(|k| {
                k.to_ascii_lowercase()
                    .contains(&selector.to_ascii_lowercase())
            })
            .map(String::as_str)
            .collect();
        similar.truncate(8);
        if similar.is_empty() {
            bail!("unknown operation '{selector}' (see `aurix api --list`)")
        }
        bail!(
            "unknown operation '{selector}'; did you mean: {}",
            similar.join(", ")
        )
    }
}

impl Operation {
    /// Substitutes `{name}` placeholders, percent-encoding each value as one path segment.
    pub fn render_path(&self, params: &BTreeMap<String, String>) -> anyhow::Result<String> {
        let mut path = self.path.clone();
        for p in &self.path_params {
            let v = params
                .get(p)
                .ok_or_else(|| anyhow!("missing path parameter '{p}' (pass -p {p}=VALUE)"))?;
            if v.is_empty() {
                bail!("path parameter '{p}' is empty");
            }
            path = path.replace(&format!("{{{p}}}"), &encode_segment(v));
        }
        for k in params.keys() {
            if !self.path_params.contains(k) {
                bail!(
                    "'{k}' is not a path parameter of {} ({})",
                    self.id,
                    self.path
                );
            }
        }
        Ok(path)
    }
}

pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_spec_parses_and_resolves_operations() {
        let s = spec();
        assert!(s.operations.len() > 90, "{}", s.operations.len());
        let op = s.find("issueToken", None).unwrap();
        assert_eq!(op.method, Method::POST);
        assert_eq!(op.path, "/v1/tokens");
        assert_eq!(op.request_schema.as_deref(), Some("GenerateTokenRequest"));
        assert!(s.enum_values("Region").contains(&"eu_west"));
        assert!(s.enum_values("GenerateTokenRequest").is_empty());
        assert!(
            op.security.iter().any(|x| x.starts_with("ApiKey")),
            "{:?}",
            op.security
        );
        let op = s.find("/v1/channels", Some(&Method::GET)).unwrap();
        assert_eq!(op.id, "listChannels");
        assert!(s
            .find("/v1/channels", None)
            .unwrap_err()
            .to_string()
            .contains("--method"));
        assert!(s
            .find("nope", None)
            .unwrap_err()
            .to_string()
            .contains("unknown operation"));
        assert!(s.find("getchannel", None).is_ok());
    }

    #[test]
    fn path_rendering_encodes_and_validates() {
        let op = spec().find("getChannel", None).unwrap();
        let mut p = BTreeMap::new();
        assert!(op
            .render_path(&p)
            .unwrap_err()
            .to_string()
            .contains("channel_id"));
        p.insert("channel_id".into(), "a b/c".into());
        assert_eq!(op.render_path(&p).unwrap(), "/v1/channels/a%20b%2Fc");
        p.insert("bogus".into(), "x".into());
        assert!(op.render_path(&p).is_err());
    }
}
