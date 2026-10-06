//! Model discovery: add the models a provider's server offers to models.json.
//!
//! The server's list comes from `GET /v1/models`. LiteLLM also reports each model's limits,
//! prices, and capabilities through `/v1/model/info`, which fills in the new entries; other
//! servers get entries with just an id, which inherit from a built-in model when the id names one.
//! Existing entries are never changed, and configured models the server no longer lists are only
//! reported.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::{Api, Model, ModelCost, ModelRegistry, models_path, names_builtin_model, update_json_object};
use crate::provider::{apply_auth, send, v1_url};

/// A model a server lists, with the metadata it reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListedModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub images: Option<bool>,
    pub reasoning: Option<bool>,
    pub cost: Option<ModelCost>,
}

/// What a sync changed in models.json.
#[derive(Debug, Default, PartialEq)]
pub struct SyncReport {
    /// Ids added to the provider, in the server's order.
    pub added: Vec<String>,
    /// Configured ids the server did not list.
    pub unlisted: Vec<String>,
}

impl SyncReport {
    pub fn describe(&self, provider: &str) -> String {
        let mut text = match self.added.as_slice() {
            [] => format!("models.json already lists every model {provider} offers."),
            added => format!("Added {} model(s) to {provider}: {}.", added.len(), added.join(", ")),
        };
        if !self.unlisted.is_empty() {
            text.push_str(&format!(" Not listed by the server (left in models.json): {}.", self.unlisted.join(", ")));
        }
        text
    }
}

/// List `provider`'s models on its server and add the new ones to models.json.
pub async fn sync(client: &reqwest::Client, registry: &ModelRegistry, provider: &str) -> Result<SyncReport> {
    let model = registry
        .provider_model(provider)
        .ok_or_else(|| anyhow!("unknown provider '{provider}' (configured: {})", registry.providers().join(", ")))?;
    let listed = list_server_models(client, model).await?;
    save_models(&models_path(), provider, &listed)
}

/// Fetch the server's models for the provider that `model` belongs to.
async fn list_server_models(client: &reqwest::Client, model: &Model) -> Result<Vec<ListedModel>> {
    let api_key = match &model.api_key {
        Some(source) => source.resolve()?,
        None => None,
    }
    .ok_or_else(|| anyhow!("no API key for provider '{}'; run /login {} first", model.provider, model.provider))?;
    let get = async |url: String, query: Option<(&str, String)>| -> Result<Value> {
        let mut builder = client.get(&url).timeout(std::time::Duration::from_secs(30));
        if let Some(query) = query {
            builder = builder.query(&[query]);
        }
        let response = send(apply_auth(builder, model, &api_key)?, &CancellationToken::new()).await?;
        response.json().await.with_context(|| format!("unexpected response from {url}"))
    };

    let mut models = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = get(v1_url(&model.base_url, "models"), after.clone().map(|id| ("after_id", id)))
            .await
            .context("could not list the server's models")?;
        models.extend(parse_listing(&page));
        // Anthropic pages its list; OpenAI-compatible servers return everything at once.
        match page.get("last_id").and_then(Value::as_str) {
            Some(last) if page["has_more"] == true && after.as_deref() != Some(last) => after = Some(last.to_string()),
            _ => break,
        }
    }
    // Only LiteLLM has this endpoint; without it, entries carry just their ids.
    if let Ok(info) = get(v1_url(&model.base_url, "model/info"), None).await {
        merge_model_info(&mut models, &info);
    }
    Ok(models)
}

fn parse_listing(page: &Value) -> Vec<ListedModel> {
    let entries = page.get("data").and_then(Value::as_array).into_iter().flatten();
    entries
        .filter_map(|entry| {
            Some(ListedModel {
                id: entry.get("id")?.as_str()?.to_string(),
                name: entry.get("display_name").and_then(Value::as_str).map(str::to_string),
                context_window: entry.get("max_input_tokens").and_then(Value::as_u64),
                max_tokens: entry.get("max_output_tokens").and_then(Value::as_u64),
                ..Default::default()
            })
        })
        .collect()
}

