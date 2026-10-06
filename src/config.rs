//! Agent directory, settings, and the model registry.
//!
//! Layout of the agent directory (`~/.viper`, overridable with `VIPER_DIR`):
//!
//! ```text
//! settings.json   user settings (merged with <project>/.viper/settings.json)
//! models.json     custom providers and models (e.g. a LiteLLM gateway)
//! AGENTS.md       global instructions added to every system prompt
//! skills/         user skills
//! sessions/       session files grouped by working directory
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

pub fn agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VIPER_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".viper")
}

pub fn settings_path() -> PathBuf {
    agent_dir().join("settings.json")
}

pub fn models_path() -> PathBuf {
    agent_dir().join("models.json")
}

pub fn sessions_dir() -> PathBuf {
    agent_dir().join("sessions")
}

pub fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(".viper").join("settings.json")
}

/// Expand a leading `~` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn read_json_file(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value = serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))?;
            Ok(Some(value))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// Recursively merge `overlay` into `base`; objects merge by key, everything else is replaced.
fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

// ---------------------------------------------------------------------------------------------
// Thinking levels
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThinkingLevel {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        ThinkingLevel::ALL.into_iter().find(|level| level.as_str().eq_ignore_ascii_case(s.trim())).ok_or_else(|| {
            anyhow!("unknown thinking level '{s}' (expected one of off, minimal, low, medium, high, xhigh, max)")
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

pub const DEFAULT_TOOLS: [&str; 7] = ["read", "bash", "edit", "write", "grep", "find", "ls"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Tokens kept free for the model's response; compaction triggers when the context exceeds
    /// `contextWindow - reserveTokens`.
    pub reserve_tokens: u64,
    /// Approximate number of recent tokens kept verbatim after compaction.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self { enabled: true, reserve_tokens: 16_384, keep_recent_tokens: 20_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrySettings {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self { enabled: true, max_retries: 3, base_delay_ms: 2_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// `provider/model-id` used for new sessions.
    pub default_model: Option<String>,
    pub default_thinking_level: Option<ThinkingLevel>,
    /// Enabled built-in tools.
    pub tools: Vec<String>,
    pub compaction: CompactionSettings,
    pub retry: RetrySettings,
    /// Shell used by the bash tool and `!` commands. Defaults to bash, falling back to sh.
    pub shell_path: Option<String>,
    /// Text appended to the system prompt (a literal or a path to a file).
    pub append_system_prompt: Option<String>,
    /// Additional skill directories.
    pub skill_paths: Vec<String>,
    pub enable_skills: bool,
    /// Interactive mode: collapse thinking blocks to a single line.
    pub hide_thinking: bool,
    /// Interactive mode: lines of tool output shown per tool call.
    pub tool_output_lines: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_model: None,
            default_thinking_level: None,
            tools: DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect(),
            compaction: CompactionSettings::default(),
            retry: RetrySettings::default(),
            shell_path: None,
            append_system_prompt: None,
            skill_paths: Vec::new(),
            enable_skills: true,
            hide_thinking: false,
            tool_output_lines: 10,
        }
    }
}

impl Settings {
    /// Load global settings merged with project settings (project wins).
    pub fn load(cwd: &Path) -> Result<Settings> {
        let mut merged = serde_json::to_value(Settings::default())?;
        if let Some(global) = read_json_file(&settings_path())? {
            merge_json(&mut merged, global);
        }
        if let Some(project) = read_json_file(&project_settings_path(cwd))? {
            merge_json(&mut merged, project);
        }
        serde_json::from_value(merged).context("invalid settings")
    }

    /// Update keys in the global settings file, preserving everything else in it.
    pub fn save_global(update: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Result<()> {
        let path = settings_path();
        let mut value = read_json_file(&path)?.unwrap_or_else(|| Value::Object(Default::default()));
        let Value::Object(map) = &mut value else {
            bail!("{} must contain a JSON object", path.display());
        };
        update(map);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&value)? + "\n")
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

/// Wire protocol used to talk to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Api {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
}

impl fmt::Display for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Api::AnthropicMessages => "anthropic-messages",
            Api::OpenAiCompletions => "openai-completions",
        })
    }
}

