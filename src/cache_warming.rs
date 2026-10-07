//! Keeping the prompt cache from expiring while the agent works.
//!
//! A cached prompt prefix lives for its retention's lifetime after it was last used. When a tool
//! call runs longer than that, the next request finds the cache gone and writes the whole context
//! again at the cache-write price (1.25x the input price, 2x for one-hour retention) instead of
//! reading it at about a tenth. Shortly before the cache expires, the agent repeats its last
//! request with a one-token output limit: the provider reads the cached prefix, which renews its
//! lifetime, at the read price. This module decides whether and when that pays off; the agent
//! sends the requests (`Agent::keep_cache_warm`).

use std::time::{Duration, Instant};

use crate::config::{Api, CacheRetention, Model, Reasoning, ThinkingLevel};
use crate::message::Usage;

/// A refresh is sent only when it is expected to save at least this many dollars.
pub const MINIMUM_SAVINGS: f64 = 0.05;

/// Warming stops this long after the request that wrote the cache, bounding what a tool that
/// never finishes can cost.
pub const MAX_WARMING_AGE: Duration = Duration::from_secs(60 * 60);

/// When to refresh a cache entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How long the entry lives after it is last used.
    pub ttl: Duration,
    /// How long after the entry was last used to refresh it.
    pub delay: Duration,
}

impl Timing {
    /// Refresh at 90% of `ttl`, leaving at least ten seconds before the entry expires.
    pub fn for_ttl(ttl: Duration) -> Option<Timing> {
        let margin = Duration::from_secs(10);
        (ttl > margin).then(|| Timing { ttl, delay: (ttl * 9 / 10).min(ttl - margin) })
    }

    /// The latest moment a refresh that was due at `due` is still sent. A timer that fires later,
    /// for example after the computer slept, would likely find the entry expired and pay for a
    /// full write.
    pub fn deadline(&self, due: Instant) -> Instant {
        due + (self.ttl - self.delay) / 2
    }
}

/// The retention a request's cache entry actually got. Some providers ignore a request for
/// one-hour retention and write five-minute entries, which shows in the reported breakdown.
pub fn applied_retention(requested: CacheRetention, usage: &Usage) -> CacheRetention {
    let wrote_only_short = usage.cache_write > 0 && usage.cache_write_1h == Some(0);
    if requested == CacheRetention::Long && wrote_only_short { CacheRetention::Short } else { requested }
}

/// Whether to keep the cache of a request with `prompt_tokens` warm while its tools run.
pub fn worthwhile(model: &Model, thinking: ThinkingLevel, retention: CacheRetention, prompt_tokens: u64) -> bool {
    model.cache_control
        && replayable(model, thinking)
        && expected_savings(model, retention, prompt_tokens).is_some_and(|savings| savings >= MINIMUM_SAVINGS)
}

/// Whether repeating a request with a one-token output limit reads its cache entry without other
/// effects. Changing thinking parameters invalidates cached messages, and a model that thinks
/// within a budget would get a budget well above one token (see `anthropic::build_body`) and
/// could think at length. Through an OpenAI-compatible gateway, how the gateway turns a
/// reasoning effort into thinking parameters is unknown, so only requests without thinking are
/// repeated there.
fn replayable(model: &Model, thinking: ThinkingLevel) -> bool {
    if model.clamp_thinking(thinking) == ThinkingLevel::Off || model.reasoning == Reasoning::None {
        return true;
    }
    model.api == Api::AnthropicMessages && model.reasoning == Reasoning::Adaptive
}

