use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::Result;
use rand::Rng;
use tokio::sync::{Mutex, Semaphore};

use crate::{
    config::RateLimitConfig,
    models::{RateLimitProgress, RateLimitReason},
    state::{StateStore, now_epoch_ms},
};

pub type RateLimitCallback = Arc<dyn Fn(RateLimitProgress) + Send + Sync + 'static>;

#[derive(Clone)]
pub struct PersistentRateLimiter {
    host: String,
    bucket: &'static str,
    hourly_limit: Option<u32>,
    minimum_interval: Duration,
    reserve_ratio: f64,
    state: StateStore,
    gate: Arc<Mutex<()>>,
    semaphore: Arc<Semaphore>,
    callback: Arc<RwLock<Option<RateLimitCallback>>>,
}

pub struct RatePermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl PersistentRateLimiter {
    pub fn api(host: String, config: &RateLimitConfig, state: StateStore) -> Self {
        Self {
            host,
            bucket: "api",
            hourly_limit: Some(config.api_requests_per_hour),
            minimum_interval: Duration::from_millis(config.minimum_interval_ms),
            reserve_ratio: config.reserve_ratio,
            state,
            gate: Arc::new(Mutex::new(())),
            semaphore: Arc::new(Semaphore::new(config.api_concurrency.max(1))),
            callback: Arc::new(RwLock::new(None)),
        }
    }

    pub fn assets(host: String, config: &RateLimitConfig, state: StateStore) -> Self {
        Self {
            host,
            bucket: "asset",
            hourly_limit: None,
            minimum_interval: Duration::from_millis(config.asset_minimum_interval_ms),
            reserve_ratio: 0.0,
            state,
            gate: Arc::new(Mutex::new(())),
            semaphore: Arc::new(Semaphore::new(config.asset_concurrency.max(1))),
            callback: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_callback(&self, callback: Option<RateLimitCallback>) {
        if let Ok(mut guard) = self.callback.write() {
            *guard = callback;
        }
    }

    pub async fn acquire(&self) -> Result<RatePermit> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        let _guard = self.gate.lock().await;

        loop {
            let now = now_epoch_ms();
            let entries = self
                .state
                .request_window(&self.host, self.bucket, now - 3_600_000)?;
            if let Some(limit) = self.hourly_limit {
                let usable = ((limit as f64) * (1.0 - self.reserve_ratio))
                    .floor()
                    .max(1.0) as usize;
                if entries.len() >= usable {
                    let wait_ms = entries[0] + 3_600_000 - now + 250;
                    self.emit(RateLimitProgress {
                        bucket: self.bucket.to_string(),
                        reason: RateLimitReason::HourlyLimit,
                        wait_until_ms: now + wait_ms.max(250),
                        wait_seconds: millis_to_seconds(wait_ms),
                        used: Some(entries.len()),
                        usable: Some(usable),
                        endpoint: None,
                        message: format!(
                            "{} 限流窗口已满：{}/{}",
                            self.bucket,
                            entries.len(),
                            usable
                        ),
                    });
                    tokio::time::sleep(Duration::from_millis(wait_ms.max(250) as u64)).await;
                    continue;
                }
            }
            if let Some(last) = entries.last() {
                let jitter = rand::rng().random_range(50_u64..=250_u64);
                let next = *last + self.minimum_interval.as_millis() as i64 + jitter as i64;
                if next > now {
                    tokio::time::sleep(Duration::from_millis((next - now) as u64)).await;
                }
            }
            self.state
                .record_request(&self.host, self.bucket, now_epoch_ms())?;
            break;
        }
        Ok(RatePermit { _permit: permit })
    }

    pub fn notify_server_retry_after(&self, wait: Duration, endpoint: Option<String>) {
        let now = now_epoch_ms();
        let (used, usable) = self
            .hourly_limit
            .map(|limit| {
                let used = self
                    .state
                    .request_window(&self.host, self.bucket, now - 3_600_000)
                    .map(|entries| entries.len())
                    .unwrap_or_default();
                let usable = ((limit as f64) * (1.0 - self.reserve_ratio))
                    .floor()
                    .max(1.0) as usize;
                (Some(used), Some(usable))
            })
            .unwrap_or((None, None));
        self.emit(RateLimitProgress {
            bucket: self.bucket.to_string(),
            reason: RateLimitReason::ServerRetryAfter,
            wait_until_ms: now + wait.as_millis() as i64,
            wait_seconds: millis_to_seconds(wait.as_millis() as i64),
            used,
            usable,
            endpoint,
            message: format!("服务端返回 429，等待 {} 秒", wait.as_secs().max(1)),
        });
    }

    pub fn remaining_this_hour(&self) -> Result<Option<u32>> {
        let Some(limit) = self.hourly_limit else {
            return Ok(None);
        };
        let now = now_epoch_ms();
        let used = self
            .state
            .request_window(&self.host, self.bucket, now - 3_600_000)?
            .len() as u32;
        let usable = ((limit as f64) * (1.0 - self.reserve_ratio)).floor() as u32;
        Ok(Some(usable.saturating_sub(used)))
    }

    fn emit(&self, event: RateLimitProgress) {
        if let Ok(guard) = self.callback.read()
            && let Some(callback) = guard.as_ref()
        {
            callback(event);
        }
    }
}

fn millis_to_seconds(ms: i64) -> u64 {
    ((ms.max(1) + 999) / 1000) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hourly_usage_survives_limiter_recreation() {
        let dir =
            std::env::temp_dir().join(format!("yuque-rate-limit-test-{}", uuid::Uuid::new_v4()));
        let state = StateStore::open(dir.join("state.sqlite3")).unwrap();
        let config = RateLimitConfig {
            api_requests_per_hour: 10,
            minimum_interval_ms: 0,
            reserve_ratio: 0.0,
            ..RateLimitConfig::default()
        };
        let limiter =
            PersistentRateLimiter::api("https://a.yuque.com".into(), &config, state.clone());
        drop(limiter.acquire().await.unwrap());
        let recreated = PersistentRateLimiter::api("https://a.yuque.com".into(), &config, state);
        assert_eq!(recreated.remaining_this_hour().unwrap(), Some(9));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn emits_event_when_hourly_window_is_full() {
        let dir =
            std::env::temp_dir().join(format!("yuque-rate-event-test-{}", uuid::Uuid::new_v4()));
        let state = StateStore::open(dir.join("state.sqlite3")).unwrap();
        let config = RateLimitConfig {
            api_requests_per_hour: 1,
            minimum_interval_ms: 0,
            reserve_ratio: 0.0,
            ..RateLimitConfig::default()
        };
        let limiter =
            PersistentRateLimiter::api("https://a.yuque.com".into(), &config, state.clone());
        drop(limiter.acquire().await.unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        limiter.set_callback(Some(Arc::new(move |event| {
            sender.send(event).unwrap();
        })));
        let result = tokio::time::timeout(Duration::from_millis(25), limiter.acquire()).await;
        assert!(result.is_err());
        let event = receiver.try_recv().unwrap();
        assert_eq!(event.reason, RateLimitReason::HourlyLimit);
        assert_eq!(event.used, Some(1));
        assert_eq!(event.usable, Some(1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