/// How a model exposes reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reasoning {
    /// No reasoning controls.
    None,
    /// Anthropic adaptive thinking controlled by `output_config.effort`.
    Adaptive,
    /// Anthropic extended thinking with a token budget (older models).
    Budget,
    /// OpenAI-style `reasoning_effort` parameter.
    Effort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthHeader {
    /// `x-api-key: <key>` (Anthropic API).
    XApiKey,
    /// `Authorization: Bearer <key>` (LiteLLM and OpenAI-compatible gateways).
    Bearer,
}

/// Prices in dollars per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// A fully resolved model, ready to be called.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub api: Api,
    pub base_url: String,
    #[serde(skip)]
    pub api_key: Option<ConfigValue>,
    pub auth_header: AuthHeader,
    #[serde(skip)]
    pub headers: BTreeMap<String, ConfigValue>,
    /// Extra top-level request body fields merged into every request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Value>,
    pub context_window: u64,
    pub max_tokens: u64,
    pub reasoning: Reasoning,
    pub thinking_levels: Vec<ThinkingLevel>,
    pub images: bool,
    pub cost: ModelCost,
    /// Mark tools with `eager_input_streaming` so large tool inputs stream as they are generated.
    pub eager_input_streaming: bool,
    /// Send prompt-caching breakpoints.
    pub cache_control: bool,
}

impl Model {
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Clamp `level` to the closest level this model supports, preferring lower levels.
    pub fn clamp_thinking(&self, level: ThinkingLevel) -> ThinkingLevel {
        if self.thinking_levels.is_empty() {
            return ThinkingLevel::Off;
        }
        if self.thinking_levels.contains(&level) {
            return level;
        }
        self.thinking_levels
            .iter()
            .copied()
            .filter(|candidate| *candidate <= level)
            .max()
            .or_else(|| self.thinking_levels.iter().copied().min())
            .unwrap_or(ThinkingLevel::Off)
    }

    pub fn compute_cost(&self, usage: &mut crate::message::Usage) {
        let per = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        usage.cost.input = per(usage.input, self.cost.input);
        usage.cost.output = per(usage.output, self.cost.output);
        usage.cost.cache_read = per(usage.cache_read, self.cost.cache_read);
        usage.cost.cache_write = per(usage.cache_write, self.cost.cache_write);
        usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    }
}

/// A models.json value that may come from the environment or a command (API keys and header
/// values). Resolved at request time so rotating keys and credential commands work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValue {
    Literal(String),
    Env(String),
    /// Output of a shell command (`!command` in models.json).
    Command(String),
}

impl ConfigValue {
    /// Parse `$VAR` / `${VAR}`, `!command`, or a literal.
    pub fn parse(value: &str) -> ConfigValue {
        let value = value.trim();
        if let Some(command) = value.strip_prefix('!') {
            return ConfigValue::Command(command.trim().to_string());
        }
        if let Some(name) = value.strip_prefix("${").and_then(|rest| rest.strip_suffix('}')) {
            return ConfigValue::Env(name.to_string());
        }
        if let Some(name) = value.strip_prefix('$')
            && !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return ConfigValue::Env(name.to_string());
        }
        ConfigValue::Literal(value.to_string())
    }

    pub fn resolve(&self) -> Result<Option<String>> {
        match self {
            ConfigValue::Literal(value) => Ok(Some(value.clone())),
            ConfigValue::Env(name) => Ok(std::env::var(name).ok().filter(|v| !v.is_empty())),
            ConfigValue::Command(command) => {
                let output = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .with_context(|| format!("failed to run `{command}`"))?;
                if !output.status.success() {
                    bail!("`{command}` failed: {}", String::from_utf8_lossy(&output.stderr).trim());
                }
                let key = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok((!key.is_empty()).then_some(key))
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            ConfigValue::Literal(_) => "literal key".to_string(),
            ConfigValue::Env(name) => format!("${name}"),
            ConfigValue::Command(command) => format!("!{command}"),
        }
    }
}

