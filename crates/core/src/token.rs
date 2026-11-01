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
    requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
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
        inner.requests += 1;
        inner.prompt_tokens += usage.prompt_tokens;
        inner.completion_tokens += usage.completion_tokens;
        inner.total_tokens += effective_total(usage);
        if let Some(cost) = usage.cost_usd {
            inner.cost_usd += cost;
            inner.cost_seen = true;
        }
        *usage
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
        self.last().map(|u| effective_total(&u)).unwrap_or(0)
    }

    /// Cumulative totals across all reports in this session.
    pub fn totals(&self) -> UsageTotals {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        UsageTotals {
            requests: inner.requests,
            prompt_tokens: inner.prompt_tokens,
            completion_tokens: inner.completion_tokens,
            total_tokens: inner.total_tokens,
        }
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

fn effective_total(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.prompt_tokens + usage.completion_tokens
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
            cost_usd: None,
        };
        assert_eq!(tracker.record(&first), first);
        assert_eq!(tracker.context_tokens(), 150);

        let second = Usage {
            prompt_tokens: 400,
            completion_tokens: 20,
            total_tokens: 420,
            cost_usd: None,
        };
        tracker.record(&second);
        assert_eq!(tracker.last(), Some(second));
        assert_eq!(tracker.context_tokens(), 420);
        assert_eq!(
            tracker.totals(),
            UsageTotals {
                requests: 2,
                prompt_tokens: 520,
                completion_tokens: 50,
                total_tokens: 570,
            }
        );
    }

    #[test]
    fn missing_total_uses_reported_components() {
        let tracker = UsageTracker::new();
        tracker.record(&Usage {
            prompt_tokens: 9,
            completion_tokens: 4,
            total_tokens: 0,
            cost_usd: None,
        });
        assert_eq!(tracker.context_tokens(), 13);
    }

    #[test]
    fn reset_drops_context_but_keeps_totals() {
        let tracker = UsageTracker::new();
        tracker.record(&Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cost_usd: None,
        });
        tracker.reset_context();
        assert_eq!(tracker.context_tokens(), 0);
        assert_eq!(tracker.last(), None);
        assert_eq!(tracker.totals().requests, 1);
    }

    #[test]
    fn missing_usage_warning_is_one_shot() {
        let tracker = UsageTracker::new();
        assert!(tracker.note_missing_usage());
        assert!(!tracker.note_missing_usage());
    }
}
