//! Output formatting: pretty JSON by default, compact `--output json` for scripts,
//! `--field a.b.0` to extract one value (strings are printed bare).

use anyhow::{anyhow, bail};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Indented JSON.
    Pretty,
    /// One compact JSON document per line.
    Json,
}

pub struct Printer {
    pub format: Format,
    pub field: Option<String>,
}

impl Printer {
    pub fn print(&self, v: &Value) -> anyhow::Result<()> {
        let v = match &self.field {
            Some(path) => extract(v, path)?,
            None => v,
        };
        match (v, self.format) {
            (Value::String(s), _) if self.field.is_some() => println!("{s}"),
            (Value::Null, _) if self.field.is_none() => {}
            (v, Format::Pretty) => println!("{}", serde_json::to_string_pretty(v)?),
            (v, Format::Json) => println!("{}", serde_json::to_string(v)?),
        }
        Ok(())
    }
}

/// Resolves a dotted path (`endpoint.ws_url`, `data.0.id`) inside `v`.
pub fn extract<'a>(v: &'a Value, path: &str) -> anyhow::Result<&'a Value> {
    let mut cur = v;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        cur = match cur {
            Value::Object(m) => m
                .get(part)
                .ok_or_else(|| anyhow!("field '{part}' not found in response"))?,
            Value::Array(a) => {
                let idx: usize = part
                    .parse()
                    .map_err(|_| anyhow!("'{part}' is not an array index"))?;
                a.get(idx)
                    .ok_or_else(|| anyhow!("index {idx} out of range"))?
            }
            _ => bail!("cannot descend into '{part}': value is not an object or array"),
        };
    }
    Ok(cur)
}

/// Parses `k=v` pairs.
pub fn kv(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected KEY=VALUE, got '{s}'")),
    }
}

/// Parses a JSON argument: inline JSON, `@file`, or `-` for stdin.
pub fn json_arg(s: &str) -> anyhow::Result<Value> {
    let text = if s == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else if let Some(path) = s.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|e| anyhow!("reading {path}: {e}"))?
    } else {
        s.to_string()
    };
    serde_json::from_str(text.trim()).map_err(|e| anyhow!("invalid JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_nested_fields() {
        let v = json!({"endpoint": {"ws_url": "wss://x"}, "data": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(extract(&v, "endpoint.ws_url").unwrap(), "wss://x");
        assert_eq!(extract(&v, "data.1.id").unwrap(), "b");
        assert!(extract(&v, "data.9").is_err());
        assert!(extract(&v, "endpoint.ws_url.x").is_err());
    }

    #[test]
    fn kv_and_json_args() {
        assert_eq!(kv("a=b=c").unwrap(), ("a".into(), "b=c".into()));
        assert!(kv("=x").is_err());
        assert_eq!(json_arg(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
        assert!(json_arg("{").is_err());
    }
}