// --- models.json ------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelsFile {
    #[serde(default)]
    providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderConfig {
    base_url: Option<String>,
    api: Option<Api>,
    api_key: Option<String>,
    auth_header: Option<AuthHeader>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelConfig {
    id: String,
    name: Option<String>,
    /// Built-in Anthropic model whose metadata this model inherits. Defaults to the built-in model
    /// that this id names, if any (see `inferred_builtin`).
    base: Option<String>,
    api: Option<Api>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    reasoning: Option<Reasoning>,
    thinking_levels: Option<Vec<ThinkingLevel>>,
    images: Option<bool>,
    cost: Option<ModelCost>,
    eager_input_streaming: Option<bool>,
    cache_control: Option<bool>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    extra_body: Option<Value>,
}

// --- Built-in catalog -------------------------------------------------------------------------

struct BuiltinModel {
    id: &'static str,
    name: &'static str,
    context_window: u64,
    max_tokens: u64,
    reasoning: Reasoning,
    levels: &'static [ThinkingLevel],
    cost: ModelCost,
}

use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off, Xhigh};

/// Adaptive-thinking models that cannot disable thinking.
const ALWAYS_THINKING: &[ThinkingLevel] = &[Low, Medium, High, Xhigh, Max];
/// Adaptive-thinking models that accept `thinking: {type: "disabled"}`.
const OPTIONAL_THINKING: &[ThinkingLevel] = &[Off, Low, Medium, High, Xhigh, Max];
const BUDGET_THINKING: &[ThinkingLevel] = &[Off, Minimal, Low, Medium, High];

const fn cost(input: f64, output: f64, cache_read: f64) -> ModelCost {
    // Five-minute cache writes cost 1.25x the input price.
    ModelCost { input, output, cache_read, cache_write: input * 1.25 }
}

const BUILTIN_ANTHROPIC: &[BuiltinModel] = &[
    BuiltinModel {
        id: "claude-opus-5-5",
        name: "Claude Opus 5.5",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: ALWAYS_THINKING,
        cost: cost(4.0, 20.0, 0.20),
    },
    BuiltinModel {
        id: "claude-sonnet-5-5",
        name: "Claude Sonnet 5.5",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: ALWAYS_THINKING,
        cost: cost(2.0, 10.0, 0.20),
    },
    BuiltinModel {
        id: "claude-fable-5-1",
        name: "Claude Fable 5.1",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: ALWAYS_THINKING,
        cost: cost(10.0, 50.0, 0.25),
    },
    BuiltinModel {
        id: "claude-fable-5",
        name: "Claude Fable 5",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: ALWAYS_THINKING,
        cost: cost(10.0, 50.0, 1.0),
    },
    BuiltinModel {
        id: "claude-opus-5",
        name: "Claude Opus 5",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: OPTIONAL_THINKING,
        cost: cost(5.0, 25.0, 0.50),
    },
    BuiltinModel {
        id: "claude-sonnet-5",
        name: "Claude Sonnet 5",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: OPTIONAL_THINKING,
        cost: cost(2.0, 10.0, 0.20),
    },
    BuiltinModel {
        id: "claude-opus-4-8",
        name: "Claude Opus 4.8",
        context_window: 1_000_000,
        max_tokens: 128_000,
        reasoning: Reasoning::Adaptive,
        levels: OPTIONAL_THINKING,
        cost: cost(5.0, 25.0, 0.50),
    },
    BuiltinModel {
        id: "claude-haiku-4-5",
        name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_tokens: 64_000,
        reasoning: Reasoning::Budget,
        levels: BUDGET_THINKING,
        cost: cost(1.0, 5.0, 0.10),
    },
];

/// Output tokens requested per response unless a model sets `maxTokens` explicitly.
const DEFAULT_REQUEST_MAX_TOKENS: u64 = 64_000;

fn builtin(id: &str) -> Option<&'static BuiltinModel> {
    BUILTIN_ANTHROPIC.iter().find(|model| model.id == id)
}

