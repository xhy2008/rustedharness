//! Prompt-cache accounting: notice when a turn re-billed prompt tokens the
//! previous turn had already cached.
//!
//! Providers report cached prompt tokens in `usage`; nothing reports *why* a
//! cache entry was lost, and no provider advertises how long it keeps one. A
//! miss therefore carries only what was observed — the gap since the previous
//! request and whether the model changed — and callers must not turn those into
//! a claim about a cache lifetime.

use std::time::{Duration, Instant};

use crate::provider::Usage;

/// Per-turn misses at or below this are cache-breakpoint granularity noise.
const NOISE_FLOOR_TOKENS: u64 = 1_024;

/// Below this a miss is not worth a notice even though it was counted: the
/// extra cost is small enough that the interruption is worse than the news.
const NOTICE_MIN_TOKENS: u64 = 20_000;

/// One observed request. Everything in its prompt should be a cache read for
/// the next request that runs without compaction or a model switch.
#[derive(Debug, Clone)]
struct Snapshot {
    prompt_tokens: u64,
    model: String,
    at: Instant,
    /// Sticky: some request in this segment reported cache activity. Tells a
    /// total miss on a cache-read-only provider apart from a provider that
    /// never reports caching at all.
    reported_cache: bool,
}

/// A miss on a completed request, relative to the request before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheMiss {
    /// Prompt tokens that were in the previous prompt but were not read from
    /// cache, so they were billed again.
    pub missed_tokens: u64,
    /// Observed gap since the previous request, which last refreshed the cache.
    /// Providers do not publish a cache lifetime, so this is information, not a
    /// verdict.
    pub idle: Duration,
    /// Whether the model changed between the two requests.
    pub model_changed: bool,
}

impl CacheMiss {
    /// Whether this miss is big enough to tell the user about.
    pub fn is_significant(&self) -> bool {
        self.missed_tokens >= NOTICE_MIN_TOKENS
    }
}

/// Tracks consecutive requests to spot cache misses.
#[derive(Debug, Default)]
pub struct CacheWatch {
    prev: Option<Snapshot>,
}

impl CacheWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the previous request. Compaction legitimately rewrites the
    /// prompt, so the next request is new content, not re-billed content.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    /// Feed one provider usage report and return the miss it paid for, if any.
    /// The first turn after a reset never reports a miss.
    pub fn observe(&mut self, model: &str, usage: &Usage, now: Instant) -> Option<CacheMiss> {
        let prompt_tokens = usage.prompt_tokens;
        if prompt_tokens == 0 {
            return None;
        }
        let cached = usage.cached_tokens;
        let miss = self.prev.as_ref().and_then(|prev| {
            // A zero-cache turn only counts when cache activity was reported
            // before: on providers that never report caching it means nothing.
            if cached == 0 && !prev.reported_cache {
                return None;
            }
            let missed = prev.prompt_tokens.min(prompt_tokens).saturating_sub(cached);
            if missed <= NOISE_FLOOR_TOKENS {
                return None;
            }
            Some(CacheMiss {
                missed_tokens: missed,
                idle: now.saturating_duration_since(prev.at),
                model_changed: prev.model != model,
            })
        });
        self.prev = Some(Snapshot {
            prompt_tokens,
            model: model.to_string(),
            at: now,
            reported_cache: cached > 0
                || self.prev.as_ref().is_some_and(|prev| prev.reported_cache),
        });
        miss
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: u64, cached: u64) -> Usage {
        Usage {
            prompt_tokens: prompt,
            cached_tokens: cached,
            ..Default::default()
        }
    }

    /// The first request of a session fills the cache; nothing is re-billed yet.
    #[test]
    fn the_first_request_is_never_a_miss() {
        let mut watch = CacheWatch::new();
        assert_eq!(watch.observe("m", &usage(50_000, 0), Instant::now()), None);
    }

    #[test]
    fn a_rebilled_prefix_is_a_miss_with_the_idle_gap() {
        let mut watch = CacheWatch::new();
        let start = Instant::now();
        watch.observe("m", &usage(50_000, 0), start);
        let miss = watch
            .observe("m", &usage(60_000, 5_000), start + Duration::from_secs(420))
            .expect("miss");
        assert_eq!(miss.missed_tokens, 45_000);
        assert_eq!(miss.idle, Duration::from_secs(420));
        assert!(!miss.model_changed);
        assert!(miss.is_significant());

        // The whole prompt was a cache hit: no miss at all.
        assert_eq!(
            watch.observe("m", &usage(60_000, 60_000), start),
            None,
            "a cache hit is not a miss"
        );
        // Only growth beyond the previous prompt is new content, so a prompt
        // that grew by 10k with 50k cached still re-billed 10k.
        assert_eq!(
            watch
                .observe("m", &usage(70_000, 50_000), start)
                .map(|miss| miss.missed_tokens),
            Some(10_000)
        );
    }

    #[test]
    fn small_misses_are_counted_but_not_announced() {
        let mut watch = CacheWatch::new();
        let start = Instant::now();
        watch.observe("m", &usage(50_000, 0), start);
        let miss = watch
            .observe("m", &usage(50_100, 48_500), start)
            .expect("counted");
        assert_eq!(miss.missed_tokens, 1_500);
        assert!(!miss.is_significant());
    }

    /// Providers that never report caching must not look like total misses
    /// forever.
    #[test]
    fn a_provider_without_cache_reporting_stays_silent() {
        let mut watch = CacheWatch::new();
        let start = Instant::now();
        for prompt in [10_000, 20_000, 30_000] {
            assert_eq!(watch.observe("m", &usage(prompt, 0), start), None);
        }
    }

    /// Once cache activity was seen, a zero-cache turn is a real total miss.
    #[test]
    fn a_total_miss_after_cache_activity_counts() {
        let mut watch = CacheWatch::new();
        let start = Instant::now();
        watch.observe("m", &usage(50_000, 40_000), start);
        let miss = watch.observe("m", &usage(60_000, 0), start).expect("miss");
        assert_eq!(miss.missed_tokens, 50_000);
    }

    #[test]
    fn a_model_switch_is_reported_as_such() {
        let mut watch = CacheWatch::new();
        let start = Instant::now();
        watch.observe("a/m", &usage(50_000, 0), start);
        let miss = watch
            .observe("b/m", &usage(60_000, 1_000), start)
            .expect("miss");
        assert!(miss.model_changed);
        assert_eq!(miss.idle, Duration::ZERO);
    }

    /// Compaction rewrites the prompt, so the turn after it is not a miss.
    #[test]
    fn a_reset_forgets_the_previous_prompt() {
        let mut watch = CacheWatch::new();
        let start = Instant::now();
        watch.observe("m", &usage(50_000, 0), start);
        watch.reset();
        assert_eq!(watch.observe("m", &usage(60_000, 0), start), None);
    }
}
