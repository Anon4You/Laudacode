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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<String>,
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
}

impl Config {
    pub fn dir() -> PathBuf {
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
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))
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

        // Environment variables fill in blanks (and override per spec).
        if let Ok(v) = std::env::var("OPENAI_BASE_URL") {
            p.base_url = v;
        }
        if let Ok(v) = std::env::var("OPENAI_API_KEY") {
            p.api_key = v;
        }
        if let Ok(v) = std::env::var("OPENAI_MODEL") {
            p.model = v;
        }

        // CLI overrides beat everything.
        if let Some(v) = cli_base_url {
            p.base_url = v.to_string();
        }
        if let Some(v) = cli_api_key {
            p.api_key = v.to_string();
        }
        if let Some(v) = cli_model {
            p.model = v.to_string();
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

        Ok(ActiveProvider {
            name,
            base_url: p.base_url.trim_end_matches('/').to_string(),
            api_key: p.api_key,
            model: p.model,
            headers: p.headers,
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