/// The built-in model a gateway model id refers to: the id itself, its last path segment
/// (`anthropic/claude-opus-5-5`), or its last dotted segment (Bedrock's
/// `us.anthropic.claude-opus-5-5`).
fn inferred_builtin(id: &str) -> Option<&'static BuiltinModel> {
    let segment = id.rsplit('/').next().unwrap_or(id);
    builtin(id).or_else(|| builtin(segment)).or_else(|| builtin(segment.rsplit('.').next().unwrap_or(segment)))
}

pub(crate) fn builtin_anthropic_models() -> Vec<Model> {
    let (api_key, auth_header) = if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        (ConfigValue::Env("ANTHROPIC_API_KEY".into()), AuthHeader::XApiKey)
    } else if std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some() {
        (ConfigValue::Env("ANTHROPIC_AUTH_TOKEN".into()), AuthHeader::Bearer)
    } else {
        (ConfigValue::Env("ANTHROPIC_API_KEY".into()), AuthHeader::XApiKey)
    };
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    BUILTIN_ANTHROPIC
        .iter()
        .map(|b| Model {
            provider: "anthropic".into(),
            id: b.id.into(),
            name: b.name.into(),
            api: Api::AnthropicMessages,
            base_url: base_url.clone(),
            api_key: Some(api_key.clone()),
            auth_header,
            headers: BTreeMap::new(),
            extra_body: None,
            context_window: b.context_window,
            max_tokens: b.max_tokens.min(DEFAULT_REQUEST_MAX_TOKENS),
            reasoning: b.reasoning,
            thinking_levels: b.levels.to_vec(),
            images: true,
            cost: b.cost,
            eager_input_streaming: true,
            cache_control: true,
        })
        .collect()
}

fn resolve_custom_model(provider_name: &str, provider: &ProviderConfig, config: &ModelConfig) -> Result<Model> {
    let base_url = provider
        .base_url
        .clone()
        .ok_or_else(|| anyhow!("provider '{provider_name}' in models.json needs a baseUrl"))?;
    let api = config.api.or(provider.api).unwrap_or(Api::OpenAiCompletions);
    let inherited = match &config.base {
        Some(base) => Some(builtin(base).ok_or_else(|| {
            anyhow!("model '{}' in provider '{provider_name}' names unknown base model '{base}'", config.id)
        })?),
        None => inferred_builtin(&config.id),
    };

    let reasoning = config.reasoning.unwrap_or(match (inherited, api) {
        (Some(b), Api::AnthropicMessages) => b.reasoning,
        (Some(_), Api::OpenAiCompletions) => Reasoning::Effort,
        (None, _) => Reasoning::None,
    });
    let thinking_levels = config.thinking_levels.clone().unwrap_or_else(|| match reasoning {
        Reasoning::None => Vec::new(),
        Reasoning::Adaptive | Reasoning::Budget => match inherited {
            Some(b) if b.reasoning == reasoning => b.levels.to_vec(),
            _ if reasoning == Reasoning::Budget => BUDGET_THINKING.to_vec(),
            _ => OPTIONAL_THINKING.to_vec(),
        },
        Reasoning::Effort => match inherited {
            // Claude through an OpenAI-compatible gateway: reasoning_effort covers low..high.
            Some(b) => b.levels.iter().copied().filter(|level| *level <= High).collect(),
            None => vec![Off, Low, Medium, High],
        },
    });
    let auth_header = provider.auth_header.unwrap_or(AuthHeader::Bearer);
    let headers =
        provider.headers.iter().chain(&config.headers).map(|(k, v)| (k.clone(), ConfigValue::parse(v))).collect();

    Ok(Model {
        provider: provider_name.to_string(),
        id: config.id.clone(),
        name: config
            .name
            .clone()
            .or_else(|| inherited.map(|b| b.name.to_string()))
            .unwrap_or_else(|| config.id.clone()),
        api,
        base_url,
        api_key: provider.api_key.as_deref().map(ConfigValue::parse),
        auth_header,
        headers,
        extra_body: config.extra_body.clone(),
        context_window: config.context_window.or(inherited.map(|b| b.context_window)).unwrap_or(128_000),
        max_tokens: config
            .max_tokens
            .or(inherited.map(|b| b.max_tokens.min(DEFAULT_REQUEST_MAX_TOKENS)))
            .unwrap_or(16_384),
        reasoning,
        thinking_levels,
        images: config.images.unwrap_or(inherited.is_some()),
        cost: config.cost.or(inherited.map(|b| b.cost)).unwrap_or_default(),
        // Proxies may reject the field; opt in per model.
        eager_input_streaming: config.eager_input_streaming.unwrap_or(false),
        cache_control: config.cache_control.unwrap_or(inherited.is_some()),
    })
}

