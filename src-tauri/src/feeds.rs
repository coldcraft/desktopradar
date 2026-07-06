use crate::config::{Config, Feed};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};

/// One aircraft in the ADSBExchange-v2-compatible shape all three feeds share.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Aircraft {
    #[serde(default)]
    pub hex: String,
    pub flight: Option<String>,
    pub r: Option<String>,
    pub t: Option<String>,
    pub desc: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Integer feet or the string "ground".
    pub alt_baro: Option<serde_json::Value>,
    pub gs: Option<f64>,
    pub track: Option<f64>,
    pub baro_rate: Option<f64>,
    pub squawk: Option<String>,
    pub emergency: Option<String>,
    pub category: Option<String>,
    #[serde(rename = "dbFlags")]
    pub db_flags: Option<u32>,
    pub seen: Option<f64>,
}

#[derive(Debug, Default)]
pub struct FeedResponse {
    pub ac: Vec<Aircraft>,
}

#[derive(Deserialize)]
struct RawResponse {
    ac: Option<Vec<serde_json::Value>>,
}

pub struct FeedClient {
    http: reqwest::Client,
    current: AtomicUsize,
}

impl FeedClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("adsb-radar-gadget/0.1 (personal desktop widget)")
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("http client");
        FeedClient {
            http,
            current: AtomicUsize::new(0),
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    async fn fetch(&self, url: &str) -> Result<FeedResponse, String> {
        let resp = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("http {}", status.as_u16()));
        }
        let text = resp.text().await.map_err(|e| e.to_string())?;
        let raw: RawResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        // A legit empty sky is `"ac": []`; a missing/null `ac` is an error
        // body wearing a 200 (rate-limit notices do this) — fail over.
        let Some(list) = raw.ac else {
            let head: String = text.chars().take(160).collect();
            return Err(format!("no ac field in response: {head}"));
        };
        let total = list.len();
        // Lenient per-aircraft parse: one odd record must not sink the poll.
        let mut first_err: Option<String> = None;
        let ac: Vec<Aircraft> = list
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<Aircraft>(v) {
                Ok(a) => Some(a),
                Err(e) => {
                    first_err.get_or_insert_with(|| e.to_string());
                    None
                }
            })
            .filter(|a| !a.hex.is_empty())
            .collect();
        if total > 0 && ac.is_empty() {
            return Err(format!(
                "all {total} records failed to parse; first error: {}",
                first_err.unwrap_or_default()
            ));
        }
        Ok(FeedResponse { ac })
    }

    /// Try the last-good feed first, then the rest in config order.
    async fn with_failover<F>(&self, cfg: &Config, build: F) -> Result<(String, FeedResponse), String>
    where
        F: Fn(&Feed) -> String,
    {
        if cfg.feeds.is_empty() {
            return Err("no feeds configured".into());
        }
        let n = cfg.feeds.len();
        let start = self.current.load(Ordering::Relaxed) % n;
        let mut last_err = String::new();
        for i in 0..n {
            let idx = (start + i) % n;
            let feed = &cfg.feeds[idx];
            match self.fetch(&build(feed)).await {
                Ok(r) => {
                    self.current.store(idx, Ordering::Relaxed);
                    return Ok((feed.name.clone(), r));
                }
                Err(e) => last_err = format!("{}: {}", feed.name, e),
            }
        }
        Err(last_err)
    }

    /// `radius_nm` is the effective query radius — the poller widens it to
    /// cover the current disc zoom so far-out traffic actually has data,
    /// capped at the feeds' 250 NM ceiling.
    pub async fn point(&self, cfg: &Config, radius_nm: f64) -> Result<(String, FeedResponse), String> {
        let lat = format!("{:.4}", cfg.home_lat);
        let lon = format!("{:.4}", cfg.home_lon);
        let nm = format!("{:.0}", radius_nm.clamp(1.0, 250.0));
        self.with_failover(cfg, |f| {
            f.point_url
                .replace("{lat}", &lat)
                .replace("{lon}", &lon)
                .replace("{nm}", &nm)
        })
        .await
    }

    pub async fn squawk(&self, cfg: &Config, sqk: &str) -> Result<(String, FeedResponse), String> {
        self.with_failover(cfg, |f| f.sqk_url.replace("{sqk}", sqk)).await
    }
}
