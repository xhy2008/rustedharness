use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::provider::Usage;

/// Token accounting sourced exclusively from the provider's `usage` object.
///
/// No local tokenizer or character heuristic is used: counts are only known
/// after the provider reports them, and every value surfaced to the UI comes
/// straight from those reports. When a provider omits `usage`, the tracker
/// stays empty and the agent emits a one-time warning instead of inventing
/// numbers.
#[derive(Debug, Default)]
pub struct UsageTracker {
    inner: Mutex<Inner>,
    missing_reported: AtomicBool,
}

#[derive(Debug, Default)]
struct Inner {
    last: Option<Usage>,
    cost_usd: f64,
    cost_seen: bool,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store one provider-reported usage record and return it unchanged.
    pub fn record(&self, usage: &Usage) -> Usage {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last = Some(*usage);
        if let Some(cost) = usage.cost_usd {
            inner.cost_usd += cost;
            inner.cost_seen = true;
        }
        *usage
    }

    /// Seed the tracker with the values persisted in a session file.
    ///
    /// Resuming must not invent numbers, but it must not forget them either:
    /// the last usage the provider reported still describes the context being
    /// restored, and the cost total belongs to the session, not to the process.
    pub fn restore(&self, usage: Usage, cost_usd: Option<f64>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last = Some(usage);
        if let Some(cost) = cost_usd {
            inner.cost_usd = cost;
            inner.cost_seen = true;
        }
    }

    /// Replace the meters with what a rewritten session file now says. Unlike
    /// `restore`, a missing total clears the cost instead of keeping the old
    /// one: the turns that spent it are gone.
    pub fn reset_to(&self, usage: Option<Usage>, cost_usd: Option<f64>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last = usage;
        inner.cost_usd = cost_usd.unwrap_or(0.0);
        inner.cost_seen = cost_usd.is_some();
    }

    /// Accumulated provider-reported cost, or None if no report ever carried
    /// cost data.
    pub fn total_cost_usd(&self) -> Option<f64> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.cost_seen {
            Some(inner.cost_usd)
        } else {
            None
        }
    }

    /// Most recent report, exactly as the provider sent it.
    pub fn last(&self) -> Option<Usage> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).last
    }

    /// Tokens occupying the context window after the latest request.
    ///
    /// Providers normally populate `total_tokens`; some compatible gateways
    /// omit it, so the sum of the two reported components is used as a
    /// fallback. Both are API fields; nothing is estimated locally.
    pub fn context_tokens(&self) -> u64 {
        self.last().map(|u| u.context_tokens()).unwrap_or(0)
    }

    /// Cache hit ratio of the most recent report, when the provider exposes
    /// cached prompt tokens. Percent (0-100) of prompt tokens served from
    /// cache; None means the provider reports no cache data at all.
    pub fn cache_hit_pct(&self) -> Option<u64> {
        self.last()?.cache_hit_pct()
    }

    /// Forget the last report. Called after compaction because the previous
    /// request no longer describes the context now being sent; the next API
    /// response re-establishes an accurate value.
    pub fn reset_context(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).last = None;
    }

    /// Returns `true` only the first time a missing usage block is observed,
    /// so the agent can warn once per session without spamming the UI.
    pub fn note_missing_usage(&self) -> bool {
        !self.missing_reported.swap(true, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_provider_values() {
        let tracker = UsageTracker::new();
        assert_eq!(tracker.context_tokens(), 0);

        let first = Usage {
            prompt_tokens: 120,
            completion_tokens: 30,
            total_tokens: 150,
            cached_tokens: 0,
            cost_usd: None,
        };
        assert_eq!(tracker.record(&first), first);
        assert_eq!(tracker.context_tokens(), 150);

        let second = Usage {
            prompt_tokens: 400,
            completion_tokens: 20,
            total_tokens: 420,
            cached_tokens: 0,
            cost_usd: None,
        };
        tracker.record(&second);
        assert_eq!(tracker.last(), Some(second));
        assert_eq!(tracker.context_tokens(), 420);
    }

    #[test]
    fn missing_total_uses_reported_components() {
        let tracker = UsageTracker::new();
        tracker.record(&Usage {
            prompt_tokens: 9,
            completion_tokens: 4,
            total_tokens: 0,
            cached_tokens: 0,
            cost_usd: None,
        });
        assert_eq!(tracker.context_tokens(), 13);
    }

    #[test]
    fn reset_drops_the_last_report() {
        let tracker = UsageTracker::new();
        tracker.record(&Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: 0,
            cost_usd: None,
        });
        tracker.reset_context();
        assert_eq!(tracker.context_tokens(), 0);
        assert_eq!(tracker.last(), None);
    }

    #[test]
    fn cache_hit_pct_uses_latest_report() {
        let tracker = UsageTracker::new();
        assert_eq!(tracker.cache_hit_pct(), None);
        tracker.record(&Usage {
            prompt_tokens: 1000,
            completion_tokens: 5,
            total_tokens: 1005,
            cached_tokens: 980,
            cost_usd: None,
        });
        assert_eq!(tracker.cache_hit_pct(), Some(98));
        tracker.record(&Usage {
            prompt_tokens: 1000,
            completion_tokens: 5,
            total_tokens: 1005,
            cached_tokens: 0,
            cost_usd: None,
        });
        assert_eq!(tracker.cache_hit_pct(), None);
    }

    #[test]
    fn missing_usage_warning_is_one_shot() {
        let tracker = UsageTracker::new();
        assert!(tracker.note_missing_usage());
        assert!(!tracker.note_missing_usage());
    }
}
