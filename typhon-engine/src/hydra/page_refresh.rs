//! Bounded work and short-lived sharing for UI background page refreshes.
//! Selection/action requests bypass the response cache.
use bytes::Bytes;
use lru::LruCache;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Duration, Instant};

const ENTRIES: usize = 8;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const TTL: Duration = Duration::from_secs(1);

type Flight = Mutex<Option<(Instant, Bytes)>>;

pub struct PageRefreshes {
    entries: Mutex<LruCache<String, Arc<Flight>>>,
    // One full scan at a time; indexed views retain a separate lane.
    pub scans: Arc<Semaphore>,
    pub indexed: Arc<Semaphore>,
    // Brief metadata reads also need a bounded worker lane when SQLite stalls.
    pub metadata: Arc<Semaphore>,
    pub computations: AtomicU64,
    pub hits: AtomicU64,
}

impl Default for PageRefreshes {
    fn default() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(NonZeroUsize::new(ENTRIES).unwrap())),
            scans: Arc::new(Semaphore::new(1)),
            indexed: Arc::new(Semaphore::new(4)),
            metadata: Arc::new(Semaphore::new(1)),
            computations: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        }
    }
}

impl PageRefreshes {
    pub async fn clear(&self) {
        self.entries.lock().await.clear();
    }

    pub async fn response<F, Fut>(&self, key: String, compute: F) -> Bytes
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = serde_json::Value>,
    {
        let flight = {
            let mut entries = self.entries.lock().await;
            if let Some(flight) = entries.get(&key) {
                flight.clone()
            } else {
                let flight = Arc::new(Mutex::new(None));
                entries.put(key, flight.clone());
                flight
            }
        };
        // Only callers of the same query wait here. The LRU lock is released
        // before computing or waiting, so another view remains independent.
        let mut cached = flight.lock().await;
        if let Some((at, bytes)) = cached.as_ref() {
            if at.elapsed() < TTL {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return bytes.clone();
            }
        }
        self.computations.fetch_add(1, Ordering::Relaxed);
        let bytes =
            Bytes::from(serde_json::to_vec(&compute().await).expect("JSON value serializes"));
        *cached = if bytes.len() <= MAX_RESPONSE_BYTES {
            Some((Instant::now(), bytes.clone()))
        } else {
            None
        };
        bytes
    }
}

/// Stable ordering of parameter names, preserving duplicate values' order.
/// Authentication is checked before lookup and is never kept as a cache key.
pub fn key(engine: &str, query: &str) -> String {
    let mut params: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k != "apikey" && k != "live")
        .collect();
    params.sort_by(|a, b| a.0.cmp(&b.0));
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    format!("{engine}?{query}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn identical_refreshes_share_one_compute_and_expire() {
        let pages = PageRefreshes::default();
        let calls = AtomicU64::new(0);
        let results = futures::future::join_all((0..12).map(|_| {
            pages.response("same".into(), || async {
                calls.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(10)).await;
                serde_json::json!({"rows": [1]})
            })
        }))
        .await;
        assert!(results.windows(2).all(|w| w[0] == w[1]));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(pages.hits.load(Ordering::Relaxed), 11);
        tokio::time::advance(TTL).await;
        pages
            .response("same".into(), || async {
                calls.fetch_add(1, Ordering::Relaxed);
                serde_json::json!({})
            })
            .await;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        pages.clear().await;
        assert!(pages.entries.lock().await.is_empty());
    }

    #[tokio::test]
    async fn response_cache_is_bounded_and_does_not_keep_large_pages() {
        let pages = PageRefreshes::default();
        for n in 0..ENTRIES * 3 {
            pages
                .response(n.to_string(), || async { serde_json::json!({"n":n}) })
                .await;
        }
        assert_eq!(pages.entries.lock().await.len(), ENTRIES);
        pages
            .response("large".into(), || async {
                serde_json::json!("x".repeat(MAX_RESPONSE_BYTES + 1))
            })
            .await;
        let flight = pages.entries.lock().await.get("large").unwrap().clone();
        assert!(flight.lock().await.is_none());
    }

    #[test]
    fn cache_keys_preserve_query_semantics_without_credentials() {
        assert_eq!(
            key("hoard", "state=downloading&apikey=secret&live=1&limit=500"),
            key("hoard", "limit=500&state=downloading")
        );
        assert_ne!(
            key("hoard", "state=a&state=b"),
            key("hoard", "state=b&state=a")
        );
        assert_ne!(key("race", "state=a"), key("hoard", "state=a"));
    }
}