/// Fill in metadata from LiteLLM's `/v1/model/info`, which has an entry per deployment.
fn merge_model_info(models: &mut [ListedModel], info: &Value) {
    let entries = info.get("data").and_then(Value::as_array).into_iter().flatten();
    for entry in entries {
        let Some(name) = entry.get("model_name").and_then(Value::as_str) else { continue };
        let Some(model) = models.iter_mut().find(|m| m.id == name) else { continue };
        let info = &entry["model_info"];
        let number = |key: &str| info.get(key).and_then(Value::as_f64);
        let flag = |key: &str| info.get(key).and_then(Value::as_bool);
        model.context_window = info["max_input_tokens"].as_u64().or(model.context_window);
        model.max_tokens = info["max_output_tokens"].as_u64().or(info["max_tokens"].as_u64()).or(model.max_tokens);
        model.images = flag("supports_vision").or(model.images);
        model.reasoning = flag("supports_reasoning").or(model.reasoning);
        if let (Some(input), Some(output)) = (number("input_cost_per_token"), number("output_cost_per_token")) {
            model.cost = Some(ModelCost {
                input: per_million(input),
                output: per_million(output),
                cache_read: number("cache_read_input_token_cost").map_or(0.0, per_million),
                cache_write: number("cache_creation_input_token_cost").map_or(0.0, per_million),
            });
        }
    }
}

/// A per-token price as dollars per million tokens, without floating-point noise.
fn per_million(per_token: f64) -> f64 {
    (per_token * 1e12).round() / 1e6
}

/// The models.json entry for a newly listed model. Models named after a built-in Claude model
/// keep the provider's API and inherit everything the server does not report; other models use
/// the OpenAI-compatible API, which gateways offer for every model.
fn model_entry(model: &ListedModel, provider_api: Option<Api>) -> Value {
    let mut entry = Map::new();
    entry.insert("id".into(), json!(model.id));
    let builtin = names_builtin_model(&model.id);
    if !builtin {
        if let Some(name) = model.name.as_ref().filter(|name| **name != model.id) {
            entry.insert("name".into(), json!(name));
        }
        if provider_api.unwrap_or(Api::OpenAiCompletions) != Api::OpenAiCompletions {
            entry.insert("api".into(), json!(Api::OpenAiCompletions));
        }
    }
    if let Some(window) = model.context_window {
        entry.insert("contextWindow".into(), json!(window));
    }
    if let Some(max_tokens) = model.max_tokens {
        entry.insert("maxTokens".into(), json!(max_tokens));
    }
    if !builtin {
        if model.images == Some(true) {
            entry.insert("images".into(), json!(true));
        }
        if model.reasoning == Some(true) {
            entry.insert("reasoning".into(), json!("effort"));
        }
    }
    if let Some(cost) = model.cost {
        let mut prices = Map::new();
        prices.insert("input".into(), json!(cost.input));
        prices.insert("output".into(), json!(cost.output));
        if cost.cache_read > 0.0 {
            prices.insert("cacheRead".into(), json!(cost.cache_read));
        }
        if cost.cache_write > 0.0 {
            prices.insert("cacheWrite".into(), json!(cost.cache_write));
        }
        entry.insert("cost".into(), Value::Object(prices));
    }
    Value::Object(entry)
}