/// All models known to viper: built-in Anthropic models plus models.json providers.
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    models: Vec<Model>,
}

impl ModelRegistry {
    pub fn load() -> Result<ModelRegistry> {
        let mut models = builtin_anthropic_models();
        let path = models_path();
        if let Some(value) = read_json_file(&path)? {
            let file: ModelsFile =
                serde_json::from_value(value).with_context(|| format!("invalid {}", path.display()))?;
            for (provider_name, provider) in &file.providers {
                if provider_name == "anthropic" && provider.models.is_empty() {
                    // Overrides for the built-in provider (e.g. a different base URL or key).
                    for model in models.iter_mut().filter(|m| m.provider == "anthropic") {
                        if let Some(url) = &provider.base_url {
                            model.base_url = url.clone();
                        }
                        if let Some(key) = &provider.api_key {
                            model.api_key = Some(ConfigValue::parse(key));
                        }
                        if let Some(auth) = provider.auth_header {
                            model.auth_header = auth;
                        }
                        model.headers.extend(provider.headers.iter().map(|(k, v)| (k.clone(), ConfigValue::parse(v))));
                    }
                    continue;
                }
                if provider_name == "anthropic" {
                    models.retain(|m| m.provider != "anthropic");
                }
                for config in &provider.models {
                    let model = resolve_custom_model(provider_name, provider, config)
                        .with_context(|| format!("in {}", path.display()))?;
                    models.retain(|m| m.key() != model.key());
                    models.push(model);
                }
            }
        }
        Ok(ModelRegistry { models })
    }

    #[cfg(test)]
    pub fn from_models(models: Vec<Model>) -> ModelRegistry {
        ModelRegistry { models }
    }

    /// Use `source` for every model of `provider`.
    pub fn set_api_key(&mut self, provider: &str, source: ConfigValue) {
        for model in self.models.iter_mut().filter(|m| m.provider == provider) {
            model.api_key = Some(source.clone());
        }
    }

    pub fn all(&self) -> &[Model] {
        &self.models
    }

    /// Models whose provider has a resolvable API key.
    pub fn available(&self) -> Vec<&Model> {
        self.models.iter().filter(|model| has_credentials(model)).collect()
    }

    /// Find a model by `provider/id`, exact id, or unique substring.
    pub fn find(&self, query: &str) -> Result<Model> {
        let query = query.trim();
        if let Some(model) = self.models.iter().find(|m| m.key() == query) {
            return Ok(model.clone());
        }
        let exact: Vec<&Model> = self.models.iter().filter(|m| m.id == query).collect();
        if let Some(model) = exact.iter().find(|m| has_credentials(m)).or(exact.first()) {
            return Ok((*model).clone());
        }
        let needle = query.to_lowercase();
        let matches: Vec<&Model> = self
            .models
            .iter()
            .filter(|m| m.key().to_lowercase().contains(&needle) || m.name.to_lowercase().contains(&needle))
            .collect();
        match matches.as_slice() {
            [] => bail!("no model matches '{query}' (run `viper --list-models`)"),
            [one] => Ok((*one).clone()),
            many => {
                let names: Vec<String> = many.iter().map(|m| m.key()).collect();
                bail!("'{query}' matches several models: {}", names.join(", "))
            }
        }
    }

