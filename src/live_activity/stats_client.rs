//! HTTP client for post stats from Funnelcake.
//!
//! Reads `POST /api/videos/stats/bulk` and the `GET /api/videos/{id}/views`
//! fallback. The client only reports what Funnelcake said; what an absent id or
//! a null view count means is decided by the content rules in the caller.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde::Deserialize;
use tracing::warn;

use crate::config::LiveActivitySettings;

/// Funnelcake's cap on ids per bulk request.
const MAX_BULK_IDS: usize = 100;

/// Counters for one event id, as Funnelcake reports them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VersionStats {
    /// `None` when Funnelcake reports zero views as null.
    #[serde(default)]
    pub views: Option<u64>,
    /// Reactions, shown as likes.
    #[serde(default)]
    pub reactions: u64,
    #[serde(default)]
    pub comments: u64,
}

/// Result of one bulk lookup, restricted to the ids asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BulkStats {
    /// Stats per present event id.
    #[serde(default)]
    pub stats: HashMap<String, VersionStats>,
    /// Ids Funnelcake says it does not have. An id in neither map is also absent.
    #[serde(default)]
    pub missing: HashSet<String>,
}

/// Why a stats request failed. A failure is never evidence of absence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatsError {
    #[error("stats request failed: {0}")]
    Request(String),
    #[error("stats request returned status {0}")]
    Status(u16),
    /// 429 or 503, with `Retry-After` when Funnelcake sent one in seconds.
    #[error("stats request rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("stats response could not be decoded: {0}")]
    Decode(String),
}

#[derive(Deserialize)]
struct ViewsBody {
    views: u64,
}

/// Reads post stats from Funnelcake.
#[derive(Debug, Clone)]
pub struct StatsClient {
    http: reqwest::Client,
    base_url: String,
    max_concurrency: usize,
}

