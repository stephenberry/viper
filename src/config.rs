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

/// Record a `/login` in models.json: set the provider's base URL (`None` leaves it alone) and
/// remove a literal `apiKey`, since auth.json now holds the key. `$VAR` and `!command` keys are
/// left as they are.
pub fn save_login(provider: &str, base_url: Option<&str>) -> Result<()> {
    save_login_to(&models_path(), provider, base_url)
}

fn save_login_to(path: &Path, provider: &str, base_url: Option<&str>) -> Result<()> {
    let mut root = read_json_file(path)?.unwrap_or_else(|| Value::Object(Default::default()));
    let invalid = || anyhow!("{} has an unexpected shape: providers must be objects", path.display());
    let Value::Object(root_map) = &mut root else { return Err(invalid()) };
    let Value::Object(providers) = root_map.entry("providers").or_insert_with(|| Value::Object(Default::default()))
    else {
        return Err(invalid());
    };
    let Value::Object(entry) = providers.entry(provider).or_insert_with(|| Value::Object(Default::default())) else {
        return Err(invalid());
    };
    if let Some(url) = base_url {
        entry.insert("baseUrl".into(), Value::String(url.to_string()));
    }
    let literal_key = entry
        .get("apiKey")
        .and_then(Value::as_str)
        .is_some_and(|key| matches!(ConfigValue::parse(key), ConfigValue::Literal(_)));
    if literal_key {
        entry.remove("apiKey");
    }
    if entry.is_empty() {
        providers.remove(provider);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&root)? + "\n")
        .with_context(|| format!("failed to write {}", path.display()))
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
    /// Context size in tokens at which auto-compaction triggers, for models without their own
    /// `modelSettings` value. Unset means `contextWindow - reserveTokens`.
    pub auto_compact_window: Option<u64>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self { enabled: true, reserve_tokens: 16_384, keep_recent_tokens: 20_000, auto_compact_window: None }
    }
}

/// Settings for one model, keyed by `provider/model-id` under `modelSettings`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelSettings {
    /// Context size in tokens at which auto-compaction triggers (set with `/autocompact`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_window: Option<u64>,
}

/// Values `/autocompact` accepts, matching Claude Code.
const AUTO_COMPACT_WINDOW_RANGE: std::ops::RangeInclusive<u64> = 100_000..=1_000_000;

/// Parse an auto-compact window: a token count (`300000`), a size with a `k` or `M` suffix
/// (`300k`, `1M`, `1.5M`), or a bare number up to 1000 meaning thousands (`300`).
pub fn parse_auto_compact_window(input: &str) -> Result<u64> {
    let text = input.trim().to_ascii_lowercase();
    let (number, scale) = match text.strip_suffix('k') {
        Some(number) => (number, Some(1_000.0)),
        None => match text.strip_suffix('m') {
            Some(number) => (number, Some(1_000_000.0)),
            None => (text.as_str(), None),
        },
    };
    let value: f64 = number
        .trim()
        .replace('_', "")
        .parse()
        .ok()
        .filter(|v: &f64| v.is_finite() && *v > 0.0)
        .ok_or_else(|| anyhow!("'{}' is not a token count (try 300k or 1M)", input.trim()))?;
    let scale = scale.unwrap_or(if value <= 1_000.0 { 1_000.0 } else { 1.0 });
    check_auto_compact_window((value * scale).round() as u64)
}