    /// The model for a new session: settings default, else the first model with credentials.
    pub fn default_model(&self, settings: &Settings) -> Result<Model> {
        if let Some(key) = &settings.default_model {
            return self.find(key);
        }
        self.available().first().map(|m| (*m).clone()).ok_or_else(|| {
            anyhow!(
                "no model has credentials. Set ANTHROPIC_API_KEY or configure a provider in {}",
                models_path().display()
            )
        })
    }
}

pub fn has_credentials(model: &Model) -> bool {
    match &model.api_key {
        Some(ConfigValue::Env(name)) => std::env::var_os(name).is_some_and(|v| !v.is_empty()),
        Some(_) => true,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_value_parsing() {
        assert_eq!(ConfigValue::parse("$FOO"), ConfigValue::Env("FOO".into()));
        assert_eq!(ConfigValue::parse("${FOO_BAR}"), ConfigValue::Env("FOO_BAR".into()));
        assert_eq!(ConfigValue::parse("!pass show x"), ConfigValue::Command("pass show x".into()));
        assert_eq!(ConfigValue::parse("sk-123"), ConfigValue::Literal("sk-123".into()));
    }

    #[test]
    fn clamp_thinking_prefers_lower_supported_level() {
        let mut model = builtin_anthropic_models().remove(0);
        model.thinking_levels = ALWAYS_THINKING.to_vec();
        assert_eq!(model.clamp_thinking(Off), Low);
        assert_eq!(model.clamp_thinking(Minimal), Low);
        assert_eq!(model.clamp_thinking(High), High);
        model.thinking_levels = BUDGET_THINKING.to_vec();
        assert_eq!(model.clamp_thinking(Max), High);
    }

    #[test]
    fn custom_model_inherits_builtin_metadata() {
        let provider = ProviderConfig {
            base_url: Some("http://localhost:4000".into()),
            api: Some(Api::AnthropicMessages),
            api_key: Some("$LITELLM_API_KEY".into()),
            ..Default::default()
        };
        let config = ModelConfig { id: "anthropic/claude-opus-5-5".into(), ..Default::default() };
        let model = resolve_custom_model("litellm", &provider, &config).unwrap();
        assert_eq!(model.reasoning, Reasoning::Adaptive);
        assert_eq!(model.context_window, 1_000_000);
        assert!(model.images);
        assert_eq!(model.auth_header, AuthHeader::Bearer);

        let config = ModelConfig { id: "us.anthropic.claude-sonnet-5-5".into(), ..Default::default() };
        let model = resolve_custom_model("litellm", &provider, &config).unwrap();
        assert_eq!(model.name, "Claude Sonnet 5.5");

        let config = ModelConfig { id: "gpt-x".into(), api: Some(Api::OpenAiCompletions), ..Default::default() };
        let model = resolve_custom_model("litellm", &provider, &config).unwrap();
        assert_eq!(model.reasoning, Reasoning::None);
        assert!(model.thinking_levels.is_empty());
    }

    #[test]
    fn model_headers_merge_and_parse() {
        let provider = ProviderConfig {
            base_url: Some("http://localhost:4000".into()),
            headers: [("x-team".into(), "$TEAM_ID".into()), ("x-env".into(), "dev".into())].into(),
            ..Default::default()
        };
        let config =
            ModelConfig { id: "gpt-x".into(), headers: [("x-env".into(), "prod".into())].into(), ..Default::default() };
        let model = resolve_custom_model("litellm", &provider, &config).unwrap();
        assert_eq!(model.headers["x-team"], ConfigValue::Env("TEAM_ID".into()));
        assert_eq!(model.headers["x-env"], ConfigValue::Literal("prod".into()));
    }

    #[test]
    fn merge_json_overlays_objects() {
        let mut base = serde_json::json!({"a": 1, "b": {"c": 2, "d": 3}});
        merge_json(&mut base, serde_json::json!({"b": {"c": 5}, "e": 6}));
        assert_eq!(base, serde_json::json!({"a": 1, "b": {"c": 5, "d": 3}, "e": 6}));
    }
}
