//! What the fleet limiter does when Redis answers but cannot serve the bucket script: enforce
//! node-locally by default, refuse with `fail_closed`. A minimal RESP server plays Redis.

use aurix_common::config::{RateLimitConfig, RedisConfig};
use aurix_common::redis_pool::RedisSource;
use aurix_common::types::MediaNodeId;
use aurix_control::{FleetLimiter, LimitBackend, LimitScope, RedisStore};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Splits complete RESP arrays off the front of `buf`, returning each command's first word.
fn pop_commands(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let text = String::from_utf8_lossy(buf).into_owned();
        let mut lines = text.split("\r\n");
        let Some(head) = lines.next() else { break };
        let Some(count) = head.strip_prefix('*').and_then(|n| n.parse::<usize>().ok()) else {
            break;
        };
        let mut consumed = head.len() + 2;
        let mut first = None;
        let mut complete = true;
        for i in 0..count {
            let (Some(len_line), Some(arg)) = (lines.next(), lines.next()) else {
                complete = false;
                break;
            };
            let Some(len) = len_line
                .strip_prefix('$')
                .and_then(|n| n.parse::<usize>().ok())
            else {
                complete = false;
                break;
            };
            if arg.len() != len {
                complete = false;
                break;
            }
            consumed += len_line.len() + 2 + len + 2;
            if i == 0 {
                first = Some(arg.to_ascii_uppercase());
            }
        }
        if !complete || consumed > buf.len() {
            break;
        }
        buf.drain(..consumed);
        out.extend(first);
    }
    out
}

/// Answers `PING`, rejects everything else with a loading error.
async fn fake_redis() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(n) = sock.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    for cmd in pop_commands(&mut buf) {
                        let reply: &[u8] = if cmd == "PING" {
                            b"+PONG\r\n"
                        } else {
                            b"-LOADING Redis is loading the dataset in memory\r\n"
                        };
                        if sock.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    format!("redis://{addr}")
}

async fn store_at(url: String) -> Arc<RedisStore> {
    let cfg = RedisConfig {
        url,
        pool_size: 1,
        sentinels: Vec::new(),
        sentinel_master: None,
    };
    let source = RedisSource::open(&cfg)
        .await
        .expect("fake redis answers PING");
    Arc::new(
        RedisStore::connect(source, MediaNodeId::new())
            .await
            .expect("store"),
    )
}

#[tokio::test]
async fn unavailable_fleet_backend_falls_back_to_local_buckets() {
    let url = fake_redis().await;
    let l = FleetLimiter::new(
        RateLimitConfig {
            enabled: true,
            reports_per_minute: 1,
            ..RateLimitConfig::default()
        },
        Some(store_at(url).await),
    );
    assert!(l.check(LimitScope::Report, "u").await.is_ok());
    let t = l.check(LimitScope::Report, "u").await.unwrap_err();
    assert_eq!(t.backend, LimitBackend::Local);
    assert_eq!(t.retry_after_secs(), 60);
}

#[tokio::test]
async fn fail_closed_refuses_without_the_fleet_backend() {
    let url = fake_redis().await;
    let l = FleetLimiter::new(
        RateLimitConfig {
            enabled: true,
            fail_closed: true,
            ..RateLimitConfig::default()
        },
        Some(store_at(url).await),
    );
    let t = l.check(LimitScope::Join, "u").await.unwrap_err();
    assert_eq!(t.backend, LimitBackend::Fleet);
    assert_eq!(t.scope, LimitScope::Join);
    assert_eq!(t.retry_after_secs(), 1);
}
