//! A provider's report of how much its API key has spent and may still spend.
//!
//! Gateways that cap spending report it at an endpoint, set per provider as `budgetUrl` in
//! models.json; without one, the common endpoints in `DEFAULT_PATHS` are tried. The response uses
//! LiteLLM's field names (`spend`, `max_budget`, `budget_duration`, `budget_reset_at`), at the top
//! level or, as in LiteLLM's `/key/info`, under `info`; a `remaining` field is used when present.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::Model;
use crate::provider::{apply_auth, send};

#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    /// Dollars spent in the current budget period.
    pub spend: f64,
    /// The cap on spending per period, if one is set.
    pub max_budget: Option<f64>,
    /// Dollars left in this period; `None` without a cap.
    pub remaining: Option<f64>,
    /// How long a budget period lasts, e.g. `30d`.
    pub duration: Option<String>,
    /// When spending resets.
    pub reset_at: Option<String>,
}

/// Tried in order, on the base URL's host, for a provider without a `budgetUrl`: the calling
/// key's budget as some gateways report it, then LiteLLM's key information.
const DEFAULT_PATHS: &[&str] = &["/me/budget", "/key/info"];

/// Ask `model`'s provider for its budget, authenticating as model requests do.
pub async fn fetch(client: &reqwest::Client, model: &Model) -> Result<Budget> {
    let api_key = match &model.api_key {
        Some(source) => source.resolve()?,
        None => None,
    }
    .ok_or_else(|| anyhow!("no API key for provider '{}'; run /login {} first", model.provider, model.provider))?;
    if let Some(budget_url) = &model.budget_url {
        return fetch_from(client, model, &api_key, budget_url).await;
    }
    // The built-in provider is Anthropic's API, which reports no budget.
    if model.provider == "anthropic" {
        bail!("Anthropic's API does not report a budget");
    }
    let mut failures = Vec::new();
    for path in DEFAULT_PATHS {
        match fetch_from(client, model, &api_key, path).await {
            Ok(budget) => return Ok(budget),
            Err(err) => failures.push(format!("{path}: {err:#}")),
        }
    }
    bail!(
        "found no budget endpoint ({}); set \"budgetUrl\" for provider '{}' in models.json",
        failures.join("; "),
        model.provider
    )
}

async fn fetch_from(client: &reqwest::Client, model: &Model, api_key: &str, budget_url: &str) -> Result<Budget> {
    let url = resolve_url(&model.base_url, budget_url)?;
    let builder = client.get(url.clone()).timeout(std::time::Duration::from_secs(30));
    let response = send(apply_auth(builder, model, api_key)?, &CancellationToken::new()).await?;
    let body: Value = response.json().await.with_context(|| format!("unexpected response from {url}"))?;
    parse(&body).with_context(|| format!("unexpected response from {url}"))
}

/// `budget_url` as given when it is a full URL; otherwise relative to `base_url`, so `/path` is
/// on the base URL's host.
fn resolve_url(base_url: &str, budget_url: &str) -> Result<reqwest::Url> {
    let base = reqwest::Url::parse(base_url).with_context(|| format!("invalid base URL {base_url}"))?;
    base.join(budget_url).with_context(|| format!("invalid budgetUrl {budget_url}"))
}

fn parse(body: &Value) -> Result<Budget> {
    if let Some(error) = body.get("error").filter(|error| !error.is_null()) {
        let message = error.get("message").and_then(Value::as_str).or(error.as_str());
        bail!("{}", message.map_or_else(|| error.to_string(), str::to_string));
    }
    let fields = body.get("info").filter(|info| info.is_object()).unwrap_or(body);
    let number = |key: &str| fields.get(key).and_then(Value::as_f64);
    let text = |key: &str| fields.get(key).and_then(Value::as_str).map(str::to_string);
    let spend = number("spend").ok_or_else(|| anyhow!("no \"spend\" field"))?;
    // A gateway may report a cap of zero or none at all for keys without one.
    let capped = fields.get("has_budget").and_then(Value::as_bool).unwrap_or(true);
    let max_budget = number("max_budget").filter(|_| capped);
    let remaining = number("remaining").filter(|_| capped).or(max_budget.map(|max| max - spend));
    Ok(Budget { spend, max_budget, remaining, duration: text("budget_duration"), reset_at: text("budget_reset_at") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_top_level_and_litellm_key_info_shapes() {
        let budget =
            parse(&json!({"user": "u", "spend": 36.05, "has_budget": true, "max_budget": 50.0, "remaining": 13.95, "budget_duration": null}))
                .unwrap();
        assert_eq!(
            budget,
            Budget { spend: 36.05, max_budget: Some(50.0), remaining: Some(13.95), duration: None, reset_at: None }
        );

        let key_info = parse(&json!({"key": "k", "info": {"spend": 10.0, "max_budget": 25.0, "budget_duration": "30d",
                                                          "budget_reset_at": "2026-11-01T00:00:00Z"}}))
        .unwrap();
        assert_eq!(key_info.remaining, Some(15.0));
        assert_eq!(key_info.duration.as_deref(), Some("30d"));
        assert_eq!(key_info.reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));

        let uncapped = parse(&json!({"spend": 3.0, "has_budget": false, "max_budget": null})).unwrap();
        assert_eq!((uncapped.max_budget, uncapped.remaining), (None, None));
    }

    #[test]
    fn reports_errors_and_missing_spend() {
        assert_eq!(parse(&json!({"error": "token revoked"})).unwrap_err().to_string(), "token revoked");
        assert_eq!(parse(&json!({"error": {"message": "nope"}})).unwrap_err().to_string(), "nope");
        assert!(parse(&json!({"max_budget": 5.0})).is_err());
    }

    #[tokio::test]
    async fn tries_common_endpoints_without_a_budget_url() {
        use crate::config::ConfigValue;
        use crate::testing::{MockResponse, MockServer};
        let key_info = json!({"info": {"spend": 4.0, "max_budget": 10.0}}).to_string();
        let server = MockServer::start(vec![
            MockResponse::error(404, "not found"),
            MockResponse { status: 200, body: key_info },
        ])
        .await;
        let model = crate::config::Model {
            api_key: Some(ConfigValue::Literal("k".into())),
            ..crate::config::Model::connection("gw", &server.url)
        };
        let budget = fetch(&reqwest::Client::new(), &model).await.unwrap();
        assert_eq!((budget.spend, budget.remaining), (4.0, Some(6.0)));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn resolves_paths_against_the_base_urls_host() {
        let resolve = |base: &str, path: &str| resolve_url(base, path).unwrap().to_string();
        assert_eq!(resolve("https://gw.example/v1", "/me/budget"), "https://gw.example/me/budget");
        assert_eq!(resolve("https://gw.example", "/key/info"), "https://gw.example/key/info");
        assert_eq!(resolve("https://gw.example", "https://other.example/b"), "https://other.example/b");
    }
}