/// Validate an exact auto-compact window in tokens.
pub fn check_auto_compact_window(tokens: u64) -> Result<u64> {
    if !AUTO_COMPACT_WINDOW_RANGE.contains(&tokens) {
        bail!("the auto-compact window must be between 100k and 1M tokens");
    }
    Ok(tokens)
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
    /// Per-model settings keyed by `provider/model-id`.
    pub model_settings: BTreeMap<String, ModelSettings>,
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
            model_settings: BTreeMap::new(),
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

    /// Persist a model's auto-compact window in the global settings (`None` removes it).
    pub fn save_auto_compact_window(model_key: &str, window: Option<u64>) -> Result<()> {
        Settings::save_global(|map| {
            let models = map.entry("modelSettings").or_insert_with(|| Value::Object(Default::default()));
            let Value::Object(models) = models else { return };
            match window {
                Some(window) => {
                    let entry = models.entry(model_key).or_insert_with(|| Value::Object(Default::default()));
                    if let Value::Object(entry) = entry {
                        entry.insert("autoCompactWindow".into(), Value::from(window));
                    }
                }
                None => {
                    if let Some(Value::Object(entry)) = models.get_mut(model_key) {
                        entry.remove("autoCompactWindow");
                        if entry.is_empty() {
                            models.remove(model_key);
                        }
                    }
                }
            }
            if models.is_empty() {
                map.remove("modelSettings");
            }
        })
    }

    /// Persist whether auto-compaction is enabled in the global settings.
    pub fn save_auto_compaction(enabled: bool) -> Result<()> {
        Settings::save_global(|map| {
            let compaction = map.entry("compaction").or_insert_with(|| Value::Object(Default::default()));
            if let Value::Object(compaction) = compaction {
                compaction.insert("enabled".into(), Value::Bool(enabled));
            }
        })
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
    /// Model id sent to the API.
    pub id: String,
    /// Name viper selects the model by in place of `id`, so a provider can list the same API
    /// model more than once (for example through two endpoints of one gateway).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
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
    /// A placeholder carrying only the connection settings of a provider that has no models
    /// configured yet, with the defaults custom providers get: the OpenAI-compatible API and a
    /// bearer token. Used to check credentials during `/login`.
    pub fn connection(provider: &str, base_url: &str) -> Model {
        Model {
            provider: provider.to_string(),
            id: String::new(),
            alias: None,
            name: provider.to_string(),
            api: Api::OpenAiCompletions,
            base_url: base_url.to_string(),
            api_key: None,
            auth_header: AuthHeader::Bearer,
            headers: BTreeMap::new(),
            extra_body: None,
            context_window: 0,
            max_tokens: 0,
            reasoning: Reasoning::None,
            thinking_levels: Vec::new(),
            images: false,
            cost: ModelCost::default(),
            eager_input_streaming: false,
            cache_control: false,
        }
    }

    /// The model's name within its provider: its alias, else its id.
    pub fn local_id(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.id)
    }

    /// `provider/local-id`, which identifies the model in settings, sessions, and `--model`.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.local_id())
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
    alias: Option<String>,
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

/// A readable default name for a gateway model id: its last path segment without leading dotted
/// routing segments (a region or vendor, which contain no digits). For example
/// `gateway/us.amazon.nova-pro-v1:0` becomes `nova-pro-v1:0`.
fn short_model_name(id: &str) -> String {
    let mut name = id.rsplit('/').next().unwrap_or(id);
    while let Some((prefix, rest)) = name.split_once('.')
        && !rest.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
    {
        name = rest;
    }
    if name.is_empty() { id.to_string() } else { name.to_string() }
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
            alias: None,
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

/// The models a models.json provider lists. Each needs a distinct `alias` or `id`.
fn provider_models(provider_name: &str, provider: &ProviderConfig) -> Result<Vec<Model>> {
    let mut models: Vec<Model> = Vec::new();
    for config in &provider.models {
        let model = resolve_custom_model(provider_name, provider, config)?;
        if models.iter().any(|m| m.local_id() == model.local_id()) {
            bail!("model '{}' appears twice in provider '{provider_name}'; give one an \"alias\"", model.local_id());
        }
        models.push(model);
    }
    Ok(models)
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

    if config.alias.as_deref().is_some_and(|alias| alias.trim().is_empty()) {
        bail!("model '{}' in provider '{provider_name}' has an empty alias", config.id);
    }
    Ok(Model {
        provider: provider_name.to_string(),
        id: config.id.clone(),
        alias: config.alias.clone(),
        name: config
            .name
            .clone()
            .or_else(|| inherited.map(|b| b.name.to_string()))
            .unwrap_or_else(|| short_model_name(&config.id)),
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
                for model in
                    provider_models(provider_name, provider).with_context(|| format!("in {}", path.display()))?
                {
                    models.retain(|m| m.key() != model.key());
                    models.push(model);
                }
            }
        }
        let mut registry = ModelRegistry { models };
        registry.apply_auth(&crate::auth::AuthStore::load()?);
        Ok(registry)
    }

    /// Use keys saved by `/login`; they take precedence over `apiKey` in models.json.
    pub fn apply_auth(&mut self, auth: &crate::auth::AuthStore) {
        for model in &mut self.models {
            if let Some(key) = auth.api_key(&model.provider) {
                model.api_key = Some(ConfigValue::Literal(key.to_string()));
            }
        }
    }

    /// Provider names, in the order their models are listed.
    pub fn providers(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for model in &self.models {
            if !names.contains(&model.provider.as_str()) {
                names.push(&model.provider);
            }
        }
        names
    }

    /// A model of `provider`, for its connection settings (base URL, API, auth header).
    pub fn provider_model(&self, provider: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.provider == provider)
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

    /// Find a model by `provider/id`, exact id, or unique substring. Substring matches prefer
    /// models with credentials, so a short query is not ambiguous because of unusable providers.
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
        let usable: Vec<&Model> = matches.iter().copied().filter(|m| has_credentials(m)).collect();
        let matches = if usable.is_empty() { matches } else { usable };
        match matches.as_slice() {
            [] => bail!("no model matches '{query}' (run `viper --list-models`)"),
            [one] => Ok((*one).clone()),
            many => {
                let names: Vec<String> = many.iter().map(|m| m.key()).collect();
                bail!("'{query}' matches several models: {}", names.join(", "))
            }
        }
    }

    /// Short label for display: the model's name, plus its provider when another usable model
    /// has the same name.
    /// Short label for display: the model's name, plus its provider (or, within one provider,
    /// its alias or id) when another usable model has the same name.
    pub fn label(&self, model: &Model) -> String {
        let available = self.available();
        let twins: Vec<&&Model> =
            available.iter().filter(|other| other.name == model.name && other.key() != model.key()).collect();
        if twins.is_empty() {
            model.name.clone()
        } else if twins.iter().any(|other| other.provider == model.provider) {
            format!("{} ({})", model.name, model.local_id())
        } else {
            format!("{} ({})", model.name, model.provider)
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

/// Fail with an actionable message when `model` has no usable API key.
pub fn ensure_credentials(model: &Model) -> Result<()> {
    if has_credentials(model) {
        return Ok(());
    }
    let expected = model.api_key.as_ref().map(ConfigValue::describe).unwrap_or_else(|| "an apiKey".into());
    bail!(
        "no API key for provider '{}' (expected {expected}); choose another model or configure the provider in {}",
        model.provider,
        models_path().display()
    )
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
    fn find_prefers_models_with_credentials() {
        let builtin = builtin_anthropic_models().into_iter().find(|m| m.id == "claude-sonnet-5-5").unwrap();
        let unusable = Model { api_key: None, ..builtin.clone() };
        let usable = Model { provider: "gateway".into(), api_key: Some(ConfigValue::Literal("key".into())), ..builtin };
        let registry = ModelRegistry::from_models(vec![unusable.clone(), usable]);
        assert_eq!(registry.find("sonnet-5-5").unwrap().provider, "gateway");
        assert!(ensure_credentials(&unusable).unwrap_err().to_string().contains("no API key for provider 'anthropic'"));
    }

    #[test]
    fn parses_auto_compact_windows() {
        assert_eq!(parse_auto_compact_window("300k").unwrap(), 300_000);
        assert_eq!(parse_auto_compact_window("1M").unwrap(), 1_000_000);
        assert_eq!(parse_auto_compact_window(" 250 ").unwrap(), 250_000);
        assert_eq!(parse_auto_compact_window("150000").unwrap(), 150_000);
        assert_eq!(parse_auto_compact_window("0.5m").unwrap(), 500_000);
        assert!(parse_auto_compact_window("50k").is_err());
        assert!(parse_auto_compact_window("2M").is_err());
        assert!(parse_auto_compact_window("lots").is_err());
    }

    #[test]
    fn short_names_drop_routing_prefixes() {
        assert_eq!(short_model_name("us.amazon.nova-pro-v1:0"), "nova-pro-v1:0");
        assert_eq!(short_model_name("gateway/openai.gpt-5.6-terra"), "gpt-5.6-terra");
        assert_eq!(short_model_name("eu-west.xai.grok-4.6"), "grok-4.6");
        assert_eq!(short_model_name("gpt-oss-120b"), "gpt-oss-120b");
        assert_eq!(short_model_name("llama3.3-70b"), "llama3.3-70b");
    }

    #[test]
    fn labels_add_the_provider_only_for_shared_names() {
        let builtin = builtin_anthropic_models().into_iter().find(|m| m.id == "claude-opus-5-5").unwrap();
        let key = Some(ConfigValue::Literal("key".into()));
        let direct = Model { provider: "direct".into(), api_key: key.clone(), ..builtin.clone() };
        let gateway = Model { provider: "gateway".into(), api_key: key, ..builtin.clone() };
        let unusable = Model { provider: "other".into(), api_key: None, ..builtin };
        let registry = ModelRegistry::from_models(vec![direct.clone(), unusable.clone()]);
        assert_eq!(registry.label(&direct), "Claude Opus 5.5");
        let registry = ModelRegistry::from_models(vec![direct.clone(), gateway, unusable]);
        assert_eq!(registry.label(&direct), "Claude Opus 5.5 (direct)");
    }

    #[test]
    fn aliases_let_a_provider_list_a_model_twice() {
        let id = "gateway.anthropic.claude-opus-5-5";
        let mut provider = ProviderConfig {
            base_url: Some("http://localhost:4000".into()),
            api: Some(Api::AnthropicMessages),
            models: vec![
                ModelConfig { id: id.into(), ..Default::default() },
                ModelConfig { id: id.into(), api: Some(Api::OpenAiCompletions), ..Default::default() },
            ],
            ..Default::default()
        };
        let err = provider_models("gw", &provider).unwrap_err().to_string();
        assert!(err.contains("give one an \"alias\""), "{err}");

        provider.models[1].alias = Some("opus-openai".into());
        let models = provider_models("gw", &provider).unwrap();
        assert_eq!(models[1].key(), "gw/opus-openai");
        assert_eq!(models[1].id, id);
        assert_eq!(models[1].api, Api::OpenAiCompletions);

        let registry = ModelRegistry::from_models(
            models.into_iter().map(|m| Model { api_key: Some(ConfigValue::Literal("k".into())), ..m }).collect(),
        );
        let opus = registry.find("gw/opus-openai").unwrap();
        assert_eq!(registry.label(&opus), "Claude Opus 5.5 (opus-openai)");
    }

    #[test]
    fn auth_keys_override_models_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth = crate::auth::AuthStore::default();
        auth.set_api_key("gw", "from-auth");
        auth.save_to(&dir.path().join("auth.json")).unwrap();
        let auth = crate::auth::AuthStore::load_from(&dir.path().join("auth.json")).unwrap();

        let model =
            Model { api_key: Some(ConfigValue::Env("UNSET_VAR".into())), ..Model::connection("gw", "http://x") };
        let mut registry = ModelRegistry::from_models(vec![model]);
        registry.apply_auth(&auth);
        assert_eq!(registry.all()[0].api_key, Some(ConfigValue::Literal("from-auth".into())));
    }

    #[test]
    fn save_login_sets_url_and_drops_literal_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"providers": {"gw": {"baseUrl": "http://old", "apiKey": "sk-secret", "models": [{"id": "m"}]},
                              "other": {"baseUrl": "http://o", "apiKey": "$OTHER_KEY"}}}"#,
        )
        .unwrap();
        save_login_to(&path, "gw", Some("https://new")).unwrap();
        save_login_to(&path, "other", None).unwrap();
        save_login_to(&path, "fresh", Some("https://fresh")).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["providers"]["gw"]["baseUrl"], "https://new");
        assert!(saved["providers"]["gw"].get("apiKey").is_none());
        assert_eq!(saved["providers"]["gw"]["models"][0]["id"], "m");
        assert_eq!(saved["providers"]["other"]["apiKey"], "$OTHER_KEY");
        assert_eq!(saved["providers"]["fresh"]["baseUrl"], "https://fresh");
    }

    #[test]
    fn merge_json_overlays_objects() {
        let mut base = serde_json::json!({"a": 1, "b": {"c": 2, "d": 3}});
        merge_json(&mut base, serde_json::json!({"b": {"c": 5}, "e": 6}));
        assert_eq!(base, serde_json::json!({"a": 1, "b": {"c": 5, "d": 3}, "e": 6}));
    }
}
