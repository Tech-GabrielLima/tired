//! The HTTP engine: a connection-pooling `reqwest` client wrapped with the policies
//! hale declares on an endpoint — timeout, automatic retry with backoff, bearer auth,
//! and a TTL response cache — plus lightweight Prometheus-style counters.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{Auth, Backoff, EndpointConfig};
use crate::value::{from_json, ErrValue, Outcome, Value};

#[derive(Default, Debug)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub cache_hits: AtomicU64,
    pub retries: AtomicU64,
    pub errors: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct MetricsSnapshot {
    pub requests: u64,
    pub cache_hits: u64,
    pub retries: u64,
    pub errors: u64,
}

impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

pub struct HttpEngine {
    client: reqwest::Client,
    cache: Mutex<HashMap<String, (Value, Instant)>>,
    pub metrics: Metrics,
}

impl HttpEngine {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent("hale/0.1")
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        HttpEngine {
            client,
            cache: Mutex::new(HashMap::new()),
            metrics: Metrics::default(),
        }
    }

    /// Perform an HTTP request against `cfg.base + path`, applying the endpoint's
    /// timeout, auth, cache and retry policy. Only idempotent methods (GET) are cached
    /// and auto-retried — a non-idempotent mutation is never silently re-sent. Returns a
    /// structured [`Outcome`].
    pub async fn request(
        &self,
        cfg: &EndpointConfig,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&serde_json::Value>,
        idempotency_key: Option<&str>,
    ) -> Outcome {
        let url = format!("{}{}", cfg.base.trim_end_matches('/'), path);
        let key = cache_key(&url, query);
        // Only true reads are cached. A request is *retry-safe* if it is a read OR it
        // carries an idempotency key (a mutation the program proved safe to repeat).
        let cacheable = matches!(method, "GET" | "HEAD");
        let retry_safe = cacheable || idempotency_key.is_some();
        let verb = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);

        if cacheable && cfg.cache_ttl_ms.is_some() {
            if let Some(v) = self.cache_get(&key) {
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Outcome::Success(v);
            }
        }

        let mut attempt = 0u32;
        loop {
            self.metrics.requests.fetch_add(1, Ordering::Relaxed);
            let mut req = self.client.request(verb.clone(), &url).query(query);
            if let Some(b) = body {
                req = req.json(b);
            }
            if let Some(k) = idempotency_key {
                req = req.header("Idempotency-Key", k);
            }
            if let Some(ms) = cfg.timeout_ms {
                req = req.timeout(Duration::from_millis(ms));
            }
            req = apply_auth(req, &cfg.auth);

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    if (200..300).contains(&status) {
                        let value = parse_body(&body);
                        if cacheable {
                            if let Some(ttl) = cfg.cache_ttl_ms {
                                self.cache_put(key.clone(), value.clone(), ttl);
                            }
                        }
                        return Outcome::Success(value);
                    }
                    if retry_safe && is_retryable(status) && attempt < cfg.retries {
                        attempt += 1;
                        self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(backoff_delay(cfg.backoff, attempt)).await;
                        continue;
                    }
                    self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    let mut err = match Value::from_status(status, Value::Null) {
                        Value::Err(e) => e,
                        _ => ErrValue::new("HttpError", Some(status), format!("HTTP {status}")),
                    };
                    if status == 429 {
                        // Surface a retry-after hint as the RateLimit payload.
                        err = err.with_payload(Value::Int(1000));
                    }
                    return Outcome::Failure(err);
                }
                Err(e) => {
                    if retry_safe && attempt < cfg.retries {
                        attempt += 1;
                        self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(backoff_delay(cfg.backoff, attempt)).await;
                        continue;
                    }
                    self.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    let variant = if e.is_timeout() {
                        "Timeout"
                    } else {
                        "NetworkError"
                    };
                    return Outcome::Failure(ErrValue::new(variant, None, e.to_string()));
                }
            }
        }
    }

    fn cache_get(&self, key: &str) -> Option<Value> {
        let cache = self.cache.lock().unwrap();
        cache.get(key).and_then(|(v, exp)| {
            if Instant::now() < *exp {
                Some(v.clone())
            } else {
                None
            }
        })
    }

    fn cache_put(&self, key: String, value: Value, ttl_ms: u64) {
        let exp = Instant::now() + Duration::from_millis(ttl_ms);
        self.cache.lock().unwrap().insert(key, (value, exp));
    }
}

impl Default for HttpEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_auth(req: reqwest::RequestBuilder, auth: &Option<Auth>) -> reqwest::RequestBuilder {
    match auth {
        Some(Auth::Bearer(env)) => match std::env::var(env) {
            Ok(tok) => req.bearer_auth(tok),
            Err(_) => req,
        },
        Some(Auth::ApiKey { header, env }) => match std::env::var(env) {
            Ok(key) => req.header(header.as_str(), key),
            Err(_) => req,
        },
        None => req,
    }
}

fn parse_body(body: &str) -> Value {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(j) => from_json(&j),
        Err(_) => Value::Str(body.to_string()),
    }
}

fn is_retryable(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn backoff_delay(strategy: Backoff, attempt: u32) -> Duration {
    match strategy {
        Backoff::Constant => Duration::from_millis(100),
        Backoff::Exponential => {
            let ms = 50u64.saturating_mul(1u64 << attempt.min(6)).min(2000);
            Duration::from_millis(ms)
        }
    }
}

fn cache_key(url: &str, query: &[(String, String)]) -> String {
    let mut k = url.to_string();
    for (a, b) in query {
        k.push('|');
        k.push_str(a);
        k.push('=');
        k.push_str(b);
    }
    k
}