impl StatsClient {
    /// Builds a client with redirects disabled and the configured timeout.
    pub fn new(settings: &LiveActivitySettings) -> Result<Self, StatsError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(settings.request_timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| StatsError::Request(e.to_string()))?;
        Ok(Self {
            http,
            base_url: settings
                .funnelcake_base_url
                .trim_end_matches('/')
                .to_string(),
            max_concurrency: settings.max_concurrency.max(1),
        })
    }

    /// Looks up several activities' ids at once.
    ///
    /// Returns one result per input group, in order. Whole groups are packed
    /// into requests of at most 100 ids, so a failing request fails only the
    /// groups in it. Each group's `BulkStats` holds only its own ids. A group
    /// of more than 100 ids cannot occur (an activity has at most 10) but is
    /// split across requests, and fails if any part fails. Empty groups get an
    /// empty result without a request.
    pub async fn bulk_grouped(&self, groups: &[Vec<String>]) -> Vec<Result<BulkStats, StatsError>> {
        // Each chunk is a deduplicated id list; `group_chunks[g]` lists the
        // chunks group `g` has ids in.
        let mut chunks: Vec<Vec<String>> = Vec::new();
        let mut group_chunks: Vec<Vec<usize>> = vec![Vec::new(); groups.len()];
        for (g, ids) in groups.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            if ids.len() > MAX_BULK_IDS {
                for part in ids.chunks(MAX_BULK_IDS) {
                    chunks.push(part.to_vec());
                    group_chunks[g].push(chunks.len() - 1);
                }
                continue;
            }
            let fits = chunks.last().is_some_and(|c| {
                // Oversized parts are always full, so this never joins one.
                c.len() + ids.len() <= MAX_BULK_IDS
            });
            if !fits {
                chunks.push(Vec::new());
            }
            let idx = chunks.len() - 1;
            let chunk = &mut chunks[idx];
            for id in ids {
                if !chunk.contains(id) {
                    chunk.push(id.clone());
                }
            }
            group_chunks[g].push(idx);
        }

        let results: Vec<Result<BulkStats, StatsError>> = stream::iter(chunks.iter())
            .map(|ids| self.bulk_request(ids))
            .buffered(self.max_concurrency)
            .collect()
            .await;

        groups
            .iter()
            .zip(&group_chunks)
            .map(|(ids, idxs)| {
                let mut out = BulkStats::default();
                for &i in idxs {
                    let chunk = results[i].as_ref().map_err(Clone::clone)?;
                    for id in ids {
                        if let Some(s) = chunk.stats.get(id) {
                            out.stats.insert(id.clone(), s.clone());
                        }
                        if chunk.missing.contains(id) {
                            out.missing.insert(id.clone());
                        }
                    }
                }
                Ok(out)
            })
            .collect()
    }

    async fn bulk_request(&self, ids: &[String]) -> Result<BulkStats, StatsError> {
        let url = format!("{}/api/videos/stats/bulk", self.base_url);
        let body = serde_json::json!({ "event_ids": ids });
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| StatsError::Request(e.without_url().to_string()))?;
        Self::decode(response, "bulk").await
    }

    /// Reads the view count for one id: `GET /api/videos/{id}/views`.
    ///
    /// Funnelcake answers `views: 0` for an unknown id, so call this only for
    /// ids already known to be present. Returns `Some(count)` on success.
    pub async fn views(&self, id: &str) -> Result<Option<u64>, StatsError> {
        // The id goes into a URL path, so refuse anything that is not a plain id.
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(StatsError::Request("invalid event id".to_string()));
        }
        let url = format!("{}/api/videos/{}/views", self.base_url, id);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| StatsError::Request(e.without_url().to_string()))?;
        let body: ViewsBody = Self::decode(response, "views").await?;
        Ok(Some(body.views))
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
        kind: &'static str,
    ) -> Result<T, StatsError> {
        let status = response.status();
        if status.as_u16() == 429 || status.as_u16() == 503 {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            warn!(
                status = status.as_u16(),
                kind, "Funnelcake rate limited stats request"
            );
            return Err(StatsError::RateLimited { retry_after });
        }
        if !status.is_success() {
            warn!(
                status = status.as_u16(),
                kind, "Funnelcake stats request failed"
            );
            return Err(StatsError::Status(status.as_u16()));
        }
        response
            .json::<T>()
            .await
            .map_err(|e| StatsError::Decode(e.without_url().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Stub {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
        status: u16,
        retry_after: Option<&'static str>,
        delay: Duration,
        /// Respond 500 to bulk requests containing this id.
        fail_id: Option<&'static str>,
    }

    async fn bulk(
        axum::extract::State(s): axum::extract::State<Stub>,
        axum::Json(body): axum::Json<serde_json::Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        tokio::time::sleep(s.delay).await;
        let ids: Vec<String> = body["event_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        s.seen.lock().unwrap().push(ids.clone());
        let mut status = s.status;
        if s.fail_id.is_some_and(|f| ids.iter().any(|i| i == f)) {
            status = 500;
        }
        if status != 200 {
            let mut r = axum::http::StatusCode::from_u16(status)
                .unwrap()
                .into_response();
            if let Some(ra) = s.retry_after {
                r.headers_mut().insert("retry-after", ra.parse().unwrap());
            }
            return r;
        }
        // Every id is present, except ids starting with "m", which are missing.
        let mut stats = serde_json::Map::new();
        let mut missing = Vec::new();
        for id in ids {
            if id.starts_with('m') {
                missing.push(id);
            } else {
                stats.insert(
                    id,
                    serde_json::json!({
                        "views": null, "reactions": 3, "comments": 2,
                        "unique_viewers": 9, "reposts": 1, "engagement_score": 1.5,
                        "has_view_data": false, "audio_reuse_suppressed": false,
                        "audio_reuse_policy_valid_until": null
                    }),
                );
            }
        }
        axum::Json(serde_json::json!({"stats": stats, "missing": missing})).into_response()
    }

    async fn views(
        axum::extract::Path(id): axum::extract::Path<String>,
    ) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "views": if id == "v1" { 42 } else { 0 },
            "unique_viewers": 5, "total_watch_time": 10, "avg_completion": 0.5
        }))
    }

    fn stub() -> Stub {
        Stub {
            seen: Arc::new(Mutex::new(Vec::new())),
            status: 200,
            retry_after: None,
            delay: Duration::ZERO,
            fail_id: None,
        }
    }

    async fn client_for(s: Stub, timeout_secs: u64) -> (StatsClient, Arc<Mutex<Vec<Vec<String>>>>) {
        let seen = Arc::clone(&s.seen);
        let app = axum::Router::new()
            .route("/api/videos/stats/bulk", axum::routing::post(bulk))
            .route("/api/videos/:id/views", axum::routing::get(views))
            .with_state(s);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let settings = LiveActivitySettings {
            funnelcake_base_url: format!("http://{addr}"),
            request_timeout_secs: timeout_secs,
            ..Default::default()
        };
        (StatsClient::new(&settings).unwrap(), seen)
    }

    fn ids(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    }

    #[tokio::test]
    async fn bulk_decodes_stats_and_missing() {
        let (c, _) = client_for(stub(), 5).await;
        let groups = vec![vec!["a1".to_string(), "m1".to_string()]];
        let out = c.bulk_grouped(&groups).await;
        let r = out[0].as_ref().unwrap();
        assert_eq!(
            r.stats["a1"],
            VersionStats {
                views: None,
                reactions: 3,
                comments: 2
            }
        );
        assert!(r.missing.contains("m1"));
        assert!(!r.stats.contains_key("m1"));
    }

    #[tokio::test]
    async fn bulk_packs_whole_groups_under_100() {
        let (c, seen) = client_for(stub(), 5).await;
        let groups = vec![ids("a", 60), ids("b", 30), ids("c", 20)];
        let out = c.bulk_grouped(&groups).await;
        assert_eq!(out.len(), 3);
        let mut sizes: Vec<usize> = seen.lock().unwrap().iter().map(Vec::len).collect();
        sizes.sort();
        assert_eq!(sizes, vec![20, 90]);
        for (g, r) in groups.iter().zip(&out) {
            let r = r.as_ref().unwrap();
            assert_eq!(r.stats.len(), g.len());
            assert!(g.iter().all(|id| r.stats.contains_key(id)));
        }
    }

    #[tokio::test]
    async fn one_failing_chunk_only_fails_its_groups() {
        let s = Stub {
            fail_id: Some("c0"),
            ..stub()
        };
        let (c, _) = client_for(s, 5).await;
        let groups = vec![ids("a", 60), ids("b", 30), ids("c", 20)];
        let out = c.bulk_grouped(&groups).await;
        assert!(out[0].is_ok());
        assert!(out[1].is_ok());
        assert_eq!(out[2], Err(StatsError::Status(500)));
    }

    #[tokio::test]
    async fn rate_limited_reads_retry_after() {
        let s = Stub {
            status: 429,
            retry_after: Some("7"),
            ..stub()
        };
        let (c, _) = client_for(s, 5).await;
        let out = c.bulk_grouped(&[vec!["a1".to_string()]]).await;
        assert_eq!(
            out[0],
            Err(StatsError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            })
        );
        let s = Stub {
            status: 503,
            ..stub()
        };
        let (c, _) = client_for(s, 5).await;
        let out = c.bulk_grouped(&[vec!["a1".to_string()]]).await;
        assert_eq!(out[0], Err(StatsError::RateLimited { retry_after: None }));
    }

    #[tokio::test]
    async fn bulk_failure_is_error_not_missing() {
        let s = Stub {
            status: 500,
            ..stub()
        };
        let (c, _) = client_for(s, 5).await;
        let out = c.bulk_grouped(&[vec!["a1".to_string()]]).await;
        assert_eq!(out[0], Err(StatsError::Status(500)));
    }

    #[tokio::test]
    async fn views_fallback_decodes() {
        let (c, _) = client_for(stub(), 5).await;
        assert_eq!(c.views("v1").await, Ok(Some(42)));
    }

    #[tokio::test]
    async fn timeout_is_request_error() {
        let s = Stub {
            delay: Duration::from_secs(3),
            ..stub()
        };
        let (c, _) = client_for(s, 1).await;
        let out = c.bulk_grouped(&[vec!["a1".to_string()]]).await;
        assert!(matches!(out[0], Err(StatsError::Request(_))));
    }

    #[tokio::test]
    async fn empty_groups_make_no_request() {
        let (c, seen) = client_for(stub(), 5).await;
        let out = c.bulk_grouped(&[vec![], vec!["a1".to_string()]]).await;
        assert_eq!(out[0], Ok(BulkStats::default()));
        assert!(out[1].is_ok());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
