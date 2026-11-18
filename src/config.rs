use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Provider {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// Transport kind. "openai" (default) is the OpenAI-compatible client;
    /// "aitopia" and "powerbrain" are built-in free providers
    /// with their own request/response shapes (see src/api.rs).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Reasoning-effort hint for reasoning models ("low"|"medium"|"high").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl Provider {
    /// Whether this provider can function without an API key (local servers,
    /// built-in free providers, and generic custom endpoints).
    pub fn key_is_optional(&self) -> bool {
        self.base_url.contains("localhost")
            || self.base_url.contains("127.0.0.1")
            || self.base_url.is_empty()
            || self.kind != "openai"
    }
}

/// Named preset of defaults, activated with `--profile <name>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// suggest | auto-edit | full-auto
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "approval_mode"
    )]
    pub approval_policy: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "reasoning_effort"
    )]
    pub model_reasoning_effort: Option<String>,
}

/// User-defined specialist for the delegate team (`[agents.<name>]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomAgent {
    #[serde(default)]
    pub description: String,
    pub prompt: String,
    /// Tool names this role may use; empty means read-only set.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub read_only: bool,
}

/// Shell commands run automatically after a file-mutating tool succeeds
/// (`[hooks]`). A non-zero exit does not undo the edit; it is reported back to
/// the model as a warning so it can react (e.g. reformat then re-test).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hooks {
    /// Each entry is a shell command run via `sh -c` in the workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_edit: Vec<String>,
    /// Per-hook wall-clock limit in seconds (default 60).
    #[serde(default = "default_hook_timeout")]
    pub post_edit_timeout_secs: u64,
}

fn default_hook_timeout() -> u64 {
    60
}

/// Session guardrails (`[limits]`). Unset fields mean "no limit".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Stop making requests once the estimated session cost exceeds this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Stop once cumulative prompt+completion tokens exceed this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// How many times to retry a failed request before giving up, on top of
    /// the first attempt. Only transient failures are retried (connection
    /// errors, 408/409/429/5xx); a 401 or a malformed request fails fast.
    /// Defaults to 5 retries, i.e. up to 6 attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
}

/// USD per 1M tokens for one model. `[pricing]` overrides the built-in table
/// in `budget.rs`, keyed by full model name or any substring of it.
///
/// ```toml
/// [pricing."anthropic/claude-sonnet-4"]
/// input = 3.0
/// output = 15.0
/// ```
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ModelPrice {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
}

/// Per-model price overrides. Transparent so it reads as `[pricing.<key>]`
/// in TOML rather than an extra layer of nesting.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Pricing(pub BTreeMap<String, ModelPrice>);