/// Add the listed models that `provider` in the models.json at `path` does not have yet.
fn save_models(path: &Path, provider: &str, listed: &[ListedModel]) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    update_json_object(path, |root| {
        let Some(config) = root.get_mut("providers").and_then(|p| p.get_mut(provider)).and_then(Value::as_object_mut)
        else {
            bail!("provider '{provider}' is not in models.json; run /login {provider} first");
        };
        let provider_api = config.get("api").map(|api| serde_json::from_value::<Api>(api.clone())).transpose()?;
        let models = config.entry("models").or_insert_with(|| Value::Array(Vec::new()));
        let Some(models) = models.as_array_mut() else { bail!("\"models\" of '{provider}' must be a list") };
        if models.is_empty() && provider == "anthropic" {
            bail!("adding a models list to 'anthropic' would replace its built-in models");
        }
        let configured: Vec<String> =
            models.iter().filter_map(|m| m.get("id").and_then(Value::as_str)).map(str::to_string).collect();
        for model in listed.iter().filter(|m| !configured.contains(&m.id)) {
            if !report.added.contains(&model.id) {
                models.push(model_entry(model, provider_api));
                report.added.push(model.id.clone());
            }
        }
        for id in configured {
            if !listed.iter().any(|m| m.id == id) && !report.unlisted.contains(&id) {
                report.unlisted.push(id);
            }
        }
        Ok(())
    })?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing() -> Vec<ListedModel> {
        let mut models = parse_listing(&json!({"object": "list", "data": [
            {"id": "us.anthropic.claude-opus-5-5", "max_input_tokens": 1000000},
            {"id": "acme.chat-large", "max_input_tokens": 256000, "max_output_tokens": 32000},
            {"id": "acme.tiny"},
        ]}));
        merge_model_info(
            &mut models,
            &json!({"data": [
                {"model_name": "acme.chat-large", "model_info": {
                    "max_input_tokens": 262144, "max_output_tokens": 32768,
                    "input_cost_per_token": 1.5e-7, "output_cost_per_token": 5.28e-7,
                    "cache_read_input_token_cost": 1.5e-8,
                    "supports_vision": true, "supports_reasoning": true,
                }},
                {"model_name": "us.anthropic.claude-opus-5-5", "model_info": {
                    "max_output_tokens": 128000, "input_cost_per_token": 4.8e-6, "output_cost_per_token": 2.4e-5,
                    "supports_vision": true, "supports_reasoning": true,
                }},
                {"model_name": "not-listed", "model_info": {"max_input_tokens": 1}},
            ]}),
        );
        models
    }

    #[test]
    fn listings_merge_litellm_model_info() {
        let models = listing();
        assert_eq!(models.len(), 3);
        assert_eq!(models[1].context_window, Some(262_144));
        assert_eq!(models[1].max_tokens, Some(32_768));
        assert_eq!(models[1].cost, Some(ModelCost { input: 0.15, output: 0.528, cache_read: 0.015, cache_write: 0.0 }));
        assert_eq!(models[2], ListedModel { id: "acme.tiny".into(), ..Default::default() });
    }

    #[test]
    fn entries_inherit_built_in_claude_models() {
        let models = listing();
        let api = Some(Api::AnthropicMessages);
        assert_eq!(
            model_entry(&models[0], api),
            json!({"id": "us.anthropic.claude-opus-5-5", "contextWindow": 1000000, "maxTokens": 128000,
                   "cost": {"input": 4.8, "output": 24.0}})
        );
        assert_eq!(
            model_entry(&models[1], api),
            json!({"id": "acme.chat-large", "api": "openai-completions", "contextWindow": 262144, "maxTokens": 32768,
                   "images": true, "reasoning": "effort", "cost": {"input": 0.15, "output": 0.528, "cacheRead": 0.015}})
        );
        assert_eq!(model_entry(&models[2], Some(Api::OpenAiCompletions)), json!({"id": "acme.tiny"}));
    }

    #[test]
    fn saving_adds_only_new_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"providers": {"gateway": {"baseUrl": "http://localhost:4000", "api": "anthropic-messages", "models": [
                {"id": "acme.tiny", "name": "Tiny"},
                {"id": "acme.retired"}
            ]}}}"#,
        )
        .unwrap();
        let report = save_models(&path, "gateway", &listing()).unwrap();
        assert_eq!(report.added, ["us.anthropic.claude-opus-5-5", "acme.chat-large"]);
        assert_eq!(report.unlisted, ["acme.retired"]);

        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let models = saved["providers"]["gateway"]["models"].as_array().unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["acme.tiny", "acme.retired", "us.anthropic.claude-opus-5-5", "acme.chat-large"]);
        assert_eq!(models[0]["name"], "Tiny");

        // A second sync finds nothing new.
        assert!(save_models(&path, "gateway", &listing()).unwrap().added.is_empty());
        assert!(save_models(&path, "missing", &listing()).is_err());
    }
}
