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
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Reasoning-effort hint for reasoning models ("low"|"medium"|"high").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reasoning_effort: Option<String>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, Provider>,
}

/// Fully resolved provider settings ready for an API call.
#[derive(Debug, Clone)]
pub struct ActiveProvider {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub headers: BTreeMap<String, String>,
    /// Where each resolved field came from: "command line", "environment",
    /// "config file", or "none". Keys: base_url, api_key, model.
    pub sources: BTreeMap<String, String>,
    pub reasoning_effort: Option<String>,
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
            Some("json") => serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display())),
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
        let mut sources: BTreeMap<String, String> = [
            ("base_url", "none"),
            ("api_key", "none"),
            ("model", "none"),
        ]
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
        for (field, var) in [("base_url", "OPENAI_BASE_URL"), ("api_key", "OPENAI_API_KEY"), ("model", "OPENAI_MODEL")] {
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

        if !name.is_empty() && !self.providers.contains_key(&name) {
            // A named provider was requested but not configured — only valid if env/CLI filled it in.
            if p.base_url.is_empty() || p.model.is_empty() {
                bail!(
                    "provider '{name}' not found. Add it with `/provider add {name}` \
                     or run `laudacode provider add`."
                );
            }
        }
        if p.base_url.is_empty() {
            p.base_url = crate::DEFAULT_BASE_URL.to_string();
        }
        if p.model.is_empty() {
            bail!(
                "no model set for '{name}'. Set OPENAI_MODEL, use --model, \
                 or configure the provider."
            );
        }
        if p.api_key.is_empty() {
            bail!(
                "no API key for '{name}'. Set OPENAI_API_KEY, use --api-key, \
                 or configure the provider."
            );
        }

        // Reasoning effort: env beats config-level default beats provider.
        let mut effort = p.reasoning_effort.clone().or_else(|| self.model_reasoning_effort.clone());
        if let Ok(v) = std::env::var("OPENAI_REASONING_EFFORT") {
            if !v.trim().is_empty() {
                effort = Some(v);
            }
        }

        Ok(ActiveProvider {
            name,
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key: p.api_key,
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
    fn resolve_precedence_cli_beats_env_beats_config() {
        let _g = env_lock();
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider { base_url: "https://config.example/v1".into(), api_key: "cfg".into(), model: "cfg-model".into(), ..Default::default() },
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
        let a = cfg.resolve_active(Some("p"), None, None, Some("cli-model")).unwrap();
        assert_eq!(a.model, "cli-model");
        std::env::remove_var("OPENAI_MODEL");
    }

    #[test]
    fn resolve_trailing_slash_trimmed_and_missing_provider_detected() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider { base_url: "https://x.example/v1/".into(), api_key: "k".into(), model: "m".into(), ..Default::default() },
        );
        let a = cfg.resolve_active(Some("p"), None, None, None).unwrap();
        assert_eq!(a.base_url, "https://x.example/v1");

        // Unknown provider with nothing filled in must fail loudly.
        let err = cfg.resolve_active(Some("ghost"), None, None, None);
        assert!(err.is_err());
    }

    #[test]
    fn sources_track_where_values_came_from() {
        let _g = env_lock();
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            Provider { base_url: "https://cfg.example/v1".into(), api_key: String::new(), model: "cfg-model".into(), ..Default::default() },
        );
        std::env::set_var("OPENAI_API_KEY", "env-key");
        let a = cfg.resolve_active(Some("p"), Some("https://cli.example/v1"), None, None).unwrap();
        std::env::remove_var("OPENAI_API_KEY");

        assert_eq!(a.sources.get("base_url").map(String::as_str), Some("command line"));
        assert_eq!(a.sources.get("api_key").map(String::as_str), Some("environment"));
        assert_eq!(a.sources.get("model").map(String::as_str), Some("config file"));
        // No key material leaks into the source map.
        assert!(!serde_json::to_string(&a.sources).unwrap().contains("env-key"));
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
        let legacy: Config =
            toml::from_str("[profiles.x]\napproval_mode = \"suggest\"").unwrap();
        assert_eq!(legacy.profiles["x"].approval_policy.as_deref(), Some("suggest"));
    }
}