impl Pricing {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Structured (JSONL) run log (`[logging]`). Off unless a file is configured.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Logging {
    #[serde(default)]
    pub enabled: bool,
    /// Log file path. `~` expands to the home directory. Omit to log to
    /// stderr only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Also mirror the same events to stderr when a `file` is set.
    #[serde(default)]
    pub stderr: bool,
}

impl Logging {
    /// Logging is on whenever it is enabled: an explicit `file` if given,
    /// otherwise stderr. Both sinks can be used at once.
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Whether a file sink is configured (as opposed to stderr-only).
    pub fn has_file(&self) -> bool {
        self.file
            .as_deref()
            .map(|f| !f.trim().is_empty())
            .unwrap_or(false)
    }
}

/// HTTP transport tuning (`[network]`), applied to the provider client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Network {
    /// Proxy URL, e.g. "http://127.0.0.1:8080" or "socks5://127.0.0.1:1080".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Path to a PEM bundle of extra trusted roots (corporate MITM, self-signed
    /// gateway). Requires the `rustls-tls` feature; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<String>,
    /// Assumed context window (tokens) used by the TUI context-left meter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Color theme name (see src/theme.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Ambient effect name (see src/effects.rs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Provider>,
    /// Per-tool allow/ask/deny rules with wildcards (see `permissions.rs`).
    #[serde(default)]
    pub permission: crate::permissions::Permissions,
    /// Custom specialists for the delegate tool (`[agents.<name>]`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, CustomAgent>,
    /// Post-edit automation (`[hooks]`).
    #[serde(default, skip_serializing_if = "is_default_hooks")]
    pub hooks: Hooks,
    /// Session guardrails (`[limits]`).
    #[serde(default, skip_serializing_if = "is_default_limits")]
    pub limits: Limits,
    /// Per-model price overrides in USD per 1M tokens (`[pricing]`).
    #[serde(default, skip_serializing_if = "Pricing::is_empty")]
    pub pricing: Pricing,
    /// External tool servers (`[mcp_servers.<name>]`), stdio transport only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, crate::mcp::ServerSpec>,
    /// Language servers (`[lsp_servers.<name>]`), stdio transport only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lsp_servers: BTreeMap<String, crate::lsp::LspSpec>,
    /// Structured run log (`[logging]`).
    #[serde(default, skip_serializing_if = "is_default_logging")]
    pub logging: Logging,
    /// Provider HTTP transport tuning (`[network]`).
    #[serde(default, skip_serializing_if = "is_default_network")]
    pub network: Network,
}

fn is_default_hooks(h: &Hooks) -> bool {
    h.post_edit.is_empty() && h.post_edit_timeout_secs == default_hook_timeout()
}
fn is_default_limits(l: &Limits) -> bool {
    // max_retries must be here too: omitting it made a config that set only
    // max_retries look "default", so the whole [limits] table was dropped
    // on save and the setting was silently lost.
    l.max_cost_usd.is_none() && l.max_tokens.is_none() && l.max_retries.is_none()
}
fn is_default_logging(l: &Logging) -> bool {
    !l.enabled && l.file.is_none() && !l.stderr
}
fn is_default_network(n: &Network) -> bool {
    n.proxy.is_none() && n.ca_bundle.is_none() && n.insecure.is_none()
}

/// Fully resolved provider settings ready for an API call.
#[derive(Debug, Clone)]
pub struct ActiveProvider {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    /// Transport kind; empty means OpenAI-compatible ("openai").
    pub kind: String,
    pub model: String,
    pub headers: BTreeMap<String, String>,
    /// Where each resolved field came from: "command line", "environment",
    /// "config file", or "none". Keys: base_url, api_key, model.
    pub sources: BTreeMap<String, String>,
    pub reasoning_effort: Option<String>,
}

/// Built-in keyless free providers (hardcoded, no API key). AITopia is the
/// out-of-the-box default chat provider; both names auto-provision when
/// requested on a blank config, so the app is usable with zero setup.
pub fn builtin_provider(name: &str) -> Option<Provider> {
    let (base_url, kind, model) = match name {
        "powerbrain" => (
            "https://powerbrainai.com/app/backend/api/api.php",
            "powerbrain",
            "gpt-5",
        ),
        "aitopia" => (
            "https://extensions.aitopia.ai/ai/send",
            "aitopia",
            "AITOPIA",
        ),
        _ => return None,
    };
    Some(Provider {
        base_url: base_url.into(),
        api_key: String::new(),
        kind: kind.into(),
        model: model.into(),
        headers: BTreeMap::new(),
        reasoning_effort: None,
    })
}

/// The keyless provider used when nothing at all selects one.
pub fn builtin_default_provider() -> Provider {
    builtin_provider("aitopia").expect("aitopia builtin exists")
}

