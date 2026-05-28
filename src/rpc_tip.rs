//! Optional trusted-RPC tip override.
//!
//! When the operator has configured a `[tip_filter.rpc_override]` block, this
//! module polls the configured zebrad/zcashd JSON-RPC endpoint for the
//! current block count and exposes the latest reading to the probe task via
//! [`RpcTipState`]. If polling fails three times in a row the cached reading
//! is invalidated and the probe task falls back to its own probe-derived tip.

use std::{
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use metrics::counter;
use serde::Deserialize;
use tokio::task::JoinHandle;

use crate::config::RpcOverrideConfig;

/// How long a successful RPC reading remains usable before we ignore it.
const RPC_READING_FRESHNESS: Duration = Duration::from_secs(60);
/// Consecutive failures before the cached reading is invalidated.
const RPC_FAILURE_THRESHOLD: u32 = 3;

#[derive(Clone, Copy, Debug)]
struct RpcTipReading {
    height: u32,
    fetched_at: Instant,
}

/// Shared, lock-protected handle to the latest trusted-RPC reading.
#[derive(Clone, Debug, Default)]
pub struct RpcTipState {
    inner: Arc<RwLock<Option<RpcTipReading>>>,
}

impl RpcTipState {
    pub fn new() -> Self {
        Self::default()
    }

    fn write(&self, height: u32) {
        if let Ok(mut guard) = self.inner.write() {
            *guard = Some(RpcTipReading {
                height,
                fetched_at: Instant::now(),
            });
        }
    }

    fn invalidate(&self) {
        if let Ok(mut guard) = self.inner.write() {
            *guard = None;
        }
    }

    /// Returns the cached RPC height if it was fetched within
    /// [`RPC_READING_FRESHNESS`]; otherwise `None`.
    pub fn read_fresh(&self) -> Option<u32> {
        let guard = self.inner.read().ok()?;
        let reading = guard.as_ref()?;
        if reading.fetched_at.elapsed() < RPC_READING_FRESHNESS {
            Some(reading.height)
        } else {
            None
        }
    }
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<u64>,
    #[allow(dead_code)]
    error: Option<serde_json::Value>,
}

/// Spawn the RPC-polling task. The returned handle is the operator's way to
/// abort it on shutdown; the task itself runs forever.
pub fn spawn(config: RpcOverrideConfig, state: RpcTipState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client must build with default tls");

        let interval = Duration::from_secs(config.poll_interval_secs.max(1));
        let mut consecutive_failures: u32 = 0;

        loop {
            match poll_once(&client, &config).await {
                Ok(height) => {
                    state.write(height);
                    consecutive_failures = 0;
                    tracing::debug!(height, "tip RPC: fetched");
                }
                Err(err) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    counter!("seeder.tip.rpc_failures_total").increment(1);
                    tracing::warn!(?err, consecutive_failures, "tip RPC: failed");
                    if consecutive_failures >= RPC_FAILURE_THRESHOLD {
                        state.invalidate();
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    })
}

async fn poll_once(
    client: &reqwest::Client,
    config: &RpcOverrideConfig,
) -> color_eyre::eyre::Result<u32> {
    use color_eyre::eyre::{eyre, Context};

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getblockcount",
        "params": [],
    });

    let mut req = client.post(&config.url).json(&body);
    if let Some((user, pass)) = &config.basic_auth {
        req = req.basic_auth(user, Some(pass));
    }

    let response = req.send().await.wrap_err("send getblockcount")?;
    let parsed: RpcResponse = response
        .json()
        .await
        .wrap_err("parse getblockcount response")?;
    let height = parsed
        .result
        .ok_or_else(|| eyre!("RPC response missing `result`"))?;
    u32::try_from(height).map_err(|_| eyre!("RPC `result` does not fit in u32: {}", height))
}
