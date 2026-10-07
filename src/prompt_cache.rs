//! The economics of the prompt cache.
//!
//! A cached prompt prefix lives for its retention's lifetime after it was last used. A request
//! that finds it gone writes the whole context again at the cache-write price (1.25x the input
//! price, 2x for one-hour retention) instead of reading it at about a tenth. Two things follow:
//!
//! - **Warming.** When a tool call runs longer than the cache lasts, the agent repeats its last
//!   request with a one-token output limit shortly before expiry: the provider reads the cached
//!   prefix, which renews its lifetime, at the read price. This module decides whether and when
//!   that pays off; the agent sends the requests (`Agent::keep_cache_warm`).
//! - **Stale warnings.** After a long pause, the next message pays for the rewrite; the
//!   interactive UI warns under the input when that is expensive (`stale_cache`).

use std::time::{Duration, Instant};

use crate::config::{Api, CacheRetention, Model, Reasoning, ThinkingLevel};
use crate::message::Usage;

/// A refresh is sent only when it is expected to save at least this many dollars.
pub const MINIMUM_SAVINGS: f64 = 0.05;

/// Warming stops this long after the request that wrote the cache, bounding what a tool that
/// never finishes can cost.
pub const MAX_WARMING_AGE: Duration = Duration::from_secs(60 * 60);

/// Warn before a message rewrites an expired cache when that costs at least this many dollars
/// more than reading it would have.
const STALE_WARNING_COST: f64 = 0.25;

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
    let prices = PrefixPrices::of(model, retention, prompt_tokens)?;
    let missed = (prices.rewrite - prices.read).max(0.0);
    let refresh = prices.read + model.cost.output / 1_000_000.0;
    Some(missed - refresh)
}

/// What sending a prompt prefix costs, in dollars.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PrefixPrices {
    /// Reading it from the cache.
    read: f64,
    /// Writing it to the cache again after the cache expired.
    rewrite: f64,
}

impl PrefixPrices {
    /// `None` when the model has no prices.
    fn of(model: &Model, retention: CacheRetention, tokens: u64) -> Option<PrefixPrices> {
        let cost = &model.cost;
        if cost.input <= 0.0 && cost.cache_read <= 0.0 {
            return None;
        }
        let rewrite_price = match retention {
            CacheRetention::Short if cost.cache_write > 0.0 => cost.cache_write,
            CacheRetention::Short => cost.input,
            CacheRetention::Long => cost.input * 2.0,
        };
        let per = |price: f64| tokens as f64 * price / 1_000_000.0;
        Some(PrefixPrices { read: per(cost.cache_read), rewrite: per(rewrite_price) })
    }
}

/// A cache entry that has expired, and what the next request pays because of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaleCache {
    /// Time since the entry was last used.
    pub idle: Duration,
    /// Tokens the next request sends again.
    pub tokens: u64,
    /// Dollars the next request pays for them.
    pub rewrite_cost: f64,
    /// Dollars it would have paid had the entry still been cached.
    pub cached_cost: f64,
}

/// The expired cache entry of `tokens` last used `idle` ago, when rewriting it costs enough more
/// than reading it to warrant a warning.
pub fn stale_cache(model: &Model, retention: CacheRetention, tokens: u64, idle: Duration) -> Option<StaleCache> {
    if !model.cache_control || idle <= retention.ttl() {
        return None;
    }
    let prices = PrefixPrices::of(model, retention, tokens)?;
    (prices.rewrite - prices.read >= STALE_WARNING_COST).then_some(StaleCache {
        idle,
        tokens,
        rewrite_cost: prices.rewrite,
        cached_cost: prices.read,
    })
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
    fn warns_about_large_expired_caches() {
        let model = opus();
        let six_minutes = Duration::from_secs(6 * 60);
        // 400k tokens of Opus 5.5: $2.00 to rewrite at $5 per million, $0.08 to read at $0.20.
        let stale = stale_cache(&model, CacheRetention::Short, 400_000, six_minutes).unwrap();
        assert!((stale.rewrite_cost - 2.0).abs() < 1e-9 && (stale.cached_cost - 0.08).abs() < 1e-9);
        assert_eq!((stale.tokens, stale.idle), (400_000, six_minutes));

        // Still cached, cheap to rewrite, or kept for an hour: no warning.
        assert_eq!(stale_cache(&model, CacheRetention::Short, 400_000, Duration::from_secs(4 * 60)), None);
        assert_eq!(stale_cache(&model, CacheRetention::Short, 40_000, six_minutes), None);
        assert_eq!(stale_cache(&model, CacheRetention::Long, 400_000, six_minutes), None);
        let mut unpriced = model.clone();
        unpriced.cost = Default::default();
        assert_eq!(stale_cache(&unpriced, CacheRetention::Short, 400_000, six_minutes), None);
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