impl Config {
    pub fn dir() -> PathBuf {
        // Test/power-user override keeps the real config untouched.
        if let Ok(p) = std::env::var("LAUDACODE_CONFIG_DIR") {
            return PathBuf::from(p);
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("laudacode")
    }

    pub fn toml_path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    pub fn json_path() -> PathBuf {
        Self::dir().join("config.json")
    }

    pub fn load() -> Result<Self> {
        // Explicit override first.
        if let Ok(p) = std::env::var("LAUDACODE_CONFIG") {
            let path = PathBuf::from(p);
            if path.exists() {
                return Self::read_from(&path);
            }
            return Ok(Self::default());
        }
        let t = Self::toml_path();
        let j = Self::json_path();
        if t.exists() {
            return Self::read_from(&t);
        }
        if j.exists() {
            return Self::read_from(&j);
        }
        Ok(Self::default())
    }

    fn read_from(path: &PathBuf) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => {
                serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
            }
            _ => toml::from_str(&raw).with_context(|| format!("parsing {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        fs::create_dir_all(&dir).context("creating config directory")?;
        // Preserve whichever format already exists; default to TOML.
        let json_exists = Self::json_path().exists();
        let toml_exists = Self::toml_path().exists();
        let use_json = json_exists && !toml_exists;
        let path = if use_json {
            Self::json_path()
        } else {
            Self::toml_path()
        };
        let raw = if use_json {
            serde_json::to_string_pretty(self)?.into_bytes()
        } else {
            let mut s = String::from(
                "# Laudacode config — managed by /provider commands\n# Edit freely.\n\n",
            );
            s.push_str(&toml::to_string_pretty(self)?);
            s.into_bytes()
        };
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        // The file holds API keys — restrict to owner-only on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Resolve which provider to actually use.
    ///
    /// Precedence: env vars > config file. `cli_*` flags beat everything.
    pub fn resolve_active(
        &self,
        cli_provider: Option<&str>,
        cli_base_url: Option<&str>,
        cli_api_key: Option<&str>,
        cli_model: Option<&str>,
    ) -> Result<ActiveProvider> {
        let name = cli_provider
            .map(|s| s.to_string())
            .or_else(|| std::env::var("LAUDACODE_PROVIDER").ok())
            .or_else(|| self.active_provider.clone())
            .unwrap_or_else(|| "default".to_string());

        let mut p = match self.providers.get(&name) {
            Some(p) => p.clone(),
            None => Provider::default(),
        };

        // If this run has NO provider info anywhere (no config, no env vars, no
        // CLI flags), fall back to the built-in keyless AITopia transport so
        // the app is usable out of the box without setup or an API key.
        // "default" is the name used when nothing selects a provider; any known
        // built-in free provider name also auto-provisions on a blank config.
        let nothing_configured = match self.providers.get(&name) {
            Some(c) => c.base_url.is_empty() && c.model.is_empty() && c.api_key.is_empty(),
            None => true,
        } && cli_base_url.is_none()
            && cli_api_key.is_none()
            && cli_model.is_none()
            && std::env::var("OPENAI_BASE_URL").is_err()
            && std::env::var("OPENAI_API_KEY").is_err()
            && std::env::var("OPENAI_MODEL").is_err();
        let provisioned_builtin =
            nothing_configured && (name == "default" || builtin_provider(&name).is_some());
        if provisioned_builtin {
            p = builtin_default_provider();
            if let Some(builtin) = builtin_provider(&name) {
                p = builtin;
            }
        }

        let mut sources: BTreeMap<String, String> =
            [("base_url", "none"), ("api_key", "none"), ("model", "none")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        if let Some(cfg_p) = self.providers.get(&name) {
            if !cfg_p.base_url.is_empty() {
                sources.insert("base_url".into(), "config file".into());
            }
            if !cfg_p.api_key.is_empty() {
                sources.insert("api_key".into(), "config file".into());
            }
            if !cfg_p.model.is_empty() {
                sources.insert("model".into(), "config file".into());
            }
        }

        // Environment variables fill in blanks (and override per spec).
        for (field, var) in [
            ("base_url", "OPENAI_BASE_URL"),
            ("api_key", "OPENAI_API_KEY"),
            ("model", "OPENAI_MODEL"),
        ] {
            if let Ok(v) = std::env::var(var) {
                match field {
                    "base_url" => p.base_url = v,
                    "api_key" => p.api_key = v,
                    _ => p.model = v,
                }
                sources.insert(field.into(), "environment".into());
            }
        }

        // CLI overrides beat everything.
        for (field, val) in [
            ("base_url", cli_base_url),
            ("api_key", cli_api_key),
            ("model", cli_model),
        ] {
            if let Some(v) = val {
                match field {
                    "base_url" => p.base_url = v.to_string(),
                    "api_key" => p.api_key = v.to_string(),
                    _ => p.model = v.to_string(),
                }
                sources.insert(field.into(), "command line".into());
            }
        }

        // The built-in keyless default transport (see `builtin_default_provider`) is
        // the out-of-the-box chat provider: when this run is entirely
        // unconfigured (fresh install or a request for a built-in free name),
        // it was provisioned above.
        if provisioned_builtin {
            sources.insert("base_url".into(), "built-in default".into());
            sources.insert("model".into(), "built-in default".into());
            sources.insert("api_key".into(), "none".into());
        }
        if p.base_url.is_empty() {
            p.base_url = crate::DEFAULT_BASE_URL.to_string();
        }
        // A named provider requested but not configured anywhere — after the
        // built-in default fallback and defaults above, only valid if env/CLI
        // filled in a model; otherwise fail loudly.
        if !self.providers.contains_key(&name) && p.model.is_empty() {
            bail!(
                "provider '{name}' not found. Add it with `/provider add {name}` \
                 or run `laudacode provider add`."
            );
        }
        if p.model.is_empty() {
            bail!(
                "no model set for '{name}'. Set OPENAI_MODEL, use --model, \
                 or configure the provider."
            );
        }
        if p.api_key.is_empty() && !p.key_is_optional() {
            bail!(
                "no API key for '{name}'. Set OPENAI_API_KEY, use --api-key, \
                 or configure the provider."
            );
        }

        // Reasoning effort: env beats config-level default beats provider.
        let mut effort = p
            .reasoning_effort
            .clone()
            .or_else(|| self.model_reasoning_effort.clone());
        if let Ok(v) = std::env::var("OPENAI_REASONING_EFFORT") {
            if !v.trim().is_empty() {
                effort = Some(v);
            }
        }

        Ok(ActiveProvider {
            name,
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key: p.api_key,
            kind: p.kind,
            model: p.model,
            headers: p.headers,
            sources,
            reasoning_effort: effort,
        })
    }
}

pub fn sanitize_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(anyhow!("provider name cannot be empty"));
    }
    let ok = n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!("provider name may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serializes tests that mutate process-wide env vars — cargo runs test
    /// threads in parallel and env races made this suite flaky.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn sanitize_accepts_reasonable_names() {
        assert_eq!(sanitize_name(" openrouter ").unwrap(), "openrouter");
        assert!(sanitize_name("my-provider_2.v1").is_ok());
    }

    #[test]
    fn sanitize_rejects_bad_names() {
        assert!(sanitize_name("").is_err());
        assert!(sanitize_name("  ").is_err());
        assert!(sanitize_name("has space").is_err());
        assert!(sanitize_name("slash/evil").is_err());
        assert!(sanitize_name("../escape").is_err());
    }

    #[test]
    fn limits_max_retries_survives_a_save() {
        // Regression: `is_default_limits` ignored `max_retries`, so a config
        // that set *only* max_retries looked untouched and the whole [limits]
        // table was skipped on save — the setting was silently lost.
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("lc-limits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LAUDACODE_CONFIG_DIR", &dir);

        let mut cfg = Config::default();
        cfg.limits.max_retries = Some(9);
        cfg.save().unwrap();

        let raw = std::fs::read_to_string(Config::toml_path()).unwrap();
        assert!(
            raw.contains("max_retries"),
            "[limits] was dropped on save; file was:\n{raw}"
        );
        assert_eq!(Config::load().unwrap().limits.max_retries, Some(9));

        // Same shape of bug: a non-default hook timeout must not vanish either.
        cfg.limits.max_retries = None;
        cfg.hooks.post_edit_timeout_secs = 42;
        cfg.save().unwrap();
        let raw = std::fs::read_to_string(Config::toml_path()).unwrap();
        assert!(
            raw.contains("post_edit_timeout_secs"),
            "hooks dropped:\n{raw}"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var("LAUDACODE_CONFIG_DIR");
    }

    #[test]
    fn mcp_servers_survive_a_save() {
        // `save` rewrites the whole file from the struct, so any section not
        // represented in `Config` is silently deleted. Same class of bug as
        // the `[limits]` regression above.
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("lc-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LAUDACODE_CONFIG_DIR", &dir);

        let mut cfg = Config::default();
        cfg.mcp_servers.insert(
            "files".to_string(),
            crate::mcp::ServerSpec {
                command: "mcp-server-filesystem".into(),
                args: vec!["--root".into(), ".".into()],
                plan: true,
                timeout_secs: 30,
                ..Default::default()
            },
        );
        cfg.save().unwrap();

        let back = Config::load().unwrap();
        let s = back
            .mcp_servers
            .get("files")
            .expect("mcp_servers dropped on save");
        assert_eq!(s.command, "mcp-server-filesystem");
        assert_eq!(s.args, vec!["--root", "."]);
        assert!(s.plan, "plan opt-in lost");
        assert_eq!(s.timeout_secs, 30);
        // `enabled` defaults to true, and a round-trip must not flip it.
        assert!(
            s.enabled,
            "an explicit-but-defaulted server came back disabled"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var("LAUDACODE_CONFIG_DIR");
    }

    #[test]
    fn lsp_servers_survive_a_save() {
        // Same reason as the mcp case above: `save` is a full rewrite, so a
        // missing `Config` field means the whole section is silently deleted
        // on the next `/theme` or provider change.
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("lc-lsp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LAUDACODE_CONFIG_DIR", &dir);

        let mut cfg = Config::default();
        cfg.lsp_servers.insert(
            "rust".to_string(),
            crate::lsp::LspSpec {
                command: "rust-analyzer".into(),
                filetypes: std::collections::BTreeMap::from([("rs".into(), "rust".into())]),
                args: vec!["--stdio".into()],
                timeout_secs: 20,
                diagnostics_secs: 5,
                ..Default::default()
            },
        );
        cfg.save().unwrap();

        let back = Config::load().unwrap();
        let s = back
            .lsp_servers
            .get("rust")
            .expect("lsp_servers dropped on save");
        assert_eq!(s.command, "rust-analyzer");
        assert_eq!(s.filetypes.get("rs").map(String::as_str), Some("rust"));
        assert_eq!(s.args, vec!["--stdio"]);
        assert_eq!(s.timeout_secs, 20);
        assert_eq!(s.diagnostics_secs, 5);
        assert!(
            s.enabled,
            "an explicit-but-defaulted server came back disabled"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var("LAUDACODE_CONFIG_DIR");
    }

    #[test]
    fn a_picked_model_is_remembered_across_saves() {
        // The provider switch was already persisted, so a model picked in the
        // TUI had to be persisted too — otherwise the app reopened on the
        // provider's old default and quietly ignored the choice.
        let _g = env_lock();
        let dir = std::env::temp_dir().join(format!("lc-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LAUDACODE_CONFIG_DIR", &dir);

        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider {
                base_url: "https://example/v1".into(),
                api_key: "k".into(),
                model: "old-default".into(),
                ..Default::default()
            },
        );
        cfg.active_provider = Some("p".into());

        // What SetModel does: write the pick onto the stored provider, save.
        cfg.providers.get_mut("p").unwrap().model = "picked-model".into();
        cfg.save().unwrap();

        // A fresh load must come back on the pick, not the old default.
        let back = Config::load().unwrap();
        assert_eq!(back.active_provider.as_deref(), Some("p"));
        let a = back.resolve_active(None, None, None, None).unwrap();
        assert_eq!(
            a.model, "picked-model",
            "the picked model was not remembered"
        );

        // Precedence is unchanged: an explicit env or CLI value still wins,
        // because the pick is a config-file-level default.
        std::env::set_var("OPENAI_MODEL", "env-model");
        let a = back.resolve_active(None, None, None, None).unwrap();
        assert_eq!(
            a.model, "env-model",
            "env must still beat the remembered pick"
        );
        let a = back
            .resolve_active(None, None, None, Some("cli-model"))
            .unwrap();
        assert_eq!(
            a.model, "cli-model",
            "CLI must still beat the remembered pick"
        );
        std::env::remove_var("OPENAI_MODEL");

        std::fs::remove_dir_all(&dir).ok();
        std::env::remove_var("LAUDACODE_CONFIG_DIR");
    }

    #[test]
    fn resolve_precedence_cli_beats_env_beats_config() {
        let _g = env_lock();
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider {
                base_url: "https://config.example/v1".into(),
                api_key: "cfg".into(),
                model: "cfg-model".into(),
                ..Default::default()
            },
        );
        // Config only.
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        assert_eq!(a.base_url, "https://config.example/v1");
        // Env beats config.
        std::env::set_var("OPENAI_MODEL", "env-model");
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        assert_eq!(a.model, "env-model");
        std::env::remove_var("OPENAI_MODEL");
        // CLI beats env.
        std::env::set_var("OPENAI_MODEL", "env-model");
        let a = cfg
            .resolve_active(Some("p"), None, None, Some("cli-model"))
            .unwrap();
        assert_eq!(a.model, "cli-model");
        std::env::remove_var("OPENAI_MODEL");
    }

    #[test]
    fn resolve_trailing_slash_trimmed_and_missing_provider_detected() {
        // `resolve_active` falls back to OPENAI_* env vars, and sibling tests
        // mutate them — serialize so this test sees a clean environment.
        let _g = env_lock();
        for var in [
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "OPENAI_MODEL",
            "LAUDACODE_PROVIDER",
        ] {
            std::env::remove_var(var);
        }
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider {
                base_url: "https://x.example/v1/".into(),
                api_key: "k".into(),
                model: "m".into(),
                ..Default::default()
            },
        );
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        assert_eq!(a.base_url, "https://x.example/v1");

        // Unknown provider with nothing filled in must fail loudly.
        let err = cfg.resolve_active(Some("ghost"), None, None, None);
        assert!(err.is_err());
    }

    #[test]
    fn unconfigured_fall_backs_to_builtin_default_provider() {
        let _g = env_lock();
        for var in [
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "OPENAI_MODEL",
            "LAUDACODE_PROVIDER",
        ] {
            std::env::remove_var(var);
        }
        let cfg = Config::default();
        // Fresh install, nothing configured anywhere → keyless aitopia.
        let a = cfg.resolve_active(None, None, None, None).unwrap();
        assert_eq!(a.kind, "aitopia");
        assert_eq!(a.model, "AITOPIA");
        assert_eq!(a.base_url, "https://extensions.aitopia.ai/ai/send");
        assert_eq!(
            a.sources.get("base_url").map(String::as_str),
            Some("built-in default")
        );
        assert_eq!(a.sources.get("api_key").map(String::as_str), Some("none"));
        // Requesting either built-in by name provisions it too.
        let a = cfg
            .resolve_active(Some("powerbrain"), None, None, None)
            .unwrap();
        assert_eq!(a.kind, "powerbrain");
        let a = cfg
            .resolve_active(Some("aitopia"), None, None, None)
            .unwrap();
        assert_eq!(a.kind, "aitopia");
        assert_eq!(a.model, "AITOPIA");
        // Unknown unconfigured providers still fail loudly.
        assert!(cfg.resolve_active(Some("ghost"), None, None, None).is_err());
        // A keyed setup via env takes precedence over the free default: any
        // env/CLI provider info voids the built-in fallback entirely.
        std::env::set_var("OPENAI_MODEL", "env-model");
        std::env::set_var("OPENAI_API_KEY", "env-key");
        let a = cfg
            .resolve_active(Some("aitopia"), None, None, None)
            .unwrap();
        std::env::remove_var("OPENAI_MODEL");
        std::env::remove_var("OPENAI_API_KEY");
        assert_eq!(a.model, "env-model");
        assert_eq!(
            a.sources.get("model").map(String::as_str),
            Some("environment")
        );
        assert_eq!(a.sources.get("base_url").map(String::as_str), Some("none"));
    }

    #[test]
    fn sources_track_where_values_came_from() {
        let _g = env_lock();
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider {
                base_url: "https://cfg.example/v1".into(),
                api_key: String::new(),
                model: "cfg-model".into(),
                ..Default::default()
            },
        );
        std::env::set_var("OPENAI_API_KEY", "env-key");
        let a = cfg
            .resolve_active(Some("p"), Some("https://cli.example/v1"), None, None)
            .unwrap();
        std::env::remove_var("OPENAI_API_KEY");

        assert_eq!(
            a.sources.get("base_url").map(String::as_str),
            Some("command line")
        );
        assert_eq!(
            a.sources.get("api_key").map(String::as_str),
            Some("environment")
        );
        assert_eq!(
            a.sources.get("model").map(String::as_str),
            Some("config file")
        );
        // No key material leaks into the source map.
        assert!(!serde_json::to_string(&a.sources)
            .unwrap()
            .contains("env-key"));
    }

    #[test]
    fn profile_accepts_reasoning_effort_alias() {
        let cfg: Config =
            toml::from_str("[profiles.fast]\nreasoning_effort = \"low\"").unwrap();
        assert_eq!(
            cfg.profiles["fast"].model_reasoning_effort.as_deref(),
            Some("low")
        );
    }

    #[test]
    fn reasoning_effort_resolves_from_provider_config_and_env() {
        let _g = env_lock();
        std::env::remove_var("OPENAI_REASONING_EFFORT");
        let mut cfg = Config::default();
        // Provider-level wins over config default when both set.
        cfg.providers.insert(
            "p".into(),
            Provider {
                base_url: "https://x/v1".into(),
                api_key: "k".into(),
                model: "m".into(),
                reasoning_effort: Some("low".into()),
                ..Default::default()
            },
        );
        cfg.model_reasoning_effort = Some("high".into());
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        assert_eq!(a.reasoning_effort.as_deref(), Some("low"));
        // Config-level default applies when the provider has none.
        cfg.providers.get_mut("p").unwrap().reasoning_effort = None;
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        assert_eq!(a.reasoning_effort.as_deref(), Some("high"));
        // Env beats everything.
        std::env::set_var("OPENAI_REASONING_EFFORT", "medium");
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        std::env::remove_var("OPENAI_REASONING_EFFORT");
        assert_eq!(a.reasoning_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn profiles_roundtrip_through_toml() {
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "fast".into(),
            Profile {
                provider: Some("groq".into()),
                model: Some("llama-3.3-70b".into()),
                approval_policy: Some("full-auto".into()),
                model_reasoning_effort: None,
            },
        );
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.profiles["fast"].provider.as_deref(), Some("groq"));
        // approval_mode (old key) still deserializes into approval_policy.
        let legacy: Config = toml::from_str("[profiles.x]\napproval_mode = \"suggest\"").unwrap();
        assert_eq!(
            legacy.profiles["x"].approval_policy.as_deref(),
            Some("suggest")
        );
    }
}