/// Dollars one refresh is expected to save: rewriting the cache costs more than reading it, and
/// the refresh itself costs a read and one output token. `None` when the model has no prices.
fn expected_savings(model: &Model, retention: CacheRetention, prompt_tokens: u64) -> Option<f64> {
    let cost = &model.cost;
    if cost.input <= 0.0 && cost.cache_read <= 0.0 {
        return None;
    }
    let per = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
    let rewrite_price = match retention {
        CacheRetention::Short if cost.cache_write > 0.0 => cost.cache_write,
        CacheRetention::Short => cost.input,
        CacheRetention::Long => cost.input * 2.0,
    };
    let read = per(prompt_tokens, cost.cache_read);
    let missed = (per(prompt_tokens, rewrite_price) - read).max(0.0);
    let refresh = read + per(1, cost.output);
    Some(missed - refresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_anthropic_models;

    fn opus() -> Model {
        builtin_anthropic_models().into_iter().find(|m| m.id == "claude-opus-5-5").unwrap()
    }

    #[test]
    fn refreshes_shortly_before_expiry() {
        let short = Timing::for_ttl(CacheRetention::Short.ttl()).unwrap();
        assert_eq!(short.delay, Duration::from_secs(270));
        let long = Timing::for_ttl(CacheRetention::Long.ttl()).unwrap();
        assert_eq!(long.delay, Duration::from_secs(3240));
        assert_eq!(Timing::for_ttl(Duration::from_secs(60)).unwrap().delay, Duration::from_secs(50));
        assert_eq!(Timing::for_ttl(Duration::from_secs(10)), None);

        let due = Instant::now();
        assert_eq!(short.deadline(due), due + Duration::from_secs(15));
    }

    #[test]
    fn warms_large_prompts_on_models_with_prices() {
        let model = opus();
        // Opus 5.5: $4 input, $5 cache write, $0.20 cache read per million tokens. A 20k-token
        // prompt saves 20k x ($5 - $0.40) = $0.092 per avoided rewrite.
        let savings = expected_savings(&model, CacheRetention::Short, 20_000).unwrap();
        assert!((savings - (0.1 - 0.004 - 0.004 - 0.00002)).abs() < 1e-9, "{savings}");
        assert!(worthwhile(&model, ThinkingLevel::High, CacheRetention::Short, 20_000));
        assert!(!worthwhile(&model, ThinkingLevel::High, CacheRetention::Short, 5_000));
        // One-hour rewrites cost twice the input price, so smaller prompts are worth keeping.
        assert!(worthwhile(&model, ThinkingLevel::High, CacheRetention::Long, 8_000));

        let mut unpriced = model.clone();
        unpriced.cost = Default::default();
        assert!(!worthwhile(&unpriced, ThinkingLevel::High, CacheRetention::Short, 1_000_000));
        let mut uncached = model.clone();
        uncached.cache_control = false;
        assert!(!worthwhile(&uncached, ThinkingLevel::High, CacheRetention::Short, 1_000_000));
    }

    #[test]
    fn falls_back_to_five_minutes_when_one_hour_retention_is_ignored() {
        let ignored = Usage { cache_write: 900, cache_write_1h: Some(0), ..Default::default() };
        assert_eq!(applied_retention(CacheRetention::Long, &ignored), CacheRetention::Short);
        let honored = Usage { cache_write: 900, cache_write_1h: Some(900), ..Default::default() };
        assert_eq!(applied_retention(CacheRetention::Long, &honored), CacheRetention::Long);
        // Nothing written, nothing learned.
        assert_eq!(applied_retention(CacheRetention::Long, &Usage::default()), CacheRetention::Long);
        assert_eq!(applied_retention(CacheRetention::Short, &honored), CacheRetention::Short);
    }

    #[test]
    fn repeats_only_requests_whose_thinking_stays_the_same() {
        let adaptive = opus();
        assert!(replayable(&adaptive, ThinkingLevel::High));

        let mut budget = adaptive.clone();
        budget.reasoning = Reasoning::Budget;
        budget.thinking_levels = vec![ThinkingLevel::Off, ThinkingLevel::High];
        assert!(!replayable(&budget, ThinkingLevel::High));
        assert!(replayable(&budget, ThinkingLevel::Off));

        let mut gateway = adaptive.clone();
        gateway.api = Api::OpenAiCompletions;
        gateway.reasoning = Reasoning::Effort;
        gateway.thinking_levels = vec![ThinkingLevel::Off, ThinkingLevel::High];
        assert!(!replayable(&gateway, ThinkingLevel::High));
        assert!(replayable(&gateway, ThinkingLevel::Off));
    }
}
